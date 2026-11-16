//! The RPC child process: an exit check that keeps its pid reserved, the tree kill and a
//! bounded reap — a PTY session's child rules (`pty::child`) for a `std::process::Child`.
//!
//! **Why the pid stays reserved.** A group or tree kill by pid is safe only while the child
//! is unreaped. On unix [`ChildState::try_wait`] reports an exit without reaping (`waitid`
//! with `WNOWAIT`): the zombie keeps the pid and its process group's id, so teardown can
//! still kill the group — and whatever the child left running in it — and then reaps. A
//! "reaped ⇒ skip the group kill" rule would leave those processes behind. On Windows the
//! open process handle keeps the pid. Only a child that something else in the host
//! process reaped (its own `SIGCHLD` handling) is never signalled: its pid may already name
//! another process. Teardown checks for that right before the kill, not only in `try_wait`.

use std::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::proc;
use crate::pty::{REAP_WAIT, SessionId};

/// The child and what is known about its exit.
pub(super) struct ChildState {
    /// `None` once reaped or handed to the reaper thread.
    child: Option<Child>,
    pid: u32,
    /// The handle's id (the reaper thread's name).
    id: SessionId,
    /// Log prefix: `[tag:key]`.
    tag: String,
    /// The exit once known: the code, `None` for a signal (or a code nobody can tell).
    exit: Option<Option<i32>>,
    /// Waited for, here or elsewhere: the pid may name another process now, so it is never
    /// signalled again.
    reaped: bool,
    pub(super) reap_status: super::RpcReapObservation,
}

impl ChildState {
    pub(super) fn new(child: Child, id: SessionId, tag: String) -> Self {
        Self {
            pid: child.id(),
            child: Some(child),
            id,
            tag,
            exit: None,
            reaped: false,
            reap_status: super::RpcReapObservation::default(),
        }
    }

    pub(super) fn pid(&self) -> u32 {
        self.pid
    }

    /// The exit code recorded so far (no new check).
    pub(super) fn exit_code(&self) -> Option<i32> {
        self.exit.flatten()
    }

    /// Takes the child's three pipes (spawned piped, so they are there).
    pub(super) fn take_stdio(
        &mut self,
        launcher: &str,
    ) -> Result<(ChildStdin, ChildStdout, ChildStderr)> {
        let missing = |what: &str| Error::Spawn(format!("{launcher} rpc child has no {what}"));
        let child = self.child.as_mut().ok_or_else(|| missing("process"))?;
        let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| missing("stderr"))?;
        Ok((stdin, stdout, stderr))
    }

    /// Non-blocking exit check, sticky once it reports an exit. Never reaps (see the module
    /// docs).
    pub(super) fn try_wait(&mut self) -> std::io::Result<Option<Option<i32>>> {
        if self.exit.is_some() {
            return Ok(self.exit);
        }
        self.check_exit()
    }

    #[cfg(unix)]
    fn check_exit(&mut self) -> std::io::Result<Option<Option<i32>>> {
        match crate::pty::peek_exit(self.pid) {
            Ok(Some(exit)) => {
                self.exit = Some(exit.status);
                Ok(self.exit)
            }
            Ok(None) => Ok(None),
            // Not our child any more: something else in the process reaped it.
            Err(e) if e.raw_os_error() == Some(libc::ECHILD) => {
                log::warn!(
                    "{} child {} was reaped outside this handle; its exit code is unknown and it is never signalled",
                    self.tag,
                    self.pid
                );
                self.reap_status
                    .0
                    .store(2, std::sync::atomic::Ordering::Release);
                self.reaped = true;
                self.exit = Some(None);
                Ok(self.exit)
            }
            Err(e) => {
                self.reap_status
                    .0
                    .store(2, std::sync::atomic::Ordering::Release);
                Err(e)
            }
        }
    }

    /// Windows keeps the process handle open after `try_wait`, so the pid stays reserved.
    #[cfg(not(unix))]
    fn check_exit(&mut self) -> std::io::Result<Option<Option<i32>>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.exit = Some(status.code());
                Ok(self.exit)
            }
            Ok(None) => Ok(None),
            Err(error) => {
                self.reap_status
                    .0
                    .store(2, std::sync::atomic::Ordering::Release);
                Err(error)
            }
        }
    }

    /// Kills the tree and the child, then reaps it (bounded by [`REAP_WAIT`]); its exit
    /// code, `None` for a signal. Signals nothing once the child is reaped.
    pub(super) fn kill_and_reap(&mut self) -> Option<i32> {
        // A reap elsewhere since the last `try_wait` (or with none at all) is seen here, not
        // by the kill. Any other error leaves the kill to go ahead.
        #[cfg(unix)]
        if !self.reaped {
            let _ = self.check_exit();
        }
        if self.reaped {
            log::debug!("{} child already reaped; nothing signalled", self.tag);
            return self.exit.flatten();
        }
        self.kill_tree();
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
        self.reap()
    }

    /// unix: SIGKILL the child's process group (it leads one: `setsid`), which also reaches
    /// what it started there. Unreaped, so the group id cannot have been reused — even
    /// after a confirmed exit.
    #[cfg(unix)]
    fn kill_tree(&mut self) {
        proc::kill_tree(self.pid);
    }

    /// Windows: `taskkill /T /F` — the agent is usually a `node.exe` grandchild under a
    /// `cmd.exe` shim, which `Child::kill` alone would leave running. Skipped only on a
    /// CONFIRMED exit (a `try_wait` error is not one); why: `proc::kill_tree`. Blocks for
    /// one `taskkill` round trip.
    #[cfg(windows)]
    fn kill_tree(&mut self) {
        if !matches!(self.try_wait(), Ok(Some(_))) {
            proc::kill_process_tree(self.pid);
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn kill_tree(&mut self) {}

    /// Waits for the killed child up to [`REAP_WAIT`], then leaves it to a detached
    /// `agent-link-rpc-reaper-<id>` thread: a child that cannot be killed (stuck in an
    /// uninterruptible call) must not hang the dropping thread. Logs `child reaped`.
    fn reap(&mut self) -> Option<i32> {
        self.reaped = true;
        let Some(mut child) = self.child.take() else {
            return self.exit.flatten();
        };
        let tag = self.tag.clone();
        let deadline = Instant::now() + REAP_WAIT;
        let mut pause = Duration::from_millis(1);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code();
                    self.reap_status
                        .0
                        .compare_exchange(
                            0,
                            1,
                            std::sync::atomic::Ordering::AcqRel,
                            std::sync::atomic::Ordering::Acquire,
                        )
                        .ok();
                    self.exit = Some(code);
                    log::info!("{tag} child reaped: exit code {code:?} (None ⇒ signalled)");
                    return code;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(20));
                }
                Ok(None) => {
                    log::warn!(
                        "{tag} child {} still running {} s after the kill; reaping it in the background",
                        self.pid,
                        REAP_WAIT.as_secs()
                    );
                    let observation = self.reap_status.clone();
                    let reaper = std::thread::Builder::new()
                        .name(format!("agent-link-rpc-reaper-{}", self.id))
                        .spawn(move || {
                            let waited = child.wait();
                            if waited.is_ok() {
                                let _ = observation.0.compare_exchange(
                                    0,
                                    1,
                                    std::sync::atomic::Ordering::AcqRel,
                                    std::sync::atomic::Ordering::Acquire,
                                );
                            } else {
                                observation.0.store(2, std::sync::atomic::Ordering::Release);
                            }
                            let code = waited.ok().and_then(|status| status.code());
                            log::info!("{tag} child reaped: exit code {code:?} (None ⇒ signalled)");
                        });
                    if let Err(e) = reaper {
                        self.reap_status
                            .0
                            .store(2, std::sync::atomic::Ordering::Release);
                        log::warn!("{} could not start the reaper thread: {e}", self.tag);
                    }
                    return self.exit.flatten();
                }
                Err(e) => {
                    self.reap_status
                        .0
                        .store(2, std::sync::atomic::Ordering::Release);
                    log::warn!("{tag} could not reap child {}: {e}", self.pid);
                    return self.exit.flatten();
                }
            }
        }
    }
}

/// Owns a freshly spawned child until its threads are up: if a later spawn step fails,
/// dropping the guard kills the tree and reaps the child, so a failed spawn leaves no
/// process behind.
pub(super) struct ChildGuard(Option<ChildState>);

impl ChildGuard {
    pub(super) fn new(state: ChildState) -> Self {
        Self(Some(state))
    }

    pub(super) fn state(&mut self) -> &mut ChildState {
        self.0
            .as_mut()
            .expect("ChildGuard holds the child until disarmed")
    }

    /// Takes the child out; the guard no longer kills it.
    pub(super) fn disarm(mut self) -> ChildState {
        self.0.take().expect("ChildGuard is disarmed only once")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut state) = self.0.take() {
            log::debug!(
                "{} spawn failed after the child started; killing pid {}",
                state.tag,
                state.pid
            );
            state.kill_and_reap();
        }
    }
}

#[cfg(all(test, unix))]
impl ChildState {
    /// Whether the child counts as reaped (tests).
    pub(super) fn is_reaped(&self) -> bool {
        self.reaped
    }

    /// Whether the handle still holds the `Child` — teardown's own reap takes it (tests).
    pub(super) fn holds_child(&self) -> bool {
        self.child.is_some()
    }
}
