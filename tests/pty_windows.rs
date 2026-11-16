//! `Terminal` on Windows (ConPTY + `cmd.exe`). Compiled by every gate run (the Windows
//! cross-check), executed only on a Windows host. A line typed into `cmd.exe` ends with
//! `\r` (a bare `\n` does not complete it under ConPTY). The Job Object tests that need
//! crate internals are unit tests in `src/pty/session.rs`.
#![cfg(windows)]

mod common;

use agent_link::{Confidence, Event, ForegroundOwner, PtySize, SpawnSpec, Terminal};
use common::{WAIT, deadline, output_until, recv_until, wait_until};
use crossbeam_channel::RecvTimeoutError;

const SIZE: PtySize = PtySize { rows: 24, cols: 80 };

#[test]
fn echo_round_trip_in_cmd() {
    let dir = tempfile::tempdir().unwrap();
    let (term, events) = Terminal::spawn(SpawnSpec::shell(dir.path(), SIZE)).unwrap();
    // The carets keep the echoed command line from matching.
    term.write(b"echo agent^-pty^-42\r").unwrap();
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

/// Unverified: ConPTY may keep the output pipe open after the child exits (until the
/// pseudoconsole closes at teardown), in which case no `Exit` arrives and this fails.
#[test]
fn a_direct_child_that_exits_sends_exit() {
    let dir = tempfile::tempdir().unwrap();
    let spec = SpawnSpec::direct("cmd.exe", ["/d", "/c", "echo hi"], dir.path(), SIZE);
    let (term, events) = Terminal::spawn(spec).unwrap();
    let deadline = deadline();
    let mut text = String::new();
    loop {
        match events.recv_deadline(deadline) {
            Ok(Event::Output { bytes, .. }) => text.push_str(&String::from_utf8_lossy(&bytes)),
            Ok(Event::Exit { .. }) => break,
            Ok(other) => panic!("unexpected {other:?}"),
            Err(e) => panic!("no Exit ({e}); output: {text:?}"),
        }
    }
    assert!(text.contains("hi"), "{text:?}");
    assert_eq!(term.try_wait().map(|e| e.status), Some(Some(0)));
}

/// Unverified, like the direct case above: a shell's `exit` may not end the output either.
#[test]
fn a_shell_that_exits_sends_exit() {
    let dir = tempfile::tempdir().unwrap();
    let (term, events) = Terminal::spawn(SpawnSpec::shell(dir.path(), SIZE)).unwrap();
    term.write(b"exit 3\r").unwrap();
    recv_until(&events, deadline(), |e| matches!(e, Event::Exit { .. }));
    wait_until("try_wait to report the exit", || term.try_wait().is_some());
    assert_eq!(term.try_wait().unwrap().status, Some(3));
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
