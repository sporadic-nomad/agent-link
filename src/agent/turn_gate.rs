//! The turn gate: an opt-in wait that keeps an explicit reset out of a running turn
//! ([`Timing::reset_turn_wait`](super::Timing::reset_turn_wait), off by default).
//!
//! A host may ask for a reset in response to something the agent did inside its own turn
//! (a tool call saying "this phase is done, start the next one fresh") while that turn is
//! still streaming. Typed then, the reset command lands in the running turn: codex refuses
//! `/clear` while a task runs (`ResetRejected`); what other CLIs do with it — queue it,
//! interrupt the turn, drop it — is not recorded. With the gate on, an explicit reset
//! (`MessageIntent::Reset`) holds its job at the head of the queue, after the ready gate,
//! while the agent's [`Activity`] is `Working`, then starts over (readiness, owner and
//! lull are read again). The wait:
//! - is bounded in total by `reset_turn_wait`, counted from the first look that finds the
//!   agent working ([`TurnWait`]); a turn seen again at the commit point
//!   under the terminal writer lock waits only for what is left. Still working at the end ⇒
//!   `AgentWorking`, nothing typed;
//! - sleeps on the controller's condvar (`Controller::wait_until`), never holding the
//!   state lock: an activity change, teardown (`Disposed`), the session's end or a failed
//!   monitor (`NotConnected`) end it at once;
//! - reads the foreground every [`LEFT_POLL`]: the shell in front ⇒ the agent left (an
//!   agent killed mid-turn leaves its spinner title behind, and a title never goes
//!   stale), marked gone, and the job starts over — a relaunch, or the shell fallback;
//!   another program ⇒ `ForegroundUnproven`; the agent in front with a recognized
//!   question ⇒ `DialogBlocking`;
//! - keeps the job running: its admission stays reserved and the jobs queued after it
//!   wait too.
//!
//! Evidence: `Working` means a Braille title spinner or a fresh OSC 9;4 progress report.
//! Among the built-in profiles only codex has a recorded spinner; the others have no
//! recorded `Working` signal, and an agent without one is not held. The gate makes a
//! mid-turn reset less likely; it does not prove that a turn ended (an agent may work
//! without either signal, and a spinner title can outlive the agent that drew it), so the
//! refusal check after the reset's Enter still applies.

use std::time::{Duration, Instant};

use super::Activity;
use super::controller::{
    LEFT_POLL, MessageIntent, MessageJob, Owner, Runner, Step, check_question, job_owner,
    not_the_agent, trace,
};
use super::state::{AgentProgram, CtlState, FrontRead};
use super::{AbortReason, TicketId};

/// An agent runs and its activity says it is in a turn.
pub(super) fn working(st: &CtlState) -> bool {
    st.gate.running() && st.activity.state() == Activity::Working
}

/// One look of the gate ([`TurnWait::look`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TurnGate {
    /// Not held: the gate is off, or the agent is not working.
    Open,
    /// Held: look again by this instant at the latest (the next foreground poll, or the
    /// end of the wait).
    Wait(Instant),
    /// Still working when the wait ran out.
    Expired,
}

/// The gate's clock for one reset job. It starts at the first look that finds the agent
/// working and bounds every later wait of the same job: a reset re-checked at the commit
/// point never gets a fresh allowance.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct TurnWait {
    deadline: Option<Instant>,
}

impl TurnWait {
    /// The decision at `now`: `max` is the configured bound (zero: off), `working` the
    /// activity read just now.
    pub(super) fn look(&mut self, max: Duration, working: bool, now: Instant) -> TurnGate {
        if max.is_zero() || !working {
            return TurnGate::Open;
        }
        let deadline = *self.deadline.get_or_insert(now + max);
        if now >= deadline {
            TurnGate::Expired
        } else {
            TurnGate::Wait((now + LEFT_POLL).min(deadline))
        }
    }
}

impl Runner {
    /// The turn gate (see the module docs) for an explicit reset not typed yet. `true`:
    /// it held the job, and the caller starts over.
    pub(super) fn wait_turn_end(&mut self, job: &mut MessageJob) -> Step<bool> {
        let max = self.ctl.cfg.timing.reset_turn_wait;
        let MessageIntent::Reset(wait) = &mut job.intent else {
            return Ok(false);
        };
        if max.is_zero() {
            return Ok(false);
        }
        let ticket = job.ticket;
        let mut held = false;
        loop {
            let now_working = self
                .ctl
                .read(|st| st.cancel_reason().map_or(Ok(working(st)), Err))?;
            match wait.look(max, now_working, Instant::now()) {
                TurnGate::Open => {
                    if held {
                        trace!(self, ticket, "the agent's turn ended");
                    }
                    return Ok(held);
                }
                TurnGate::Expired => {
                    trace!(
                        self,
                        ticket,
                        "the agent is still working after {} ms: the reset is not typed",
                        max.as_millis()
                    );
                    return Err(AbortReason::AgentWorking);
                }
                TurnGate::Wait(until) => {
                    if !held {
                        held = true;
                        trace!(
                            self,
                            ticket, "the agent is working: the reset waits for its turn to end"
                        );
                    }
                    let changed = self.ctl.wait_until(until, |st| {
                        if let Some(reason) = st.cancel_reason() {
                            return Some(Err(reason));
                        }
                        (!working(st)).then_some(Ok(()))
                    });
                    if changed.transpose()?.is_none()
                        && wait.deadline.is_some_and(|end| Instant::now() < end)
                    {
                        self.turn_poll(ticket)?;
                    }
                }
            }
        }
    }

    /// A look at the terminal while the agent works (see the module docs). The shell in
    /// front marks the agent gone; nothing else would end the wait for an agent killed
    /// mid-turn: its spinner title stays, and the ready gate does not read the foreground
    /// while it is ready.
    fn turn_poll(&mut self, ticket: TicketId) -> Step<()> {
        let lifecycle = self.ctl.read(CtlState::lifecycle_revision);
        let front = self.ctl.read_front();
        let Some((owner, agent)) = self.ctl.update(|st, _| turn_owner(st, lifecycle, &front))?
        else {
            return Ok(());
        };
        match owner {
            Owner::Shell => {
                trace!(
                    self,
                    ticket, "the shell is in front: the agent left during its turn"
                );
                Ok(())
            }
            Owner::Unproven => {
                trace!(
                    self,
                    ticket,
                    "{} during the turn: not pasted",
                    not_the_agent(agent)
                );
                Err(AbortReason::ForegroundUnproven)
            }
            Owner::Agent => {
                let question = self.profile_read(None, |profile, screen, leftover| {
                    check_question(profile, leftover.fresh(screen))
                })?;
                if let Some(Err(reason)) = question.value {
                    trace!(self, ticket, "a question is on screen during the turn");
                    return Err(reason);
                }
                Ok(())
            }
        }
    }
}

/// Apply foreground evidence only to the working launch for which it was collected.
fn turn_owner(
    st: &mut CtlState,
    lifecycle: u64,
    front: &FrontRead,
) -> Step<Option<(Owner, AgentProgram)>> {
    if let Some(reason) = st.cancel_reason() {
        return Err(reason);
    }
    if st.lifecycle_revision() != lifecycle || !working(st) {
        return Ok(None);
    }
    st.observe(front);
    let agent = st.program;
    let owner = job_owner(front.shell, front.program, agent);
    if owner == Owner::Shell {
        st.agent_left();
    }
    Ok(Some((owner, agent)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Off, or an agent not working: open, and no clock starts.
    #[test]
    fn the_gate_is_open_when_off_or_not_working() {
        let t0 = Instant::now();
        let mut wait = TurnWait::default();
        assert_eq!(wait.look(Duration::ZERO, true, t0), TurnGate::Open);
        assert_eq!(wait.look(ms(1000), false, t0), TurnGate::Open);
        let later = t0 + ms(5000);
        assert_eq!(
            wait.look(ms(1000), true, later),
            TurnGate::Wait(later + LEFT_POLL)
        );
    }

    /// Polls at `LEFT_POLL`, the last one at the deadline; a turn that ends and starts
    /// again (the commit point's re-check) keeps the first clock; at the deadline it
    /// expires, and an idle agent still opens it afterwards.
    #[test]
    fn the_wait_is_bounded_in_total_from_the_first_working_look() {
        let t0 = Instant::now();
        let max = ms(1000);
        let mut wait = TurnWait::default();
        assert_eq!(wait.look(max, true, t0), TurnGate::Wait(t0 + LEFT_POLL));
        assert_eq!(wait.look(max, true, t0 + ms(900)), TurnGate::Wait(t0 + max));
        assert_eq!(wait.look(max, false, t0 + ms(950)), TurnGate::Open);
        assert_eq!(wait.look(max, true, t0 + ms(990)), TurnGate::Wait(t0 + max));
        assert_eq!(wait.look(max, true, t0 + max), TurnGate::Expired);
        assert_eq!(wait.look(max, true, t0 + ms(9000)), TurnGate::Expired);
        assert_eq!(wait.look(max, false, t0 + ms(9000)), TurnGate::Open);
    }

    /// The largest clamped bound cannot overflow `Instant` arithmetic.
    #[test]
    fn the_largest_bound_is_representable() {
        let t0 = Instant::now();
        let max = crate::agent::Timing::MAX_WAIT;
        let mut wait = TurnWait::default();
        assert_eq!(wait.look(max, true, t0), TurnGate::Wait(t0 + LEFT_POLL));
    }

    /// The job thread's side against a real PTY (`/bin/sh` running `cat` as a direct
    /// agent); the activity is set through the controller state as the monitor sets it.
    #[cfg(unix)]
    mod runner {
        use std::sync::Arc;

        use crossbeam_channel::{Receiver, bounded};

        use super::*;
        use crate::agent::state::{AgentProgram, Config, Controller};
        use crate::agent::{
            AgentLimits, AgentProfile, Dialog, ForegroundPolicy, SendOptions, ShellFallback,
            ShellKind, Timing, TrustPolicy,
        };
        use crate::identity::TerminalIdentity;
        use crate::{Event, PtySize, SpawnSpec, Terminal};

        const WAIT: Duration = Duration::from_secs(5);

        struct Questions;

        impl AgentProfile for Questions {
            fn id(&self) -> &str {
                "turn-gate"
            }
            fn dialog(&self, _: &str) -> Option<Dialog> {
                None
            }
            fn input_ready(&self, _: &str) -> bool {
                true
            }
            fn question_dialog(&self, screen: &str) -> bool {
                screen.contains("QUESTION")
            }
        }

        struct Session {
            term: Option<Terminal>,
            ctl: Arc<Controller>,
            drain: Option<std::thread::JoinHandle<()>>,
            _home: tempfile::TempDir,
        }

        impl Session {
            fn new(reset_turn_wait: Duration) -> Self {
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
                    profile: Some(Arc::new(Questions)),
                    profile_id: "turn-gate".into(),
                    launch_line: None,
                    has_model_choice: false,
                    reset_command: Some("/new"),
                    shell: ShellKind::Posix,
                    trust: TrustPolicy::default(),
                    timing: Timing {
                        reset_turn_wait,
                        ..Timing::default()
                    },
                    limits: AgentLimits::default(),
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
                let drain_ctl = ctl.clone();
                let drain = std::thread::spawn(move || {
                    for event in events {
                        if let Event::Output { bytes, .. } = event {
                            drain_ctl.update_quietly(|st, now| st.on_output(&bytes, now));
                        }
                    }
                });
                let session = Self {
                    term: Some(term),
                    ctl,
                    drain: Some(drain),
                    _home: home,
                };
                session.wait_screen("READY");
                session
            }

            fn wait_screen(&self, text: &str) {
                let deadline = Instant::now() + WAIT;
                loop {
                    let screen = self.term.as_ref().unwrap().screen().unwrap();
                    let fed = self.ctl.term().output_chunks();
                    let caught_up = self.ctl.read(|st| st.view.output_serial == fed);
                    if caught_up && screen.text().contains(text) {
                        return;
                    }
                    assert!(Instant::now() < deadline, "{text:?} never on screen");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }

            /// `cat` echoes the text: the screen shows it as the agent's own output.
            fn draw(&self, text: &str) {
                let term = self.term.as_ref().unwrap();
                term.write(format!("\x1b[2J\x1b[H{text}").as_bytes())
                    .unwrap();
                self.wait_screen(text);
            }

            /// A spinner title, or a plain one with its idle debounce over.
            fn working(&self, on: bool) {
                self.ctl.update_quietly(|st, now| {
                    st.on_title(if on { "⠋ agent" } else { "agent" }, now);
                    if !on {
                        st.activity.on_deadline(now + Duration::from_secs(60));
                    }
                });
            }

            /// Runs the gate for `job` on its own thread; the result arrives on the receiver.
            fn gate(&self, mut job: MessageJob) -> Receiver<Step<bool>> {
                let mut runner = Runner::new(self.ctl.clone());
                let (done, result) = bounded(1);
                std::thread::spawn(move || {
                    let _ = done.send(runner.wait_turn_end(&mut job));
                });
                result
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

        fn reset_job() -> MessageJob {
            MessageJob::reset(
                TicketId(1),
                "/new",
                ShellFallback::Fail,
                crate::agent::UnreadableComposer::Refuse,
            )
        }

        /// Off (the default), a reset to a working agent goes on at once. On, neither an
        /// ordinary send (even of the reset command) nor a reset to an idle agent waits.
        #[test]
        fn only_a_gated_reset_to_a_working_agent_is_held() {
            let session = Session::new(Duration::ZERO);
            session.working(true);
            let mut runner = Runner::new(session.ctl.clone());
            assert_eq!(runner.wait_turn_end(&mut reset_job()), Ok(false));
            drop(session);

            let session = Session::new(Duration::from_secs(60));
            let mut runner = Runner::new(session.ctl.clone());
            assert_eq!(runner.wait_turn_end(&mut reset_job()), Ok(false));
            session.working(true);
            let submit = SendOptions {
                submit: true,
                ..SendOptions::default()
            };
            let mut send = MessageJob::new(TicketId(2), "/new", submit);
            assert_eq!(runner.wait_turn_end(&mut send), Ok(false));
        }

        /// Held past a foreground poll (the agent in front, no question), then released by
        /// the activity's change; the caller is told to start over.
        #[test]
        fn a_held_reset_goes_on_once_the_activity_leaves_working() {
            let session = Session::new(Duration::from_secs(60));
            session.working(true);
            let result = session.gate(reset_job());
            assert!(
                result.recv_timeout(LEFT_POLL * 3).is_err(),
                "a working agent's reset went on"
            );
            session.working(false);
            assert_eq!(result.recv_timeout(WAIT).unwrap(), Ok(true));
        }

        /// The bound ends the wait with `AgentWorking`; the same job gets no new allowance.
        #[test]
        fn a_turn_that_outlasts_the_bound_ends_agent_working() {
            let max = Duration::from_millis(400);
            let session = Session::new(max);
            session.working(true);
            let mut runner = Runner::new(session.ctl.clone());
            let mut job = reset_job();
            let start = Instant::now();
            assert_eq!(
                runner.wait_turn_end(&mut job),
                Err(AbortReason::AgentWorking)
            );
            assert!(start.elapsed() >= max, "{:?}", start.elapsed());
            let again = Instant::now();
            assert_eq!(
                runner.wait_turn_end(&mut job),
                Err(AbortReason::AgentWorking)
            );
            assert!(again.elapsed() < max, "{:?}", again.elapsed());
        }

        #[test]
        fn a_shell_sample_from_before_relaunch_cannot_clear_the_working_replacement() {
            let session = Session::new(Timing::MAX_WAIT);
            session.working(true);
            let lifecycle = session.ctl.read(CtlState::lifecycle_revision);
            let shell = FrontRead {
                shell: true,
                program: None,
                ..session.ctl.read_front()
            };
            session.ctl.update_quietly(|st, now| {
                st.launch(now);
                st.program = AgentProgram::Expected {
                    shell_lines: shell.shell_lines.wrapping_add(1),
                };
            });
            session.working(true);
            session.ctl.update_quietly(|st, _| {
                let replacement = st.program;
                assert_eq!(turn_owner(st, lifecycle, &shell), Ok(None));
                assert!(working(st));
                assert_eq!(st.program, replacement);
                assert_eq!(
                    turn_owner(st, st.lifecycle_revision(), &shell)
                        .unwrap()
                        .unwrap()
                        .0,
                    Owner::Shell
                );
                assert!(!working(st));
            });
        }

        #[test]
        fn the_final_timeout_does_not_poll_for_questions() {
            let session = Session::new(Duration::from_millis(10));
            session.draw("QUESTION");
            session.working(true);
            assert_eq!(
                session.gate(reset_job()).recv_timeout(WAIT).unwrap(),
                Err(AbortReason::AgentWorking)
            );
        }

        /// Teardown ends a wait that could otherwise last a day, at once.
        #[test]
        fn teardown_wakes_a_held_reset() {
            let session = Session::new(Timing::MAX_WAIT);
            session.working(true);
            let result = session.gate(reset_job());
            assert!(result.recv_timeout(LEFT_POLL * 3).is_err());
            session.ctl.dispose();
            assert_eq!(
                result.recv_timeout(WAIT).expect("teardown ended the wait"),
                Err(AbortReason::Disposed)
            );
        }

        /// A question owns the input: the next poll ends the wait, nothing typed.
        #[test]
        fn a_question_during_the_turn_ends_dialog_blocking() {
            let session = Session::new(Duration::from_secs(60));
            session.draw("QUESTION");
            session.working(true);
            let result = session.gate(reset_job());
            assert_eq!(
                result.recv_timeout(WAIT).unwrap(),
                Err(AbortReason::DialogBlocking)
            );
        }
    }
}
