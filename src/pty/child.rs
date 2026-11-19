//! The session's child process: an exit check that keeps its pid reserved, and the
//! teardown kill — the whole process tree first, then the child — with a bounded reap.
//!
//! **Why a tree kill, not `Child::kill`.** portable-pty kills only the direct child (the
//! shell): SIGHUP to it on unix, `TerminateProcess` on its one handle on Windows. Whatever
//! the shell started — an agent CLI typed into it, on Windows a `node.exe` under a
//! `cmd.exe` shim — is a descendant it does not touch. So teardown kills the tree itself:
//! on unix the shell's process group plus the terminal's foreground group (an agent run
//! from an interactive shell sits in its own job group and would otherwise die only if it
//! honours SIGHUP); on Windows the child's Job Object (`taskkill /T /F` on the shell when
//! there is none, or when the shell started a process before it joined the job). The job
//! holds every process started from the session, so on Windows teardown also kills
//! detached programs launched from it (an editor opened with `code .`, a browser) that
//! `taskkill /T` alone would miss; see `proc::job`.
//!
//! **Why not leave it to the PTY closing.** On Windows the pseudoconsole is closed only
//! when the master drops, after the kill, and whether that ends every attached client is
//! not something to rely on: it would make orphaned agents depend on drop order. A tree
//! kill on a pid that is still unreaped (so cannot have been reused) does not.
//!
//! **Why the pid stays reserved.** A group or tree kill by pid is safe only while the child
//! is unreaped. On unix [`ChildSlot::try_wait`] therefore reports an exit without reaping
//! (`waitid` with `WNOWAIT`): the zombie keeps the pid and its group id until teardown
//! has killed the group and reaps it. On Windows the open process handle does the same.
//! An `ECHILD` response means the host already reaped the child: its exit is recorded
//! with unknown status and it is never signalled. Teardown rechecks ownership immediately
//! before signalling. A host that reaps children concurrently must still coordinate with
//! teardown: portable PID-based signalling cannot make that check and the kill atomic.

#[cfg(windows)]
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{ExitInfo, SessionId};
use crate::proc;
#[cfg(windows)]
use crate::proc::job::Job;

type BoxedChild = Box<dyn portable_pty::Child + Send + Sync>;

/// How long teardown waits for the killed child before handing the reap to a background
/// thread (a child that cannot be killed must not hang the caller). The RPC child's
/// teardown uses the same bound.
pub(crate) const REAP_WAIT: Duration = Duration::from_secs(2);

/// The child and what is known about its exit.
pub(crate) struct ChildSlot {
    /// `None` once reaped or handed to a background reaper.
    child: Option<BoxedChild>,
    pid: Option<u32>,
    /// The session the child belongs to (log tags, the reaper thread's name).
    id: SessionId,
    /// The exit, once known.
    exit: Option<ExitInfo>,
    /// The child was waited for (or handed off): its pid may now belong to another
    /// process, so it is never signalled again.
    reaped: bool,
    /// Windows: the Job Object the child joined at spawn, if that worked.
    #[cfg(windows)]
    job: Option<Arc<Job>>,
}

impl ChildSlot {
    fn new(child: BoxedChild, id: SessionId) -> Self {
        Self {
            pid: child.process_id(),
            id,
            child: Some(child),
            exit: None,
            reaped: false,
            #[cfg(windows)]
            job: None,
        }
    }

    /// Log prefix: `[agent-link:<id>]`.
    fn tag(&self) -> String {
        format!("[agent-link:{}]", self.id)
    }

    /// The pid taken at spawn.
    pub(crate) fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The exit, if one is known.
    pub(crate) fn exit(&self) -> Option<ExitInfo> {
        self.exit
    }

    /// Non-blocking exit check; once it returns `Some` it keeps returning it. On unix the
    /// child is left unreaped (see the module docs).
    pub(crate) fn try_wait(&mut self) -> Option<ExitInfo> {
        if self.exit.is_none() {
            self.exit = self.check_exit();
        }
        self.exit
    }

    #[cfg(unix)]
    fn check_exit(&mut self) -> Option<ExitInfo> {
        if self.reaped {
            return None;
        }
        if let Some(pid) = self.pid {
            match peek_exit(pid) {
                Ok(exit) => return exit,
                Err(e) if e.raw_os_error() == Some(libc::ECHILD) => {
                    return Some(self.reaped_elsewhere());
                }
                // Fall back to a reaping check when the non-reaping query is unavailable.
                Err(e) => log::debug!("{} waitid({pid}) failed: {e}", self.tag()),
            }
        }
        self.reap_if_exited()
    }

    /// Windows keeps the process handle open after `try_wait`, so the pid stays reserved.
    #[cfg(not(unix))]
    fn check_exit(&mut self) -> Option<ExitInfo> {
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => Some(status.into()),
            _ => None,
        }
    }

    /// portable-pty's `try_wait`, which reaps on unix.
    #[cfg(unix)]
    fn reap_if_exited(&mut self) -> Option<ExitInfo> {
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => {
                self.reaped = true;
                self.child = None;
                Some(status.into())
            }
            Err(e) if e.raw_os_error() == Some(libc::ECHILD) => Some(self.reaped_elsewhere()),
            _ => None,
        }
    }

    #[cfg(unix)]
    fn reaped_elsewhere(&mut self) -> ExitInfo {
        log::warn!(
            "{} child {:?} was reaped outside this handle; it is never signalled",
            self.tag(),
            self.pid
        );
        self.reaped = true;
        self.child = None;
        let exit = self.exit.unwrap_or_default();
        self.exit = Some(exit);
        exit
    }

    /// Kills the tree and the child, then reaps it (bounded by [`REAP_WAIT`]). `fg` is the
    /// terminal's foreground process group (unix), read before the call. Does nothing to
    /// a child that is already reaped.
    pub(crate) fn kill_and_reap(&mut self, fg: Option<i32>) -> ExitInfo {
        self.kill_and_reap_with(fg, |slot, fg| {
            slot.kill_tree(fg);
            if let Some(child) = slot.child.as_mut() {
                let _ = child.kill();
            }
        })
    }

    /// The signal boundary is injectable so tests never signal an already-reaped PID.
    fn kill_and_reap_with(
        &mut self,
        fg: Option<i32>,
        signal: impl FnOnce(&mut Self, Option<i32>),
    ) -> ExitInfo {
        // Do not use sticky try_wait here: a host may have reaped the child since a
        // previous successful peek, or without making any exit query through us.
        #[cfg(unix)]
        if !self.reaped {
            if let Some(exit) = self.check_exit() {
                self.exit = Some(exit);
            }
        }
        if !self.reaped {
            signal(self, fg);
            self.reap();
        }
        self.exit.unwrap_or_default()
    }

    /// unix: SIGKILL the foreground job's group when it is not the child's own, then the
    /// child's group (the PTY spawn makes it a session leader with `setsid`).
    #[cfg(unix)]
    fn kill_tree(&mut self, fg: Option<i32>) {
        let own = self.pid.and_then(|pid| i32::try_from(pid).ok());
        if let Some(fg) = fg {
            // SAFETY: `getpgrp` has no preconditions.
            let host = unsafe { libc::getpgrp() };
            if fg > 1 && Some(fg) != own && fg != host {
                // SAFETY: a plain kill(2) of the group in front of the child's terminal.
                unsafe {
                    libc::kill(-fg, libc::SIGKILL);
                }
            }
        }
        if let Some(pid) = self.pid {
            proc::kill_tree(pid);
        }
    }

    /// Windows: terminate the child's Job Object — always, even after a confirmed exit
    /// (members may outlive the child) and without a pid. `taskkill /T /F` on the child
    /// first when it has a direct child outside the job (started before it joined) or that
    /// check fails, and instead when there is no job or terminating it fails; `taskkill`
    /// runs only while the child's exit is unconfirmed (see `proc::kill_tree`), and before
    /// the job kill because it walks from the live child.
    #[cfg(windows)]
    fn kill_tree(&mut self, _fg: Option<i32>) {
        let pid = self.pid;
        let Some(job) = self.job.clone() else {
            self.taskkill_unless_exited(pid);
            return;
        };
        let tag = self.tag();
        let mut taskkilled = false;
        if let Some(pid) = pid {
            match job.has_children_outside(pid) {
                Ok(false) => {}
                Ok(true) => {
                    log::debug!(
                        "{tag} child {pid} started a process outside its job; taskkill too"
                    );
                    taskkilled = self.taskkill_unless_exited(Some(pid));
                }
                Err(e) => {
                    log::debug!(
                        "{tag} could not compare child {pid}'s tree with its job ({e}); taskkill too"
                    );
                    taskkilled = self.taskkill_unless_exited(Some(pid));
                }
            }
        }
        if let Err(e) = job.terminate() {
            log::warn!("{tag} could not terminate child {pid:?}'s job: {e}; using taskkill");
            if !taskkilled {
                self.taskkill_unless_exited(pid);
            }
        }
    }

    /// `taskkill /T /F` on `pid` unless it is unknown or the child's exit is confirmed;
    /// whether it ran.
    #[cfg(windows)]
    fn taskkill_unless_exited(&mut self, pid: Option<u32>) -> bool {
        let Some(pid) = pid else {
            return false;
        };
        if self.try_wait().is_some() {
            return false;
        }
        proc::kill_tree(pid);
        true
    }

    #[cfg(not(any(unix, windows)))]
    fn kill_tree(&mut self, _fg: Option<i32>) {}

    /// Waits for the killed child up to [`REAP_WAIT`], then leaves the rest to a detached
    /// thread.
    fn reap(&mut self) {
        let tag = self.tag();
        self.reaped = true;
        let Some(mut child) = self.child.take() else {
            return;
        };
        let deadline = Instant::now() + REAP_WAIT;
        let mut pause = Duration::from_millis(1);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.exit = Some(status.into());
                    return;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(20));
                }
                Ok(None) => {
                    log::warn!(
                        "{tag} child {:?} still running {} s after the kill; reaping it in the background",
                        self.pid,
                        REAP_WAIT.as_secs()
                    );
                    let name = format!("agent-link-reaper-{}", self.id);
                    let reaper = std::thread::Builder::new().name(name).spawn(move || {
                        let _ = child.wait();
                    });
                    if let Err(e) = reaper {
                        log::warn!("{tag} could not start the reaper thread: {e}");
                    }
                    return;
                }
                Err(e) => {
                    log::warn!("{tag} could not reap child {:?}: {e}", self.pid);
                    return;
                }
            }
        }
    }
}

/// Whether `pid` has exited, without reaping it: `waitid(WEXITED | WNOHANG | WNOWAIT)`.
/// The RPC child's exit check uses it too.
#[cfg(unix)]
pub(crate) fn peek_exit(pid: u32) -> std::io::Result<Option<ExitInfo>> {
    // SAFETY: siginfo_t is plain data; waitid fills it (si_pid stays 0 while the child
    // runs).
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    // SAFETY: `info` is a valid out-pointer; WNOWAIT leaves the child waitable.
    if unsafe { libc::waitid(libc::P_PID, pid, &mut info, flags) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: waitid succeeded, so `info` is initialised.
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    // Only an exit counts: some kernels (macOS among them) can report a stopped or continued
    // child here even though only WEXITED was asked for.
    Ok(match info.si_code {
        libc::CLD_EXITED => Some(ExitInfo {
            // SAFETY: as above; for CLD_EXITED si_status is the exit code.
            status: Some(unsafe { info.si_status() }),
        }),
        libc::CLD_KILLED | libc::CLD_DUMPED => Some(ExitInfo { status: None }),
        _ => None,
    })
}

/// Owns a freshly spawned child until the session is complete: if a later spawn step fails,
/// dropping the guard kills the tree and reaps the child, so no failed spawn leaves a
/// process behind.
pub(crate) struct ChildGuard(Option<ChildSlot>);

impl ChildGuard {
    /// Guards `child` of session `id`.
    pub(crate) fn new(child: BoxedChild, id: SessionId) -> Self {
        Self(Some(ChildSlot::new(child, id)))
    }

    /// Takes the child out; the guard no longer kills it.
    pub(crate) fn disarm(mut self) -> ChildSlot {
        self.0.take().expect("ChildGuard is disarmed only once")
    }

    /// Puts the child in a Job Object from `make_job` (normally [`Job::create`]) and
    /// returns it; teardown then terminates the job. On failure (e.g. access denied when
    /// the host's own job forbids nesting) one `warn` is logged and `None` returned:
    /// teardown falls back to `taskkill`.
    #[cfg(windows)]
    pub(crate) fn join_job(&mut self, make_job: fn() -> std::io::Result<Job>) -> Option<Arc<Job>> {
        let slot = self.0.as_mut()?;
        let handle = slot.child.as_ref().and_then(|c| c.as_raw_handle());
        let joined = match handle {
            Some(handle) => make_job().and_then(|job| job.assign(handle).map(|()| job)),
            None => Err(std::io::Error::other("the child has no process handle")),
        };
        match joined {
            Ok(job) => {
                let job = Arc::new(job);
                slot.job = Some(Arc::clone(&job));
                Some(job)
            }
            Err(e) => {
                let tag = slot.tag();
                if slot.try_wait().is_some() {
                    log::debug!("{tag} child exited before it could join a Job Object: {e}");
                } else {
                    log::warn!(
                        "{tag} child {:?} runs without a Job Object ({e}); teardown falls back to taskkill",
                        slot.pid
                    );
                }
                None
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut slot) = self.0.take() {
            log::debug!(
                "{} spawn failed after the child started; killing pid {:?}",
                slot.tag(),
                slot.pid
            );
            slot.kill_and_reap(None);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn externally_reaped_child() -> ChildSlot {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .spawn()
            .expect("spawn child");
        let pid = child.id();
        let slot = ChildSlot::new(Box::new(child), SessionId(1));
        let mut status = 0;
        // SAFETY: this test owns the child. The signal hook below never sends signals.
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, &mut status, 0) },
            pid as i32
        );
        slot
    }

    #[test]
    fn an_externally_reaped_child_is_exited_and_never_signalled() {
        let mut slot = externally_reaped_child();
        assert_eq!(slot.try_wait(), Some(ExitInfo { status: None }));
        assert_eq!(slot.try_wait(), Some(ExitInfo { status: None }));
        assert!(slot.reaped && slot.child.is_none());
        slot.kill_and_reap_with(None, |_, _| panic!("must not signal a reaped child"));
    }

    #[test]
    fn teardown_checks_for_external_reaping_without_an_earlier_exit_query() {
        let mut slot = externally_reaped_child();
        assert_eq!(
            slot.kill_and_reap_with(None, |_, _| panic!("must not signal a reaped child")),
            ExitInfo { status: None }
        );
        assert!(slot.reaped && slot.child.is_none());
    }

    #[test]
    fn teardown_checks_ownership_even_with_a_cached_exit() {
        let mut slot = externally_reaped_child();
        slot.exit = Some(ExitInfo { status: Some(7) });
        assert_eq!(
            slot.kill_and_reap_with(None, |_, _| panic!("must not signal a reaped child")),
            ExitInfo { status: Some(7) }
        );
        assert!(slot.reaped && slot.child.is_none());
    }
}
