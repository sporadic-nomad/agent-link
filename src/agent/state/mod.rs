//! The controller's shared state and its one rule: every change runs under one lock
//! ([`Controller::update`]) and only *returns* what to do — keys to write, lines to log,
//! events to publish. The PTY write and the log happen after the lock is released; events
//! queue in an outbox that only the monitor thread publishes, in order, so a host thread
//! never blocks on a full event receiver and the state lock is not held while publishing. A queued
//! event waits until the monitor counted the output chunks fed when it was queued
//! ([`Controller::publish_queued`]), so it never overtakes the output its screen read held.
//!
//! The delivery job thread waits on the same lock's condvar: every update notifies it.
//!
//! Dropping the terminal is the one exception to "only the monitor publishes":
//! [`Controller::dispose`] resolves tickets independently of the job thread and closes
//! event publication. It offers final `Aborted(Disposed)` events without waiting for
//! capacity, only if earlier fanout finished. Tickets are authoritative when final events
//! are omitted; a blocked consumer never delays process teardown.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};

use super::activity::ActivityTracker;
use super::controller::{Job, PasteFence, Step};
use super::delivery::Resolution;
use super::leftover::Leftover;
use super::settle::{Evidence, OutputView, interrupt_due};
use super::startup::{GateEffect, StartupGate};
use super::turn_gate::{TurnGate, TurnWait, working};
use super::{
    AbortReason, Activity, AgentLimits, AgentProfile, AgentState, DeliveryOutcome,
    ForegroundPolicy, ShellKind, TicketId, Timing, TrustPolicy,
};
use crate::error::{Error, Result};
use crate::identity::TerminalIdentity;
use crate::pty::{ProgramId, TerminalHandle};
use crate::screen::{ScreenText, normalize};
use crate::{Confidence, Event, Foreground, ForegroundOwner, SessionId};

/// Rows the startup-marker report logs.
const REPORT_ROWS: usize = 12;

/// A blocked monitor cannot notify its condvar when the reader detects overflow.
const FAILURE_POLL: Duration = Duration::from_millis(100);

/// What the controller was spawned with; never changes.
pub(crate) struct Config {
    pub(crate) id: SessionId,
    /// `None`: a bare shell — no gate, no activity, no agent events.
    pub(crate) profile: Option<Arc<dyn AgentProfile>>,
    /// The profile's id, read once at spawn (log lines never call into the profile).
    pub(crate) profile_id: String,
    /// The line [`launch`](super::AgentTerminal::launch) types, terminator included; `None`
    /// without a profile or for a direct child (it already is the agent).
    pub(crate) launch_line: Option<String>,
    /// A selected model can appear anywhere on the startup screen, including shell echo.
    /// Marker diagnostics for these launches omit screen contents.
    pub(crate) has_model_choice: bool,
    /// The profile's fresh-conversation command, read once at spawn; `None` without a
    /// profile or when it has none (a reset is then `Unsupported`).
    pub(crate) reset_command: Option<&'static str>,
    /// The shell the launch line is typed into (how its input line is cleared first, and
    /// whether it can be).
    pub(crate) shell: ShellKind,
    pub(crate) trust: TrustPolicy,
    pub(crate) timing: Timing,
    pub(crate) limits: AgentLimits,
    pub(crate) foreground_policy: ForegroundPolicy,
    pub(crate) trace: bool,
    pub(crate) identity: TerminalIdentity,
}

impl Config {
    /// `fg`'s owner when the foreground policy trusts the answer — an exact one, or a
    /// heuristic one under `AcceptHeuristic`; `None` when it cannot tell (an unknown owner,
    /// a heuristic answer under `ExactOnly`).
    pub(crate) fn trusted_owner(&self, fg: Foreground) -> Option<ForegroundOwner> {
        let trusted = fg.confidence == Confidence::Exact
            || self.foreground_policy == ForegroundPolicy::AcceptHeuristic;
        (trusted && fg.owner != ForegroundOwner::Unknown).then_some(fg.owner)
    }

    /// Only a trusted shell answer permits shell fallback or an automatic launch.
    /// Unknown ownership is never permission to type into the terminal.
    pub(crate) fn shell_in_front(&self, fg: Foreground) -> bool {
        self.trusted_owner(fg) == Some(ForegroundOwner::Shell)
    }

    /// `program` when the policy trusts `fg` and it names a program other than the shell.
    pub(crate) fn trusted_program(
        &self,
        fg: Foreground,
        program: Option<ProgramId>,
    ) -> Option<ProgramId> {
        program.filter(|_| self.trusted_owner(fg) == Some(ForegroundOwner::Agent))
    }
}

/// Who owned the terminal when a thread looked ([`Controller::read_front`], outside the
/// lock).
#[derive(Copy, Clone, Debug)]
pub(crate) struct FrontRead {
    pub(crate) fg: Foreground,
    /// The shell owns it ([`Config::shell_in_front`]).
    pub(crate) shell: bool,
    /// The program owning it, when the policy trusts the answer and the platform names one
    /// ([`Config::trusted_program`]).
    pub(crate) program: Option<ProgramId>,
    /// `TerminalHandle::shell_lines`, read after the foreground.
    pub(crate) shell_lines: u64,
}

/// Which program owning the terminal is the agent this terminal launched — the only one a
/// delivery types into (`controller::job_owner`). A leftover screen never says: an inline
/// agent (codex, claude) leaves its last frame, ready composer included, on the screen it
/// exits from, under whatever the user starts next (a nested shell, `ssh`, `nix develop`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgentProgram {
    /// None: nothing launched into the shell, the agent left, or a line the host submitted
    /// to the shell made the next program ambiguous.
    Unknown,
    /// A launch line went to the shell, when the host had submitted `shell_lines` lines to
    /// it: the first program seen owning the terminal is the agent — unless the host
    /// submitted another line to the shell first (the user may have started that program).
    Expected { shell_lines: u64 },
    /// The agent's program.
    Known(ProgramId),
    /// A direct session: the child is the agent; whatever owns the terminal is its own.
    Session,
}

/// What a launch types ([`CtlState::type_launch`]; [`Controller::type_keys`] does it). The
/// keys are the shell's line kill, then the launch line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LaunchKeys {
    /// Type them now.
    Now(String),
    /// Write a Ctrl-C now and report its fence for launch number `.0`
    /// ([`CtlState::launch_interrupted`]); the keys, held, follow once the shell took it.
    AfterInterrupt(u64),
    /// The monitor's: the Ctrl-C the crate wrote a moment ago is still being handled, so
    /// the next one waits until it settled, and the keys until the shell took that one.
    Held,
}

/// A Ctrl-C the crate wrote: the chunks fed right after it was queued (the shell's answer
/// comes later; one fed before the count was read only delays the line to the cap) and when.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Interrupt {
    pub(crate) fence: u64,
    pub(crate) at: Instant,
}

impl Interrupt {
    /// When a launch line after it may be typed (`settle::interrupt_due`).
    fn due(self, view: &OutputView) -> Instant {
        interrupt_due(view, self.fence, self.at)
    }
}

/// A launch line held back until the shell took a Ctrl-C; the monitor types it
/// ([`CtlState::on_deadlines`]) — and this launch's own Ctrl-C too, when an earlier one was
/// still being handled — each only after a fresh look at who owns the terminal
/// ([`held_step_refusal`]).
#[derive(Debug)]
struct PendingLaunch {
    keys: String,
    /// The Ctrl-C to wait for; `None` while this launch's own is being written (its fence
    /// comes with [`CtlState::launch_interrupted`]).
    after: Option<Interrupt>,
    /// This launch's own Ctrl-C is still to be written, once `after` settled.
    interrupt: bool,
    /// The host's lines to the shell when the launch was decided (`FrontRead::shell_lines`).
    shell_lines: u64,
    /// Tells this launch from one decided while its Ctrl-C was being written.
    number: u64,
}

/// Why a held launch step — its own Ctrl-C or its keys — must not be written now (`front`
/// read right before), if so. It was decided with the shell in front, and only that shell
/// may get it: not a program started since (it would take the Ctrl-C as SIGINT and the
/// launch line as input — written up to about 2 s after the launch was decided), nor a shell
/// the host submitted a line to meanwhile (the program that line starts may not own the
/// terminal yet).
fn held_step_refusal(front: &FrontRead, shell_lines: u64) -> Option<&'static str> {
    if !front.shell {
        Some("the shell no longer owns the terminal")
    } else if front.shell_lines != shell_lines {
        Some("a line was submitted to the shell meanwhile")
    } else {
        None
    }
}

/// Where a commit-point handshake stands (`Revalidate::Handshake`): one delivery runs at a
/// time, so one slot.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Commit {
    #[default]
    Idle,
    /// `Event::CommitPoint` was queued for this delivery; its answer is awaited.
    Awaiting(TicketId),
    /// The host answered (`AgentTerminal::confirm`): paste or not.
    Answered(TicketId, bool),
}

/// Work an update hands back, done after the lock is released.
#[derive(Debug)]
pub(crate) enum Action {
    #[cfg(feature = "questions")]
    EndAnswer(
        super::questions::AnswerTicket,
        super::questions::AnswerError,
    ),
    #[cfg(feature = "questions")]
    ReleaseQuestions(super::questions::Source),
    Write(Vec<u8>),
    Log(log::Level, String),
}

/// Messages to the monitor thread.
#[derive(Debug)]
pub(crate) enum MonitorMsg {
    /// State changed elsewhere: publish the outbox, recompute the next deadline.
    Wake,
    /// The terminal is being disposed.
    Stop,
}

/// Everything behind the controller's lock.
pub(crate) struct CtlState {
    #[cfg(feature = "questions")]
    pub(in crate::agent) questions: super::questions::Book,
    cfg: Arc<Config>,
    pub(crate) gate: StartupGate,
    /// Invalidates profile evaluations performed outside the state lock.
    gate_revision: u64,
    /// Delivery observations survive routine gate evaluation, but not an agent restart
    /// or departure. Output and resize invalidation use the screen model's revision.
    lifecycle_revision: u64,
    /// Set by the reader without taking the controller lock or waiting for the monitor.
    monitor_overflow: Arc<AtomicBool>,
    /// Transport shutdown is visible even while the monitor is inside host code.
    session_closed: Receiver<()>,
    pub(crate) activity: ActivityTracker,
    /// Output bookkeeping the delivery waits read (serial and time of the last chunk,
    /// frame ends, cleared progress reports), updated by the monitor per event.
    pub(crate) view: OutputView,
    /// The `AgentTerminal` was dropped.
    pub(crate) disposed: bool,
    /// The monitor stopped automation after a panic or queue overflow: no further
    /// readiness or delivery evidence can be trusted.
    pub(crate) monitor_failed: bool,
    /// Deliveries not started yet, in `send` order.
    pub(crate) jobs: VecDeque<Job>,
    /// Passive cancellation handles, including the job currently executing host code.
    resolutions: HashMap<TicketId, Resolution>,
    /// Remains reserved until the Delivery event finishes publication.
    admissions: HashMap<TicketId, usize>,
    pending_bytes: usize,
    /// Only deliveries admitted before a failed automatic control write are cancelled.
    write_failed: HashSet<TicketId>,
    /// The delivery the job thread runs (taken from `jobs`, not ended yet).
    pub(crate) running: Option<TicketId>,
    /// The delivery that was running when the terminal was dropped: its ticket is already
    /// `Disposed` and a final event was offered, whatever the job later ends with.
    disposed_running: Option<TicketId>,
    /// The commit-point handshake.
    commit: Commit,
    /// The crate typed a launch line or a paste into this session: the shell has been at its
    /// prompt since (a Ctrl-C no longer cuts its startup files short), and something the
    /// crate typed may sit on its line.
    typed: bool,
    /// A launch line waiting for the shell to take the Ctrl-C before it.
    pending_launch: Option<PendingLaunch>,
    /// Launches held back so far (numbers them).
    held_launches: u64,
    /// The last Ctrl-C the crate wrote, while nothing it typed followed (a paste or a
    /// launch line clears it).
    last_interrupt: Option<Interrupt>,
    /// Which program is the agent this terminal launched.
    pub(crate) program: AgentProgram,
    /// Wall-clock time of the last shell [`launch`](Self::launch), for best-effort transcript
    /// lookups that compare file times.
    #[cfg(feature = "turns")]
    launched_at: Option<std::time::SystemTime>,
    /// The screen the last launch line was typed over: its rows are not the agent's.
    leftover: Leftover,
    /// The last ticket handed out.
    last_ticket: u64,
    /// The last `AgentState` / `blocking` / activity published (or the initial values).
    published: (AgentState, bool),
    published_activity: Activity,
    /// Chunks the reader had fed when the current update began (set by
    /// [`Controller::update_quietly`]; 0 in unit tests).
    fed_at_update: u64,
    /// Events to publish, each with [`fed_at_update`](Self::fed_at_update) from when it was
    /// queued.
    outbox: VecDeque<(u64, Event)>,
    pub(in crate::agent) actions: Vec<Action>,
}

impl CtlState {
    pub(crate) fn new(cfg: Arc<Config>) -> Self {
        let timing = &cfg.timing;
        let gate = StartupGate::new(timing.clone(), cfg.trust.steer_dialogs);
        let published = (gate.state(), gate.blocking());
        let activity = ActivityTracker::new(timing.idle_debounce, timing.progress_stale);
        let published_activity = activity.state();
        Self {
            #[cfg(feature = "questions")]
            questions: super::questions::Book::new(cfg.id),
            cfg,
            gate,
            gate_revision: 0,
            lifecycle_revision: 0,
            monitor_overflow: Arc::new(AtomicBool::new(false)),
            session_closed: crossbeam_channel::never(),
            activity,
            view: OutputView::default(),
            disposed: false,
            monitor_failed: false,
            jobs: VecDeque::new(),
            resolutions: HashMap::new(),
            admissions: HashMap::new(),
            pending_bytes: 0,
            write_failed: HashSet::new(),
            running: None,
            disposed_running: None,
            commit: Commit::Idle,
            typed: false,
            pending_launch: None,
            held_launches: 0,
            last_interrupt: None,
            program: AgentProgram::Unknown,
            #[cfg(feature = "turns")]
            launched_at: None,
            leftover: Leftover::default(),
            last_ticket: 0,
            published,
            published_activity,
            fed_at_update: 0,
            outbox: VecDeque::new(),
            actions: Vec::new(),
        }
    }

    /// The agent was launched (typed into the shell, or the direct child started): it
    /// runs, the startup gate is armed, activity starts over as unknown.
    pub(crate) fn launch(&mut self, now: Instant) {
        self.lifecycle_revision = self.lifecycle_revision.wrapping_add(1);
        #[cfg(feature = "turns")]
        {
            // A direct child's wall-clock fence comes from the pre-spawn hook.
            self.launched_at = self
                .cfg
                .launch_line
                .as_ref()
                .map(|_| std::time::SystemTime::now());
        }
        self.gate_revision = self.gate_revision.wrapping_add(1);
        #[cfg(feature = "questions")]
        self.invalidate_questions();
        self.gate.launch(now);
        self.activity.reset();
    }

    /// The agent is launched in the shell now: arms the gate and returns what to type — the
    /// input line cleared first (an unsubmitted paste or a draft may sit there), then
    /// `line`. `leftover` is the screen read before: from now on only the rows drawn since
    /// count as the agent's ([`Leftover::fresh`]). With the shell read in
    /// front (`front.shell`) the agent's program is expected
    /// ([`observe`](Self::observe)) and a Ctrl-C goes first: one
    /// line kill leaves the earlier lines of a multi-line buffer in zsh (they would run with
    /// the launch line), Ctrl-C discards all of it (and the tty flushes input not read
    /// yet). Not before the crate typed anything into this session: the shell may still be
    /// reading its startup files (a SIGINT would cut them short), and nothing of the
    /// crate's can be on its line. A launch line still waiting for its Ctrl-C is dropped:
    /// this one replaces it. While a Ctrl-C the crate wrote a moment ago is still being
    /// handled, this one waits until it settled ([`LaunchKeys::Held`]): a second interrupt
    /// while the shell handles the first made bash 3.2 drop the launch line's first
    /// character. (Not skipped: something may have reached the line since.) What is held
    /// back is written only if the shell still owns the terminal then, with no line the
    /// host submitted to it since ([`release_launch`](Self::release_launch)).
    ///
    /// A line typed into another program (the policy cannot tell the shell, or a host
    /// `launch()` into whatever is in front) teaches nothing: what was known stays.
    pub(crate) fn type_launch(
        &mut self,
        line: &str,
        now: Instant,
        front: &FrontRead,
        leftover: Leftover,
    ) -> LaunchKeys {
        let shell_in_front = front.shell;
        self.launch(now);
        self.leftover = leftover;
        if shell_in_front {
            self.program = AgentProgram::Expected {
                shell_lines: front.shell_lines,
            };
        }
        self.pending_launch = None;
        let keys = launch_keys(self.cfg.shell, line);
        let typed_before = std::mem::replace(&mut self.typed, true);
        if !shell_in_front || !typed_before {
            self.last_interrupt = None;
            return LaunchKeys::Now(keys);
        }
        let earlier = self
            .last_interrupt
            .filter(|i| !self.monitor_unavailable() && i.due(&self.view) > now);
        if earlier.is_some() {
            self.trace("launch: the Ctrl-C and the launch line wait for the one written before");
        }
        self.held_launches += 1;
        let number = self.held_launches;
        self.pending_launch = Some(PendingLaunch {
            keys,
            after: earlier,
            interrupt: earlier.is_some(),
            shell_lines: front.shell_lines,
            number,
        });
        match earlier {
            Some(_) => LaunchKeys::Held,
            None => LaunchKeys::AfterInterrupt(number),
        }
    }

    /// The crate typed a launch line or a paste into this session: the shell is past its
    /// startup files, a Ctrl-C no longer cuts them short.
    pub(crate) fn typed(&self) -> bool {
        self.typed
    }

    /// A paste is being written: something the crate typed may end up on the shell's line.
    pub(crate) fn note_paste(&mut self) {
        self.typed = true;
        self.last_interrupt = None;
    }

    /// The crate wrote a Ctrl-C to the shell (`after`): a launch line typed while the shell
    /// still handles it waits for it instead of interrupting again.
    pub(crate) fn note_interrupt(&mut self, after: Interrupt) {
        self.last_interrupt = Some(after);
    }

    /// Apply the result of interrupting a shell that inherited an agent paste. A
    /// replacement can start between queueing Ctrl-C and recording its output fence.
    pub(super) fn finish_paste_interrupt(
        &mut self,
        lifecycle: u64,
        after: Option<Interrupt>,
    ) -> Step<()> {
        self.check_delivery_lifecycle(Some(lifecycle))?;
        self.agent_left();
        if let Some(after) = after {
            self.note_interrupt(after);
        }
        Ok(())
    }

    /// The Ctrl-C of held launch `number` was written; `written` is its fence, read right
    /// after the write was queued — output fed before it is not the shell's answer —
    /// `None` when the write failed (including a full input queue): that launch and its
    /// admitted deliveries fail; no gate advances on unwritten control input.
    /// The monitor types the keys once the shell took it (`settle::interrupt_due`); after a
    /// monitor failure the held line is discarded. A launch decided meanwhile waits for
    /// this Ctrl-C before writing its own.
    pub(crate) fn launch_interrupted(&mut self, number: u64, written: Option<Interrupt>) {
        let ours = self
            .pending_launch
            .as_ref()
            .is_some_and(|p| p.number == number);
        let Some(after) = written else {
            if ours {
                self.control_write_failed();
            }
            return;
        };
        self.last_interrupt = Some(after);
        if self.monitor_unavailable() {
            self.pending_launch = None;
            return;
        }
        let Some(pending) = self.pending_launch.as_mut() else {
            return;
        };
        if !ours {
            if pending.interrupt {
                pending.after = Some(after);
            }
            return;
        }
        pending.after = Some(after);
        self.trace("launch: the launch line waits for the shell to take a Ctrl-C");
    }

    /// A held launch step is due ([`release_launch`](Self::release_launch)): the monitor
    /// reads who owns the terminal before it fires the deadlines.
    pub(crate) fn launch_due(&self, now: Instant) -> bool {
        self.pending_launch
            .as_ref()
            .and_then(|p| p.after)
            .is_some_and(|after| after.due(&self.view) <= now)
    }

    /// The held-back launch, when due: its own Ctrl-C once the earlier one settled (its
    /// number is returned: the monitor writes it and reports its fence,
    /// [`launch_interrupted`](Self::launch_interrupted)), else its keys. Each only while
    /// `front` — read right before (`None`: not read; the next deadline pass reads it) —
    /// passes [`held_step_refusal`]; otherwise the launch is dropped: nothing more typed,
    /// no agent runs.
    fn release_launch(&mut self, now: Instant, front: Option<&FrontRead>) -> Option<u64> {
        let pending = self.pending_launch.as_ref()?;
        if pending.after?.due(&self.view) > now {
            return None;
        }
        let front = front?;
        if let Some(why) = held_step_refusal(front, pending.shell_lines) {
            self.pending_launch = None;
            self.trace(format!("launch: not typed after the Ctrl-C, {why}"));
            // Undoes the start the launch armed.
            self.agent_left();
            return None;
        }
        let pending = self.pending_launch.as_mut()?;
        if pending.interrupt {
            pending.interrupt = false;
            pending.after = None;
            let number = pending.number;
            self.trace("launch: the Ctrl-C written before settled; interrupting again");
            return Some(number);
        }
        let pending = self.pending_launch.take()?;
        self.trace("launch: typing the launch line after the Ctrl-C");
        self.last_interrupt = None;
        // The startup it armed (its trust window, its cap) runs from the line, not the
        // Ctrl-C.
        if self.gate.running() {
            self.launch(now);
        }
        self.actions.push(Action::Write(pending.keys.into_bytes()));
        None
    }

    /// Counts one output chunk and updates the gate's bounded raw buffer. Profile code
    /// runs separately on a snapshot, never while this state is locked.
    pub(crate) fn on_output(&mut self, bytes: &[u8], now: Instant) {
        self.view.output_serial += 1;
        self.view.last_output_at = Some(now);
        self.gate_revision = self.gate_revision.wrapping_add(1);
        self.gate.on_output(bytes, now);
    }

    /// A synchronized-output frame ended in the chunk counted last. `open_after` is the
    /// last chunk after which a frame was still open (`TerminalHandle::frame_open_after`,
    /// read now): when that is this chunk — it ended one frame and began the next — the
    /// screen is mid-redraw and no evidence is kept. (Read late, it may name a later chunk:
    /// that one is output after this frame end, which voids the evidence anyway.)
    pub(crate) fn on_frame(&mut self, now: Instant, open_after: u64) {
        if open_after != self.view.output_serial {
            self.view.last_frame_end = Some(self.evidence(now));
        }
    }

    fn evidence(&self, now: Instant) -> Evidence {
        Evidence {
            at: now,
            serial: self.view.output_serial,
        }
    }

    /// Identifies the current launch/departure boundary independently of gate polling.
    pub(crate) fn lifecycle_revision(&self) -> u64 {
        self.lifecycle_revision
    }

    /// After a paste, foreground evidence and control input belong to that launch.
    pub(super) fn check_delivery_lifecycle(&self, expected: Option<u64>) -> Step<()> {
        if let Some(reason) = self.cancel_reason() {
            return Err(reason);
        }
        if expected.is_some_and(|revision| revision != self.lifecycle_revision) {
            return Err(AbortReason::SubmitUnverified);
        }
        Ok(())
    }

    /// When the shell launch line was issued. Direct children use the PTY spawn time.
    /// Kept after the shell-launched agent left.
    #[cfg(feature = "turns")]
    pub(crate) fn launched_at(&self) -> Option<std::time::SystemTime> {
        self.launched_at
    }

    /// The rows present at launch. A delivery can clone this snapshot under the lock
    /// and find the fresh part of its current screen after releasing the lock.
    pub(super) fn leftover(&self) -> &Leftover {
        &self.leftover
    }

    /// A launch line is held back until the shell took a Ctrl-C: the agent is not started
    /// yet, whatever the screen shows.
    pub(crate) fn launch_held(&self) -> bool {
        self.pending_launch.is_some()
    }

    /// A title (activity; nothing without a profile).
    pub(crate) fn on_title(&mut self, title: &str, now: Instant) {
        if self.cfg.profile.is_some() {
            self.activity.push_title(title, now);
        }
    }

    /// A progress report: activity (nothing without a profile); a cleared one is also
    /// settle evidence for the delivery waits.
    pub(crate) fn on_progress(&mut self, working: bool, now: Instant) {
        if !working {
            self.view.last_progress_clear = Some(self.evidence(now));
        }
        if self.cfg.profile.is_some() {
            self.activity.push_progress(working, now);
        }
    }

    /// The session ended: no agent, no gate, activity idle (a bare shell's stays unknown).
    pub(crate) fn on_session_end(&mut self) {
        #[cfg(feature = "questions")]
        self.invalidate_questions();
        self.lifecycle_revision = self.lifecycle_revision.wrapping_add(1);
        self.gate_revision = self.gate_revision.wrapping_add(1);
        self.gate.end_session();
        self.pending_launch = None;
        if self.cfg.profile.is_some() {
            self.activity.mark_exited();
        }
    }

    /// The shell, not the agent, is in front: the agent is gone (state `ShellOnly`,
    /// activity idle, its program unknown) until it is launched again.
    pub(crate) fn agent_left(&mut self) {
        #[cfg(feature = "questions")]
        self.invalidate_questions();
        self.lifecycle_revision = self.lifecycle_revision.wrapping_add(1);
        self.program = AgentProgram::Unknown;
        self.gate_revision = self.gate_revision.wrapping_add(1);
        self.gate.leave();
        if self.cfg.profile.is_some() {
            self.activity.mark_exited();
        }
    }

    /// Learns the agent's program from a look at the terminal (`front`) while one is
    /// expected: the first program the policy trusts and names — unless the host submitted
    /// a line to the shell since the launch line, which may have started that program (then
    /// none is known: no delivery is typed into a program until the next launch).
    pub(crate) fn observe(&mut self, front: &FrontRead) {
        let AgentProgram::Expected { shell_lines } = self.program else {
            return;
        };
        if front.shell_lines != shell_lines {
            self.program = AgentProgram::Unknown;
            self.trace("a line was submitted to the shell after the launch line: the agent's program is not known");
        } else if let Some(program) = front.program {
            self.program = AgentProgram::Known(program);
            self.trace(format!("the agent's program: {}", program.0));
        }
    }

    /// The agent's program is still to be learned ([`observe`](Self::observe)).
    pub(crate) fn expects_program(&self) -> bool {
        matches!(self.program, AgentProgram::Expected { .. })
    }

    /// The gate waits for an agent that may have left unseen: launched (the line typed),
    /// not ready, and its program was seen or a dialog is on screen — a dialog left to the
    /// user stays on screen, parking the gate, after its agent exited from it. The shell
    /// then read in front (a trusted answer) means it left: nothing would make the gate
    /// ready any more. Not before its program was seen otherwise: the shell is in front
    /// until it started it.
    pub(crate) fn could_have_left(&self) -> bool {
        self.gate.running()
            && !self.gate.ready()
            && !self.launch_held()
            && (matches!(self.program, AgentProgram::Known(_))
                || self.gate.state() == AgentState::TrustPrompt)
    }

    /// Why a delivery cannot go on: the terminal was dropped (`Disposed`), the session
    /// ended or monitoring failed (`NotConnected`).
    pub(crate) fn cancel_reason(&self) -> Option<AbortReason> {
        if self.disposed {
            Some(AbortReason::Disposed)
        } else if self
            .running
            .is_some_and(|id| self.write_failed.contains(&id))
        {
            Some(AbortReason::WriteFailed)
        } else if self.session_ended() || self.monitor_unavailable() {
            Some(AbortReason::NotConnected)
        } else {
            None
        }
    }

    /// The monitor can no longer provide complete, ordered evidence for automation.
    pub(crate) fn monitor_unavailable(&self) -> bool {
        self.monitor_failed || self.monitor_overflow.load(Ordering::Acquire)
    }

    pub(crate) fn monitor_stopped(&mut self) {
        self.monitor_failed = true;
        #[cfg(feature = "questions")]
        self.invalidate_questions();
    }

    fn session_ended(&self) -> bool {
        self.gate.ended()
            || matches!(
                self.session_closed.try_recv(),
                Err(TryRecvError::Disconnected)
            )
    }

    /// The next ticket id (from 1).
    pub(crate) fn next_ticket(&mut self) -> TicketId {
        self.last_ticket += 1;
        TicketId(self.last_ticket)
    }

    fn reserve_delivery(
        &mut self,
        bytes: usize,
        largest: usize,
    ) -> (TicketId, Option<AbortReason>) {
        let ticket = self.next_ticket();
        let limits = self.cfg.limits;
        #[cfg(feature = "questions")]
        let (answer_jobs, answer_bytes) = self
            .questions
            .admitted_bytes()
            .map_or((0, 0), |bytes| (1, bytes));
        #[cfg(not(feature = "questions"))]
        let (answer_jobs, answer_bytes) = (0, 0);
        let reason = if self.disposed {
            Some(AbortReason::Disposed)
        } else if self.session_ended() || self.monitor_unavailable() {
            Some(AbortReason::NotConnected)
        } else if largest > limits.max_message_bytes || bytes > limits.max_pending_bytes {
            Some(AbortReason::InputTooLarge)
        } else if self.admissions.len() + answer_jobs >= limits.max_pending_jobs
            || bytes > limits.max_pending_bytes - self.pending_bytes - answer_bytes
        {
            Some(AbortReason::QueueFull)
        } else {
            self.admissions.insert(ticket, bytes);
            self.pending_bytes += bytes;
            None
        };
        (ticket, reason)
    }

    fn release_delivery(&mut self, ticket: TicketId) {
        self.write_failed.remove(&ticket);
        if let Some(bytes) = self.admissions.remove(&ticket) {
            self.pending_bytes -= bytes;
        }
    }

    #[cfg(feature = "questions")]
    pub(in crate::agent) fn answer_room(&self, bytes: usize) -> bool {
        self.admissions.len() < self.cfg.limits.max_pending_jobs
            && bytes <= self.cfg.limits.max_pending_bytes - self.pending_bytes
    }

    /// Keeps teardown independent of the job thread, including host callbacks.
    pub(crate) fn track_resolution(&mut self, ticket: TicketId, resolution: Resolution) {
        self.resolutions.insert(ticket, resolution);
    }

    /// Queues an event for the monitor to publish (after the events queued before it).
    pub(crate) fn publish(&mut self, event: Event) {
        self.outbox.push_back((self.fed_at_update, event));
    }

    /// A delivery ended with `outcome`: queues its `Event::Delivery` and returns the
    /// outcome for its ticket — `Disposed` for the one that was running when the terminal
    /// was dropped ([`dispose`](Self::dispose) resolved that already), so the ticket and
    /// the event agree.
    pub(crate) fn delivery_ended(
        &mut self,
        ticket: TicketId,
        outcome: DeliveryOutcome,
    ) -> DeliveryOutcome {
        let outcome = if self.write_failed.remove(&ticket) {
            DeliveryOutcome::Aborted(AbortReason::WriteFailed)
        } else {
            outcome
        };
        self.resolutions.remove(&ticket);
        if self.running == Some(ticket) {
            self.running = None;
        }
        if self.disposed_running == Some(ticket) {
            return DeliveryOutcome::Aborted(AbortReason::Disposed);
        }
        self.publish(Event::Delivery {
            id: self.cfg.id,
            ticket,
            outcome: outcome.clone(),
        });
        outcome
    }

    fn control_write_failed(&mut self) {
        self.write_failed.extend(self.admissions.keys().copied());
        self.write_failed.extend(self.resolutions.keys().copied());
        self.write_failed.extend(self.running);
        self.pending_launch = None;
        self.agent_left();
    }

    /// The terminal is being dropped: deliveries end. Queues `Aborted(Disposed)` for the
    /// running delivery and every queued one. Cancellation handles and jobs are returned
    /// so ticket wakers and host-owned destructors run outside the state lock.
    pub(crate) fn dispose(&mut self) -> (Vec<Job>, Vec<Resolution>) {
        self.disposed = true;
        #[cfg(feature = "questions")]
        self.invalidate_questions();
        self.commit = Commit::Idle;
        let resolutions = self
            .resolutions
            .drain()
            .map(|(_, resolution)| resolution)
            .collect();
        let queued: Vec<Job> = self.jobs.drain(..).collect();
        self.disposed_running = self.running.take();
        let tickets: Vec<TicketId> = self
            .disposed_running
            .into_iter()
            .chain(queued.iter().filter_map(Job::ticket))
            .collect();
        for ticket in tickets {
            self.publish(Event::Delivery {
                id: self.cfg.id,
                ticket,
                outcome: DeliveryOutcome::Aborted(AbortReason::Disposed),
            });
        }
        (queued, resolutions)
    }

    /// A delivery with a handshake reached its commit point: queues `Event::CommitPoint`
    /// and awaits the host's answer ([`confirm`](Self::confirm)).
    pub(crate) fn await_commit(&mut self, ticket: TicketId) {
        self.commit = Commit::Awaiting(ticket);
        self.publish(Event::CommitPoint {
            id: self.cfg.id,
            ticket,
        });
    }

    /// The host's answer for `ticket`: kept when that delivery awaits one (`true`), else
    /// ignored — a late answer, a second one, an unknown ticket.
    pub(crate) fn confirm(&mut self, ticket: TicketId, proceed: bool) -> bool {
        if self.commit != Commit::Awaiting(ticket) {
            return false;
        }
        self.commit = Commit::Answered(ticket, proceed);
        true
    }

    /// Takes the host's answer for `ticket`, if it came.
    pub(crate) fn take_answer(&mut self, ticket: TicketId) -> Option<bool> {
        match self.commit {
            Commit::Answered(answered, proceed) if answered == ticket => {
                self.commit = Commit::Idle;
                Some(proceed)
            }
            _ => None,
        }
    }

    /// The handshake ended without an answer (its timeout, or the delivery was cancelled):
    /// a later answer is ignored.
    pub(crate) fn end_commit(&mut self) {
        self.commit = Commit::Idle;
    }

    /// The earliest gate, activity or held-back launch line deadline.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let launch = self
            .pending_launch
            .as_ref()
            .and_then(|p| p.after)
            .map(|after| after.due(&self.view));
        [
            (!self.monitor_unavailable())
                .then(|| self.gate.next_deadline())
                .flatten(),
            self.activity.next_deadline(),
            launch,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Fires non-profile deadlines after the monitor evaluated the gate outside the lock.
    /// `front` is read after that evaluation, so a slow callback cannot stale the ownership
    /// check for a held launch. Returns the held launch's next interrupt, when due.
    pub(crate) fn on_deadlines(&mut self, now: Instant, front: Option<&FrontRead>) -> Option<u64> {
        if self.cancel_reason().is_some() {
            return None;
        }
        self.activity.on_deadline(now);
        self.release_launch(now, front)
    }

    pub(crate) fn agent_state(&self) -> AgentState {
        if self.session_ended() {
            AgentState::Exited
        } else {
            self.gate.state()
        }
    }

    /// The host's loading overlay: the gate's, lifted after monitoring or transport ends.
    pub(crate) fn blocking(&self) -> bool {
        self.gate.blocking() && !self.monitor_unavailable() && !self.session_ended()
    }

    /// A trace line: logged at `info` when tracing is on.
    pub(crate) fn trace(&mut self, line: impl Into<String>) {
        if self.cfg.trace {
            self.actions
                .push(Action::Log(log::Level::Info, line.into()));
        }
    }

    fn apply(&mut self, effects: Vec<GateEffect>, screen: &str) {
        for effect in effects {
            match effect {
                GateEffect::Key { kind, key } => {
                    self.trace(format!("{kind:?} dialog: pressing {key:?}"));
                    self.actions.push(Action::Write(key.bytes().to_vec()));
                }
                GateEffect::Note(line) => self.trace(line),
                GateEffect::MarkersUnrecognised { elapsed } => {
                    let line = self.marker_report(elapsed.as_millis(), screen);
                    self.actions.push(Action::Log(log::Level::Warn, line));
                }
                GateEffect::ProfilePanicked(message) => {
                    let line = format!(
                        "profile {} panicked: {message}; its dialogs are no longer answered, readiness falls back to the startup timers",
                        self.cfg.profile_id
                    );
                    self.actions.push(Action::Log(log::Level::Warn, line));
                }
            }
        }
    }

    /// Names the profile and time since launch. For a launch with default model
    /// selection, the terminal identity and bottom rows help diagnose changed markers.
    fn marker_report(&self, elapsed_ms: u128, screen: &str) -> String {
        let profile = &self.cfg.profile_id;
        if self.cfg.has_model_choice {
            return format!(
                "startup markers not recognised for {profile} after {elapsed_ms} ms (model choice supplied)"
            );
        }
        let text = ScreenText::from_text(screen);
        let rows: Vec<String> = text
            .bottom_rows(REPORT_ROWS)
            .iter()
            .map(|row| normalize(row))
            .filter(|row| !row.is_empty())
            .collect();
        format!(
            "startup markers not recognised for {profile} after {elapsed_ms} ms (identity {}); bottom rows: {}",
            identity_label(&self.cfg.identity),
            rows.join(" | ")
        )
    }

    /// Queues an `AgentState` / `Activity` event for every published value that changed.
    /// A bare shell (no profile) publishes none: its receivers carry only session events.
    fn sync_events(&mut self) {
        if self.cfg.profile.is_none() {
            return;
        }
        let id = self.cfg.id;
        let now = (self.agent_state(), self.blocking());
        if now != self.published {
            self.published = now;
            self.publish(Event::AgentState {
                id,
                state: now.0,
                blocking: now.1,
            });
        }
        let activity = self.activity.state();
        if activity != self.published_activity {
            self.published_activity = activity;
            self.publish(Event::Activity {
                id,
                state: activity,
            });
        }
    }
}

/// What starting the agent types: [`ShellKind::clear_line`], then `line`.
pub(crate) fn launch_keys(shell: ShellKind, line: &str) -> String {
    format!("{}{line}", shell.clear_line().unwrap_or_default())
}

/// The identity for a log line — variable names only, never values.
fn identity_label(identity: &TerminalIdentity) -> String {
    match identity {
        TerminalIdentity::Honest => "honest".into(),
        TerminalIdentity::Impersonate(terminal) => format!("impersonating {terminal:?}"),
        TerminalIdentity::Custom { vars } => {
            let keys: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
            format!("custom [{}]", keys.join(", "))
        }
    }
}

/// The state behind one lock, the condvar the job thread waits on, and the monitor's
/// wake-up channel.
pub(crate) struct Controller {
    pub(crate) cfg: Arc<Config>,
    term: TerminalHandle,
    state: Mutex<CtlState>,
    cv: Condvar,
    monitor: Sender<MonitorMsg>,
    /// Held while events are taken from the outbox and published (the monitor, and the
    /// drop), so two threads never interleave their batches. Never taken under `state`.
    publishing: Mutex<()>,
}

impl Controller {
    pub(crate) fn new(cfg: Config, term: TerminalHandle, monitor: Sender<MonitorMsg>) -> Self {
        let cfg = Arc::new(cfg);
        let mut state = CtlState::new(Arc::clone(&cfg));
        state.monitor_overflow = term.monitor_overflow();
        state.session_closed = term.closed_signal();
        Self {
            state: Mutex::new(state),
            cfg,
            term,
            cv: Condvar::new(),
            monitor,
            publishing: Mutex::new(()),
        }
    }

    pub(crate) fn term(&self) -> &TerminalHandle {
        &self.term
    }

    /// Called with the terminal writer locked. Record the paste for later shell-line
    /// cleanup only once this launch is allowed to receive it.
    pub(super) fn prepare_paste(
        &self,
        expected_lifecycle: Option<u64>,
        turn_wait: Option<&mut TurnWait>,
    ) -> Step<Option<PasteFence>> {
        let mut st = self.lock();
        if let Some(reason) = st.cancel_reason() {
            return Err(reason);
        }
        let lifecycle = st.lifecycle_revision();
        if expected_lifecycle.is_some_and(|expected| lifecycle != expected) {
            return Ok(None);
        }
        if let Some(wait) = turn_wait {
            match wait.look(
                self.cfg.timing.reset_turn_wait,
                working(&st),
                Instant::now(),
            ) {
                TurnGate::Open => {}
                TurnGate::Wait(_) => return Ok(None),
                TurnGate::Expired => return Err(AbortReason::AgentWorking),
            }
        }
        st.note_paste();
        Ok(Some(PasteFence {
            lifecycle,
            serial: self.term.output_chunks(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, CtlState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn reserve_delivery(
        &self,
        bytes: usize,
        largest: usize,
    ) -> (TicketId, Option<AbortReason>) {
        // No update or wake on refusal: there is no work and no event to publish.
        self.lock().reserve_delivery(bytes, largest)
    }

    pub(crate) fn track_admission(&self, ticket: TicketId, resolution: Resolution) -> bool {
        let mut st = self.lock();
        if st.disposed {
            return false;
        }
        st.track_resolution(ticket, resolution);
        true
    }

    pub(crate) fn cancel_admission(&self, ticket: TicketId) {
        let mut st = self.lock();
        st.release_delivery(ticket);
        st.resolutions.remove(&ticket);
    }

    pub(crate) fn release_delivery(&self, ticket: TicketId) {
        self.lock().release_delivery(ticket);
    }

    /// Runs `f` under the lock with the current time, then — lock released — writes and
    /// logs what it returned, wakes waiting jobs and the monitor (which publishes the
    /// queued events). For any thread but the monitor.
    pub(crate) fn update<R>(&self, f: impl FnOnce(&mut CtlState, Instant) -> R) -> R {
        let r = self.update_quietly(f);
        let _ = self.monitor.try_send(MonitorMsg::Wake);
        r
    }

    /// [`update`](Self::update) without waking the monitor (the monitor's own updates).
    pub(crate) fn update_quietly<R>(&self, f: impl FnOnce(&mut CtlState, Instant) -> R) -> R {
        let (r, actions) = {
            let mut st = self.lock();
            // At or past every screen read this update was given (they come before it).
            st.fed_at_update = self.term.output_chunks();
            let r = f(&mut st, Instant::now());
            st.sync_events();
            (r, std::mem::take(&mut st.actions))
        };
        self.perform(actions);
        self.cv.notify_all();
        r
    }

    /// Runs profile code on a private gate snapshot, without holding any controller lock.
    /// A changed launch, gate, output fence, failure, or teardown invalidates the result:
    /// stale readiness and dialog keys are discarded together. The monitor and job thread
    /// use this same boundary; neither can block state queries or disposal inside a profile.
    pub(crate) fn evaluate_gate(&self, deadlines: bool) -> bool {
        let Some(profile) = self.cfg.profile.as_deref() else {
            return true;
        };
        let (revision, mut gate, leftover) = {
            let st = self.lock();
            if st.cancel_reason().is_some() {
                return false;
            }
            (st.gate_revision, st.gate.clone(), st.leftover.clone())
        };
        let Some((screen, fed, screen_revision)) = self.term.screen_observation() else {
            return false;
        };
        if !self.term.screen_observation_current(fed, screen_revision) {
            return false;
        }
        let screen = screen.text();
        let fresh = leftover.fresh(&screen);
        let now = Instant::now();
        let effects = if deadlines {
            gate.on_deadline(now, fresh, profile)
        } else {
            gate.evaluate(fresh, profile, now)
        };
        self.update_quietly(|st, _| {
            if st.cancel_reason().is_some()
                || st.gate_revision != revision
                || !self.term.screen_observation_current(fed, screen_revision)
            {
                return false;
            }
            st.gate_revision = st.gate_revision.wrapping_add(1);
            st.gate = gate;
            // Keep the source fence from before profile evaluation, not a later output.
            st.fed_at_update = fed;
            st.apply(effects, fresh);
            true
        })
    }

    /// Arms the startup gate, then types the launch line (`AgentTerminal::launch`) after
    /// the keys that clear the shell's input line — with the shell in front (the
    /// foreground, read now) after a Ctrl-C too ([`CtlState::type_launch`]). No-op without
    /// a launch line; [`Error::Closed`] once the session ended.
    pub(crate) fn launch(&self) -> Result<()> {
        let Some(line) = self.cfg.launch_line.as_deref() else {
            return Ok(());
        };
        let front = self.read_front();
        let leftover = self.leftover();
        let launch = self.update(|st, now| {
            if st.cancel_reason().is_some() {
                return Err(Error::Closed);
            }
            st.trace("launch: typing the launch line");
            Ok(st.type_launch(line, now, &front, leftover))
        })?;
        self.type_keys(launch)
    }

    /// The screen a launch line is about to be typed over.
    fn leftover(&self) -> Leftover {
        Leftover::new(self.term.screen().map(|s| s.rows).unwrap_or_default())
    }

    /// Who owns the terminal now: the foreground and the program it names, then the
    /// host's lines to the shell — in that order, so a program such a line started is never
    /// seen without its line counted.
    pub(crate) fn read_front(&self) -> FrontRead {
        let (fg, program) = self.term.foreground_program();
        let shell_lines = self.term.shell_lines();
        FrontRead {
            fg,
            shell: self.cfg.shell_in_front(fg),
            program: self.cfg.trusted_program(fg, program),
            shell_lines,
        }
    }

    /// Starts the agent unless it runs (or the session ended, or there is nothing to
    /// type): a delivery to a stopped agent launches it first — unless the foreground,
    /// read now, is a program the policy tells from the shell (an agent started by hand,
    /// a nested shell, `ssh`, an editor): nothing is typed into it, and the delivery ends
    /// there (`Runner::front`: no launch of this terminal started that program). Unknown
    /// ownership also gets no launch line. Checked and armed in one
    /// update, so two sends never type the line twice.
    pub(crate) fn launch_if_stopped(&self) {
        let Some(line) = self.cfg.launch_line.as_deref() else {
            return;
        };
        if self.read(|st| st.gate.running() || st.cancel_reason().is_some()) {
            return;
        }
        let front = self.read_front();
        if self.cfg.trusted_owner(front.fg) != Some(ForegroundOwner::Shell) {
            if self.cfg.trace {
                let id = self.cfg.id;
                log::info!("[agent-link:{id}] launch: not typed, shell ownership is unproven");
            }
            return;
        }
        let leftover = self.leftover();
        let launch = self.update(|st, now| {
            // Nothing to launch for a delivery that cannot run (ended, failed monitor).
            if st.gate.running() || st.cancel_reason().is_some() {
                return None;
            }
            st.trace("launch: typing the launch line (a delivery is waiting)");
            Some(st.type_launch(line, now, &front, leftover))
        });
        if let Some(launch) = launch {
            self.type_keys_logged(launch);
        }
    }

    /// The agent left the foreground — the shell was just read in front (`front`): marks it
    /// gone and launches it again (the gate is armed again; nothing is typed without a
    /// launch line).
    pub(crate) fn relaunch(&self, front: &FrontRead) {
        let line = self.cfg.launch_line.as_deref();
        let leftover = self.leftover();
        let launch = self.update(|st, now| {
            st.agent_left();
            let line = line.filter(|_| st.cancel_reason().is_none())?;
            st.trace("launch: typing the launch line again");
            Some(st.type_launch(line, now, front, leftover))
        });
        if let Some(launch) = launch {
            self.type_keys_logged(launch);
        }
    }

    /// Types `launch`: its keys at once, or a Ctrl-C now and the keys once the shell took
    /// it (the monitor types them: a shell handling an interrupt can drop input that
    /// arrives meanwhile, and no host thread waits for it); held keys are the monitor's
    /// already. The write's error (the Ctrl-C's, when held back) is returned.
    fn type_keys(&self, launch: LaunchKeys) -> Result<()> {
        if self.read(|st| st.cancel_reason().is_some()) {
            return Err(Error::Closed);
        }
        match launch {
            LaunchKeys::Now(keys) => {
                let result = self.term.write(keys.as_bytes());
                if result.is_err() {
                    self.update(|st, _| st.control_write_failed());
                }
                result
            }
            LaunchKeys::AfterInterrupt(number) => self.interrupt_launch(number),
            LaunchKeys::Held => Ok(()),
        }
    }

    /// Shared by immediate and monitor-owned launch interrupts. A failed write must not
    /// leave the startup gate expecting a launch whose control input never arrived.
    pub(crate) fn interrupt_launch(&self, number: u64) -> Result<()> {
        let written = self.interrupt();
        let fence = written.as_ref().ok().copied();
        self.update(|st, _| st.launch_interrupted(number, fence));
        written.map(drop)
    }

    /// The host's Ctrl-C (`AgentTerminal::interrupt`): written like the crate's own, so a
    /// launch line right after it waits until the shell took it instead of interrupting
    /// again.
    pub(crate) fn host_interrupt(&self) -> Result<()> {
        let after = self.interrupt()?;
        self.update(|st, _| st.note_interrupt(after));
        Ok(())
    }

    /// The host's answer to `Event::CommitPoint` for `ticket` (`AgentTerminal::confirm`);
    /// ignored (a `debug` line) unless that delivery awaits it.
    pub(crate) fn confirm(&self, ticket: TicketId, proceed: bool) {
        let kept = self.update(|st, _| {
            let kept = st.confirm(ticket, proceed);
            if kept {
                st.trace(format!(
                    "delivery #{ticket}: the host answered the commit point: {}",
                    if proceed { "paste" } else { "do not paste" }
                ));
            }
            kept
        });
        if !kept {
            let id = self.cfg.id;
            log::debug!(
                "[agent-link:{id}] confirm for delivery #{ticket} ignored: it awaits no answer"
            );
        }
    }

    /// Writes a Ctrl-C; returns its fence (the chunks fed right after it was queued).
    pub(crate) fn interrupt(&self) -> Result<Interrupt> {
        self.term.write(b"\x03")?;
        Ok(self.interrupt_fence())
    }

    pub(super) fn interrupt_fence(&self) -> Interrupt {
        Interrupt {
            fence: self.term.output_chunks(),
            at: Instant::now(),
        }
    }

    fn type_keys_logged(&self, launch: LaunchKeys) {
        if let Err(e) = self.type_keys(launch) {
            let id = self.cfg.id;
            log::debug!("[agent-link:{id}] launch line not written: {e}");
        }
    }

    /// Reads the state under the lock.
    pub(crate) fn read<R>(&self, f: impl FnOnce(&CtlState) -> R) -> R {
        f(&self.lock())
    }

    /// Blocks until `done` gives a value (checked now and after every update).
    pub(crate) fn wait_for<R>(&self, mut done: impl FnMut(&mut CtlState) -> Option<R>) -> R {
        let mut st = self.lock();
        loop {
            if let Some(r) = done(&mut st) {
                return r;
            }
            st = self
                .cv
                .wait_timeout(st, FAILURE_POLL)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// [`wait_for`](Self::wait_for), giving up at `deadline` (`None`).
    pub(crate) fn wait_until<R>(
        &self,
        deadline: Instant,
        mut done: impl FnMut(&mut CtlState) -> Option<R>,
    ) -> Option<R> {
        let mut st = self.lock();
        loop {
            if let Some(r) = done(&mut st) {
                return Some(r);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            st = self
                .cv
                .wait_timeout(st, left.min(FAILURE_POLL))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Blocks until `deadline`; `Err` with the reason as soon as the delivery is cancelled
    /// (other updates do not end the wait).
    pub(crate) fn sleep_until(&self, deadline: Instant) -> std::result::Result<(), AbortReason> {
        let mut st = self.lock();
        loop {
            if let Some(reason) = st.cancel_reason() {
                return Err(reason);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            st = self
                .cv
                .wait_timeout(st, left.min(FAILURE_POLL))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Never called with the lock held.
    fn perform(&self, actions: Vec<Action>) {
        let id = self.cfg.id;
        let mut may_write = true;
        for action in actions {
            match action {
                Action::Write(bytes) => {
                    if !may_write || self.read(|st| st.cancel_reason().is_some()) {
                        continue;
                    }
                    if let Err(e) = self.term.write(&bytes) {
                        log::debug!("[agent-link:{id}] controller write failed: {e}");
                        self.update(|st, _| st.control_write_failed());
                        // Remaining writes were decided using the failed control input.
                        may_write = false;
                    }
                }
                #[cfg(feature = "questions")]
                Action::EndAnswer(ticket, error) => ticket.finish(Err(error)),
                #[cfg(feature = "questions")]
                Action::ReleaseQuestions(source) => drop(source),
                Action::Log(level, line) => log::log!(level, "[agent-link:{id}] {line}"),
            }
        }
    }

    /// Keep capacity through a blocked fanout, even after draining the outbox to a local
    /// vector. Releasing on drain would let completed deliveries accumulate without bound.
    fn publish_reserved(&self, event: Event) {
        let ticket = match &event {
            Event::Delivery { ticket, .. } => Some(*ticket),
            _ => None,
        };
        #[cfg(feature = "questions")]
        let question = matches!(event, Event::Question { .. } | Event::QuestionClosed { .. });
        self.term.publish(event);
        #[cfg(feature = "questions")]
        if question {
            self.lock().questions.event_published();
        }
        if let Some(ticket) = ticket {
            self.release_delivery(ticket);
        }
    }

    /// Publishes the queued events, in order, outside the lock — up to the first one queued
    /// after an output chunk the monitor has not counted yet. The monitor's receiver is
    /// served last, so once it counted a chunk every host receiver holds that `Output`: an
    /// agent event never reaches a host before the output of the screen it was read from
    /// (the reader feeds the screen model before it publishes). The rest waits for the
    /// monitor's next pass. The monitor calls this (and [`dispose`](Self::dispose) its
    /// sibling); publishing is serialised, so events keep the order of the updates that
    /// queued them.
    pub(crate) fn publish_queued(&self) {
        let _publishing = self.publishing.lock().unwrap_or_else(|e| e.into_inner());
        let events: Vec<Event> = {
            let mut st = self.lock();
            let counted = st.view.output_serial;
            let due = st
                .outbox
                .iter()
                .take_while(|(fed, _)| *fed <= counted)
                .count();
            st.outbox.drain(..due).map(|(_, event)| event).collect()
        };
        for event in events {
            self.publish_reserved(event);
        }
    }

    /// Publishes every queued event, in order, outside the lock: once nothing counts output
    /// any more (the monitor stopped, or failed; the terminal is being dropped).
    pub(crate) fn publish_all(&self) {
        let _publishing = self.publishing.lock().unwrap_or_else(|e| e.into_inner());
        let events = std::mem::take(&mut self.lock().outbox);
        for (_, event) in events {
            self.publish_reserved(event);
        }
    }

    /// Resolves every outstanding ticket before shutdown, independently of the job
    /// thread. Final events are best effort: no wait for an in-flight publisher or for
    /// receiver capacity, and nothing can overtake an unfinished earlier fanout.
    pub(crate) fn dispose(&self) {
        let (jobs, resolutions) = self.update_quietly(|st, _| st.dispose());
        #[cfg(feature = "questions")]
        {
            let ticket = self.lock().questions.take_ticket();
            if let Some(ticket) = ticket {
                ticket.finish(Err(super::questions::AnswerError::NotOpen));
            }
        }
        for resolution in resolutions {
            resolution.resolve(DeliveryOutcome::Aborted(AbortReason::Disposed));
        }
        drop(jobs);
        let publishing = match self.publishing.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        let events: Vec<Event> = {
            let mut st = self.lock();
            let counted = st.view.output_serial;
            let outbox = std::mem::take(&mut st.outbox);
            if publishing.is_some() {
                // Never publish a reaction whose source output was not delivered yet.
                outbox
                    .into_iter()
                    .take_while(|(fed, _)| *fed <= counted)
                    .map(|(_, event)| event)
                    .collect()
            } else {
                Vec::new()
            }
        };
        self.term.close_events_with_final(events);
        // A queued Wake is sufficient: both monitor loops check disposed after waking.
        // Never wait for a full notification slot during teardown.
        let _ = self.monitor.try_send(MonitorMsg::Stop);
    }
}

#[cfg(test)]
mod tests;
