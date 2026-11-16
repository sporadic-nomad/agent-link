// Real process crash and detached-owner checks; no provider executables or credentials.
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use agent_link_supervisor::*;

const WAIT: Duration = Duration::from_secs(15);

fn binding() -> Binding {
    Binding {
        provider: "fixture".into(),
        session: "existing-session".into(),
    }
}

struct Peer {
    child: Child,
    input: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Peer {
    fn start(mode: &str, path: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_supervisor-fixture"))
            .arg(mode)
            .arg(path)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = child.stdout.take().unwrap();
        let (send, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else {
                    break;
                };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            lines,
        }
    }
    fn line(&self) -> String {
        self.lines
            .recv_timeout(WAIT)
            .expect("fixture output before deadline")
    }
    fn send(&mut self, line: &str) {
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{line}").unwrap();
        input.flush().unwrap();
    }
    fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "fixture exit deadline");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(mut input) = self.input.take() {
            let _ = writeln!(input, "stop");
            let _ = input.flush();
        }
        // EOF also stops an orphaned worker if the launcher already exited.
        let deadline = Instant::now() + WAIT;
        let mut exited = false;
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if !exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // A launcher can already be reaped while its worker still cleans up. Wait
        // for the worker's output pipe to close too, including on assertion failure.
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if self.lines.recv_timeout(remaining).is_err() {
                break;
            }
        }
    }
}

#[test]
fn launcher_death_preserves_same_worker_child_and_client_disconnect_does_not_resend() {
    let dir = tempfile::tempdir().unwrap();
    let mut peer = Peer::start("launcher", dir.path());
    let ready = peer.line();
    assert!(ready.starts_with("READY "));
    assert!(peer.wait().success(), "launcher must exit before dispatch");
    assert!(matches!(
        Supervisor::open(dir.path(), binding(), Limits::default()),
        Err(Error::Owned)
    ));
    peer.send("send");
    assert_eq!(peer.line(), ready.replace("READY", "ACCEPTED"));
    peer.send("send");
    assert_eq!(peer.line(), ready.replace("READY", "ACCEPTED"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("dispatch.log")).unwrap(),
        "payload\n"
    );
    // The host's EOF policy tears down this test worker, with a positive native wait.
    peer.input.take();
    assert_eq!(peer.line(), "STOPPED");
    assert!(peer.lines.recv_timeout(WAIT).is_err());
    let owner = Supervisor::open(dir.path(), binding(), Limits::default()).unwrap();
    assert_eq!(owner.fenced_runtime().unwrap(), None);
    assert!(matches!(
        owner.receipt("op").unwrap().unwrap().outcome,
        Outcome::Accepted(_)
    ));
}

#[test]
fn crash_boundaries_recover_receipts_without_dispatch_or_duplicate_workers() {
    for phase in [
        "before-intent",
        "uncommitted",
        "before-dispatch",
        "after-dispatch",
        "after-accept",
        "after-terminal",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut peer = Peer::start("worker", dir.path());
        assert!(peer.line().starts_with("READY "));
        peer.send(phase);
        assert_eq!(peer.wait().code(), Some(77), "phase {phase}");
        // The fixture provider explicitly acknowledges stdin EOF; no stray native
        // process is left after owner-crash testing. This is cleanup, not replay proof.
        let deadline = Instant::now() + WAIT;
        while !dir.path().join("provider-eof").exists() {
            assert!(Instant::now() < deadline, "provider cleanup at {phase}");
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut owner = Supervisor::open(dir.path(), binding(), Limits::default()).unwrap();
        assert!(matches!(
            owner.resume(Instant::now(), |_, _| panic!("must not spawn after crash")),
            Err(Error::RecoveryRequired)
        ));
        let receipt = owner.receipt("op").unwrap();
        match phase {
            "before-intent" | "uncommitted" => assert!(receipt.is_none()),
            "before-dispatch" | "after-dispatch" => {
                assert_eq!(receipt.as_ref().unwrap().outcome, Outcome::SubmittedUnknown)
            }
            "after-accept" => assert!(matches!(
                receipt.as_ref().unwrap().outcome,
                Outcome::Accepted(_)
            )),
            "after-terminal" => assert!(matches!(
                receipt.as_ref().unwrap().outcome,
                Outcome::Terminal { .. }
            )),
            _ => unreachable!(),
        }
        if let Some(receipt) = receipt {
            assert_eq!(owner.submit("op", b"payload").unwrap(), receipt);
            assert!(matches!(
                owner.submit("op", b"changed"),
                Err(Error::Conflict)
            ));
        }
        let log = std::fs::read_to_string(dir.path().join("dispatch.log")).unwrap();
        assert_eq!(
            log,
            if phase.starts_with("after-") {
                "payload\n"
            } else {
                ""
            },
            "phase {phase}"
        );
    }
}
