//! Delivery callbacks run against real PTY output, with channels fixing the race:
//! a callback reads a composer/dialog, output changes, then its result is released.

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};

use crate::agent::state::{AgentProgram, Config, Controller};
use crate::agent::{
    AgentProfile, Dialog, DialogKind, ForegroundPolicy, SendOptions, ShellKind, Timing, TrustPolicy,
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
        self.term()
            .write(format!("\x1b[2J\x1b[H{text}").as_bytes())
            .unwrap();
        self.wait_screen(text, after)
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
    let worker = std::thread::spawn(move || runner.wait_submit_outcome("pending", 0));
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
fn cancellation_is_rechecked_after_a_composer_callback() {
    let (session, entered, resume) = Session::new(Callback::Composer, false);
    session.draw("> empty");
    let mut runner = session.runner();
    let worker = std::thread::spawn(move || runner.wait_submit_outcome("pending", 0));
    entered.recv_timeout(WAIT).unwrap();
    session.ctl.dispose();
    drop(resume);
    assert_eq!(worker.join().unwrap(), Err(AbortReason::Disposed));
}

#[test]
fn continuously_stale_callbacks_do_not_extend_verification_forever() {
    let (session, entered, resume) = Session::new(Callback::Composer, true);
    session.draw("> empty");
    let mut runner = session.runner();
    let (done, result) = bounded(1);
    let worker = std::thread::spawn(move || {
        let _ = done.send(runner.wait_submit_outcome("pending", 0));
    });
    let deadline = Instant::now() + WAIT;
    let mut changes = 0;
    let outcome = loop {
        crossbeam_channel::select! {
            recv(result) -> result => break result.unwrap(),
            recv(entered) -> _ => {
                changes += 1;
                session.draw(&format!("> changed {changes}"));
                resume.0.send(()).unwrap();
            },
            default(deadline.saturating_duration_since(Instant::now())) => panic!("stale callbacks extended the verification deadline"),
        }
    };
    drop(resume);
    worker.join().unwrap();
    assert!(changes > 1);
    assert_eq!(outcome, Ok(Submit::Unverified));
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
            runner.profile_read(false, |profile, screen| {
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
