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

    /// `UNREADABLE` on screen: no composer this profile can read (`None`).
    fn pending_input(&self, screen: &str, sent: &str) -> Option<bool> {
        let pending = (!screen.contains("UNREADABLE")).then(|| screen.contains(sent));
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
            has_model_choice: false,
            reset_command: None,
            shell: ShellKind::Posix,
            trust: TrustPolicy::default(),
            timing: Timing {
                reset_turn_wait: Duration::from_millis(50),
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

    fn paste_fence(&self) -> PasteFence {
        PasteFence {
            lifecycle: self.ctl.read(|st| st.lifecycle_revision()),
            serial: self.ctl.term().output_chunks(),
        }
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
    let lifecycle = session.paste_fence().lifecycle;
    let worker =
        std::thread::spawn(move || runner.wait_submit_outcome("pending", 0, lifecycle, None));
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
    let paste = session.paste_fence();
    session.draw("> pending");
    let mut runner = session.runner();
    let worker = std::thread::spawn(move || {
        let job = MessageJob::new(TicketId(1), "pending", SendOptions::default());
        runner.wait_visible_paste(&job, paste)
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
        (open, clear, Ok(Composer::Visible)),
    ] {
        let (session, entered, resume) = Session::new(Callback::Question, false);
        let paste = session.paste_fence();
        session.draw(before);
        let mut runner = session.runner();
        let worker = std::thread::spawn(move || {
            let job = MessageJob::new(TicketId(1), "pending", SendOptions::default());
            runner.wait_visible_paste(&job, paste)
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
        let paste = session.paste_fence();
        session.draw("QUESTION DIALOG\n> pending");
        let job = MessageJob::new(TicketId(1), "pending", SendOptions::default());
        let mut runner = session.runner();
        let result = if callback == Callback::Dialog {
            runner.no_dialog_before_enter(job.ticket)
        } else {
            runner.wait_visible_paste(&job, paste).map(|_| ())
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
        session.runner().wait_submit_outcome(
            "pending",
            enter_fence,
            session.paste_fence().lifecycle,
            None
        ),
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
    let paste_fence = session.paste_fence();
    // The new frame preserves the prior rows, then adds the continuation of the paste.
    session.draw("QUESTION\n> first line\nsecond line");
    let job = MessageJob::new(
        TicketId(1),
        "first line\nsecond line",
        SendOptions::default(),
    );
    let mut runner = session.runner();
    assert_eq!(runner.no_dialog_before_enter(job.ticket), Ok(()));
    assert_eq!(
        runner.wait_visible_paste(&job, paste_fence),
        Ok(Composer::Visible)
    );
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
    let lifecycle = session.paste_fence().lifecycle;
    let worker =
        std::thread::spawn(move || runner.wait_submit_outcome("pending", 0, lifecycle, None));
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
    let (_, outcome) =
        during_repeated_redraws(Callback::Composer, "> empty", |runner, baseline| {
            runner.wait_submit_outcome("pending", 0, baseline.lifecycle, None)
        });
    assert_eq!(outcome, Ok(Submit::Unverified));
}

#[test]
fn continuously_stale_question_checks_expire_even_when_the_paste_is_visible() {
    let (_, outcome) =
        during_repeated_redraws(Callback::Question, "> pending", |runner, baseline| {
            let job = MessageJob::new(TicketId(1), "pending", SendOptions::default());
            runner.wait_visible_paste(
                &job,
                PasteFence {
                    lifecycle: baseline.lifecycle,
                    serial: baseline.fence.serial,
                },
            )
        });
    assert_eq!(outcome, Err(AbortReason::SubmitUnverified));
}

#[test]
fn a_turn_starting_during_the_final_screen_check_holds_the_reset() {
    let (session, entered, resume) = Session::new(Callback::Question, false);
    session.draw("> ready");
    assert!(session.ctl.evaluate_gate(false));
    let mut runner = session.runner();
    let worker = std::thread::spawn(move || {
        let mut reset = MessageJob::reset(
            TicketId(1),
            "/new",
            ShellFallback::Fail,
            UnreadableComposer::Refuse,
        );
        runner.process_message(&mut reset, None)
    });

    entered
        .recv_timeout(WAIT)
        .expect("pre-paste question check");
    session.draw("> working");
    session
        .ctl
        .update_quietly(|st, now| st.on_title("⠋ agent", now));
    drop(resume);

    assert_eq!(worker.join().unwrap(), Err(AbortReason::AgentWorking));
    session.assert_not_pasted("/new");
}

#[test]
fn a_reset_rechecks_its_pre_enter_guards_after_a_redraw() {
    let (session, entered, resume) = Session::new(Callback::Composer, false);
    let paste = session.paste_fence();
    session.draw("> /new");
    let mut runner = session.runner();
    let worker = std::thread::spawn(move || {
        let job = MessageJob::reset(
            TicketId(1),
            "/new",
            ShellFallback::Fail,
            UnreadableComposer::Refuse,
        );
        runner
            .write_reset_enter(&job, paste, Composer::Visible)
            .map(|_| ())
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
        session.runner().wait_submit_outcome(
            "/new",
            baseline.fence.serial,
            baseline.lifecycle,
            Some(&baseline)
        ),
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
            runner.wait_submit_outcome(
                "/new",
                baseline.fence.serial,
                baseline.lifecycle,
                Some(&baseline),
            )
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
        session.runner().wait_submit_outcome(
            "/new",
            baseline.fence.serial,
            baseline.lifecycle,
            Some(&baseline)
        ),
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
fn a_turn_resuming_while_the_reset_waits_for_the_writer_withholds_the_paste() {
    let (session, _, resume) = Session::new(Callback::Refusal, false);
    drop(resume);
    session.draw("> ready");
    assert!(session.ctl.evaluate_gate(false));
    let mut runner = session.runner();
    let (at_paste, reached) = bounded(1);
    let (done, result) = bounded(1);
    let mut job = MessageJob::reset(
        TicketId(1),
        "/new",
        ShellFallback::Fail,
        UnreadableComposer::Refuse,
    );
    let mut working_at = None;
    session
        .ctl
        .term()
        .write_if(b"other write", || {
            std::thread::spawn(move || {
                let outcome = runner.process_message_with(&mut job, None, |_| {
                    at_paste.try_send(()).unwrap();
                });
                let _ = done.send(outcome);
            });
            reached.recv_timeout(WAIT).expect("reset reached its paste");
            working_at = Some(Instant::now());
            session
                .ctl
                .update_quietly(|st, now| st.on_title("⠋ agent", now));
            true
        })
        .unwrap();
    assert_eq!(
        result.recv_timeout(WAIT).unwrap(),
        Err(AbortReason::AgentWorking)
    );
    assert!(working_at.unwrap().elapsed() >= session.ctl.cfg.timing.reset_turn_wait);
    session.assert_not_pasted("/new");
    assert!(!session.ctl.read(|st| st.typed()));
}

#[test]
fn sends_and_resets_recheck_the_replacement_before_pasting() {
    for mut job in [
        MessageJob::new(TicketId(1), "ordinary payload", SendOptions::default()),
        MessageJob::reset(
            TicketId(2),
            "/new",
            ShellFallback::Fail,
            UnreadableComposer::Refuse,
        ),
    ] {
        let (session, _, resume) = Session::new(Callback::Refusal, false);
        drop(resume);
        session.draw("> ready");
        assert!(session.ctl.evaluate_gate(false));
        let outcome = session
            .runner()
            .process_message_with(&mut job, None, |runner| {
                runner.ctl.update_quietly(|st, now| st.launch(now));
                session.draw("QUESTION");
                assert!(runner.ctl.evaluate_gate(false));
            });
        assert_eq!(outcome, Err(AbortReason::DialogBlocking));
        session.assert_not_pasted(&job.text);
    }
}

#[test]
fn shell_cleanup_checks_the_paste_launch_at_enqueue_and_afterwards() {
    for relaunch_before_enqueue in [true, false] {
        let (session, _, resume) = Session::new(Callback::Refusal, false);
        drop(resume);
        let runner = session.runner();
        let paste = session.paste_fence();
        let events = session.term().subscribe();
        // The shell was found in front. Order the relaunch on either side of
        // queueing Ctrl-C, before the delivery can record the agent's departure.
        let result = if relaunch_before_enqueue {
            session.ctl.update_quietly(|st, now| st.launch(now));
            runner.interrupt_the_shell(TicketId(1), paste, "shell cleanup")
        } else {
            let after = session.ctl.interrupt().unwrap();
            session.ctl.update_quietly(|st, now| {
                st.launch(now);
                st.finish_paste_interrupt(paste.lifecycle, Some(after))
            })
        };
        assert_eq!(result, Err(AbortReason::SubmitUnverified));
        assert!(session.ctl.read(|st| st.gate.running()));

        // Raw cat echoes each byte. This marker follows all queued cleanup input.
        session.term().write(b"!").unwrap();
        let deadline = Instant::now() + WAIT;
        let mut echo = Vec::new();
        while !echo.contains(&b'!') {
            if let Event::Output { bytes, .. } = events.recv_deadline(deadline).unwrap() {
                echo.extend_from_slice(&bytes);
            }
        }
        let expected: &[u8] = if relaunch_before_enqueue {
            b"!"
        } else {
            b"\x03!"
        };
        assert_eq!(echo, expected);
    }
}

#[test]
fn a_relaunch_after_the_follow_up_screen_check_withholds_the_paste() {
    let (session, _, resume) = Session::new(Callback::Refusal, false);
    drop(resume);
    let baseline = session.reset_baseline();
    session.draw("> fresh conversation");
    assert!(session.ctl.evaluate_gate(false));
    let mut job = MessageJob::new(
        TicketId(1),
        "follow-up payload",
        SendOptions {
            submit: true,
            on_shell: ShellFallback::Fail,
            ..SendOptions::default()
        },
    );
    let result = session
        .runner()
        .process_message_with(&mut job, Some(&baseline), |runner| {
            runner.ctl.update_quietly(|st, now| st.launch(now));
            runner.ctl.term().write(b" replacement").unwrap();
        });
    assert_eq!(result, Err(AbortReason::SubmitUnverified));
    session.wait_screen(" replacement", baseline.fence.serial);
    session.assert_not_pasted("follow-up payload");
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

fn enter_once(text: &str) -> MessageJob {
    MessageJob::new(
        TicketId(1),
        text,
        SendOptions {
            submit: true,
            unreadable_composer: UnreadableComposer::EnterOnce,
            ..SendOptions::default()
        },
    )
}

#[test]
fn readability_needs_answers_and_none_may_read_a_composer() {
    let mut seen = Readability::default();
    assert!(!seen.never_readable(), "no answer yet");
    seen.note(None);
    assert!(!seen.never_readable(), "a stale read is no answer");
    seen.note(Some(None));
    assert!(seen.never_readable());
    seen.note(Some(Some(false)));
    assert!(
        !seen.never_readable(),
        "one readable screen keeps the verified path"
    );
    let mut seen = Readability::default();
    seen.note(Some(Some(true)));
    seen.note(Some(None));
    assert!(!seen.never_readable());
}

/// The whole window without a readable composer: no Enter by default; the opt-in
/// leaves the paste check as `Unreadable` (its Enter follows only after the echo).
#[test]
fn an_unreadable_composer_is_reported_only_to_an_opted_in_send() {
    let (session, _, resume) = Session::new(Callback::Refusal, false);
    drop(resume);
    let paste = session.paste_fence();
    session.draw("UNREADABLE pending");
    let mut runner = session.runner();
    let refuse = MessageJob::new(
        TicketId(1),
        "pending",
        SendOptions {
            submit: true,
            ..SendOptions::default()
        },
    );
    assert_eq!(
        runner.wait_visible_paste(&refuse, paste),
        Err(AbortReason::SubmitUnverified)
    );
    let started = Instant::now();
    assert_eq!(
        runner.wait_visible_paste(&enter_once("pending"), paste),
        Ok(Composer::Unreadable)
    );
    assert!(
        started.elapsed() >= WINDOW,
        "decided before the window ended"
    );
    // A question still stops it first.
    session.draw("QUESTION\nUNREADABLE pending");
    assert_eq!(
        runner.wait_visible_paste(&enter_once("pending"), paste),
        Err(AbortReason::DialogBlocking)
    );
}

/// One current readable screen in the window keeps the verified path: the opt-in
/// never applies to a profile that can read its composer.
#[test]
fn one_readable_screen_keeps_the_verified_path() {
    let (session, entered, resume) = Session::new(Callback::Composer, true);
    let paste = session.paste_fence();
    session.draw("UNREADABLE");
    let mut runner = session.runner();
    let worker =
        std::thread::spawn(move || runner.wait_visible_paste(&enter_once("pending"), paste));
    // A current unreadable answer, then a readable composer without the text.
    entered.recv_timeout(WAIT).expect("first composer read");
    resume.0.send(()).unwrap();
    entered.recv_timeout(WAIT).expect("second composer read");
    session.draw("> other");
    resume.0.send(()).unwrap();
    entered.recv_timeout(WAIT).expect("readable composer read");
    // Later callbacks neither wait nor block on a full channel.
    drop(resume);
    drop(entered);
    assert_eq!(worker.join().unwrap(), Err(AbortReason::SubmitUnverified));
}

/// Stale reads and a failed profile are no answers: neither reads as an unreadable
/// composer, so neither gets the opted-in Enter.
#[test]
fn stale_reads_and_a_failed_profile_never_read_as_unreadable() {
    let (_, outcome) =
        during_repeated_redraws(Callback::Composer, "UNREADABLE", |runner, baseline| {
            runner.wait_visible_paste(
                &enter_once("pending"),
                PasteFence {
                    lifecycle: baseline.lifecycle,
                    serial: baseline.fence.serial,
                },
            )
        });
    assert_eq!(outcome, Err(AbortReason::SubmitUnverified));

    let (session, _, resume) = Session::new(Callback::Refusal, false);
    drop(resume);
    let paste = session.paste_fence();
    session.draw("PANIC UNREADABLE");
    assert_eq!(
        session
            .runner()
            .wait_visible_paste(&enter_once("pending"), paste),
        Err(AbortReason::SubmitUnverified)
    );
}

/// A reset: `Ok(None)` hands an unreadable command to the opt-in; the unverified Enter
/// itself needs a composer that is still unreadable.
#[test]
fn a_reset_gets_its_unverified_enter_only_on_an_unreadable_composer() {
    let reset = |unreadable_composer| {
        MessageJob::reset(
            TicketId(1),
            "/new",
            ShellFallback::Fail,
            unreadable_composer,
        )
    };
    let (session, _, resume) = Session::new(Callback::Refusal, false);
    drop(resume);
    let paste = session.paste_fence();
    session.draw("UNREADABLE /new");
    let mut runner = session.runner();
    assert_eq!(
        runner
            .write_reset_enter(&reset(UnreadableComposer::Refuse), paste, Composer::Visible)
            .map(|_| ()),
        Err(AbortReason::SubmitUnverified)
    );
    assert!(matches!(
        runner.write_reset_enter(
            &reset(UnreadableComposer::EnterOnce),
            paste,
            Composer::Visible
        ),
        Ok(None)
    ));
    // The composer became readable: no unverified Enter.
    session.draw("> /new");
    assert_eq!(
        runner
            .write_reset_enter(
                &reset(UnreadableComposer::EnterOnce),
                paste,
                Composer::Unreadable
            )
            .map(|_| ()),
        Err(AbortReason::SubmitUnverified)
    );
    // Still unreadable: the Enter is written and its baseline kept.
    session.draw("UNREADABLE /new");
    let written = runner.write_reset_enter(
        &reset(UnreadableComposer::EnterOnce),
        paste,
        Composer::Unreadable,
    );
    assert!(matches!(written, Ok(Some(_))));
}

#[test]
fn a_readable_empty_reset_composer_ends_the_final_check_before_a_retry() {
    let (session, entered, resume) = Session::new(Callback::Composer, true);
    let paste = session.paste_fence();
    session.draw("> empty");
    let mut runner = session.runner();
    let (done, result) = bounded(1);
    let worker = std::thread::spawn(move || {
        let mut job = MessageJob::reset(
            TicketId(1),
            "/new",
            ShellFallback::Fail,
            UnreadableComposer::EnterOnce,
        );
        let outcome = runner.unverified_reset(&mut job, paste);
        done.send((outcome, matches!(job.intent, MessageIntent::Reset(_))))
            .unwrap();
    });
    entered
        .recv_timeout(WAIT)
        .expect("final readable composer check");
    resume.0.send(()).unwrap();
    // A buggy retry now sees an unreadable screen and can send the reset Enter.
    let outcome = crossbeam_channel::select! {
        recv(result) -> outcome => outcome.unwrap(),
        recv(entered) -> _ => {
            session.draw("UNREADABLE");
            drop(entered);
            drop(resume);
            result.recv_timeout(WAIT).expect("reset outcome after redraw")
        }
        default(WAIT) => panic!("reset did not resolve the readable observation"),
    };
    worker.join().unwrap();
    assert_eq!(outcome, (Err(AbortReason::SubmitUnverified), true));
}

#[test]
fn a_composer_that_becomes_readable_before_unverified_enter_gets_no_enter() {
    let (session, entered, resume) = Session::new(Callback::Dialog, true);
    let paste_fence = session.paste_fence();
    session.draw("UNREADABLE");
    let mut runner = session.runner();
    let worker = std::thread::spawn(move || {
        runner.submit_with_verification(&mut enter_once("pending"), paste_fence)
    });

    entered.recv_timeout(WAIT).expect("initial dialog check");
    resume.0.send(()).unwrap();
    entered.recv_timeout(WAIT).expect("final dialog check");
    session.draw(">");
    drop(entered);
    drop(resume);
    let result = worker.join().unwrap();

    // A stray carriage return moves cat's cursor back over the empty prompt.
    let after = session.ctl.term().output_chunks();
    session.term().write(b"marker").unwrap();
    session.wait_screen("marker", after);
    assert!(
        session.term().screen().unwrap().text().contains(">marker"),
        "unexpected Enter: {result:?}"
    );
    assert_eq!(result, Err(AbortReason::SubmitUnverified));
}

#[test]
fn a_relaunch_after_enter_cannot_supply_the_submit_receipt() {
    let (session, entered, resume) = Session::new(Callback::Composer, true);
    let mut runner = session.runner();
    let mut job = MessageJob::new(TicketId(1), "pending", SendOptions::default());
    let paste = runner
        .paste(&job.text, Some(session.paste_fence().lifecycle), None)
        .unwrap()
        .unwrap();
    session.wait_screen("pending", paste.serial);
    let worker = std::thread::spawn(move || runner.submit_with_verification(&mut job, paste));
    entered.recv_timeout(WAIT).expect("visible paste check");
    resume.0.send(()).unwrap();
    entered
        .recv_timeout(WAIT)
        .expect("post-Enter composer check");
    session.ctl.update_quietly(|st, now| st.launch(now));
    session.draw("> replacement");
    drop(entered);
    drop(resume);
    assert_eq!(worker.join().unwrap(), Err(AbortReason::SubmitUnverified));
}

#[test]
fn a_readable_replacement_showing_the_same_text_cannot_authorize_enter() {
    let (session, _, resume) = Session::new(Callback::Refusal, false);
    drop(resume);
    let mut runner = session.runner();
    let mut job = enter_once("pending");
    let paste = runner
        .paste(&job.text, Some(session.paste_fence().lifecycle), None)
        .unwrap()
        .unwrap();
    session.wait_screen("pending", paste.serial);
    session.ctl.update_quietly(|st, now| st.launch(now));
    session.draw("> pending");

    let result = runner.submit_with_verification(&mut job, paste);
    let after = session.ctl.term().output_chunks();
    session.term().write(b"marker").unwrap();
    session.wait_screen("marker", after);
    assert!(
        session
            .term()
            .screen()
            .unwrap()
            .text()
            .contains("> pendingmarker")
    );
    assert_eq!(result, Err(AbortReason::SubmitUnverified));
}

/// The unverified Enter needs output after the paste that went quiet, then passes the
/// same pre-Enter checks as a verified one.
#[test]
fn the_unverified_enter_needs_a_quiet_echo_and_its_pre_enter_checks() {
    let (session, _, resume) = Session::new(Callback::Refusal, false);
    drop(resume);
    session.draw("UNREADABLE pending");
    let mut runner = session.runner();
    let job = enter_once("pending");
    // Nothing after the paste fence: the cap ends the wait, no Enter.
    let no_echo = session.paste_fence();
    assert_eq!(
        runner.unverified_enter(&job, no_echo),
        Err(AbortReason::SubmitUnverified)
    );
    for (screen, expected) in [
        ("QUESTION\nUNREADABLE pending", AbortReason::DialogBlocking),
        (
            "DIALOG\nUNREADABLE pending",
            AbortReason::TrustPromptBlocking,
        ),
        ("> pending", AbortReason::SubmitUnverified),
        ("UNREADABLE pending", AbortReason::UnverifiedEnter),
    ] {
        let fence = session.paste_fence();
        session.draw(screen);
        assert_eq!(
            runner.unverified_enter(&job, fence),
            Err(expected),
            "{screen:?}"
        );
    }
}

#[test]
fn unverified_enter_waits_out_a_redraw_after_the_initial_quiet_check() {
    for reset in [false, true] {
        let (session, entered, resume) = Session::new(Callback::Dialog, false);
        let paste = session.paste_fence();
        session.draw("UNREADABLE pending");
        let quiet = session.ctl.cfg.timing.paste_echo_quiet;
        let mut runner = session.runner();
        let worker = std::thread::spawn(move || {
            if reset {
                let job = MessageJob::reset(
                    TicketId(1),
                    "/new",
                    ShellFallback::Fail,
                    UnreadableComposer::EnterOnce,
                );
                runner.paste_went_quiet(job.ticket, paste.serial).unwrap();
                runner
                    .write_reset_enter(&job, paste, Composer::Unreadable)
                    .unwrap()
                    .unwrap()
                    .fence
                    .at
            } else {
                assert_eq!(
                    runner.unverified_enter(&enter_once("pending"), paste),
                    Err(AbortReason::UnverifiedEnter)
                );
                Instant::now()
            }
        });
        entered
            .recv_timeout(WAIT)
            .expect("screen check after quiet");
        let redrawn = session.draw("UNREADABLE redraw");
        drop(resume);
        assert!(worker.join().unwrap() >= redrawn + quiet);
    }
}
