//! Host profile callbacks may stall forever; they must not own controller state or turn
//! fast raw output into unbounded internal buffering.
#![cfg(all(unix, feature = "agent"))]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use agent_link::agent::{
    AbortReason, AgentProfile, AgentSpec, AgentTerminal, DeliveryOutcome, Dialog, SendOptions,
};
use agent_link::{Event, PtySize, SpawnMode, SpawnSpec};
use crossbeam_channel::{Receiver, Sender, bounded};

use common::{LAUNCH_WAIT, WAIT, wait_until};

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockAt {
    Dialog,
    InputReady,
}

struct BlockingProfile {
    at: BlockAt,
    blocked: AtomicBool,
    entered: Sender<()>,
    resume: Receiver<()>,
}

impl BlockingProfile {
    fn block(&self, at: BlockAt, screen: &str) {
        if self.at == at
            && screen.contains("BLOCK HERE")
            && !self.blocked.swap(true, Ordering::SeqCst)
        {
            let _ = self.entered.send(());
            let _ = self.resume.recv();
        }
    }
}

impl AgentProfile for BlockingProfile {
    fn id(&self) -> &str {
        "blocked-profile"
    }
    fn dialog(&self, screen: &str) -> Option<Dialog> {
        self.block(BlockAt::Dialog, screen);
        None
    }
    fn input_ready(&self, screen: &str) -> bool {
        self.block(BlockAt::InputReady, screen);
        screen.contains("BLOCK HERE")
    }
}

/// Release callbacks even if an assertion fails, so a regression cannot leave test
/// teardown parked behind the callback being exercised.
struct Resume(Sender<()>);
impl Drop for Resume {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

fn spawn(
    at: BlockAt,
    script: &str,
) -> (
    AgentTerminal,
    Receiver<Event>,
    Receiver<()>,
    Resume,
    tempfile::TempDir,
) {
    let home = tempfile::tempdir().unwrap();
    let (entered_tx, entered) = bounded(1);
    let (resume, resumed) = bounded(1);
    let profile = Arc::new(BlockingProfile {
        at,
        blocked: AtomicBool::new(false),
        entered: entered_tx,
        resume: resumed,
    });
    let mut spec = SpawnSpec::shell(
        home.path(),
        PtySize {
            rows: 24,
            cols: 100,
        },
    );
    spec.mode = SpawnMode::Direct {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
    };
    spec.env.set("HOME", home.path()).set("ENV", "");
    let (term, events) = {
        let _spawn = common::spawn_lock();
        AgentTerminal::spawn(spec, AgentSpec::new(profile)).unwrap()
    };
    (term, events, entered, Resume(resume), home)
}

#[test]
fn blocked_dialog_and_input_ready_do_not_block_disposal_or_ticket_resolution() {
    for at in [BlockAt::Dialog, BlockAt::InputReady] {
        let (term, events, entered, resume, _home) = spawn(at, "printf 'BLOCK HERE\\n'; exec cat");
        let drain = std::thread::spawn(move || for _ in events {});
        entered.recv_timeout(LAUNCH_WAIT).expect("profile entered");
        let ticket = term.send("must never be typed", SendOptions::default());
        let (done, dropped) = bounded(1);
        let closer = std::thread::spawn(move || {
            drop(term);
            let _ = done.send(());
        });
        let finished = dropped.recv_timeout(Duration::from_secs(2));
        let outcome = ticket.try_outcome();
        // Release before asserting: old code failed by holding the controller mutex.
        drop(resume);
        closer.join().unwrap();
        drain.join().unwrap();
        assert!(finished.is_ok(), "dispose waited for host profile code");
        assert_eq!(
            outcome,
            Some(DeliveryOutcome::Aborted(AbortReason::Disposed))
        );
    }
}

#[test]
fn a_blocked_profile_overflow_aborts_automation_but_keeps_raw_streaming() {
    for at in [BlockAt::Dialog, BlockAt::InputReady] {
        let script = "printf 'BLOCK HERE\\n'; while [ ! -e flood ]; do sleep 0.01; done; dd if=/dev/zero bs=8192 count=2048 2>/dev/null; printf '\\nFLOOD_END\\n'; exec cat";
        let (term, events, entered, resume, home) = spawn(at, script);
        let chunks = Arc::new(AtomicUsize::new(0));
        let flood_ended = Arc::new(AtomicBool::new(false));
        let raw_seen = Arc::new(AtomicBool::new(false));
        let auto_seen = Arc::new(AtomicBool::new(false));
        let (outcome_tx, outcomes) = bounded(4);
        let drain = {
            let chunks = Arc::clone(&chunks);
            let flood_ended = Arc::clone(&flood_ended);
            let raw_seen = Arc::clone(&raw_seen);
            let auto_seen = Arc::clone(&auto_seen);
            std::thread::spawn(move || {
                let mut tail = String::new();
                let mut next_seq = 0;
                for event in events {
                    match event {
                        Event::Output { seq, bytes, .. } => {
                            assert_eq!(seq, next_seq, "host output lost its order");
                            next_seq += 1;
                            chunks.fetch_add(1, Ordering::SeqCst);
                            tail.push_str(&String::from_utf8_lossy(&bytes));
                            if tail.contains("FLOOD_END") {
                                flood_ended.store(true, Ordering::SeqCst);
                            }
                            if tail.contains("RAW_STREAM_ALIVE") {
                                raw_seen.store(true, Ordering::SeqCst);
                            }
                            if tail.contains("AUTOMATION_MUST_STOP") {
                                auto_seen.store(true, Ordering::SeqCst);
                            }
                            if tail.len() > 100 {
                                tail.drain(..tail.len() - 100);
                            }
                        }
                        Event::Delivery { outcome, .. } => {
                            let _ = outcome_tx.send(outcome);
                        }
                        _ => {}
                    }
                }
            })
        };
        entered.recv_timeout(LAUNCH_WAIT).expect("profile entered");
        let ticket = term.send(
            "AUTOMATION_MUST_STOP",
            SendOptions {
                submit: true,
                ..SendOptions::default()
            },
        );
        std::fs::write(home.path().join("flood"), "go").unwrap();
        let aborted = DeliveryOutcome::Aborted(AbortReason::NotConnected);
        assert_eq!(
            ticket.wait(WAIT),
            Some(aborted.clone()),
            "overflow cancellation must not wait for the callback"
        );
        assert!(
            !term.blocking(),
            "disabled automation cannot keep an overlay up"
        );
        wait_until("raw output after monitor overflow", || {
            flood_ended.load(Ordering::SeqCst)
        });
        assert!(chunks.load(Ordering::SeqCst) > 1024);
        // Refusals consume no queue slots even while the monitor is still blocked.
        for _ in 0..300 {
            assert_eq!(
                term.send("AUTOMATION_MUST_STOP", SendOptions::default())
                    .try_outcome(),
                Some(aborted.clone())
            );
        }
        term.terminal().write(b"RAW_STREAM_ALIVE\n").unwrap();
        wait_until("raw input after monitor overflow", || {
            raw_seen.load(Ordering::SeqCst)
        });
        drop(resume);
        assert_eq!(outcomes.recv_timeout(WAIT).unwrap(), aborted);
        assert!(!auto_seen.load(Ordering::SeqCst));
        drop(term);
        drain.join().unwrap();
        assert!(
            !auto_seen.load(Ordering::SeqCst),
            "stale callback authorized input after overflow"
        );
    }
}
