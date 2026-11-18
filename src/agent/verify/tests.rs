//! Delivery callbacks run against real PTY output, with channels fixing the race:
//! a callback reads a composer/dialog, output changes, then its result is released.

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};

use crate::agent::leftover::Leftover;
use crate::agent::state::{AgentProgram, Config, Controller};
use crate::agent::{
    AgentProfile, Dialog, DialogKind, ForegroundPolicy, SendOptions, ShellFallback, ShellKind,
    Timing, TrustPolicy,
};
use crate::identity::TerminalIdentity;
use crate::{Event, PtySize, SpawnSpec, Terminal};

const WAIT: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(500);
const WINDOW: Duration = Duration::from_millis(800);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Callback {
    Composer,
    Dialog,
    Question,
    Refusal,
}

struct Blocked {
    at: Callback,
    once: AtomicBool,
    repeat: bool,
    entered: Sender<()>,
    resumed: Receiver<()>,
}

impl Blocked {
    fn block(&self, at: Callback) {
        if at == self.at && (self.repeat || !self.once.swap(true, Ordering::SeqCst)) {
            let _ = self.entered.send(());
            let _ = self.resumed.recv_timeout(WAIT);
        }
    }
}

impl AgentProfile for Blocked {
    fn id(&self) -> &str {
        "blocked-delivery"
    }
    fn dialog(&self, screen: &str) -> Option<Dialog> {
        let dialog = screen.contains("DIALOG").then_some(Dialog {
            kind: DialogKind::Trust,
            key: None,
        });
        self.block(Callback::Dialog);
        dialog
    }
    fn input_ready(&self, _: &str) -> bool {
        true
    }
    fn question_dialog(&self, screen: &str) -> bool {
        let question = screen.contains("QUESTION");
        self.block(Callback::Question);
        assert!(
            !screen.contains("PANIC"),
            "scripted question predicate panic"
        );
        question
    }
    fn reset_rejected(&self, fresh: &str) -> bool {
        let refused = fresh.lines().any(|row| row == "REFUSED");
        self.block(Callback::Refusal);
        refused
    }

    fn pending_input(&self, screen: &str, sent: &str) -> Option<bool> {
        let pending = Some(screen.contains(sent));
        self.block(Callback::Composer);
        pending
    }
}

struct Resume(Sender<()>);
impl Drop for Resume {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

struct Session {
    term: Option<Terminal>,
    ctl: Arc<Controller>,
    output: Receiver<()>,
    drain: Option<std::thread::JoinHandle<()>>,
    _home: tempfile::TempDir,
}

impl Session {
    fn new(at: Callback, repeat: bool) -> (Self, Receiver<()>, Resume) {
        let (entered_tx, entered) = bounded(1);
        let (resume, resumed) = bounded(1);
        let profile = Arc::new(Blocked {
            at,
            once: AtomicBool::new(false),
            repeat,
            entered: entered_tx,
            resumed,
        });
        let home = tempfile::tempdir().unwrap();
        let mut spec = SpawnSpec::direct(
            "/bin/sh",
            ["-c", "stty raw -echo; printf READY; exec cat"],
            home.path(),
            PtySize { rows: 24, cols: 80 },
        );
        spec.env.set("HOME", home.path()).set("ENV", "");
        let (term, events) = {
            let _spawn = crate::pty::test_spawn_lock();
            Terminal::spawn(spec).unwrap()
        };
        let cfg = Config {
            id: term.id(),
            profile: Some(profile),
            profile_id: "blocked-delivery".into(),
            launch_line: None,
            reset_command: None,
            shell: ShellKind::Posix,
            trust: TrustPolicy::default(),
            timing: Timing {
                submit_verify_quiet: QUIET,
                submit_verify_window: WINDOW,
                submit_verify_poll: Duration::from_millis(10),
                ..Timing::default()
            },
            limits: crate::agent::AgentLimits::default(),
            foreground_policy: ForegroundPolicy::default(),
            trace: false,
            identity: TerminalIdentity::Honest,
        };
        let (monitor, _rx) = bounded(1);
        let ctl = Arc::new(Controller::new(cfg, term.handle(), monitor));
        ctl.update_quietly(|st, now| {
            st.program = AgentProgram::Session;
            st.launch(now);
        });
        let (output_tx, output) = bounded(1);
        let drain_ctl = ctl.clone();
        let drain = std::thread::spawn(move || {
            for event in events {
                if let Event::Output { bytes, .. } = event {
                    drain_ctl.update_quietly(|st, now| st.on_output(&bytes, now));
                    let _ = output_tx.try_send(());
                }
            }
        });
        let session = Self {
            term: Some(term),
            ctl,
            output,
            drain: Some(drain),
            _home: home,
        };
        session.wait_screen("READY", 0);
        (session, entered, Resume(resume))
    }

    fn term(&self) -> &Terminal {
        self.term.as_ref().unwrap()
    }

    fn wait_screen(&self, text: &str, after: u64) -> Instant {
        let deadline = Instant::now() + WAIT;
        loop {
            let screen = self.term().screen().unwrap();
            let fed = self.ctl.term().output_chunks();
            let view = self.ctl.read(|st| st.view);
            if fed > after && view.output_serial == fed && screen.text().contains(text) {
                return view.last_output_at.unwrap();
            }
            self.output
                .recv_deadline(deadline)
                .expect("PTY output and monitor must catch up");
        }
    }

    fn draw(&self, text: &str) -> Instant {
        let after = self.ctl.term().output_chunks();
        let rows = text.replace('\n', "\r\n");
        self.term()
            .write(format!("\x1b[2J\x1b[H{rows}").as_bytes())
            .unwrap();
        self.wait_screen(text, after)
    }

    /// cat echoes in input order. Its later marker fences any write from the job.
    fn assert_not_pasted(&self, payload: &str) {
        let after = self.ctl.term().output_chunks();
        self.term().write(b" fence").unwrap();
        self.wait_screen(" fence", after);
        assert!(!self.term().screen().unwrap().text().contains(payload));
    }

    /// Snapshot just before the test draws the simulated response.
    fn reset_baseline(&self) -> ResetBaseline {
        let runner = self.runner();
        let obs = runner.observation().unwrap();
        ResetBaseline {
            rows: Leftover::new(obs.screen.rows),
            lifecycle: obs.lifecycle,
            fence: runner.fence(),
        }
    }

    fn runner(&self) -> Runner {
        Runner::new(self.ctl.clone())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.ctl.dispose();
        self.term.take().unwrap().kill();
        if let Some(drain) = self.drain.take() {
            drain.join().unwrap();
        }
    }
}

#[test]
fn stale_empty_composer_cannot_become_a_stable_submit_receipt() {
    let (session, entered, resume) = Session::new(Callback::Composer, false);
    let empty_at = session.draw("> empty");
    let mut runner = session.runner();
    let worker = std::thread::spawn(move || runner.wait_submit_outcome("pending", 0, None));
    entered.recv_timeout(WAIT).unwrap();
    let restored_at = session.draw("> pending");
    assert!(
        restored_at.duration_since(empty_at) < QUIET,
        "empty composer must never have been stable"
    );
    // Meet the stale observation's quiet deadline while the new composer holds text.
    // This timer establishes the behavior under test; channels establish the race.
    crossbeam_channel::after((empty_at + QUIET).saturating_duration_since(Instant::now()))
        .recv_timeout(WAIT)
        .unwrap();
    drop(resume);
    assert_eq!(worker.join().unwrap(), Ok(Submit::Pending));
    assert!(session.term().screen().unwrap().text().contains("pending"));
}

#[test]
fn stale_visible_paste_cannot_authorize_enter() {
    let (session, entered, resume) = Session::new(Callback::Composer, false);
    session.draw("> pending");
    let mut runner = session.runner();
    let worker = std::thread::spawn(move || {
        let job = MessageJob::new(TicketId(1), "pending", SendOptions::default());
        runner.wait_visible_paste(&job, 0)
    });
    entered.recv_timeout(WAIT).unwrap();
    session.draw("> empty");
    drop(resume);
    assert_eq!(worker.join().unwrap(), Err(AbortReason::SubmitUnverified));
}

#[test]
fn stale_absence_of_dialog_cannot_authorize_enter() {
    let (session, entered, resume) = Session::new(Callback::Dialog, false);
    session.draw("> ready");
    let mut runner = session.runner();
    let worker = std::thread::spawn(move || runner.no_dialog_before_enter(TicketId(1)));
    entered.recv_timeout(WAIT).unwrap();
    session.draw("DIALOG");
    drop(resume);
    assert_eq!(
        worker.join().unwrap(),
        Err(AbortReason::TrustPromptBlocking)
    );
}

#[test]
fn a_redraw_during_the_question_check_discards_both_stale_verdicts() {
    let open = "QUESTION\n> pending";
    let clear = "> pending (clear)";
    for (before, after, expected) in [
        (clear, open, Err(AbortReason::DialogBlocking)),
        (open, clear, Ok(())),
    ] {
        let (session, entered, resume) = Session::new(Callback::Question, false);
        session.draw(before);
        let mut runner = session.runner();
        let worker = std::thread::spawn(move || {
            let job = MessageJob::new(TicketId(1), "pending", SendOptions::default());
            runner.wait_visible_paste(&job, 0)
        });
        entered.recv_timeout(WAIT).expect("question check started");
        session.draw(after);
        drop(resume);
        assert_eq!(worker.join().unwrap(), expected, "redraw to {after:?}");
    }
}

#[test]
fn a_question_takes_precedence_over_startup_and_composer_callbacks() {
    for callback in [Callback::Dialog, Callback::Composer] {
        let (session, entered, resume) = Session::new(callback, false);
        drop(resume);
        session.draw("QUESTION DIALOG\n> pending");
        let job = MessageJob::new(TicketId(1), "pending", SendOptions::default());
        let mut runner = session.runner();
        let result = if callback == Callback::Dialog {
            runner.no_dialog_before_enter(job.ticket)
        } else {
            runner.wait_visible_paste(&job, 0)
        };
        assert_eq!(result, Err(AbortReason::DialogBlocking));
        assert!(entered.is_empty(), "a lower-priority callback ran");
    }
}

#[test]
fn a_question_after_enter_can_accompany_a_verified_submit() {
    let (session, entered, resume) = Session::new(Callback::Question, false);
    drop(resume);
    session.draw("> pending");
    let enter_fence = session.ctl.term().output_chunks();
    session.draw("QUESTION\n> empty");
    assert_eq!(
        session
            .runner()
            .wait_submit_outcome("pending", enter_fence, None),
        Ok(Submit::Submitted)
    );
    assert!(entered.is_empty(), "reading the receipt needs no input");
}

#[test]
fn an_old_question_is_ignored_while_the_composer_can_span_the_launch_boundary() {
    let (session, _, resume) = Session::new(Callback::Dialog, false);
    drop(resume);
    session.draw("QUESTION\n> first line");
    let front = session.ctl.read_front();
    let leftover = Leftover::new(session.term().screen().unwrap().rows);
    session.ctl.update_quietly(|st, now| {
        st.type_launch("agent\n", now, &front, leftover);
    });
    let paste_fence = session.ctl.term().output_chunks();
    // The new frame preserves the prior rows, then adds the continuation of the paste.
    session.draw("QUESTION\n> first line\nsecond line");
    let job = MessageJob::new(
        TicketId(1),
        "first line\nsecond line",
        SendOptions::default(),
    );
    let mut runner = session.runner();
    assert_eq!(runner.no_dialog_before_enter(job.ticket), Ok(()));
    assert_eq!(runner.wait_visible_paste(&job, paste_fence), Ok(()));
}

#[test]
fn a_panicking_question_predicate_cannot_authorize_a_paste() {
    let (session, entered, resume) = Session::new(Callback::Question, false);
    session.draw("PANIC");
    assert!(session.ctl.evaluate_gate(false));
    let mut runner = session.runner();
    let worker = std::thread::spawn(move || {
        let mut job = MessageJob::new(TicketId(1), "must stay unsent", SendOptions::default());
        runner.process_message(&mut job, None)
    });
    entered
        .recv_timeout(WAIT)
        .expect("commit-point question check started");
    session.draw("> ready");
    drop(resume);
    assert_eq!(worker.join().unwrap(), Err(AbortReason::SubmitUnverified));

    session.assert_not_pasted("must stay unsent");
}

#[test]
fn cancellation_is_rechecked_after_a_composer_callback() {
    let (session, entered, resume) = Session::new(Callback::Composer, false);
    session.draw("> empty");
    let mut runner = session.runner();
    let worker = std::thread::spawn(move || runner.wait_submit_outcome("pending", 0, None));
    entered.recv_timeout(WAIT).unwrap();
    session.ctl.dispose();
    drop(resume);
    assert_eq!(worker.join().unwrap(), Err(AbortReason::Disposed));
}

fn during_repeated_redraws<T: Send + 'static>(
    callback: Callback,
    text: &str,
    check: impl FnOnce(&mut Runner, ResetBaseline) -> T + Send + 'static,
) -> (Session, T) {
    let (session, entered, resume) = Session::new(callback, true);
    let baseline = session.reset_baseline();
    session.draw(text);
    let mut runner = session.runner();
    let (done, result) = bounded(1);
    let started = Instant::now();
    let worker = std::thread::spawn(move || {
        let _ = done.send(check(&mut runner, baseline));
    });
    let timeout = crossbeam_channel::after(WAIT);
    let mut changes = 0;
    let outcome = loop {
        crossbeam_channel::select! {
            recv(result) -> result => break result.unwrap(),
            recv(entered) -> _ => {
                changes += 1;
                session.draw(&format!("{text} {changes}"));
                resume.0.send(()).unwrap();
            },
            recv(timeout) -> _ => panic!("stale callbacks extended the verification deadline"),
        }
    };
    drop(resume);
    worker.join().unwrap();
    assert!(changes > 1);
    assert!(
        started.elapsed() >= WINDOW,
        "gave up before the submit window expired"
    );
    (session, outcome)
}

#[test]
fn continuously_stale_callbacks_do_not_extend_verification_forever() {
    let (_, outcome) = during_repeated_redraws(Callback::Composer, "> empty", |runner, _| {
        runner.wait_submit_outcome("pending", 0, None)
    });
    assert_eq!(outcome, Ok(Submit::Unverified));
}

#[test]
fn continuously_stale_question_checks_expire_even_when_the_paste_is_visible() {
    let (_, outcome) = during_repeated_redraws(Callback::Question, "> pending", |runner, _| {
        let job = MessageJob::new(TicketId(1), "pending", SendOptions::default());
        runner.wait_visible_paste(&job, 0)
    });
    assert_eq!(outcome, Err(AbortReason::SubmitUnverified));
}

#[test]
fn a_reset_rechecks_its_pre_enter_guards_after_a_redraw() {
    let (session, entered, resume) = Session::new(Callback::Composer, false);
    session.draw("> /new");
    let mut runner = session.runner();
    let worker = std::thread::spawn(move || {
        let job = MessageJob::reset(TicketId(1), "/new", ShellFallback::Fail);
        runner.write_reset_enter(&job, 0).map(|_| ())
    });
    entered.recv_timeout(WAIT).expect("visible paste checked");
    session.draw("QUESTION\n> /new");
    drop(resume);
    assert_eq!(worker.join().unwrap(), Err(AbortReason::DialogBlocking));
}

#[test]
fn a_fresh_refusal_can_end_verification_while_the_command_is_still_in_the_composer() {
    let (session, _, resume) = Session::new(Callback::Refusal, false);
    drop(resume);
    session.draw("> /new");
    let baseline = session.reset_baseline();
    session.draw("REFUSED\n> /new");
    assert_eq!(
        session
            .runner()
            .wait_submit_outcome("/new", baseline.fence.serial, Some(&baseline)),
        Err(AbortReason::ResetRejected),
    );
}

#[test]
fn a_redraw_or_relaunch_invalidates_the_reset_refusal_verdict() {
    for relaunch in [false, true] {
        let (session, entered, resume) = Session::new(Callback::Refusal, false);
        session.draw("> /new");
        let baseline = session.reset_baseline();
        session.draw("REFUSED\n> empty");
        let mut runner = session.runner();
        let worker = std::thread::spawn(move || {
            runner.wait_submit_outcome("/new", baseline.fence.serial, Some(&baseline))
        });
        entered
            .recv_timeout(WAIT)
            .expect("refusal callback entered");
        let expected = if relaunch {
            session.ctl.update_quietly(|st, now| st.launch(now));
            Err(AbortReason::SubmitUnverified)
        } else {
            session.draw("> empty");
            Ok(Submit::Submitted)
        };
        drop(resume);
        assert_eq!(worker.join().unwrap(), expected);
    }
}

#[test]
fn a_header_redraw_exposing_an_old_refusal_is_ambiguous() {
    let (session, _, resume) = Session::new(Callback::Refusal, false);
    drop(resume);
    session.draw("old header\nREFUSED\n> /new");
    let baseline = session.reset_baseline();
    session.draw("changed header\nREFUSED\n> empty");
    assert_eq!(
        session
            .runner()
            .wait_submit_outcome("/new", baseline.fence.serial, Some(&baseline)),
        Err(AbortReason::SubmitUnverified),
    );
}

#[test]
fn a_follow_up_with_continuously_stale_refusal_checks_expires_before_paste() {
    let (session, outcome) =
        during_repeated_redraws(Callback::Refusal, "> ready", |runner, baseline| {
            assert!(runner.ctl.evaluate_gate(false));
            let mut job = MessageJob::new(TicketId(1), "must stay unsent", SendOptions::default());
            runner.process_message(&mut job, Some(&baseline))
        });
    assert_eq!(outcome, Err(AbortReason::SubmitUnverified));
    session.assert_not_pasted("must stay unsent");
}

#[test]
fn profile_observations_survive_gate_polling_but_not_resize_or_relaunch() {
    #[derive(Debug)]
    enum Change {
        None,
        GatePoll,
        Resize,
        Relaunch,
    }
    for change in [
        Change::None,
        Change::GatePoll,
        Change::Resize,
        Change::Relaunch,
    ] {
        let (session, entered, resume) = Session::new(Callback::Composer, false);
        session.draw("> empty");
        let mut runner = session.runner();
        let worker = std::thread::spawn(move || {
            runner.profile_read(None, |profile, screen, _| {
                profile.pending_input(screen, "pending")
            })
        });
        entered.recv_timeout(WAIT).unwrap();
        match change {
            Change::None => {}
            Change::GatePoll => {
                assert!(session.ctl.evaluate_gate(false));
            }
            Change::Resize => session
                .term()
                .resize(PtySize { rows: 25, cols: 81 })
                .unwrap(),
            Change::Relaunch => session.ctl.update_quietly(|st, now| st.launch(now)),
        }
        drop(resume);
        let read = worker.join().unwrap().unwrap();
        let expected = match change {
            Change::None | Change::GatePoll => Some(Some(false)),
            Change::Resize | Change::Relaunch => None,
        };
        assert_eq!(read.value, expected, "{change:?}");
        assert!(
            !read.unseen,
            "{change:?}: invalidation is not monitor backlog"
        );
    }
}
