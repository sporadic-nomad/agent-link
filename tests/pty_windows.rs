//! `Terminal` on Windows (ConPTY + `cmd.exe`). Compiled by every gate run (the Windows
//! cross-check), executed only on a Windows host. A line typed into `cmd.exe` ends with
//! `\r` (a bare `\n` does not complete it under ConPTY). The Job Object tests that need
//! crate internals are unit tests in `src/pty/session.rs`.
#![cfg(windows)]

mod common;

use agent_link::{Confidence, Event, ForegroundOwner, PtySize, SpawnSpec, Terminal};
use common::{WAIT, deadline, output_until, wait_until};
use crossbeam_channel::{Receiver, RecvTimeoutError};

const SIZE: PtySize = PtySize { rows: 24, cols: 80 };

fn assert_disconnects(events: &Receiver<Event>) {
    let deadline = deadline();
    loop {
        match events.recv_deadline(deadline) {
            Ok(_) => {}
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => panic!("the receiver did not disconnect"),
        }
    }
}

#[test]
fn echo_round_trip_in_cmd() {
    let dir = tempfile::tempdir().unwrap();
    let (term, events) = Terminal::spawn(SpawnSpec::shell(dir.path(), SIZE)).unwrap();
    // The carets keep the echoed command line from matching.
    term.write(b"echo agent^-link^-42\r").unwrap();
    output_until(&events, "agent-link-42");
}

#[test]
fn teardown_sends_no_exit_and_disconnects() {
    let dir = tempfile::tempdir().unwrap();
    let (term, events) = Terminal::spawn(SpawnSpec::shell(dir.path(), SIZE)).unwrap();
    term.write(b"ping -n 30 127.0.0.1\r").unwrap();
    output_until(&events, "127.0.0.1");
    let started = std::time::Instant::now();
    term.kill();
    assert!(
        started.elapsed() < WAIT,
        "teardown took {:?}",
        started.elapsed()
    );
    let deadline = deadline();
    loop {
        match events.recv_deadline(deadline) {
            Ok(Event::Exit { .. }) => panic!("Exit after teardown"),
            Ok(_) => {}
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!("the receiver did not disconnect"),
        }
    }
}

/// ConPTY can retain its output pipe after the child exits. Process completion is
/// observable through `try_wait`; `Exit` means output EOF, not process completion.
#[test]
fn a_direct_child_exit_reports_status_and_preserves_output() {
    let dir = tempfile::tempdir().unwrap();
    let spec = SpawnSpec::direct("cmd.exe", ["/d", "/c", "echo hi"], dir.path(), SIZE);
    let (term, events) = Terminal::spawn(spec).unwrap();
    output_until(&events, "hi");
    wait_until("try_wait to report the exit", || term.try_wait().is_some());
    assert_eq!(term.try_wait().map(|e| e.status), Some(Some(0)));
    assert_eq!(term.kill().status, Some(0), "teardown keeps the real code");
    assert_disconnects(&events);
}

/// Shell completion has the same process/output distinction as a direct child.
#[test]
fn a_shell_exit_reports_status_and_preserves_output() {
    let dir = tempfile::tempdir().unwrap();
    let (term, events) = Terminal::spawn(SpawnSpec::shell(dir.path(), SIZE)).unwrap();
    term.write(b"echo exiting^-3 & exit 3\r").unwrap();
    output_until(&events, "exiting-3");
    wait_until("try_wait to report the exit", || term.try_wait().is_some());
    assert_eq!(term.try_wait().unwrap().status, Some(3));
    assert_eq!(term.kill().status, Some(3), "teardown keeps the real code");
    assert_disconnects(&events);
}

/// The heuristic from the child's Job Object: the shell alone ⇒ `Shell`, a program it runs
/// ⇒ `Agent`; always `Heuristic`.
#[test]
fn foreground_follows_a_program_heuristically() {
    let dir = tempfile::tempdir().unwrap();
    let (term, events) = Terminal::spawn(SpawnSpec::shell(dir.path(), SIZE)).unwrap();
    term.write(b"echo ready^-1\r").unwrap();
    output_until(&events, "ready-1");
    let is = |owner| {
        let answer = term.foreground();
        assert_eq!(answer.confidence, Confidence::Heuristic);
        answer.owner == owner
    };
    wait_until("the shell in front", || is(ForegroundOwner::Shell));
    // About two seconds, then cmd.exe is back at its prompt.
    term.write(b"ping -n 3 127.0.0.1\r").unwrap();
    wait_until("ping in front", || is(ForegroundOwner::Agent));
    wait_until("the shell back in front", || is(ForegroundOwner::Shell));
}
