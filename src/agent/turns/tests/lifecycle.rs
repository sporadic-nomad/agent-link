use std::fs;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, bounded};

use super::super::*;
use super::peer::*;
use crate::Event;
use crate::agent::{AbortReason, DeliveryOutcome, Revalidate, SendOptions};

// Queue validated replies and hold the next turn in the parser so tests can
// exercise the cutoff on pending text, in-flight text, and status.
fn paused(peer: &Peer) -> (TurnReceiver, Barrier, Receiver<()>) {
    let (barrier, pause) = Barrier::new();
    let (done, reset) = bounded(1);
    let parser = JsonParser {
        pause: Some(pause),
        reset: Some(done),
    };
    let rx = peer
        .term
        .attach_turns(peer.source(), parser, Options::default())
        .unwrap();
    observed(&rx, &peer.file);
    turn(&peer.file, "still current");
    turn(&peer.file, "queued");
    observed(&rx, &peer.file);
    turn(&peer.file, "pause");
    barrier.wait();
    (rx, barrier, reset)
}

#[test]
fn a_paused_batch_loses_both_queued_and_staged_text_on_in_process_switch() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "original", false, None);
    let (rx, barrier, reset) = paused(&peer);
    assert_eq!(rx.try_recv().unwrap().text, "still current");
    // Same writer, same lifecycle, and even the same file again by the next poll.
    peer.input("/clear");
    peer.input("/resume");
    until("writer switched away and back", || {
        fs::read_to_string(&peer.witness)
            .unwrap()
            .starts_with("original\noriginal\n2\n")
    });
    turn(&peer.file, "another writer appended to the old file");
    assert!(
        rx.try_recv().is_none(),
        "the previous session's reply escaped"
    );
    gap(&rx, Gap::SessionChanged);
    barrier.release();
    reset.recv_timeout(WAIT).unwrap();
    let fresh = peer.attach();
    observed(&fresh, &peer.file);
    assert!(fresh.try_recv().is_none());
    turn(&peer.file, "after the new baseline");
    observed(&fresh, &peer.file);
    assert_eq!(fresh.try_recv().unwrap().text, "after the new baseline");
}

#[test]
fn replacing_the_transcript_cuts_off_queued_replies_while_the_parser_is_blocked() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "replaced", false, None);
    let (rx, barrier, reset) = paused(&peer);
    let replacement = peer.file.with_extension("new");
    fs::copy(&peer.file, &replacement).unwrap();
    fs::rename(replacement, &peer.file).unwrap();
    assert!(rx.try_recv().is_none());
    assert_eq!(rx.status(), Status::Gap(Gap::Replaced));
    barrier.release();
    reset.recv_timeout(WAIT).unwrap();
    gap(&rx, Gap::Replaced);
}

#[test]
fn changed_bytes_inside_a_paused_candidate_do_not_commit() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "rewrite", false, None);
    let (rx, barrier, _) = paused(&peer);
    let contents = fs::read_to_string(&peer.file).unwrap();
    fs::write(&peer.file, contents.replace("pause", "other")).unwrap();
    barrier.release();
    gap(&rx, Gap::Rewritten);
}

#[test]
fn a_rewritten_prefix_of_a_split_record_invalidates_the_whole_batch() {
    let (_lock, home) = project();
    for during_parse in [false, true] {
        let id = if during_parse { "during" } else { "before" };
        let peer = Peer::start(home.path(), id, false, None);
        let (barrier, pause) = Barrier::new();
        let parser = JsonParser {
            pause: during_parse.then_some(pause),
            ..JsonParser::default()
        };
        let rx = peer
            .term
            .attach_turns(peer.source(), parser, Options::default())
            .unwrap();
        observed(&rx, &peer.file);
        turn(&peer.file, "queued");
        append(&peer.file, b"\"user\"\n");
        let start = peer.file.metadata().unwrap().len();
        let mut prefix = vec![b' '; if during_parse { 70 * 1024 } else { 0 }];
        let value = start + prefix.len() as u64 + br#"{"end":""#.len() as u64;
        prefix.extend_from_slice(br#"{"end":"pause"}"#);
        // Keep the rewrite well outside the cursor overlap and span several reads.
        prefix.resize(80 * 1024, b' ');
        append(&peer.file, &prefix);
        observed(&rx, &peer.file);
        let rewrite = || {
            fs::OpenOptions::new()
                .write(true)
                .open(&peer.file)
                .unwrap()
                .write_all_at(b"other", value)
                .unwrap();
        };
        if during_parse {
            append(&peer.file, b"\n");
            barrier.wait();
            rewrite();
            barrier.release();
        } else {
            rewrite();
            append(&peer.file, b"\n");
        }
        gap(&rx, Gap::Rewritten);
    }
}

// After the parser releases, let the first validation pass finish and park the
// next writer check. The test can now modify bytes the previous pass already read.
struct BetweenPasses {
    writer: Writer,
    after_parse: Receiver<()>,
    pause_next: bool,
    pause: Pause,
}

impl Source for BetweenPasses {
    fn witness(&self) -> Arc<dyn Witness> {
        self.writer.witness()
    }

    fn current(&mut self, launch: &Launch) -> Result<Binding, Gap> {
        if self.pause_next {
            self.pause.park();
        }
        self.pause_next = self.after_parse.try_recv().is_ok();
        self.writer.current(launch)
    }
}

#[test]
fn an_append_during_prefix_verification_preserves_replies_only_if_prior_bytes_still_match() {
    let (_lock, home) = project();
    for rewrite in [false, true] {
        let id = if rewrite { "edited" } else { "appended" };
        let peer = Peer::start(home.path(), id, false, None);
        let (arm, after_parse) = bounded(1);
        let (verifying, pause) = Barrier::new();
        let source = BetweenPasses {
            writer: peer.source(),
            after_parse,
            pause_next: false,
            pause,
        };
        let (parsed, pause) = Barrier::new();
        let parser = JsonParser {
            pause: Some(pause),
            ..JsonParser::default()
        };
        let rx = peer
            .term
            .attach_turns(source, parser, Options::default())
            .unwrap();
        observed(&rx, &peer.file);
        let mut line = b"\"user\"\n{\"end\":\"pause\"}".to_vec();
        line.resize(80 * 1024, b' ');
        append(&peer.file, &line);
        observed(&rx, &peer.file);
        append(&peer.file, b"\n");
        parsed.wait();
        arm.send(()).unwrap();
        parsed.release();
        verifying.wait();
        if rewrite {
            let offset = b"\"user\"\n{\"end\":\"".len() as u64;
            fs::OpenOptions::new()
                .write(true)
                .open(&peer.file)
                .unwrap()
                .write_all_at(b"other", offset)
                .unwrap();
        }
        turn(&peer.file, "next");
        verifying.release();
        if rewrite {
            gap(&rx, Gap::Rewritten);
        } else {
            observed(&rx, &peer.file);
            assert_eq!(rx.try_recv().unwrap().text, "pause");
            assert_eq!(rx.try_recv().unwrap().text, "next");
            assert!(rx.try_recv().is_none());
        }
    }
}

#[test]
fn stopped_or_dropped_receivers_keep_blocked_resolvers_within_both_worker_limits() {
    let (_lock, home) = project();
    let mut barriers = Vec::new();
    let mut peers = Vec::new();
    for i in 0..MAX_WORKERS {
        let peer = Peer::start(home.path(), &format!("blocked-{i}"), false, None);
        let (barrier, pause) = Barrier::new();
        let mut source = peer.source();
        source.pause = Some(pause);
        let rx = peer
            .term
            .attach_turns(source, JsonParser::default(), Options::default())
            .unwrap();
        barrier.wait();
        if i % 2 == 0 {
            rx.stop();
            assert_eq!(rx.status(), Status::Closed);
        }
        drop(rx);
        for _ in 0..3 {
            assert!(matches!(
                peer.term
                    .attach_turns(peer.source(), JsonParser::default(), Options::default()),
                Err(AttachError::Busy)
            ));
        }
        peers.push(peer);
        barriers.push(barrier);
    }
    let extra = Peer::start(home.path(), "extra", false, None);
    assert!(matches!(
        extra
            .term
            .attach_turns(extra.source(), JsonParser::default(), Options::default()),
        Err(AttachError::Busy)
    ));
    barriers.pop().unwrap().release();
    let rx = extra.attach();
    observed(&rx, &extra.file);
    drop(barriers); // disconnect and release the rest
    drop(peers);
}

#[test]
fn blocked_parser_does_not_delay_ticket_disposal_or_reap() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "dispose", false, None);
    let (rx, barrier, reset) = paused(&peer);
    let ticket = peer.term.send(
        "await confirmation",
        SendOptions {
            revalidate: Revalidate::Handshake { timeout: WAIT * 3 },
            ..SendOptions::default()
        },
    );
    let deadline = Instant::now() + WAIT;
    loop {
        if let Event::CommitPoint { ticket: id, .. } = peer.events.recv_deadline(deadline).unwrap()
        {
            assert_eq!(id, ticket.id());
            break;
        }
    }
    let pid = peer.term.terminal().pid().unwrap();
    let (tx, dropped) = bounded(1);
    let dropper = std::thread::spawn(move || {
        drop(peer.term);
        tx.send(()).unwrap();
    });
    dropped
        .recv_timeout(WAIT)
        .expect("teardown while parser is parked");
    assert_eq!(
        ticket.try_outcome(),
        Some(DeliveryOutcome::Aborted(AbortReason::Disposed))
    );
    assert_eq!(rx.status(), Status::Closed);
    assert!(rx.try_recv().is_none());
    assert_eq!(
        crate::pty::peek_exit(pid).unwrap_err().raw_os_error(),
        Some(libc::ECHILD)
    );
    barrier.release();
    reset.recv_timeout(WAIT).unwrap();
    dropper.join().unwrap();
}

#[test]
fn a_shell_regaining_the_foreground_cuts_off_a_pull_before_relaunch() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "foreground", true, None);
    let (rx, barrier, reset) = paused(&peer);
    peer.input("exit-peer");
    until("shell regained the foreground", || {
        peer.term.terminal().foreground().owner == crate::ForegroundOwner::Shell
    });
    assert!(rx.try_recv().is_none());
    assert_eq!(rx.status(), Status::Gap(Gap::Lifecycle));
    barrier.release();
    reset.recv_timeout(WAIT).unwrap();
}

#[test]
fn a_shell_relaunch_invalidates_the_old_queue_while_parser_is_blocked() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "shell-agent", true, None);
    let (rx, barrier, reset) = paused(&peer);
    peer.input("exit-peer");
    until("agent left the foreground", || {
        peer.term.terminal().foreground().owner == crate::ForegroundOwner::Shell
    });
    peer.term.launch().unwrap();
    gap(&rx, Gap::Lifecycle);
    barrier.release();
    reset.recv_timeout(WAIT).unwrap();
    // The cached shell command repeated the launch ID. This old claim cannot rebind.
    until("retired launch no longer attachable", || {
        matches!(
            peer.term
                .attach_turns(peer.source(), JsonParser::default(), Options::default()),
            Err(AttachError::NoLaunch)
        )
    });
}

#[test]
fn reader_eof_cuts_off_a_queue_before_the_blocked_monitor_receives_exit() {
    let (_lock, home) = project();
    let (monitor, pause) = Barrier::new();
    let peer = Peer::start(home.path(), "eof", false, Some(pause));
    let (rx, parser, reset) = paused(&peer);
    peer.input("park-monitor");
    monitor.wait();
    peer.input("exit-peer");
    let deadline = Instant::now() + WAIT;
    while !matches!(
        peer.events.recv_deadline(deadline).unwrap(),
        Event::Exit { .. }
    ) {}
    // No process polling here; neither the parser nor the monitor can record this exit.
    assert_ne!(peer.term.state(), AgentState::Exited);
    assert_eq!(rx.status(), Status::Closed);
    assert!(rx.try_recv().is_none());
    parser.release();
    monitor.release();
    reset.recv_timeout(WAIT).unwrap();
}

#[test]
fn monitor_loss_immediately_discards_replies_while_the_parser_is_blocked() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "overflow-monitor", false, None);
    let (rx, barrier, reset) = paused(&peer);
    // Exercise the real watcher overflow path with one over-budget title.
    let terminal = peer.term.terminal();
    terminal.handle().publish(Event::Title {
        id: terminal.id(),
        title: "x".repeat(8193),
    });
    gap(&rx, Gap::Monitor);
    barrier.release();
    reset.recv_timeout(WAIT).unwrap();
}

fn with_blocked_fanout_after_exit(check: impl FnOnce(&TurnReceiver, &Sender<Instant>)) {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "fanout", false, None);
    // Use a stopped clock while filling a subscriber. The first receiver is drained,
    // so the final Output on it witnesses the reader reaching the full second subscriber.
    let (clock, options) = clock();
    let rx = peer
        .term
        .attach_turns(peer.source(), JsonParser::default(), options)
        .unwrap();
    observed(&rx, &peer.file);
    turn(&peer.file, "queued");
    tick(&clock);
    observed(&rx, &peer.file);
    let terminal = peer.term.terminal();
    let full = terminal.subscribe();
    let handle = terminal.handle();
    let id = terminal.id();
    let capacity = full.capacity().unwrap();
    // Only one producer fills the queue; wake the peer after the last available slot.
    let filler = std::thread::spawn(move || {
        for _ in 0..capacity {
            handle.publish(Event::Frame { id });
        }
    });
    until("subscriber queue full", || {
        peer.events.try_iter().for_each(drop);
        full.is_full()
    });
    let before = terminal.handle().output_chunks();
    peer.input("exit-peer");
    until("exit command reached blocked reader fanout", || {
        terminal.handle().output_chunks() > before
    });
    // Observe kernel exit without updating the terminal's cached lifecycle.
    let pid = terminal.pid().unwrap();
    until("child exited with output fanout blocked", || {
        crate::pty::peek_exit(pid).unwrap().is_some()
    });
    assert!(matches!(rx.status(), Status::Active { .. }));
    check(&rx, &clock);
    drop(full);
    drop(peer.events);
    filler.join().unwrap();
}

#[test]
fn an_exited_child_cuts_off_a_queue_even_if_reader_fanout_cannot_reach_eof() {
    with_blocked_fanout_after_exit(|rx, clock| {
        tick(clock);
        until("worker detected child exit", || {
            rx.status() == Status::Closed
        });
        assert!(rx.try_recv().is_none());
    });
}

#[test]
fn pulling_with_the_worker_paused_detects_child_exit_before_eof() {
    with_blocked_fanout_after_exit(|rx, _| {
        assert!(rx.try_recv().is_none());
        assert_eq!(rx.status(), Status::Closed);
    });
}

// This logger records one bit and never retains unrelated tests' output.
const SENTINEL: &str = "private-final-from-transcript-only";
static LOG_LEAK: AtomicBool = AtomicBool::new(false);
struct SentinelLogger;
impl log::Log for SentinelLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        if record.args().to_string().contains(SENTINEL) {
            LOG_LEAK.store(true, Ordering::Release);
        }
    }
    fn flush(&self) {}
}

#[test]
fn stalled_turn_consumer_preserves_delivery_raw_stream_and_privacy() {
    log::set_logger(&SentinelLogger).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "private", false, None);
    #[cfg(feature = "observers")]
    let hub = crate::observers::ObserverHub::new(peer.term.terminal(), Default::default()).unwrap();
    let rx = peer.attach_with(Limits {
        queue_items: 2,
        ..CEILINGS
    });
    observed(&rx, &peer.file);
    turn(&peer.file, SENTINEL);
    observed(&rx, &peer.file);
    let reply = rx.try_recv().unwrap();
    assert_eq!(reply.text, SENTINEL);
    assert!(!format!("{reply:?}").contains(SENTINEL));
    turn(&peer.file, "retained");
    observed(&rx, &peer.file);
    for _ in 0..2 {
        turn(&peer.file, SENTINEL);
    }
    gap(&rx, Gap::Overflow);
    let ticket = peer.term.send(
        "ordinary input",
        SendOptions {
            submit: true,
            ..SendOptions::default()
        },
    );
    let deadline = Instant::now() + WAIT;
    let mut raw = Vec::new();
    loop {
        let event = peer.events.recv_deadline(deadline).unwrap();
        assert!(!format!("{event:?}").contains(SENTINEL));
        match event {
            Event::Output { bytes, .. } => raw.extend(bytes),
            Event::Delivery {
                ticket: id,
                outcome,
                ..
            } if id == ticket.id() => {
                assert_eq!(outcome, DeliveryOutcome::Delivered);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(ticket.try_outcome(), Some(DeliveryOutcome::Delivered));
    assert_eq!(rx.status(), Status::Gap(Gap::Overflow));
    assert!(String::from_utf8_lossy(&raw).contains("accepted input"));
    assert!(!String::from_utf8_lossy(&raw).contains(SENTINEL));
    let (replay, _) = peer.term.terminal().attach();
    assert!(!String::from_utf8_lossy(&replay.into_bytes().unwrap()).contains(SENTINEL));
    #[cfg(feature = "observers")]
    {
        let viewer = hub.attach(None).unwrap();
        let mut bytes = Vec::new();
        until("ordinary output replayed to raw observer", || {
            while let Some(crate::observers::Item::Output { bytes: output, .. }) = viewer.try_recv()
            {
                bytes.extend_from_slice(&output);
            }
            String::from_utf8_lossy(&bytes).contains("accepted input")
        });
        assert!(!String::from_utf8_lossy(&bytes).contains(SENTINEL));
    }
    assert!(!LOG_LEAK.load(Ordering::Acquire));
}

#[test]
fn invalid_limits_and_in_flight_byte_overflow_do_not_admit_excess_replies() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "byte-budget", false, None);
    for limits in [
        Limits {
            read_bytes: usize::MAX,
            ..CEILINGS
        },
        Limits {
            records: 0,
            ..CEILINGS
        },
        Limits {
            queue_bytes: 2,
            reply_bytes: 3,
            ..CEILINGS
        },
        Limits {
            line_bytes: CEILINGS.line_bytes + 1,
            ..CEILINGS
        },
    ] {
        assert!(matches!(
            peer.term.attach_turns(
                peer.source(),
                JsonParser::default(),
                Options {
                    limits,
                    ..Options::default()
                }
            ),
            Err(AttachError::InvalidLimits)
        ));
    }
    let (clock, mut options) = clock();
    options.limits.queue_bytes = 8;
    options.limits.reply_bytes = 4;
    let rx = peer
        .term
        .attach_turns(peer.source(), JsonParser::default(), options)
        .unwrap();
    observed(&rx, &peer.file);
    turn(&peer.file, "1234");
    tick(&clock);
    observed(&rx, &peer.file);
    turn(&peer.file, "5678");
    turn(&peer.file, "9");
    tick(&clock);
    gap(&rx, Gap::Overflow);
}
