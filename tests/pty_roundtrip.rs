//! `Terminal` against a real `/bin/sh` on a PTY: output, input, resize, exit, teardown,
//! foreground, environment, direct mode, the event bus, the session log with `attach`, and
//! the registry. Every wait has a deadline; every session is torn down by the end of its
//! test.
#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use agent_link::{
    Confidence, Event, ForegroundOwner, PtySize, Replay, SessionLogConfig, SessionRegistry,
    SpawnSpec, Terminal, TerminalIdentity, forward_to,
};
use common::{WAIT, deadline, output_until, recv_until, sh_spec, start, start_sh, wait_until};
use crossbeam_channel::{Receiver, RecvTimeoutError};

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

/// Waits until the receiver disconnects, failing on an `Exit` (a teardown never sends one).
fn assert_disconnects_without_exit(rx: &Receiver<Event>) {
    let deadline = deadline();
    loop {
        match rx.recv_deadline(deadline) {
            Ok(Event::Exit { .. }) => panic!("Exit after teardown"),
            Ok(_) => {}
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => panic!("the receiver did not disconnect"),
        }
    }
}

/// `(seq, bytes)` of every output on `rx` until `needle` was seen.
fn outputs_until(rx: &Receiver<Event>, needle: &str) -> Vec<(u64, Vec<u8>)> {
    let deadline = deadline();
    let mut seen = Vec::new();
    let mut text = String::new();
    while !text.contains(needle) {
        match rx.recv_deadline(deadline) {
            Ok(Event::Output { seq, bytes, .. }) => {
                text.push_str(&String::from_utf8_lossy(&bytes));
                seen.push((seq, bytes));
            }
            Ok(other) => panic!("unexpected {other:?}"),
            Err(e) => panic!("no {needle:?} ({e}); output so far: {text:?}"),
        }
    }
    seen
}

fn assert_consecutive(seqs: &[(u64, Vec<u8>)]) {
    for pair in seqs.windows(2) {
        assert_eq!(pair[1].0, pair[0].0 + 1, "seq must grow by one per chunk");
    }
}

#[test]
fn echo_round_trip() {
    let home = temp();
    let (term, events) = start_sh(home.path());
    // Computed, so the terminal's echo of the typed line cannot match.
    term.write(b"echo agent-link-$((40+2))\n").unwrap();
    output_until(&events, "agent-link-42");
}

#[test]
fn resize_reaches_the_child() {
    let home = temp();
    let (term, events) = start_sh(home.path());
    term.resize(PtySize {
        rows: 33,
        cols: 101,
    })
    .unwrap();
    term.write(b"stty size\n").unwrap();
    output_until(&events, "33 101");
    assert!(term.resize(PtySize { rows: 0, cols: 10 }).is_err());
    let (snapshot, _) = term.attach_recovery(None).unwrap();
    assert_eq!(
        snapshot.size,
        PtySize {
            rows: 33,
            cols: 101
        }
    );
    #[cfg(feature = "screen")]
    assert_eq!(snapshot.display.unwrap().size, snapshot.size);
}

#[test]
fn a_shell_that_exits_sends_exit_and_try_wait_reports_the_code() {
    let home = temp();
    let (term, events) = start_sh(home.path());
    assert_eq!(term.try_wait(), None);
    term.write(b"exit 3\n").unwrap();
    let exit = recv_until(&events, deadline(), |e| matches!(e, Event::Exit { .. }));
    assert_eq!(
        exit,
        Event::Exit {
            id: term.id(),
            status: None
        }
    );
    wait_until("try_wait to report the exit", || term.try_wait().is_some());
    assert_eq!(term.try_wait().unwrap().status, Some(3));
    assert_eq!(term.kill().status, Some(3), "teardown keeps the real code");
}

#[test]
fn teardown_sends_no_exit_and_disconnects_receivers() {
    let home = temp();
    let (term, events) = start_sh(home.path());
    let second = term.subscribe();
    term.write(b"echo ready-$((1+1))\n").unwrap();
    output_until(&events, "ready-2");
    let started = Instant::now();
    let exit = term.kill();
    assert!(
        started.elapsed() < WAIT,
        "teardown took {:?}",
        started.elapsed()
    );
    assert_eq!(exit.status, None, "killed by a signal");
    assert_disconnects_without_exit(&events);
    assert_disconnects_without_exit(&second);
}

#[test]
fn foreground_follows_a_job_and_returns_to_the_shell() {
    let home = temp();
    let (term, events) = start_sh(home.path());
    term.write(b"echo ready-$((2+2))\n").unwrap();
    output_until(&events, "ready-4");
    wait_until("the shell in front", || {
        term.foreground().owner == ForegroundOwner::Shell
    });

    term.write(b"sleep 30\n").unwrap();
    wait_until("sleep in front", || {
        term.foreground().owner == ForegroundOwner::Agent
    });
    assert_eq!(term.foreground().confidence, Confidence::Exact);

    term.interrupt().unwrap();
    wait_until("the shell back in front", || {
        term.foreground().owner == ForegroundOwner::Shell
    });
    assert_eq!(term.foreground().confidence, Confidence::Exact);
}

/// The job ignores SIGHUP, so only the foreground-group SIGKILL can end it (the shell's
/// own group kill does not reach a job in its own group).
#[test]
fn drop_kills_the_foreground_job_group() {
    let home = temp();
    let (term, events) = start_sh(home.path());
    term.write(b"sh -c 'trap \"\" HUP; echo \"fg-pid:$$:\"; exec sleep 1000'\n")
        .unwrap();
    let text = output_until(&events, ":\r\n");
    // The echoed command line holds `fg-pid:$$:`; the job's line holds its digits.
    let pid: libc::pid_t = text
        .split("fg-pid:")
        .filter_map(|rest| rest.split(':').next()?.parse().ok())
        .next()
        .unwrap_or_else(|| panic!("no pid in {text:?}"));
    wait_until("the job in front", || {
        term.foreground().owner == ForegroundOwner::Agent
    });
    // SAFETY: signal 0 only checks that the process exists.
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "the job runs");

    drop(term);
    wait_until("the job to be gone", || {
        // SAFETY: as above.
        let gone = unsafe { libc::kill(pid, 0) } != 0;
        gone && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    });
}

#[test]
fn identity_and_host_policy_reach_the_child() {
    let home = temp();
    let env_lines = |identity: TerminalIdentity, set: &[(&str, &str)]| -> Vec<String> {
        let mut spec = sh_spec(home.path());
        spec.identity = identity;
        for (k, v) in set {
            spec.env.set(*k, *v);
        }
        let (term, events) = start(spec);
        term.write(b"env; echo env-done-$((1+1))\n").unwrap();
        let text = output_until(&events, "env-done-2");
        text.lines()
            .map(|l| l.trim_end_matches('\r').to_string())
            .collect()
    };
    let dumb = || TerminalIdentity::Custom {
        vars: vec![("TERM".into(), "dumb".into())],
    };

    let lines = env_lines(dumb(), &[("FOO", "bar")]);
    assert!(lines.iter().any(|l| l == "TERM=dumb"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "FOO=bar"), "{lines:?}");

    // The host's policy is applied after the identity: it wins.
    let lines = env_lines(dumb(), &[("TERM", "vt100")]);
    assert!(lines.iter().any(|l| l == "TERM=vt100"), "{lines:?}");
    assert!(!lines.iter().any(|l| l == "TERM=dumb"), "{lines:?}");
}

#[test]
fn direct_mode_runs_the_program_itself() {
    let home = temp();
    let spec = SpawnSpec::direct(
        "/bin/echo",
        ["hi"],
        home.path(),
        PtySize { rows: 24, cols: 80 },
    );
    let (term, events) = common::spawn(spec).unwrap();
    let mut text = String::new();
    let deadline = deadline();
    loop {
        match events.recv_deadline(deadline).expect("output, then Exit") {
            Event::Output { bytes, .. } => text.push_str(&String::from_utf8_lossy(&bytes)),
            Event::Exit { .. } => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(text.contains("hi"), "{text:?}");
    wait_until("try_wait to report the exit", || term.try_wait().is_some());
    assert_eq!(term.try_wait().unwrap().status, Some(0));
}

#[test]
fn direct_mode_child_in_front_is_the_agent() {
    let home = temp();
    let spec = SpawnSpec::direct(
        "/bin/sleep",
        ["30"],
        home.path(),
        PtySize { rows: 24, cols: 80 },
    );
    let (term, _events) = common::spawn(spec).unwrap();
    wait_until("the direct child in front", || {
        term.foreground().owner == ForegroundOwner::Agent
    });
    assert_eq!(term.foreground().confidence, Confidence::Exact);
}

#[test]
fn every_subscriber_gets_every_output_in_order() {
    let home = temp();
    let (term, first) = start_sh(home.path());
    let second = term.subscribe();
    term.write(b"echo one-$((0+1)); echo two-$((1+1))\n")
        .unwrap();
    let a = outputs_until(&first, "two-2");
    let b = outputs_until(&second, "two-2");
    assert_consecutive(&a);
    assert_consecutive(&b);
    // The second subscribed later: what it saw is the tail of what the first saw.
    let start = a
        .iter()
        .position(|(seq, _)| *seq == b[0].0)
        .expect("same seq space");
    assert_eq!(&a[start..start + b.len()], &b[..]);

    // Dropping one receiver does not stall the other, even through lots of output.
    drop(first);
    term.write(
        b"i=0; while [ $i -lt 3000 ]; do echo line-$i; i=$((i+1)); done; echo end-$((3+4))\n",
    )
    .unwrap();
    let rest = outputs_until(&second, "end-7");
    assert_consecutive(&rest);
}

#[test]
fn forward_to_delivers_in_order_and_ends_after_drop() {
    let home = temp();
    let (term, events) = start_sh(home.path());
    let (seen_tx, seen_rx) = std::sync::mpsc::channel();
    let forwarder = forward_to(events, move |event| {
        let _ = seen_tx.send(event);
    })
    .unwrap();
    term.write(b"echo fwd-$((5+5))\n").unwrap();
    let deadline = deadline();
    let mut text = String::new();
    let mut next_seq = 0;
    while !text.contains("fwd-10") {
        match seen_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Output { seq, bytes, .. }) => {
                assert_eq!(seq, next_seq, "in order, none skipped");
                next_seq += 1;
                text.push_str(&String::from_utf8_lossy(&bytes));
            }
            other => panic!("unexpected {other:?}; output so far: {text:?}"),
        }
    }
    drop(term);
    wait_until("the forwarder to end", || forwarder.is_finished());
    forwarder.join().unwrap();
}

#[test]
fn session_ids_are_monotonic_across_spawns() {
    let home = temp();
    let (a, _a_events) = common::spawn(sh_spec(home.path())).unwrap();
    let (b, _b_events) = common::spawn(sh_spec(home.path())).unwrap();
    assert!(a.id().0 >= 1);
    assert!(b.id() > a.id());
}

/// A receiver nobody drains fills up and parks the reader; teardown still completes, and
/// the receiver disconnects after its backlog.
#[test]
fn teardown_frees_a_reader_blocked_on_an_undrained_receiver() {
    let home = temp();
    let (term, events) = start_sh(home.path());
    term.write(b"yes\n").unwrap();
    wait_until("the receiver to fill", || events.is_full());
    let started = Instant::now();
    term.kill();
    assert!(
        started.elapsed() < WAIT,
        "teardown took {:?}",
        started.elapsed()
    );
    let deadline = deadline();
    let mut drained = 0;
    loop {
        match events.recv_deadline(deadline) {
            Ok(Event::Output { .. }) => drained += 1,
            Ok(other) => panic!("unexpected {other:?}"),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!("no disconnect after {drained} events"),
        }
    }
    assert!(drained >= 1000, "the backlog is delivered: {drained}");
}

#[test]
fn write_queues_without_waiting_for_the_child() {
    let home = temp();
    let (term, events) = start_sh(home.path());
    // `sleep` leaves the input unread; the queued lines reach the shell after it, in call
    // order. Stay well under the tty's canonical input limit (1 KiB on macOS): typed-ahead
    // input past it is garbled by the line discipline, whoever writes it.
    term.write(b"sleep 1\n").unwrap();
    let started = Instant::now();
    for i in 0..10 {
        term.write(format!("echo q{i}-$(({i}+1000))\n").as_bytes())
            .unwrap();
    }
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "write must not wait for the child: {:?}",
        started.elapsed()
    );
    let text = output_until(&events, "q9-1009");
    let positions: Vec<usize> = (0..10)
        .map(|i| {
            let line = format!("q{i}-{}\r\n", i + 1000);
            text.find(&line)
                .unwrap_or_else(|| panic!("no {line:?} in {text:?}"))
        })
        .collect();
    assert!(positions.windows(2).all(|p| p[0] < p[1]), "{text:?}");
}

/// Sets `$a` to 2 000 `a`s with shell builtins only: no process start, which can take
/// seconds on a loaded host whose endpoint-security agent vets every exec.
const SET_A2000: &[u8] =
    b"a=aaaaaaaaaa; a=$a$a$a$a$a$a$a$a$a$a; a=$a$a$a$a$a$a$a$a$a$a$a$a$a$a$a$a$a$a$a$a\n";

/// `(seq, bytes)` of each event on `rx` until it disconnects (an `Exit` ends it too).
fn drain_outputs(rx: Receiver<Event>) -> Vec<(u64, Vec<u8>)> {
    let mut seen = Vec::new();
    for event in rx {
        match event {
            Event::Output { seq, bytes, .. } => seen.push((seq, bytes)),
            Event::Exit { .. } => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    seen
}

/// The bytes of every chunk in `stream` (all chunks from seq 0) before `seq`.
fn bytes_before(stream: &[(u64, Vec<u8>)], seq: u64) -> Vec<u8> {
    stream
        .iter()
        .take_while(|(s, _)| *s < seq)
        .flat_map(|(_, b)| b.iter().copied())
        .collect()
}

/// Lossy text of a byte stream, for `contains` checks.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Attaches race a shell printing 2 000-byte lines: for every attach, the replay followed
/// by the live chunks is exactly the stream a receiver from spawn saw — no gap, no
/// duplicate.
#[test]
fn attach_has_no_gap_and_no_overlap_while_output_flows() {
    use std::sync::{Arc, Mutex};

    const ROUNDS: usize = 20;
    let home = temp();
    let mut spec = sh_spec(home.path());
    spec.log = SessionLogConfig::Memory { cap_bytes: 1 << 20 };
    let (term, events) = start(spec);
    term.write(SET_A2000).unwrap();
    // Every receiver is drained on its own thread, so none fills up and stalls the reader.
    let full_text = Arc::new(Mutex::new(Vec::<u8>::new()));
    let full = {
        let full_text = Arc::clone(&full_text);
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            for event in events {
                if let Event::Output { seq, bytes, .. } = event {
                    full_text.lock().unwrap().extend_from_slice(&bytes);
                    seen.push((seq, bytes));
                }
            }
            seen
        })
    };
    let mut attached = Vec::new();
    let mut recovered = Vec::new();
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            for _ in 0..ROUNDS {
                term.write(b"echo $a\n").unwrap();
                std::thread::sleep(Duration::from_millis(15));
            }
        });
        while !writer.is_finished() {
            let (replay, rx) = term.attach();
            let drain = std::thread::spawn(move || drain_outputs(rx));
            attached.push((replay, drain));
            let (snapshot, rx) = term.attach_recovery(None).unwrap();
            let drain = std::thread::spawn(move || drain_outputs(rx));
            recovered.push((snapshot, drain));
            std::thread::sleep(Duration::from_millis(3));
        }
    });
    // A last line every attached receiver sees live.
    term.write(b"echo attach-done-$((6*7))\n").unwrap();
    wait_until("the marker on the full stream", || {
        text(&full_text.lock().unwrap()).contains("attach-done-42")
    });
    // The tty echo of a line typed ahead can land inside an output line, so count bytes.
    let printed = full_text
        .lock()
        .unwrap()
        .iter()
        .filter(|&&b| b == b'a')
        .count();
    assert!(printed >= ROUNDS * 2000, "every echo ran: {printed}");
    // Let the prompt after the marker land, then tear down; compare per seq so a chunk cut
    // off by the teardown race cannot matter.
    std::thread::sleep(Duration::from_millis(200));
    drop(term);
    let full = full.join().unwrap();
    assert_consecutive(&full);
    assert_eq!(full[0].0, 0);
    let total: usize = full.iter().map(|(_, b)| b.len()).sum();
    let mut mid_stream = 0;
    for (replay, drain) in attached {
        let replay = replay.into_bytes().unwrap();
        let live = drain.join().unwrap();
        assert!(!live.is_empty(), "every attach sees the marker live");
        assert_consecutive(&live);
        assert_eq!(
            replay,
            bytes_before(&full, live[0].0),
            "the replay is everything before the first live chunk"
        );
        // `full` is consecutive from 0, so a seq is its index.
        for (seq, bytes) in &live {
            if let Some((full_seq, want)) = full.get(*seq as usize) {
                assert_eq!((full_seq, want), (seq, bytes), "live chunk {seq}");
            }
        }
        if !replay.is_empty() && replay.len() < total {
            mid_stream += 1;
        }
    }
    assert!(mid_stream > 0, "no attach landed while output flowed");
    for (snapshot, drain) in recovered {
        let live = drain.join().unwrap();
        let replay = snapshot.replay.into_bytes().unwrap();
        assert!(snapshot.gaps.is_empty());
        assert_eq!(snapshot.replay_range, 0..snapshot.boundary.offset);
        assert_eq!(replay.len() as u64, snapshot.boundary.offset);
        assert_eq!(live.first().unwrap().0, snapshot.next_output_seq);
        assert_eq!(replay, bytes_before(&full, snapshot.next_output_seq));
        assert_consecutive(&live);
        for (seq, bytes) in live {
            if let Some((_, want)) = full.get(seq as usize) {
                assert_eq!(&bytes, want);
            }
        }
        #[cfg(feature = "screen")]
        {
            use agent_link::screen::{ScreenModel, Vt100Model};
            let mut model = Vt100Model::new(snapshot.size, 0, None);
            model.process(&replay);
            let display = snapshot.display.unwrap();
            assert_eq!(display.rows, model.snapshot().rows);
            assert_eq!(display.cursor, model.snapshot().cursor);
        }
    }
}

#[test]
fn recovery_cursor_reconnects_without_repeating_consumed_output() {
    let home = temp();
    let (term, events) = start_sh(home.path());
    term.write(b"echo recovery-$((10+1))\n").unwrap();
    output_until(&events, "recovery-11");
    let (first, live) = term.attach_recovery(None).unwrap();
    term.write(b"echo recovery-$((10+2))\n").unwrap();
    let consumed = outputs_until(&live, "recovery-12");
    drop(live);
    let (resumed, _) = term.attach_recovery(Some(first.boundary)).unwrap();
    let replay = resumed.replay.into_bytes().unwrap();
    let expected: Vec<u8> = consumed
        .iter()
        .flat_map(|(_, bytes)| bytes.iter().copied())
        .collect();
    assert!(replay.starts_with(&expected));
    assert!(resumed.gaps.is_empty());
    assert_eq!(
        resumed.replay_range,
        first.boundary.offset..resumed.boundary.offset
    );
    assert_eq!(consumed[0].0, first.next_output_seq);
}

#[test]
fn a_small_memory_log_keeps_only_the_newest_output() {
    let home = temp();
    let mut spec = sh_spec(home.path());
    spec.log = SessionLogConfig::Memory { cap_bytes: 1024 };
    let (term, events) = start(spec);
    term.write(SET_A2000).unwrap();
    term.write(b"echo $a$a; echo cap-$((5*5))\n").unwrap();
    let mut stream = Vec::new();
    let deadline = deadline();
    while !text(&bytes_before(&stream, u64::MAX)).contains("cap-25") {
        match events.recv_deadline(deadline) {
            Ok(Event::Output { seq, bytes, .. }) => stream.push((seq, bytes)),
            other => panic!("unexpected {other:?}"),
        }
    }
    let (replay, live) = term.attach();
    term.write(b"echo after-$((2+2))\n").unwrap();
    let first_live = outputs_until(&live, "after-4")[0].0;
    // Everything up to the first live chunk, from the receiver attached at spawn.
    stream.extend(outputs_until(&events, "after-4"));
    let before = bytes_before(&stream, first_live);
    let Replay::Bytes(kept) = replay else {
        panic!("memory replay expected, got {replay:?}");
    };
    assert!(kept.len() <= 1024 + 8 * 1024, "kept {} bytes", kept.len());
    assert!(kept.len() < before.len(), "something was evicted");
    assert!(before.ends_with(&kept), "the kept bytes are the newest");
}

#[test]
fn a_file_log_records_the_stream_and_replays_its_length() {
    let home = temp();
    let path = home.path().join("session.log");
    let mut spec = sh_spec(home.path());
    spec.log = SessionLogConfig::File { path: path.clone() };
    let (term, events) = start(spec);
    term.write(b"echo file-$((2+3))\n").unwrap();
    let mut stream = outputs_until(&events, "file-5");

    let (replay, live) = term.attach();
    term.write(b"echo after-$((3+4))\n").unwrap();
    let first_live = outputs_until(&live, "after-7")[0].0;
    // Up to the prompt after the marker; the shell is idle then.
    stream.extend(outputs_until(&events, "after-7\r\n$ "));
    let Replay::File {
        path: replay_path,
        len,
    } = replay.clone()
    else {
        panic!("file replay expected, got {replay:?}");
    };
    assert_eq!(replay_path, path);
    let before = bytes_before(&stream, first_live);
    assert_eq!(len, before.len() as u64);
    assert_eq!(replay.into_bytes().unwrap(), before);

    drop(term);
    let rest: Vec<(u64, Vec<u8>)> = drain_outputs(events);
    stream.extend(rest);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes_before(&stream, u64::MAX),
        "the file holds the whole stream"
    );
}

/// The file log is emptied only once the child runs: a failed spawn keeps an earlier log.
#[test]
fn a_failed_spawn_keeps_an_earlier_file_log() {
    let home = temp();
    let path = home.path().join("session.log");
    std::fs::write(&path, b"earlier session").unwrap();
    let mut spec = SpawnSpec::direct(
        home.path().join("no-such-program"),
        Vec::<String>::new(),
        home.path(),
        PtySize { rows: 24, cols: 80 },
    );
    spec.log = SessionLogConfig::File { path: path.clone() };
    assert!(common::spawn(spec).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"earlier session");

    let mut spec = sh_spec(home.path());
    spec.log = SessionLogConfig::File { path: path.clone() };
    let (term, events) = start(spec);
    term.write(b"echo new-$((3+3))\n").unwrap();
    output_until(&events, "new-6");
    let replay = term.attach().0.into_bytes().unwrap();
    assert!(text(&replay).contains("new-6"), "{:?}", text(&replay));
    assert!(!text(&replay).contains("earlier"), "{:?}", text(&replay));
}

/// A receiver added after the child exited gets the whole output as its replay, then the
/// `Exit`: a late attacher waiting for the end does not hang.
#[test]
fn attach_after_the_exit_replays_everything_and_then_the_exit() {
    let home = temp();
    let spec = SpawnSpec::direct(
        "/bin/echo",
        ["late-attach"],
        home.path(),
        PtySize { rows: 24, cols: 80 },
    );
    let (term, events) = common::spawn(spec).unwrap();
    recv_until(&events, deadline(), |e| matches!(e, Event::Exit { .. }));
    let exit = Event::Exit {
        id: term.id(),
        status: None,
    };
    let (replay, attached) = term.attach();
    assert!(text(&replay.into_bytes().unwrap()).contains("late-attach"));
    assert_eq!(attached.recv_deadline(deadline()), Ok(exit.clone()));
    let subscribed = term.subscribe();
    assert_eq!(subscribed.recv_deadline(deadline()), Ok(exit));
    drop(term);
    assert_disconnects_without_exit(&attached);
    assert_disconnects_without_exit(&subscribed);
}

#[test]
fn attach_with_the_log_off_replays_nothing() {
    let home = temp();
    let mut spec = sh_spec(home.path());
    spec.log = SessionLogConfig::Off;
    let (term, events) = start(spec);
    output_until(&events, "$ ");
    let (replay, live) = term.attach();
    assert_eq!(replay, Replay::None);
    term.write(b"echo off-$((1+8))\n").unwrap();
    output_until(&live, "off-9");
}

#[test]
fn a_registry_keeps_one_terminal_per_key_and_evicted_ones_die() {
    let home = temp();
    let (first, _first_events) = start_sh(home.path());
    let (second, second_events) = start_sh(home.path());
    let first_pid = first.pid().expect("pid") as libc::pid_t;
    let second_id = second.id();

    let mut registry = SessionRegistry::new();
    assert!(registry.is_empty());
    assert!(registry.insert("pane", first).is_none());
    let evicted = registry
        .insert("pane", second)
        .expect("the first comes back");
    assert_eq!(registry.len(), 1);
    assert_eq!(registry.get(&"pane").map(Terminal::id), Some(second_id));
    // SAFETY: signal 0 only checks that the process exists.
    assert_eq!(
        unsafe { libc::kill(first_pid, 0) },
        0,
        "alive until dropped"
    );
    drop(evicted);
    // Teardown reaped it, so the pid is gone at once (unless the reap was handed off).
    wait_until("the evicted shell to be gone", || {
        // SAFETY: as above.
        let gone = unsafe { libc::kill(first_pid, 0) } != 0;
        gone && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    });

    // The stored one still works; `drain` hands it out and dropping it ends it.
    registry
        .get(&"pane")
        .unwrap()
        .write(b"echo kept-$((4+4))\n")
        .unwrap();
    output_until(&second_events, "kept-8");
    let drained: Vec<_> = registry.drain().collect();
    assert_eq!(drained.len(), 1);
    assert!(registry.is_empty());
    drop(drained);
    assert_disconnects_without_exit(&second_events);
    assert!(registry.remove(&"pane").is_none());
}

#[cfg(feature = "observers")]
#[test]
fn observer_live_attachment_has_geometry_and_no_replay_copy() {
    use agent_link::observers::{Disconnect, Item, Limits, ObserverHub};
    let home = temp();
    let (term, events) = start_sh(home.path());
    term.write(b"echo before-observer-$((1+1))\n").unwrap();
    output_until(&events, "before-observer-2");
    term.resize(PtySize { rows: 31, cols: 97 }).unwrap();
    let (fence, live) = term.attach_live_recovery().unwrap();
    assert_eq!(
        fence.replay_range,
        fence.boundary.offset..fence.boundary.offset
    );
    assert!(fence.gaps.is_empty());
    assert!(fence.replay.into_bytes().unwrap().is_empty());
    assert_eq!(fence.size, PtySize { rows: 31, cols: 97 });
    drop(live);
    let (metadata, temporary) = term.attach_live_recovery_metadata().unwrap();
    assert_eq!(metadata.boundary, metadata.requested);
    assert_eq!(
        metadata.replay_range,
        metadata.boundary.offset..metadata.boundary.offset
    );
    assert!(metadata.replay.into_bytes().unwrap().is_empty());
    assert!(metadata.gaps.is_empty());
    assert_eq!(metadata.size, PtySize { rows: 31, cols: 97 });
    #[cfg(feature = "screen")]
    assert!(metadata.display.is_none());
    drop(temporary);
    let mut hub = ObserverHub::new(&term, Limits::default()).unwrap();
    term.resize(PtySize { rows: 32, cols: 98 }).unwrap();
    let viewer = hub.attach(None).unwrap();
    assert_eq!(viewer.initial_geometry.1, PtySize { rows: 31, cols: 97 });
    // Shell output can arrive between hub startup and viewer attachment. The
    // unavailable prefix ends at startup; retained bytes then precede live output.
    assert_eq!(viewer.gap, Some(0..viewer.initial_geometry.0.offset));
    let (fresh, temporary) = term.attach_live_recovery().unwrap();
    assert_eq!(fresh.size, PtySize { rows: 32, cols: 98 });
    drop(temporary);
    term.write(b"echo after-observer-$((2+2))\n").unwrap();
    let mut data = Vec::new();
    let mut cursor = viewer.initial_geometry.0;
    while !String::from_utf8_lossy(&data).contains("after-observer-4") {
        let Item::Output {
            start, end, bytes, ..
        } = viewer.recv_timeout(WAIT).expect("observer output")
        else {
            panic!("unexpected exit")
        };
        assert_eq!(start, cursor);
        cursor = end;
        data.extend_from_slice(&bytes);
    }
    assert!(cursor.offset >= viewer.boundary.offset);
    output_until(&events, "after-observer-4");
    hub.shutdown();
    hub.shutdown();
    assert_eq!(viewer.reason(), Some(Disconnect::Closed));
    let mut tight = ObserverHub::new(
        &term,
        Limits {
            viewer_bytes: 1,
            ..Limits::default()
        },
    )
    .unwrap();
    let slow = tight.attach(None).unwrap();
    term.write(b"echo independent-drain-$((3+3))\n").unwrap();
    output_until(&events, "independent-drain-6");
    wait_until("slow observer disconnect", || {
        slow.reason() == Some(Disconnect::SlowViewer)
    });
    term.write(b"echo raw-still-live-$((4+4))\n").unwrap();
    output_until(&events, "raw-still-live-8");
    tight.shutdown();
    term.kill();
}
