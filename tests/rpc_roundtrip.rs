//! `RpcChild` against real children: `cat` round trips with `seq`, the newline refusal, an
//! exit with its stderr tail and code, teardown (no `Exit`; the group killed even after a
//! confirmed exit; a reader blocked on a full receiver released; the receiver disconnected
//! while a process outside the group holds stdout), `Closed` once the child's stdin is
//! gone, the login-shell launch in a session of its own, `forward_rpc`, the env defaults,
//! the log lines, and `preflight`'s refusals. Every wait has a deadline; children
//! run through `/bin/sh` (no rc files) except the login-shell test.
#![cfg(feature = "rpc")]

mod common;

use std::time::{Duration, Instant};

use agent_link::Error;
use agent_link::agent::RpcLaunch;
use agent_link::rpc::{RpcChild, RpcEvent, RpcSpawnSpec, preflight};
use crossbeam_channel::{Receiver, RecvTimeoutError};

/// How long one wait may take. It can include a child's start — two execs, the shell and
/// then the launcher — and where endpoint-security software vets every exec, starts under
/// parallel load have taken over 20 s. A passing wait is not slower.
const WAIT: Duration = Duration::from_secs(60);

fn deadline() -> Instant {
    Instant::now() + WAIT
}

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

/// The next event; panics at the deadline or on disconnect.
fn next_event(rx: &Receiver<RpcEvent>, what: &str) -> RpcEvent {
    match rx.recv_deadline(deadline()) {
        Ok(event) => event,
        Err(e) => panic!("no event ({what}): {e}"),
    }
}

/// Waits until the receiver disconnects, failing on an `Exit` (a teardown never sends one).
fn assert_disconnects_without_exit(rx: &Receiver<RpcEvent>) {
    let deadline = deadline();
    loop {
        match rx.recv_deadline(deadline) {
            Ok(RpcEvent::Exit { .. }) => panic!("Exit after teardown"),
            Ok(_) => {}
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => panic!("the receiver did not disconnect"),
        }
    }
}

/// Polls `try_wait` until it reports the exit; returns the code.
fn wait_exit(child: &RpcChild) -> Option<i32> {
    let deadline = deadline();
    loop {
        if let Some(code) = child.try_wait().expect("liveness readable") {
            return code;
        }
        assert!(Instant::now() < deadline, "the child did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `preflight` and `spawn` both refuse these invalid specs before a process exists.
fn assert_refused(spec: RpcSpawnSpec, what: &str) -> Error {
    let err = preflight(&spec).expect_err(what);
    assert!(matches!(err, Error::InvalidInput(_)), "{what}: {err}");
    match RpcChild::spawn(spec) {
        Err(Error::InvalidInput(_)) => {}
        Err(other) => panic!("{what}: spawn refused differently: {other}"),
        Ok(_) => panic!("{what}: spawn started a child"),
    }
    err
}

#[test]
fn preflight_refuses_what_spawn_refuses() {
    let dir = temp();
    let file = dir.path().join("file");
    std::fs::write(&file, "x").unwrap();
    let launch = RpcLaunch::new("agent-link-rpc-test", &["--rpc"]);

    let err = assert_refused(RpcSpawnSpec::new(launch.clone(), &file), "a file as cwd");
    assert!(err.to_string().contains("is not a directory"), "{err}");
    assert_refused(
        RpcSpawnSpec::new(launch.clone(), dir.path().join("missing")),
        "a missing cwd",
    );
    let err = assert_refused(
        RpcSpawnSpec::new(RpcLaunch::new("", &[]), dir.path()),
        "an empty launcher",
    );
    assert!(err.to_string().contains("launcher is empty"), "{err}");
}

#[cfg(unix)]
mod unix {
    use std::path::Path;
    use std::sync::Arc;

    use agent_link::agent::{Permissions, ProfileRegistry, RpcLaunchOptions};
    use agent_link::rpc::{RpcLimits, RpcStream, RpcTransportError, forward_rpc};

    use super::common::{logger, wait_until};
    use super::*;

    /// `launcher args…` run through `/bin/sh` (fast, no rc files), logged as
    /// `[rpc-test:<key>]`.
    fn sh_spec(dir: &Path, key: &str, launcher: &str, args: &[&str]) -> RpcSpawnSpec {
        let mut spec = RpcSpawnSpec::new(RpcLaunch::new(launcher, args), dir);
        spec.shell = Some("/bin/sh".into());
        spec.log_tag = "rpc-test".into();
        spec.log_key = key.into();
        spec
    }

    /// The logged lines tagged `[rpc-test:<key>]`.
    fn tagged(key: &str) -> Vec<String> {
        let tag = format!("[rpc-test:{key}]");
        logger::lines()
            .into_iter()
            .filter(|l| l.contains(&tag))
            .collect()
    }

    /// Waits until a `[rpc-test:<key>]` line contains `needle`; returns it.
    fn wait_log(key: &str, needle: &str) -> String {
        let deadline = deadline();
        loop {
            let lines = tagged(key);
            if let Some(line) = lines.iter().find(|l| l.contains(needle)) {
                return line.clone();
            }
            assert!(
                Instant::now() < deadline,
                "no {needle:?} line for {key}; its lines: {lines:#?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// `pid` no longer names a process.
    fn gone(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        let gone = unsafe { libc::kill(pid, 0) } != 0;
        gone && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    fn line(id: agent_link::SessionId, seq: u64, line: &str) -> RpcEvent {
        RpcEvent::Line {
            id,
            seq,
            line: line.into(),
        }
    }

    #[test]
    fn tracked_writes_and_half_close_drain_in_order_then_receive_final_output() {
        use agent_link::rpc::RpcWriteOutcome;
        let dir = temp();
        let script = "cat; printf 'after-eof\\n'";
        let (child, events) =
            RpcChild::spawn(sh_spec(dir.path(), "half-close", "sh", &["-c", script])).unwrap();
        let first = child.send_line_tracked("first", deadline()).unwrap();
        let empty = child.send_line_tracked("", deadline()).unwrap();
        child.send_line("last").unwrap();
        child.close_stdin().unwrap();
        assert!(matches!(child.send_line("late"), Err(Error::Closed)));
        assert!(matches!(
            child.send_line("invalid\nline"),
            Err(Error::Closed)
        ));
        assert!(matches!(
            child.send_line_tracked("invalid\nline", Instant::now()),
            Err(Error::Closed)
        ));
        assert!(matches!(
            child.send_line_tracked("late", deadline()),
            Err(Error::Closed)
        ));
        for (seq, text) in ["first", "", "last", "after-eof"].into_iter().enumerate() {
            assert_eq!(
                next_event(&events, text),
                line(child.id(), seq as u64, text)
            );
        }
        assert_eq!(first.wait_until(deadline()), Some(RpcWriteOutcome::Written));
        assert_eq!(empty.wait_until(deadline()), Some(RpcWriteOutcome::Written));
        assert!(matches!(next_event(&events, "EOF"), RpcEvent::Exit { .. }));
        assert_eq!(wait_exit(&child), Some(0));
    }

    #[test]
    fn strict_utf8_accepts_fragmented_multibyte_and_rejects_corrupt_or_truncated_frames() {
        use agent_link::rpc::RpcUtf8Mode;
        let dir = temp();
        for (script, valid) in [
            ("printf '\\303'; printf '\\251\\r\\n'; printf 'last'", true),
            ("printf '\\377private\\n'", false),
            ("printf '\\303'", false),
        ] {
            let mut spec = sh_spec(dir.path(), "strict-utf8", "sh", &["-c", script]);
            spec.stdout_utf8 = RpcUtf8Mode::Strict;
            let (child, events) = RpcChild::spawn(spec).unwrap();
            if valid {
                assert_eq!(next_event(&events, "multibyte"), line(child.id(), 0, "é"));
                assert_eq!(
                    next_event(&events, "unterminated"),
                    line(child.id(), 1, "last")
                );
                assert!(matches!(next_event(&events, "EOF"), RpcEvent::Exit { .. }));
            } else {
                assert_eq!(
                    next_event(&events, "corruption"),
                    RpcEvent::Error {
                        id: child.id(),
                        error: RpcTransportError::InvalidUtf8,
                    }
                );
                assert!(matches!(child.send_line("late"), Err(Error::Closed)));
            }
            assert!(matches!(
                events.recv_deadline(deadline()),
                Err(RecvTimeoutError::Disconnected)
            ));
        }
    }

    #[test]
    fn active_write_deadline_settles_receipts_with_both_pipes_backpressured() {
        use agent_link::rpc::{RpcWriteFailure, RpcWriteOutcome};
        logger::install();
        let dir = temp();
        let mut spec = sh_spec(
            dir.path(),
            "blocked-deadline",
            "sh",
            &["-c", "echo ready; echo full; echo blocked; exec sleep 60"],
        );
        spec.limits.event_capacity = 1;
        let (child, events) = RpcChild::spawn(spec).unwrap();
        assert_eq!(next_event(&events, "ready"), line(child.id(), 0, "ready"));
        wait_for("full output", || events.is_full());
        // Admit everything up front so no step races the short active deadline. The idle
        // writer claims the 1 MiB line at once and blocks on the unread stdin pipe.
        let active = child
            .send_line_tracked(
                &"x".repeat(1024 * 1024 - 1),
                Instant::now() + Duration::from_millis(500),
            )
            .unwrap();
        let queued = child.send_line_tracked("queued", deadline()).unwrap();
        child.close_stdin().unwrap();
        // `PossiblyWritten` proves the writer claimed the line: queued expiry would
        // report `NotWritten`.
        assert_eq!(
            active.wait_until(deadline()),
            Some(RpcWriteOutcome::PossiblyWritten(
                RpcWriteFailure::DeadlineExceeded
            ))
        );
        assert_eq!(
            queued.wait_until(deadline()),
            Some(RpcWriteOutcome::NotWritten(RpcWriteFailure::Closed))
        );
        assert!(matches!(child.send_line("late"), Err(Error::Closed)));
        // No need to drain the full event receiver to tear down the child.
        let started = Instant::now();
        child.kill();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_disconnects_without_exit(&events);
        wait_log("blocked-deadline", "writer thread ending");
        wait_log("blocked-deadline", "transport failed");
        assert_eq!(
            tagged("blocked-deadline")
                .iter()
                .filter(|line| line.contains("transport failed"))
                .count(),
            1,
            "the writer must not log another failure after the deadline worker wins"
        );
    }

    #[test]
    fn queued_deadline_expires_behind_a_blocked_untracked_write() {
        use agent_link::rpc::{RpcWriteFailure, RpcWriteOutcome};
        let dir = temp();
        let mut spec = sh_spec(
            dir.path(),
            "queued-deadline",
            "sh",
            &["-c", "echo ready; exec sleep 60"],
        );
        spec.limits.max_pending_stdin_lines = 2;
        let (child, events) = RpcChild::spawn(spec).unwrap();
        next_event(&events, "ready");
        child.send_line(&"x".repeat(1024 * 1024 - 1)).unwrap();
        let queued = child
            .send_line_tracked("expires", Instant::now() + Duration::from_millis(100))
            .unwrap();
        assert_eq!(
            queued.wait_until(deadline()),
            Some(RpcWriteOutcome::NotWritten(
                RpcWriteFailure::DeadlineExceeded
            ))
        );
        // Queued expiry frees its slot, without invalidating an uncorrupted stream.
        child.send_line("later").unwrap();
        child.kill();
        assert_disconnects_without_exit(&events);
    }

    #[test]
    fn eof_and_teardown_settle_receipts_without_waiting_for_blocked_stdin() {
        use agent_link::rpc::{RpcWriteFailure, RpcWriteOutcome, RpcWriteStatus};
        for close_stdout in [false, true] {
            let dir = temp();
            let script = "echo ready; while [ ! -e close-output ]; do sleep 0.01; done; exec 1>&-; exec sleep 60";
            let (child, events) =
                RpcChild::spawn(sh_spec(dir.path(), "blocked-close", "sh", &["-c", script]))
                    .unwrap();
            next_event(&events, "ready");
            let active = child
                .send_line_tracked(&"x".repeat(1024 * 1024 - 1), deadline())
                .unwrap();
            wait_for("active write", || {
                active.status() == RpcWriteStatus::Writing
            });
            let queued = child.send_line_tracked("queued", deadline()).unwrap();
            if close_stdout {
                std::fs::write(dir.path().join("close-output"), "").unwrap();
            } else {
                drop(child);
            }
            assert_eq!(
                active.wait_until(deadline()),
                Some(RpcWriteOutcome::PossiblyWritten(RpcWriteFailure::Closed))
            );
            assert_eq!(
                queued.wait_until(deadline()),
                Some(RpcWriteOutcome::NotWritten(RpcWriteFailure::Closed))
            );
        }
    }

    #[test]
    fn lines_round_trip_through_cat_with_seq_and_the_spawn_is_logged() {
        logger::install();
        let dir = temp();
        let (child, events) =
            RpcChild::spawn(sh_spec(dir.path(), "roundtrip", "cat", &[])).expect("spawn cat");
        child.send_line(r#"{"a":1}"#).unwrap();
        assert_eq!(
            next_event(&events, "first line"),
            line(child.id(), 0, r#"{"a":1}"#)
        );
        child.send_line(r#"{"b":2}"#).unwrap();
        assert_eq!(
            next_event(&events, "second line"),
            line(child.id(), 1, r#"{"b":2}"#)
        );

        let spawning = wait_log("roundtrip", "spawning cat:");
        assert!(
            spawning.contains("spawning cat: 0 argument(s)")
                && spawning.ends_with("(isolation=setsid, trace=false)"),
            "{spawning}"
        );
        wait_log(
            "roundtrip",
            &format!("cat child spawned: pid={}", child.pid()),
        );
        wait_log("roundtrip", "first stdout line received (8 bytes)");
        wait_log("roundtrip", "first stdin line flushed (8 bytes)");

        drop(child);
        // Stopping input can let cat exit normally before the process-group kill.
        // Both paths must reap it and suppress the teardown Exit event below.
        wait_log("roundtrip", "child reaped: exit code");
        assert_disconnects_without_exit(&events);
        let lines = tagged("roundtrip");
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("< seq=") || l.contains("> #2")),
            "no trace lines without trace: {lines:#?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains(r#"{"a":1}"#) || line.contains(r#"{"b":2}"#))
        );
    }

    #[test]
    fn trace_logs_every_line_both_ways() {
        logger::install();
        let dir = temp();
        let mut spec = sh_spec(dir.path(), "trace", "cat", &[]);
        spec.trace = true;
        let (child, events) = RpcChild::spawn(spec).expect("spawn cat");
        for (seq, text) in ["one", "two"].into_iter().enumerate() {
            child.send_line(text).unwrap();
            assert_eq!(
                next_event(&events, text),
                line(child.id(), seq as u64, text)
            );
        }
        wait_log("trace", "(isolation=setsid, trace=true)");
        wait_log("trace", "< seq=1 two");
        wait_log("trace", "> #2 flushed to stdin: two");
    }

    #[test]
    fn a_line_with_a_newline_is_refused_and_nothing_is_written() {
        let dir = temp();
        let (child, events) =
            RpcChild::spawn(sh_spec(dir.path(), "newline", "cat", &[])).expect("spawn cat");
        let err = child.send_line("{\"a\":1}\n{\"b\":2}").unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{err}");
        child.send_line("ok").unwrap();
        assert_eq!(next_event(&events, "ok"), line(child.id(), 0, "ok"));
    }

    #[test]
    fn a_child_that_exits_reports_its_stderr_tail_and_code() {
        let dir = temp();
        let spec = sh_spec(dir.path(), "exit", "sh", &["-c", "echo err >&2; exit 3"]);
        let (child, events) = RpcChild::spawn(spec).expect("spawn");
        assert_eq!(
            next_event(&events, "exit"),
            RpcEvent::Exit {
                id: child.id(),
                stderr_tail: "err".into()
            }
        );
        assert_eq!(wait_exit(&child), Some(3));
        assert_eq!(child.try_wait().unwrap(), Some(Some(3)), "sticky");
        // Not reaped by `try_wait`: the pid stays reserved, so the teardown's group kill
        // cannot hit a reused one, and the reap still gets the code.
        assert_eq!(child.kill(), Some(3));
        match events.recv_deadline(deadline()) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("the receiver must disconnect after the Exit: {other:?}"),
        }
    }

    #[test]
    fn teardown_after_a_confirmed_exit_still_kills_what_the_child_left_in_its_group() {
        let dir = temp();
        // The child exits at once; `sleep` stays in its process group and holds stdout
        // open, so there is no `Exit` yet.
        let spec = sh_spec(
            dir.path(),
            "leftover",
            "sh",
            &["-c", "sleep 60 & echo $!; exit 3"],
        );
        let (child, events) = RpcChild::spawn(spec).expect("spawn");
        let sleep_pid: libc::pid_t = match next_event(&events, "the sleep's pid") {
            RpcEvent::Line { line, .. } => line.trim().parse().expect("a pid"),
            other => panic!("{other:?}"),
        };
        assert_eq!(wait_exit(&child), Some(3));
        // SAFETY: a plain syscall on a live process.
        let pgid = unsafe { libc::getpgid(sleep_pid) };
        assert_eq!(
            pgid,
            child.pid() as libc::pid_t,
            "the sleep is in its group"
        );
        assert!(!gone(sleep_pid), "the sleep runs");

        assert_eq!(child.kill(), Some(3), "the code the child exited with");
        wait_until("the leftover sleep to be gone", || gone(sleep_pid));
        assert_disconnects_without_exit(&events);
    }

    #[test]
    fn kill_ends_a_running_child_and_sends_no_exit() {
        let dir = temp();
        let (child, events) =
            RpcChild::spawn(sh_spec(dir.path(), "sleep", "sleep", &["30"])).expect("spawn");
        let pid = child.pid() as libc::pid_t;
        assert_eq!(child.try_wait().unwrap(), None, "running");
        let started = Instant::now();
        assert_eq!(child.kill(), None, "SIGKILL: no exit code");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(gone(pid), "reaped");
        assert_disconnects_without_exit(&events);
    }

    /// Polls `ready` until it holds, up to this file's [`WAIT`].
    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = deadline();
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// An undrained receiver fills up and stops the reader inside a publish (the child then
    /// blocks on its stdout). Teardown releases it: `kill()` returns promptly and the
    /// receiver, once drained, disconnects with no `Exit`.
    #[test]
    fn teardown_releases_a_reader_blocked_on_a_full_receiver() {
        let dir = temp();
        let (child, events) =
            RpcChild::spawn(sh_spec(dir.path(), "full", "yes", &["{}"])).expect("spawn yes");
        wait_for("the receiver to fill up", || events.is_full());
        let started = Instant::now();
        assert_eq!(child.kill(), None, "SIGKILL: no exit code");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "kill took {:?}",
            started.elapsed()
        );
        assert_disconnects_without_exit(&events);
    }

    /// A child that closed its stdin: the first write fails, the writer stops, and from
    /// then on `send_line` reports `Closed` — while the child still runs.
    #[test]
    fn send_line_reports_closed_once_the_childs_stdin_is_gone() {
        logger::install();
        let dir = temp();
        let script = "exec 0<&-; echo closed; exec sleep 60";
        let spec = sh_spec(dir.path(), "stdin-gone", "sh", &["-c", script]);
        let (child, events) = RpcChild::spawn(spec).expect("spawn");
        assert_eq!(next_event(&events, "closed"), line(child.id(), 0, "closed"));
        let deadline = deadline();
        loop {
            match child.send_line("{}") {
                Ok(()) => {}
                Err(Error::Closed) => break,
                Err(other) => panic!("{other}"),
            }
            assert!(Instant::now() < deadline, "send_line never reported Closed");
            std::thread::sleep(Duration::from_millis(10));
        }
        wait_log("stdin-gone", "transport failed: RPC Stdin I/O failed");
        assert_eq!(child.try_wait().unwrap(), None, "the child still runs");
    }

    #[test]
    fn payloads_arguments_and_stderr_are_logged_only_with_explicit_trace() {
        logger::install();
        let dir = temp();
        for trace in [false, true] {
            let key = if trace {
                "privacy-opt-in"
            } else {
                "privacy-default"
            };
            let arg_secret = format!("PRIVATE_ARGUMENT_{key}");
            let err_secret = format!("PRIVATE_STDERR_{key}");
            let input_secret = format!("PRIVATE_INPUT_{key}");
            // The first stdout frame starts with invalid UTF-8, too: replacement must
            // not accidentally route it through a payload-bearing diagnostic path.
            let script = "printf '\\377%s\\n' \"$1\"; printf '%s\\n' \"$2\" >&2; IFS= read -r value; printf '%s\\n' \"$value\"";
            let mut spec = sh_spec(
                dir.path(),
                key,
                "sh",
                &["-c", script, "_", &arg_secret, &err_secret],
            );
            spec.trace = trace;
            let (child, events) = RpcChild::spawn(spec).unwrap();
            assert_eq!(
                next_event(&events, "first output"),
                line(child.id(), 0, &format!("�{arg_secret}"))
            );
            child.send_line(&input_secret).unwrap();
            assert_eq!(
                next_event(&events, "echoed input"),
                line(child.id(), 1, &input_secret)
            );
            assert_eq!(
                next_event(&events, "exit"),
                RpcEvent::Exit {
                    id: child.id(),
                    stderr_tail: err_secret.clone()
                }
            );
            wait_log(key, "writer thread ending");
            let logs = tagged(key).join("\n");
            for secret in [&arg_secret, &err_secret, &input_secret] {
                assert_eq!(logs.contains(secret), trace, "trace={trace}: {logs}");
            }
        }
    }

    #[test]
    fn failed_stdin_writes_report_metadata_without_logging_the_payload() {
        logger::install();
        let dir = temp();
        let spec = sh_spec(
            dir.path(),
            "private-failure",
            "sh",
            &["-c", "exec 0<&-; echo ready; exec sleep 60"],
        );
        let (child, events) = RpcChild::spawn(spec).unwrap();
        assert_eq!(next_event(&events, "ready"), line(child.id(), 0, "ready"));
        child.send_line("PRIVATE_FAILED_WRITE_PAYLOAD").unwrap();
        assert!(matches!(
            next_event(&events, "write failure"),
            RpcEvent::Error {
                error: RpcTransportError::Io {
                    stream: RpcStream::Stdin,
                    ..
                },
                ..
            }
        ));
        wait_log("private-failure", "writer thread ending");
        assert!(
            !tagged("private-failure")
                .iter()
                .any(|entry| entry.contains("PRIVATE_FAILED_WRITE_PAYLOAD"))
        );
        assert!(matches!(child.send_line("later"), Err(Error::Closed)));
    }

    #[test]
    fn oversized_stdout_and_stderr_frames_end_with_a_structured_error() {
        let dir = temp();
        for stream in [RpcStream::Stdout, RpcStream::Stderr] {
            let script = if stream == RpcStream::Stdout {
                "printf '123456789'; exec sleep 60"
            } else {
                "printf '123456789' >&2; exec sleep 60"
            };
            let mut spec = sh_spec(dir.path(), "oversized-output", "sh", &["-c", script]);
            spec.limits.max_stdout_line_bytes = 8;
            spec.limits.max_stderr_line_bytes = 8;
            let (child, events) = RpcChild::spawn(spec).unwrap();
            assert_eq!(
                next_event(&events, "oversized frame"),
                RpcEvent::Error {
                    id: child.id(),
                    error: RpcTransportError::LineTooLong { stream, limit: 8 }
                }
            );
            assert!(matches!(
                events.recv_deadline(deadline()),
                Err(RecvTimeoutError::Disconnected)
            ));
            assert!(matches!(child.send_line("later"), Err(Error::Closed)));
        }
    }

    #[test]
    fn stdin_frame_limits_reject_whole_messages_and_allow_the_exact_boundary() {
        let dir = temp();
        let mut spec = sh_spec(dir.path(), "input-size", "cat", &[]);
        spec.limits.max_stdin_line_bytes = 4;
        spec.limits.max_stdout_line_bytes = 4;
        let (child, events) = RpcChild::spawn(spec).unwrap();
        assert!(matches!(
            child.send_line("abcd"),
            Err(Error::InputTooLarge { size: 5, limit: 4 })
        ));
        child.send_line("abc").unwrap();
        assert_eq!(next_event(&events, "boundary"), line(child.id(), 0, "abc"));
    }

    #[test]
    fn an_unread_stdin_has_a_nonblocking_byte_budget_including_the_active_write() {
        let dir = temp();
        let mut spec = sh_spec(
            dir.path(),
            "input-budget",
            "sh",
            &["-c", "echo ready; exec sleep 60"],
        );
        spec.limits = RpcLimits {
            max_stdin_line_bytes: 64 * 1024,
            max_pending_stdin_bytes: 128 * 1024,
            max_pending_stdin_lines: 2,
            ..RpcLimits::default()
        };
        let (child, events) = RpcChild::spawn(spec).unwrap();
        assert_eq!(next_event(&events, "ready"), line(child.id(), 0, "ready"));
        let frame = "x".repeat(64 * 1024 - 1);
        let start = Instant::now();
        child.send_line(&frame).unwrap();
        child.send_line(&frame).unwrap();
        assert!(matches!(child.send_line(&frame), Err(Error::QueueFull)));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "admission must not wait for the child"
        );
    }

    /// SIGKILLs a process the test moved out of the child's group, also when it fails.
    struct KillOnDrop(libc::pid_t);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            // SAFETY: a plain signal to a process this test started.
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }

    /// A process in a session of its own holds the child's stdout past the group kill, so
    /// the reader never reads EOF — the receiver still disconnects at teardown. Needs
    /// `/usr/bin/perl` for `setsid`; skipped without it.
    #[test]
    fn teardown_disconnects_the_receiver_while_a_process_outside_the_group_holds_stdout() {
        const PERL: &str = "/usr/bin/perl";
        if !Path::new(PERL).exists() {
            eprintln!("skipped: no {PERL}");
            return;
        }
        let dir = temp();
        // The holder prints its pid only after `setsid`: once the line is here, it is out of
        // the child's group.
        let script = format!(
            "{PERL} -MPOSIX -e 'POSIX::setsid() or die \"setsid: $!\"; $| = 1; print \"$$\\n\"; sleep 60' & exec sleep 60"
        );
        let spec = sh_spec(dir.path(), "holder", "sh", &["-c", &script]);
        let (child, events) = RpcChild::spawn(spec).expect("spawn");
        let holder: libc::pid_t = match next_event(&events, "the holder's pid") {
            RpcEvent::Line { line, .. } => line.trim().parse().expect("a pid"),
            other => panic!("{other:?}"),
        };
        let _holder = KillOnDrop(holder);
        // SAFETY: a plain syscall on a live process.
        assert_eq!(
            unsafe { libc::getsid(holder) },
            holder,
            "leads its own session"
        );

        assert_eq!(child.kill(), None);
        assert_disconnects_without_exit(&events);
        assert!(!gone(holder), "the holder outlived the group kill");
    }

    /// The real login shell (`login_shell_argv`: `-l -i -c` for zsh and bash) in a session
    /// of its own: it is not stopped by SIGTTIN/SIGTTOU before it runs the launch, and the
    /// child leads its own process group and session — what the group kill relies on.
    ///
    /// The test process usually has no controlling terminal (cargo test), where no shell
    /// can be stopped; what always holds is that the launch runs and the ids.
    ///
    /// Determinism: the only spawn is the one under test. The child is held open by `read`
    /// — a builtin blocking on the stdin pipe the handle owns — not by a `sleep`, so only
    /// the test ends it (and if the test process dies, the pipe closes and `read` returns).
    /// The ids are read with syscalls, not by spawning `ps`. The one wait is a deadline on
    /// the marker, never a sleep; a miss says so.
    #[test]
    fn the_login_shell_runs_the_launch_in_a_session_and_group_of_its_own() {
        /// A hang guard, not a timing assertion: the marker usually lands in well under a
        /// second, and a stopped shell never writes at all.
        const MARKER_DEADLINE: Duration = Duration::from_secs(60);
        /// Impossible in rc output (a login shell's rc files print to the same stdout).
        const MARKER: &str = "agent-link-setsid-exec-ok";

        let dir = temp();
        let script = format!("echo {MARKER}; read _agent_link_hold_open");
        let mut spec = RpcSpawnSpec::new(RpcLaunch::new("sh", &["-c", &script]), dir.path());
        spec.log_tag = "rpc-test".into();
        spec.log_key = "login-shell".into();
        assert_eq!(spec.shell, None, "the user's login shell");
        let (child, events) = RpcChild::spawn(spec).expect("spawn through the login shell");
        let pid = child.pid() as libc::pid_t;

        let deadline = Instant::now() + MARKER_DEADLINE;
        let mut before = Vec::new();
        loop {
            match events.recv_deadline(deadline) {
                Ok(RpcEvent::Line { line, .. }) if line.contains(MARKER) => break,
                Ok(RpcEvent::Line { line, .. }) => before.push(line),
                Ok(RpcEvent::Exit { stderr_tail, .. }) => {
                    panic!("exited before the marker; stderr {stderr_tail:?}, stdout {before:?}")
                }
                Ok(other) => panic!("{other:?}"),
                Err(e) => panic!(
                    "no marker within {MARKER_DEADLINE:?} ({e}) — a stopped shell (SIGTTIN/SIGTTOU) \
                     writes nothing; stdout so far {before:?}"
                ),
            }
        }
        // SAFETY: plain syscalls on our own child, alive (blocked on its stdin) and unreaped.
        let (pgid, sid) = unsafe { (libc::getpgid(pid), libc::getsid(pid)) };
        let err = std::io::Error::last_os_error();
        assert_ne!(pgid, -1, "getpgid({pid}) failed: {err}");
        assert_eq!(pgid, pid, "leads its own process group");
        assert_eq!(sid, pid, "leads its own session (setsid)");
        assert_eq!(child.kill(), None);
    }

    #[test]
    fn forward_rpc_delivers_lines_in_order_and_ends_after_the_child_is_dropped() {
        let dir = temp();
        let (child, events) =
            RpcChild::spawn(sh_spec(dir.path(), "forward", "cat", &[])).expect("spawn cat");
        let (tx, rx) = crossbeam_channel::unbounded();
        let handle = forward_rpc(events, move |event| {
            let _ = tx.send(event);
        })
        .expect("forward thread");
        for i in 0..100 {
            child.send_line(&format!("line {i}")).unwrap();
        }
        for i in 0..100u64 {
            match rx.recv_deadline(deadline()) {
                Ok(RpcEvent::Line { seq, line, .. }) => {
                    assert_eq!((seq, line), (i, format!("line {i}")));
                }
                other => panic!("line {i}: {other:?}"),
            }
        }
        drop(child);
        // The forward thread ends (dropping its callback and so `tx`).
        match rx.recv_deadline(deadline()) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("the forward thread must end with no Exit: {other:?}"),
        }
        wait_until("the forward thread to finish", || handle.is_finished());
        handle.join().expect("no panic");
    }

    #[test]
    fn env_defaults_reach_the_child_unless_the_policy_sets_or_removes_the_key() {
        const KEY: &str = "AGENT_LINK_RPC_TEST_DEFAULT";
        type Edit = fn(&mut RpcSpawnSpec);
        let dir = temp();
        let edits: [(&str, Edit); 3] = [
            ("default", |_| {}),
            ("user", |spec| {
                spec.env.set(KEY, "user");
            }),
            ("unset", |spec| {
                spec.env.remove(KEY);
            }),
        ];
        let script = format!("echo \"${{{KEY}-unset}}\"");
        for (want, edit) in edits {
            let mut spec = sh_spec(dir.path(), "env", "sh", &["-c", &script]);
            spec.launch.env_defaults = vec![(KEY.into(), "default".into())];
            edit(&mut spec);
            let (child, events) = RpcChild::spawn(spec).expect("spawn");
            assert_eq!(next_event(&events, want), line(child.id(), 0, want));
        }
    }

    /// A provider-named stand-in on an isolated PATH, without caller-control markers,
    /// credentials or real CLI state. Only shell builtins are needed by these peers.
    fn isolated_peer(dir: &Path, name: &str, script: &str, launch: RpcLaunch) -> RpcSpawnSpec {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut spec = RpcSpawnSpec::new(launch, dir);
        spec.shell = Some("/bin/sh".into());
        spec.env.inherit = false;
        spec.env.set("PATH", dir).set("HOME", dir);
        spec.limits = RpcLimits {
            max_stdout_line_bytes: 512,
            max_stderr_line_bytes: 512,
            max_stdin_line_bytes: 512,
            max_pending_stdin_bytes: 512,
            max_pending_stdin_lines: 1,
            event_capacity: 1,
        };
        spec
    }

    #[test]
    fn opencode_launch_policy_reaches_the_child_with_explicit_restrictions_taking_precedence() {
        const KEY: &str = "OPENCODE_PERMISSION";
        const ALLOW: &str = r#"{"*":"allow"}"#;
        const ASK: &str = r#"{"*":"ask"}"#;
        let dir = temp();
        let profile = ProfileRegistry::builtin().get("opencode").unwrap();
        let cases = [
            (Permissions::default(), None, false, ALLOW),
            (Permissions::default(), Some(ASK), false, ASK),
            (Permissions::AgentDefault, None, false, "unset"),
            (
                Permissions::Custom {
                    args: vec![],
                    env: vec![(KEY.into(), ASK.into())],
                },
                Some(ALLOW),
                false,
                ASK,
            ),
            (
                Permissions::Custom {
                    args: vec![],
                    env: vec![(KEY.into(), ASK.into())],
                },
                Some(ALLOW),
                true,
                "unset",
            ),
        ];
        for (permissions, host, remove, expected) in cases {
            let launch = profile
                .rpc_launch_with_options(&RpcLaunchOptions::default().with_permissions(permissions))
                .unwrap();
            let mut spec = isolated_peer(
                dir.path(),
                "opencode",
                "[ \"$#\" -eq 1 ] && [ \"$1\" = acp ] || exit 9\nprintf '%s\\n' \"${OPENCODE_PERMISSION-unset}\"",
                launch,
            );
            if let Some(host) = host {
                spec.env.set(KEY, host);
            }
            if remove {
                spec.env.remove(KEY);
            }
            let (child, events) = RpcChild::spawn(spec).unwrap();
            assert_eq!(
                next_event(&events, "effective permission config"),
                line(child.id(), 0, expected)
            );
            assert!(matches!(
                next_event(&events, "peer exit"),
                RpcEvent::Exit { .. }
            ));
            assert_eq!(wait_exit(&child), Some(0));
        }
    }

    #[test]
    fn restricted_peer_interleaves_approval_and_progress_then_reports_terminal_failure() {
        let dir = temp();
        let profile = ProfileRegistry::builtin().get("claude").unwrap();
        let launch = profile
            .rpc_launch_with_options(&RpcLaunchOptions::default().with_permissions(
                Permissions::Custom {
                    args: vec!["--permission-mode".into(), "default".into()],
                    env: vec![],
                },
            ))
            .unwrap();
        // This synthetic protocol exercises the transport, not Claude wire compatibility.
        // The peer rejects an accidental bypass launch before emitting any approval. It
        // checks only the trailing custom flags, so new transport flags do not break it.
        let script = r#"
prev= last=
for arg do
  [ "$arg" = bypassPermissions ] && exit 9
  prev=$last last=$arg
done
[ "$prev" = --permission-mode ] && [ "$last" = default ] || exit 9
printf '%s\n' '{"id":7,"method":"approval/request"}' '{"method":"progress"}'
IFS= read -r reply || exit 8
[ "$reply" = '{"id":7,"result":"deny"}' ] || exit 8
printf '%s\n' '{"id":1,"error":{"code":-1,"message":"denied"}}'
exit 7
"#;
        let spec = isolated_peer(dir.path(), "claude", script, launch);
        let (child, events) = RpcChild::spawn(spec).unwrap();
        assert_eq!(
            next_event(&events, "approval"),
            line(child.id(), 0, r#"{"id":7,"method":"approval/request"}"#)
        );
        assert_eq!(
            next_event(&events, "interleaved progress"),
            line(child.id(), 1, r#"{"method":"progress"}"#)
        );
        // Approval handling belongs to the host, even with a one-event transport budget.
        child.send_line(r#"{"id":7,"result":"deny"}"#).unwrap();
        assert_eq!(
            next_event(&events, "terminal result"),
            line(
                child.id(),
                2,
                r#"{"id":1,"error":{"code":-1,"message":"denied"}}"#
            )
        );
        assert!(matches!(next_event(&events, "EOF"), RpcEvent::Exit { .. }));
        assert_eq!(wait_exit(&child), Some(7));
        assert!(matches!(child.send_line("late reply"), Err(Error::Closed)));
    }

    #[test]
    fn ids_come_from_the_session_counter() {
        let dir = temp();
        let (a, _ra) = RpcChild::spawn(sh_spec(dir.path(), "ids", "cat", &[])).expect("spawn");
        let (b, _rb) = RpcChild::spawn(sh_spec(dir.path(), "ids", "cat", &[])).expect("spawn");
        assert!(b.id() > a.id(), "{:?} then {:?}", a.id(), b.id());
        assert_ne!(a.pid(), b.pid());
        let shared = Arc::new(a);
        let other = Arc::clone(&shared);
        std::thread::spawn(move || other.send_line("from another thread"))
            .join()
            .unwrap()
            .expect("send from another thread");
    }

    /// unix-only refusals: an empty shell and a word the POSIX quoting refuses.
    #[test]
    fn preflight_refuses_an_empty_shell_and_a_control_character() {
        let dir = temp();
        let mut spec = sh_spec(dir.path(), "refuse", "cat", &[]);
        spec.shell = Some(String::new());
        assert_refused(spec, "an empty shell");
        let spec = sh_spec(dir.path(), "refuse", "agent", &["a\u{1b}[Ab"]);
        let err = assert_refused(spec, "a control character");
        assert!(err.to_string().contains("control character"), "{err}");
        preflight(&sh_spec(dir.path(), "refuse", "cat", &[])).expect("a valid spec passes");
    }
}

/// Run on the next Windows host pass: never executed so far (cross-linted only).
#[cfg(windows)]
mod windows {
    use super::*;

    #[test]
    fn an_unresolvable_launcher_is_not_found_before_any_process_exists() {
        let dir = temp();
        let bin = "agent-link-no-such-launcher";
        let mut spec = RpcSpawnSpec::new(RpcLaunch::new(bin, &["--rpc"]), dir.path());
        // An empty directory as the child's whole PATH: nothing resolves.
        spec.env.set("PATH", dir.path());
        let want = format!(
            "`{bin}` was not found on PATH — install the {bin} CLI, or pick a different agent for this project"
        );
        match preflight(&spec) {
            Err(Error::NotFound(msg)) => assert_eq!(msg, want),
            other => panic!("preflight: {other:?}"),
        }
        match RpcChild::spawn(spec) {
            Err(Error::NotFound(msg)) => assert_eq!(msg, want),
            Err(other) => panic!("spawn: {other}"),
            Ok(_) => panic!("spawn started a child"),
        }
    }

    #[test]
    fn a_resolved_launcher_runs_and_its_exit_is_reported() {
        let dir = temp();
        let spec = RpcSpawnSpec::new(
            RpcLaunch::new("cmd", &["/d", "/c", "echo", "hello"]),
            dir.path(),
        );
        let (child, events) = RpcChild::spawn(spec).expect("spawn cmd");
        match next_event(&events, "hello") {
            RpcEvent::Line { seq: 0, line, .. } => assert_eq!(line, "hello"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(next_event(&events, "exit"), RpcEvent::Exit { .. }));
        assert_eq!(wait_exit(&child), Some(0));
        assert_eq!(child.kill(), Some(0));

        let spec = RpcSpawnSpec::new(
            RpcLaunch::new("cmd", &["/d", "/c", "exit", "3"]),
            dir.path(),
        );
        let (child, events) = RpcChild::spawn(spec).expect("spawn cmd");
        assert!(matches!(next_event(&events, "exit"), RpcEvent::Exit { .. }));
        assert_eq!(wait_exit(&child), Some(3));
        assert_eq!(child.kill(), Some(3), "no taskkill after a confirmed exit");
    }

    #[test]
    fn kill_ends_a_running_tree_and_sends_no_exit() {
        let dir = temp();
        let spec = RpcSpawnSpec::new(
            RpcLaunch::new("ping", &["-n", "30", "127.0.0.1"]),
            dir.path(),
        );
        let (child, events) = RpcChild::spawn(spec).expect("spawn ping");
        assert_eq!(child.try_wait().unwrap(), None, "running");
        let code = child.kill();
        assert!(
            code.is_some(),
            "Windows reports the terminate code: {code:?}"
        );
        assert_disconnects_without_exit(&events);
    }
}
