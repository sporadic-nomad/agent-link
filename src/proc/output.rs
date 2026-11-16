//! Pipe readers never own the child: only the caller decides between success and
//! timeout cleanup, so publishing a reader result cannot release PID ownership.

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(5);
const REAP_GRACE: Duration = Duration::from_secs(2);

struct Owner(Option<Child>);

impl Owner {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("owner holds child until completion")
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let result = self.child().try_wait();
        // Reaping and disarming happen on the caller, with no other signalling
        // thread. On Unix ECHILD also means the identity is no longer ours.
        let released = matches!(result, Ok(Some(_)));
        #[cfg(unix)]
        let released =
            released || matches!(&result, Err(error) if error.raw_os_error() == Some(libc::ECHILD));
        if released {
            self.0 = None;
        }
        result
    }

    fn terminate_with(&mut self, signal: impl FnOnce(&mut Child)) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        #[cfg(unix)]
        {
            // An external reaper can still race the subsequent signal. Unlike our
            // own reaping, that requires coordination with the embedding host.
            if matches!(crate::pty::peek_exit(child.id()), Err(error) if error.raw_os_error() == Some(libc::ECHILD))
            {
                return;
            }
        }
        #[cfg(windows)]
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        signal(&mut child);
        let deadline = Instant::now() + REAP_GRACE;
        loop {
            match child.try_wait() {
                Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
                Ok(None) => {
                    if let Err(error) = std::thread::Builder::new()
                        .name("agent-link-output-reaper".into())
                        .spawn(move || {
                            let _ = child.wait();
                        })
                    {
                        log::warn!("[proc-output] could not start reaper thread: {error}");
                    }
                    return;
                }
                _ => return,
            }
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.terminate_with(|child| {
            super::kill_tree(child.id());
            let _ = child.kill();
        });
    }
}

type PipeResult = Receiver<io::Result<Vec<u8>>>;

fn reader(mut pipe: impl Read + Send + 'static) -> io::Result<PipeResult> {
    let (tx, rx) = sync_channel(1);
    std::thread::Builder::new()
        .name("agent-link-process-output".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe.read_to_end(&mut bytes).map(|_| bytes);
            let _ = tx.send(result);
        })?;
    Ok(rx)
}

fn poll(pipe: &PipeResult, output: &mut Option<Vec<u8>>) -> io::Result<()> {
    if output.is_none() {
        match pipe.try_recv() {
            Ok(result) => *output = Some(result?),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                return Err(io::Error::other("process output reader disconnected"));
            }
        }
    }
    Ok(())
}

fn collect(
    owner: &mut Owner,
    out: PipeResult,
    err: PipeResult,
    deadline: Instant,
) -> io::Result<Option<Output>> {
    let (mut stdout, mut stderr) = (None, None);
    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        poll(&out, &mut stdout)?;
        poll(&err, &mut stderr)?;
        if let (Some(stdout), Some(stderr)) = (&mut stdout, &mut stderr) {
            // This is the only successful-reap path. Until BOTH pipe results exist,
            // the root stays owned so timeout can still kill its descendants.
            if let Some(status) = owner.try_wait()? {
                return Ok(Some(Output {
                    status,
                    stdout: std::mem::take(stdout),
                    stderr: std::mem::take(stderr),
                }));
            }
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

pub(super) fn capture(mut cmd: Command, timeout: Duration) -> io::Result<Option<Output>> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "process timeout exceeds the supported deadline range",
        )
    })?;
    let mut owner = Owner(Some(
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?,
    ));
    // Preserve wait_with_output's stdin behavior, including caller-piped stdin.
    drop(owner.child().stdin.take());
    let stdout = owner
        .child()
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("process stdout was not piped"))?;
    let stderr = owner
        .child()
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("process stderr was not piped"))?;
    let out = reader(stdout)?;
    let err = reader(stderr)?;
    collect(&mut owner, out, err, deadline)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    fn child(script: &str) -> Owner {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script])
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Owner(Some(cmd.spawn().expect("spawn isolated test child")))
    }

    fn observe_exit(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if crate::pty::peek_exit(pid)
                .expect("observe without reaping")
                .is_some()
            {
                return;
            }
            assert!(Instant::now() < deadline, "child {pid} did not exit");
            std::thread::sleep(POLL);
        }
    }

    fn reap_externally(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            // SAFETY: this test owns this exact child's PID; no unrelated children
            // are waited for; the null status pointer discards its exit status.
            let result =
                unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
            if result == pid as libc::pid_t {
                return;
            }
            assert_eq!(
                result,
                0,
                "external wait failed: {}",
                io::Error::last_os_error()
            );
            assert!(Instant::now() < deadline, "child {pid} did not exit");
            std::thread::sleep(POLL);
        }
    }

    #[test]
    fn completed_pipe_waiting_to_publish_keeps_child_owned_until_timeout_cleanup() {
        let mut owner = child("printf output; printf diagnostics >&2; exit 7");
        let pid = owner.child().id();
        let mut stdout = owner.child().stdout.take().unwrap();
        let err = reader(owner.child().stderr.take().unwrap()).unwrap();
        let (out_tx, out) = sync_channel(1);
        let (finished_tx, finished) = sync_channel(1);
        let (release, released) = sync_channel::<()>(1);
        let worker = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
            let _ = finished_tx.send(());
            // Sender drop releases this worker even when the test fails.
            let _ = released.recv_timeout(Duration::from_secs(60));
            let _ = out_tx.send(result);
        });
        finished.recv_timeout(Duration::from_secs(60)).unwrap();
        observe_exit(pid);

        // The pipe has finished, but its publication is deliberately withheld.
        // A timeout must still own the root when it signals the process group.
        let output = collect(&mut owner, out, err, Instant::now()).unwrap();
        let mut owned_when_signalled = false;
        owner.terminate_with(|child| {
            owned_when_signalled = matches!(crate::pty::peek_exit(child.id()), Ok(Some(_)));
            super::super::kill_tree(child.id());
            let _ = child.kill();
        });
        drop(release);
        worker.join().unwrap();
        assert!(output.is_none());
        assert!(
            owned_when_signalled,
            "pipe completion released the child's PID"
        );
        assert!(
            matches!(crate::pty::peek_exit(pid), Err(error) if error.raw_os_error() == Some(libc::ECHILD))
        );
    }

    #[test]
    fn successful_capture_disarms_cleanup_before_releasing_ownership() {
        let mut owner = child("printf output; printf diagnostics >&2; exit 7");
        let out = reader(owner.child().stdout.take().unwrap()).unwrap();
        let err = reader(owner.child().stderr.take().unwrap()).unwrap();
        let output = collect(
            &mut owner,
            out,
            err,
            Instant::now() + Duration::from_secs(60),
        )
        .unwrap()
        .unwrap();
        let mut signalled = false;
        owner.terminate_with(|_| signalled = true);
        assert!(
            !signalled,
            "a completed capture must never signal its former PID"
        );
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"output");
        assert_eq!(output.stderr, b"diagnostics");
    }

    #[test]
    fn timeout_never_signals_an_externally_reaped_child() {
        for observed_before_reap in [false, true] {
            let mut owner = child("exit 0");
            let pid = owner.child().id();
            if observed_before_reap {
                observe_exit(pid);
            }
            reap_externally(pid);
            // Unpublished pipe results must not restore an externally lost identity.
            let (_out_tx, out) = sync_channel(1);
            let (_err_tx, err) = sync_channel(1);
            assert!(
                collect(&mut owner, out, err, Instant::now())
                    .unwrap()
                    .is_none()
            );
            let mut signalled = false;
            owner.terminate_with(|_| signalled = true);
            assert!(!signalled, "externally reaped PID {pid} was signalled");
        }
    }

    #[test]
    fn pipe_read_failure_cleans_up_the_owned_process() {
        let mut owner = child("sleep 30");
        let pid = owner.child().id();
        let (out_tx, out) = sync_channel(1);
        let (_err_tx, err) = sync_channel(1);
        out_tx
            .send(Err(io::Error::other("controlled read failure")))
            .unwrap();
        let error = collect(
            &mut owner,
            out,
            err,
            Instant::now() + Duration::from_secs(60),
        )
        .unwrap_err();
        drop(owner);
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(
            matches!(crate::pty::peek_exit(pid), Err(error) if error.raw_os_error() == Some(libc::ECHILD))
        );
    }
}
