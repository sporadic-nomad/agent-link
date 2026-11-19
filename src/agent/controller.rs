//! The job thread (`agent-link-jobs-<id>`): runs programmatic deliveries one at a time, in
//! `send` order — a port of the embedded terminal panel's send queue (`processQueue`,
//! `processMessageJob`).
//!
//! Why one at a time: each message owns its own Enter. Two overlapping submits could
//! otherwise collapse into one Enter (the second paste lands before the first Enter), and a
//! multi-step job (a reset, then a message) could be split by other work.
//!
//! A message job, step by step (a text with nothing to paste — empty, or only whitespace
//! and control characters — is refused first, `EmptyText`: an Enter alone would submit the
//! user's draft; so is a reset without a reset command, `Unsupported`):
//! 1. **Ready gate**: wait until the agent is ready for input, or none runs (a bare shell,
//!    an agent that left). A dialog the startup gate leaves to the user keeps it waiting,
//!    and so does a launch line still held back after its Ctrl-C (a relaunch). Once the
//!    agent could have left unseen (its program was seen, or a dialog is up) the foreground
//!    is read every 250 ms: the shell in front ⇒ it left — an agent that exits from a
//!    dialog left to the user leaves the dialog on screen, parking the gate. The gate
//!    reads only the rows drawn since the launch line (`leftover`): the last agent's frame
//!    left on screen never makes the new one ready — read while the shell was still in
//!    front, it once sent the message to the shell's line (cleared by the launch line that
//!    followed) and closed the new agent's trust window. The dialog and lull checks below
//!    read the same rows.
//! 2. **Agent or shell** (every send): the foreground owner, asked now ([`job_owner`]).
//!    The startup gate's "running" is a launch heuristic that stays true after the agent
//!    exits back to the shell — exactly when a paste would land on the shell's line (a
//!    multi-line one runs there at once in a shell without bracketed paste) and an Enter
//!    would run it. The shell in front ⇒ relaunch the agent once and start over; still the
//!    shell ⇒ the shell fallback (`Runner::shell_fallback`): only a single line (tabs as
//!    spaces), without Enter (`PastedUnsubmitted`; it cannot run — no newline — and every
//!    later launch line kills it first), on a fresh input line (a Ctrl-C first, once the
//!    crate typed into the session: no draft or earlier text joins it, vi mode is back in
//!    insert mode), pasted where the shell turned bracketed paste on after that Ctrl-C,
//!    else typed as is; a multi-line text or `ShellFallback::Fail` ⇒ `ShellOnly`, nothing
//!    typed. Another program in front is the
//!    agent only when it is the agent's own program: the one a launch line this terminal
//!    typed into the shell started (`state::AgentProgram`; unix: its process group).
//!    Anything else — a nested shell, `ssh`, `nix develop`, an editor, `cat`, an agent
//!    started by hand — gets nothing, `ForegroundUnproven`, whatever the screen shows: an
//!    inline agent leaves its last frame, ready composer included, under what the user
//!    starts after it exited, and a submit there would run the text (on a remote host, for
//!    `ssh`). An owner the policy cannot name (unknown; a heuristic answer under
//!    `ExactOnly`; a Windows shell running several programs) is unproven, even after a
//!    launch. A **strict** send (`proof`) adds checks at the commit point and after submit:
//!    it proceeds only on a trusted answer naming the agent's own program. The
//!    shell keeps the relaunch rule (then `ShellOnly`, never the fallback's paste), anything
//!    else ends `ForegroundUnproven`.
//! 3. **Input lull** (`settle::lull_ready`): never paste into a burst of output.
//! 4. **Commit point**: the last check before the irreversible paste — the host's
//!    revalidation (`proof`: a sync check, or the `Event::CommitPoint` handshake), the
//!    strict proof (*pre-paste readiness*), then a screen check. A recognized question
//!    ends the job with `DialogBlocking`. A startup dialog goes back to the startup
//!    gate, which may steer it inside its post-ready watch and make the job wait again.
//!    If the gate stays ready the job ends with `TrustPromptBlocking`. A gate that is no
//!    longer ready (its watch un-readied it during the lull, a launch re-armed it) ⇒ wait
//!    for it again. Then the foreground once more (the agent may have left during the
//!    lull), handled as in step 2 — for a strict send the *paste* proof: the shell there
//!    ends it `ShellOnly`, no relaunch.
//! 5. **Paste**: one bracketed-paste write without a newline, so a multi-line message
//!    lands as one inert block (never line by line, never with its Enter: a TUI can
//!    process an Enter in the same write before the paste reached its input buffer).
//! 6. **Submit** (`verify`): wait until the paste landed (`settle::paste_landed`) and
//!    was recognised in the composer, write Enter separately and require fresh output
//!    showing a stable readable composer without the text. Retries require explicit
//!    opt-in and a composer still holding the text; a late redraw can cause duplicates. The foreground is read again right before every
//!    Enter: the agent may have left after the paste without reading it (the shell in
//!    front ⇒ its line interrupted, `ShellOnly`; another program ⇒ `ForegroundUnproven`,
//!    no Enter; strict: the proof on that read, and once more after the verified submit).
//!    A multi-line
//!    paste without submit also waits until it landed and reads the foreground once: the
//!    shell in front holds it as a multi-line buffer — interrupted (Ctrl-C), `ShellOnly`.
//!
//! A **reset and send** (`Job::ResetAndSend`, a port of the panel's `processAutoStartJob`)
//! is one job: the reset command as a message (submit, `Fail` on the shell, not strict),
//! the fence of its first Enter, a lull that needs fresh output after it (a slow reset is
//! silent at first, and the stale screen still shows a ready composer), then the message
//! (submit, `Fail`, the caller's strict flag and revalidation) — nothing else can run in
//! between.
//!
//! Every wait checks for cancellation first: the terminal dropped (`Disposed`), the
//! session ended or the monitor stopped after a panic (`NotConnected`). A host receiver
//! left full can stall the session. Verification requires counted output. Explicit reset
//! checks expire if a current observation stays unavailable. Writes, logs and
//! profile calls happen outside the controller's lock; the outcome goes to the ticket and,
//! through the monitor, out as `Event::Delivery`.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::delivery::Resolver;
use super::leftover::Leftover;
use super::settle::{self, Fence, LullParams, OutputView, Settle, interrupt_due};
use super::state::{AgentProgram, Controller, CtlState, FrontRead, Interrupt};
use super::verify::ResetBaseline;
use super::{
    AbortReason, AgentProfile, DeliveryOutcome, Dialog, ForegroundPolicy, Revalidate, ScreenState,
    SendOptions, ShellFallback, TicketId,
};
use crate::error::{Error, Result};
use crate::pty::ProgramId;
use crate::screen::{ScreenSnapshot, catch_panic};
use crate::{Foreground, ForegroundOwner};

/// How often a delivery waiting for readiness reads the foreground while the agent could
/// have left unseen (`CtlState::could_have_left`; on Windows each read is a process
/// snapshot).
const LEFT_POLL: Duration = Duration::from_millis(250);

/// `[agent-link:<id>] delivery #<ticket>: …` at `info` when tracing is on (formatted only
/// then). Never logs the message text.
macro_rules! trace {
    ($runner:expr, $ticket:expr, $($arg:tt)*) => {
        if $runner.ctl.cfg.trace {
            log::info!(
                "[agent-link:{}] delivery #{}: {}",
                $runner.ctl.cfg.id,
                $ticket,
                format_args!($($arg)*)
            );
        }
    };
}
pub(crate) use trace;

/// Only explicit resets inspect the response. A verified reset retains the evidence
/// its follow-up needs at the commit point, including after host revalidation.
pub(super) enum MessageIntent {
    Send,
    Reset,
    SubmittedReset(ResetBaseline),
}

/// One message to deliver.
pub(crate) struct MessageJob {
    pub(crate) ticket: TicketId,
    /// The text as pasted ([`paste_text`]).
    pub(crate) text: String,
    /// Characters [`paste_text`] dropped from the caller's text (traced).
    pub(crate) dropped: usize,
    pub(crate) opts: SendOptions,
    /// The agent was relaunched once for this job already.
    pub(crate) relaunched: bool,
    pub(super) intent: MessageIntent,
    /// Refused before it was queued (a reset without a reset command): ended in its turn,
    /// nothing typed.
    refused: Option<AbortReason>,
}

impl MessageJob {
    /// A job for `text`, made safe to paste ([`paste_text`]).
    pub(crate) fn new(ticket: TicketId, text: &str, opts: SendOptions) -> Self {
        let (text, dropped) = paste_text(text);
        Self {
            ticket,
            text,
            dropped,
            opts,
            relaunched: false,
            intent: MessageIntent::Send,
            refused: None,
        }
    }

    pub(super) fn reset(ticket: TicketId, command: &str, on_shell: ShellFallback) -> Self {
        Self {
            intent: MessageIntent::Reset,
            ..Self::new(
                ticket,
                command,
                SendOptions {
                    submit: true,
                    on_shell,
                    ..SendOptions::default()
                },
            )
        }
    }

    /// A job that ends with `reason` in its turn, nothing typed (keeps `Event::Delivery` in
    /// ticket order).
    pub(crate) fn refused(ticket: TicketId, reason: AbortReason) -> Self {
        Self {
            refused: Some(reason),
            ..Self::new(ticket, "", SendOptions::default())
        }
    }

    /// Why this job is refused before anything is typed (no launch either), if it is: a
    /// refusal it was made with, or a text with nothing to paste once sanitised (empty, or
    /// only whitespace): its Enter would submit whatever the user left in the composer.
    pub(crate) fn refusal(&self) -> Option<AbortReason> {
        if self.refused.is_some() {
            self.refused
        } else if self.text.trim().is_empty() {
            Some(AbortReason::EmptyText)
        } else {
            None
        }
    }
}

/// `text` as a delivery pastes it, and how many characters were dropped: the
/// bracketed-paste markers (`ESC[200~`, `ESC[201~`) removed, CR LF and a lone CR turned
/// into LF, every other control character (C0, DEL, C1) but LF and tab dropped — ESC
/// included, so no marker survives. An end marker inside the paste would end it early (the
/// rest would act as keys, a newline as Enter), and a terminal's line discipline acts on
/// control characters (^C, ^D, ^U, …) whatever the application in front does with a paste.
pub(crate) fn paste_text(text: &str) -> (String, usize) {
    let kept = text.replace("\x1b[200~", "").replace("\x1b[201~", "");
    // The markers are ASCII: bytes removed = characters removed.
    let mut dropped = text.len() - kept.len();
    let mut out = String::with_capacity(kept.len());
    let mut chars = kept.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                chars.next_if_eq(&'\n');
                out.push('\n');
            }
            '\n' | '\t' => out.push(c),
            c if c.is_control() => dropped += 1,
            c => out.push(c),
        }
    }
    (out, dropped)
}

/// A queued delivery and where its outcome goes.
pub(crate) enum Job {
    #[cfg(feature = "questions")]
    Answer(super::questions::AnswerJob),
    Message {
        job: MessageJob,
        resolver: Resolver,
    },
    /// The reset command, then the message, as one job; both carry the same ticket.
    ResetAndSend {
        reset: MessageJob,
        message: MessageJob,
        resolver: Resolver,
    },
}

impl Job {
    pub(crate) fn ticket(&self) -> Option<TicketId> {
        match self {
            Job::Message { job, .. } => Some(job.ticket),
            Job::ResetAndSend { message, .. } => Some(message.ticket),
            #[cfg(feature = "questions")]
            Job::Answer(_) => None,
        }
    }
}

/// A step's result: go on, or end the job with this reason.
pub(crate) type Step<T> = std::result::Result<T, AbortReason>;

/// A question owns input until the user answers or dismisses it.
pub(super) fn check_question(profile: &dyn AgentProfile, fresh: &str) -> Step<()> {
    if profile.question_dialog(fresh) {
        Err(AbortReason::DialogBlocking)
    } else {
        Ok(())
    }
}

/// Who a job takes for the terminal's owner ([`job_owner`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    Shell,
    Agent,
    /// Another program, not the agent's own.
    Unproven,
}

/// The owner a job goes by. `shell_in_front` is a trusted shell answer; `program`
/// identifies a foreground program only when the configured policy trusts the answer.
/// Unknown or untrusted ownership is never inferred from a launch or screen contents.
pub(super) fn job_owner(
    shell_in_front: bool,
    program: Option<ProgramId>,
    agent: AgentProgram,
) -> Owner {
    if shell_in_front {
        return Owner::Shell;
    }
    match (agent, program) {
        (AgentProgram::Session, Some(_)) => Owner::Agent,
        (AgentProgram::Known(known), Some(program)) if program == known => Owner::Agent,
        _ => Owner::Unproven,
    }
}

/// Why a program in front is not taken for the agent, for a trace line.
pub(crate) fn not_the_agent(agent: AgentProgram) -> &'static str {
    match agent {
        AgentProgram::Known(_) => "the agent's program is not in front",
        _ => "a program this terminal did not launch owns it",
    }
}

/// Who owns the terminal, for a job ([`Runner::front`]).
enum Front {
    /// The trusted foreground program is the agent: go on.
    Agent,
    /// The agent relaunched or the startup gate rearmed. Wait for readiness again.
    WaitForReady,
    /// The shell stayed in front: the job ends with the shell fallback's outcome.
    Shell(DeliveryOutcome),
}

/// Why `text` must not be left on the shell's line, if so (`clearable`: a later launch
/// line can kill it first, or none will be typed):
/// - more than one line — never pasted onto a shell line, whatever the terminal's
///   bracketed-paste mode says: the mode can be stale (left on by an agent that crashed),
///   a shell without it runs each line at once (`cmd.exe`, bash before 5.1, dash), and a
///   multi-line buffer needs more than one line kill in zsh and fish, so a later launch
///   line could run its first lines;
/// - not `clearable` (`cmd.exe` with a launch line to type later): the text would run as
///   part of that line.
fn inert_paste_refusal(text: &str, clearable: bool) -> Option<&'static str> {
    if text.contains(['\n', '\r']) {
        Some("the text has more than one line")
    } else if !clearable {
        Some("its line cannot be cleared before the next launch line")
    } else {
        None
    }
}

/// The text the shell fallback leaves on the shell's line: tabs as spaces. Typed into a shell
/// without bracketed paste (bash before 5.1, dash, `cmd.exe`) a tab is the Tab key —
/// completion, a listing prompt; the agent's paste keeps them.
fn shell_line(text: &str) -> String {
    text.replace('\t', " ")
}

/// Starts the job thread; it ends when the terminal is disposed (after resolving every
/// queued job).
pub(crate) fn spawn(ctl: Arc<Controller>) -> Result<JoinHandle<()>> {
    let id = ctl.cfg.id;
    std::thread::Builder::new()
        .name(format!("agent-link-jobs-{id}"))
        .spawn(move || {
            let mut runner = Runner::new(ctl);
            while let Some(job) = runner.next_job() {
                runner.run(job);
            }
        })
        .map_err(|e| Error::Spawn(format!("spawn job thread: {e}")))
}

/// The job thread's context.
pub(crate) struct Runner {
    pub(crate) ctl: Arc<Controller>,
    /// Profile code panicked on this thread once: it is not called here again.
    pub(super) profile_failed: bool,
}

/// Owned rows and the controller/render revisions they were read against.
pub(super) struct Observation {
    pub(super) screen: ScreenSnapshot,
    pub(super) serial: u64,
    revision: u64,
    pub(super) lifecycle: u64,
}

/// A profile result and the current output view at its validation boundary. `None`
/// discards stale or unavailable evidence; `unseen` describes actual monitor backlog.
/// Keeping these separate prevents repeated stale callbacks from extending a deadline.
pub(crate) struct ProfileRead<T> {
    pub(crate) value: Option<T>,
    pub(crate) view: OutputView,
    pub(crate) unseen: bool,
}

impl Runner {
    pub(crate) fn new(ctl: Arc<Controller>) -> Self {
        Self {
            ctl,
            profile_failed: false,
        }
    }

    /// The next job (now the running one); `None` once the terminal is disposed and the
    /// queue is empty.
    fn next_job(&self) -> Option<Job> {
        self.ctl.wait_for(|st| match st.jobs.pop_front() {
            Some(job) => {
                st.running = job.ticket();
                Some(Some(job))
            }
            None if st.disposed => Some(None),
            None => None,
        })
    }

    fn run(&mut self, job: Job) {
        let (ticket, outcome, resolver) = match job {
            Job::Message { mut job, resolver } => {
                let ticket = job.ticket;
                self.trace_start(&job, "start");
                (
                    ticket,
                    self.guarded(ticket, |r| r.process_message(&mut job, None)),
                    resolver,
                )
            }
            Job::ResetAndSend {
                mut reset,
                mut message,
                resolver,
            } => {
                let ticket = reset.ticket;
                let outcome = self.guarded(ticket, |r| {
                    r.process_reset_and_send(&mut reset, &mut message)
                });
                (ticket, outcome, resolver)
            }
            #[cfg(feature = "questions")]
            Job::Answer(job) => {
                self.run_answer(job);
                return;
            }
        };
        self.resolve(ticket, &resolver, outcome);
    }

    /// `[agent-link:<id>] delivery #<n>: <what> (submit …, strict …, … bytes, … characters
    /// dropped)`.
    fn trace_start(&self, job: &MessageJob, what: &str) {
        trace!(
            self,
            job.ticket,
            "{what} (submit {}, strict {}, {} bytes, {} characters dropped)",
            job.opts.submit,
            job.opts.strict,
            job.text.len(),
            job.dropped
        );
    }

    /// Runs a job's work; a panic (a crate bug) ends it `NotConnected` with an `error` line.
    fn guarded(
        &mut self,
        ticket: TicketId,
        work: impl FnOnce(&mut Self) -> Step<DeliveryOutcome>,
    ) -> DeliveryOutcome {
        match catch_panic(|| work(self)) {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(reason)) => DeliveryOutcome::Aborted(reason),
            Err(message) => {
                let id = self.ctl.cfg.id;
                log::error!("[agent-link:{id}] delivery #{ticket} panicked: {message}");
                DeliveryOutcome::Aborted(AbortReason::NotConnected)
            }
        }
    }

    /// The outcome goes out as an event (queued for the monitor, `CtlState::delivery_ended`)
    /// and to the ticket — `Disposed` instead when the terminal was dropped while it ran
    /// (that event went out with the drop).
    fn resolve(&self, ticket: TicketId, resolver: &Resolver, outcome: DeliveryOutcome) {
        let outcome = self.ctl.update(|st, _| st.delivery_ended(ticket, outcome));
        trace!(self, ticket, "{outcome:?}");
        resolver.resolve(outcome);
    }

    /// Port of `processAutoStartJob`: the reset, then the message, once fresh output after
    /// the reset's first Enter settled (see the module docs). The reset not delivered ⇒ its
    /// outcome, nothing more typed. The follow-up keeps the original Enter fence through
    /// host revalidation and the last screen and foreground checks before its paste.
    pub(crate) fn process_reset_and_send(
        &mut self,
        reset: &mut MessageJob,
        message: &mut MessageJob,
    ) -> Step<DeliveryOutcome> {
        let ticket = message.ticket;
        self.trace_start(reset, "reset first");
        let outcome = self.process_message(reset, None)?;
        if outcome != DeliveryOutcome::Delivered {
            trace!(
                self,
                ticket, "the reset was not delivered: the message is not sent"
            );
            return Ok(outcome);
        }
        let MessageIntent::SubmittedReset(baseline) = &reset.intent else {
            return Err(AbortReason::SubmitUnverified);
        };
        self.trace_start(message, "then the message");
        self.process_message(message, Some(baseline))
    }

    /// Port of `processMessageJob` (the steps are in the module docs).
    pub(super) fn process_message(
        &mut self,
        job: &mut MessageJob,
        after_reset: Option<&ResetBaseline>,
    ) -> Step<DeliveryOutcome> {
        let ticket = job.ticket;
        self.cancelled()?;
        if let Some(reason) = job.refusal() {
            match reason {
                AbortReason::EmptyText => trace!(self, ticket, "nothing to paste"),
                _ => trace!(self, ticket, "no reset command"),
            }
            return Err(reason);
        }
        loop {
            // 1. Ready gate.
            trace!(self, ticket, "waiting for the agent");
            if self.wait_agent_ready(ticket)? {
                trace!(self, ticket, "agent ready");
            } else {
                trace!(self, ticket, "no agent running");
            }

            // 2. Agent or shell in front.
            match self.front(job, false)? {
                Front::Agent => {}
                Front::WaitForReady => continue,
                Front::Shell(outcome) => return Ok(outcome),
            }

            // 3. Input lull.
            let start = Instant::now();
            let why = self.wait_lull(
                LullParams {
                    quiet: self.ctl.cfg.timing.pre_paste_quiet,
                    max: self.ctl.cfg.timing.pre_paste_max,
                    after: None,
                    require_fresh: false,
                    start,
                },
                None,
            )?;
            trace!(self, ticket, "input lull ({why:?})");

            // 4. Commit point.
            self.cancelled()?;
            self.revalidate(ticket, &job.opts.revalidate)?;
            if matches!(job.opts.revalidate, Revalidate::Handshake { .. }) {
                // The host's answer took a round trip: output may have started meanwhile.
                // Free when the screen stayed quiet (all output counts as fresh here).
                let why = self.wait_lull(
                    LullParams {
                        quiet: self.ctl.cfg.timing.pre_paste_quiet,
                        max: self.ctl.cfg.timing.pre_paste_max,
                        after: None,
                        require_fresh: false,
                        start: Instant::now(),
                    },
                    None,
                )?;
                trace!(self, ticket, "input lull after the host's answer ({why:?})");
                self.cancelled()?;
            }
            if job.opts.strict {
                self.strict_check(ticket, "pre-paste readiness")?;
            }
            match self.before_paste(job, after_reset)? {
                Front::Agent => {}
                Front::WaitForReady => continue,
                Front::Shell(outcome) => return Ok(outcome),
            }

            // 5. Paste.
            let after_serial = self.paste(&job.text)?;
            trace!(self, ticket, "pasted");
            self.cancelled()?;

            // 6. Submit.
            if job.opts.submit {
                self.wait_paste_landed(after_serial)?;
                let submitted = self.submit_with_verification(job, after_serial);
                if submitted == Err(AbortReason::DialogBlocking) {
                    // The agent can leave its question on screen and its unread paste
                    // on the shell's line. Check the owner after any post-paste match.
                    self.agent_before_enter(
                        ticket,
                        job.opts.strict.then_some("question after paste"),
                    )?;
                }
                submitted?;
            } else if job.text.contains('\n') {
                // An agent that left without reading the paste leaves it to the shell — a
                // multi-line buffer one line kill does not empty (zsh). It leaves only once
                // the input is there: read the owner once the paste settled, not at once.
                self.wait_paste_landed(after_serial)?;
                self.no_shell_holding_the_paste(
                    ticket,
                    "the shell is in front after the paste: its input line interrupted",
                )?;
            }
            return Ok(DeliveryOutcome::Delivered);
        }
    }

    /// At a follow-up paste, the original reset must still authorize the screen after
    /// the final foreground proof. A stale read retries both within the same deadline,
    /// without repeating the host's revalidation.
    fn before_paste(
        &mut self,
        job: &mut MessageJob,
        after_reset: Option<&ResetBaseline>,
    ) -> Step<Front> {
        let ticket = job.ticket;
        let deadline = Instant::now() + self.ctl.cfg.timing.submit_verify_window;
        loop {
            let (dialog, observation) = self.dialog_on_screen(after_reset, deadline)?;
            if let Some(dialog) = dialog {
                trace!(
                    self,
                    ticket, "{:?} dialog on screen at the commit point", dialog.kind
                );
                if self.recheck_gate() {
                    return Ok(Front::WaitForReady);
                }
                return Err(AbortReason::TrustPromptBlocking);
            }
            if self
                .ctl
                .read(|st| st.gate.running() && (!st.gate.ready() || st.launch_held()))
            {
                trace!(self, ticket, "the agent is no longer ready");
                return Ok(Front::WaitForReady);
            }
            let front = self.front(job, true)?;
            if after_reset.is_none()
                || !matches!(front, Front::Agent)
                || observation
                    .is_some_and(|obs| self.ctl.read(|st| self.observation_current(&obs, st)))
            {
                return Ok(front);
            }
            self.retry_screen_check(deadline)?;
        }
    }

    /// Who owns the terminal now ([`owner`](Self::owner)), and what that means for `job`:
    /// the shell in front ⇒ relaunch the agent once ([`Front::WaitForReady`]: start over),
    /// then the shell fallback ([`Front::Shell`] or `Err`); a program other than the
    /// agent's own ⇒ `ForegroundUnproven`. A strict job needs the proof on the same read
    /// (`proof::strict_owner`) — right before the paste (`paste`) also against the shell:
    /// `ShellOnly`, no relaunch there; before the lull a shell the policy trusts keeps the
    /// relaunch rule (an untrusted answer, which a bare shell takes for the shell, proves
    /// nothing: `ForegroundUnproven`).
    fn front(&mut self, job: &mut MessageJob, paste: bool) -> Step<Front> {
        let ticket = job.ticket;
        let (owner, front, agent) = self.owner(ticket)?;
        let trusted_shell = self.ctl.cfg.trusted_owner(front.fg) == Some(ForegroundOwner::Shell);
        if job.opts.strict && (paste || !trusted_shell) {
            let point = if paste { "paste" } else { "the foreground" };
            self.strict_verdict(ticket, point, &front, agent)?;
        }
        match owner {
            Owner::Agent => return Ok(Front::Agent),
            Owner::Unproven => {
                trace!(self, ticket, "{}: not pasted", not_the_agent(agent));
                return Err(AbortReason::ForegroundUnproven);
            }
            Owner::Shell => {}
        }
        if !job.relaunched {
            trace!(self, ticket, "the shell is in front: relaunching the agent");
            self.ctl.relaunch(&front);
            job.relaunched = true;
            return Ok(Front::WaitForReady);
        }
        self.shell_fallback(job, &front).map(Front::Shell)
    }

    /// Who owns the terminal now ([`job_owner`]): read (`Controller::read_front`), handed to
    /// the state, which learns the agent's program from it while one is expected
    /// (`CtlState::observe`), and traced. Also the read and the agent's program it was
    /// judged against.
    pub(crate) fn owner(&self, ticket: TicketId) -> Step<(Owner, FrontRead, AgentProgram)> {
        self.cancelled()?;
        let front = self.ctl.read_front();
        self.cancelled()?;
        trace!(
            self,
            ticket, "foreground {:?} ({:?})", front.fg.owner, front.fg.confidence
        );
        let agent = self.ctl.update(|st, _| {
            st.observe(&front);
            st.program
        });
        let owner = job_owner(front.shell, front.program, agent);
        Ok((owner, front, agent))
    }

    /// The shell owns the terminal by an answer the foreground policy accepts (an exact
    /// one, or a heuristic one under `AcceptHeuristic`) — or, without a profile, by any
    /// answer it cannot tell (`Config::shell_in_front`).
    pub(crate) fn shell_in_front(&self, fg: Foreground) -> bool {
        self.ctl.cfg.shell_in_front(fg)
    }

    /// The shell stayed in front after the relaunch (`front`, read just now; any send, a
    /// paste-only one too): the text is never typed where a key could run it.
    /// `ShellFallback::Fail` ⇒ `ShellOnly`; a strict send ⇒ `ShellOnly`, the agent marked
    /// gone (its paste proof failed: an inert paste is a paste too); else a single line
    /// goes onto a fresh input line ([`fresh_shell_line`](Self::fresh_shell_line)) without
    /// Enter ([`shell_line`]: tabs as spaces) ⇒ `PastedUnsubmitted` (no newline, so nothing
    /// runs; the next launch line kills it first), unless [`inert_paste_refusal`] says no ⇒
    /// `ShellOnly`, nothing typed.
    pub(crate) fn shell_fallback(
        &self,
        job: &MessageJob,
        front: &FrontRead,
    ) -> Step<DeliveryOutcome> {
        let ticket = job.ticket;
        if job.opts.strict {
            trace!(
                self,
                ticket, "strict proof (paste): still the shell: not pasted"
            );
            self.ctl.update(|st, _| st.agent_left());
            return Err(AbortReason::ShellOnly);
        }
        if job.opts.on_shell == ShellFallback::Fail {
            trace!(self, ticket, "still the shell: not pasted (fail)");
            return Err(AbortReason::ShellOnly);
        }
        let cfg = &self.ctl.cfg;
        let clearable = cfg.launch_line.is_none() || cfg.shell.clear_line().is_some();
        if let Some(why) = inert_paste_refusal(&job.text, clearable) {
            trace!(self, ticket, "still the shell, and {why}: not pasted");
            return Err(AbortReason::ShellOnly);
        }
        // Also a paste: the host's check guards it like the commit point's. The host may
        // take its time (a handshake): the write goes by a read made after its answer.
        let after_check;
        let front = if matches!(job.opts.revalidate, Revalidate::None) {
            front
        } else {
            self.revalidate(ticket, &job.opts.revalidate)?;
            after_check = self.still_the_shell(ticket, front)?;
            &after_check
        };
        let text = shell_line(&job.text);
        if self.fresh_shell_line(ticket, front)? {
            trace!(
                self,
                ticket, "still the shell: pasting one line without Enter"
            );
            self.paste(&text)?;
        } else {
            trace!(
                self,
                ticket, "still the shell: typing one line without Enter (no bracketed paste)"
            );
            self.type_plain(&text)?;
        }
        Ok(DeliveryOutcome::PastedUnsubmitted)
    }

    /// The fallback's read after the host's check (`before`: the read it was given): the
    /// shell must still own the terminal (else `ForegroundUnproven`: a program started
    /// meanwhile gets neither a Ctrl-C nor the text) and have got no line from the host
    /// meanwhile (else `ShellOnly`).
    fn still_the_shell(&self, ticket: TicketId, before: &FrontRead) -> Step<FrontRead> {
        let (owner, now, _) = self.owner(ticket)?;
        if owner != Owner::Shell {
            trace!(
                self,
                ticket, "the shell no longer owns the terminal after the host's check: not pasted"
            );
            return Err(AbortReason::ForegroundUnproven);
        }
        if now.shell_lines != before.shell_lines {
            trace!(
                self,
                ticket, "a line was submitted to the shell meanwhile: not pasted"
            );
            return Err(AbortReason::ShellOnly);
        }
        Ok(now)
    }

    /// Readies the shell's input line for the fallback's text (`front`: the shell read in
    /// front just now). A Ctrl-C empties it — a draft, an earlier fallback's text (they
    /// would join on one line) — and starts a fresh one, which a line editor in vi mode
    /// starts in insert mode: in command mode the text's letters are vi commands (bash: `#`
    /// accepts the line, `v` opens `$EDITOR` and runs what it leaves). Sent only where it
    /// reaches the shell (a trusted `Shell` answer) and once the crate typed into the
    /// session (before, the shell may still be reading its startup files, which a SIGINT
    /// cuts short); then the shell must take it, still own the terminal, and have got no
    /// line from the host meanwhile — else nothing is typed (`ForegroundUnproven`,
    /// `ShellOnly`).
    ///
    /// `true` when the shell turned bracketed paste on after that Ctrl-C (bash 5.1+ and zsh
    /// do at every new line): the text goes in as a bracketed paste. Otherwise it is typed
    /// as is — a shell that does not take a bracketed paste (bash 3.2, dash; the terminal's
    /// mode may be on all the same, left by a program that crashed) reads the start
    /// marker's ESC as a key, in vi mode the switch to command mode.
    fn fresh_shell_line(&self, ticket: TicketId, front: &FrontRead) -> Step<bool> {
        let trusted_shell = self.ctl.cfg.trusted_owner(front.fg) == Some(ForegroundOwner::Shell);
        if !trusted_shell || !self.ctl.read(|st| st.typed()) {
            return Ok(false);
        }
        trace!(self, ticket, "still the shell: interrupting its input line");
        let written = self.ctl.interrupt();
        if let Ok(after) = written {
            // A launch line typed meanwhile waits for it instead of interrupting again.
            self.ctl.update(|st, _| st.note_interrupt(after));
        }
        let after = written.map_err(|e| self.write_failed(e))?;
        self.wait_interrupt_taken(after)?;
        let now = self.ctl.read_front();
        self.cancelled()?;
        if !now.shell {
            trace!(
                self,
                ticket, "the shell no longer owns the terminal after its Ctrl-C: not pasted"
            );
            return Err(AbortReason::ForegroundUnproven);
        }
        if now.shell_lines != front.shell_lines {
            trace!(
                self,
                ticket, "a line was submitted to the shell meanwhile: not pasted"
            );
            return Err(AbortReason::ShellOnly);
        }
        Ok(self.ctl.term().paste_on_at() > after.fence)
    }

    /// Until the shell took the Ctrl-C `after` (`settle::interrupt_due`: its answer, then a
    /// short quiet, or the cap), re-reading the due time at every update (output moves it).
    fn wait_interrupt_taken(&self, after: Interrupt) -> Step<()> {
        let due = |st: &super::state::CtlState| interrupt_due(&st.view, after.fence, after.at);
        loop {
            let at = self
                .ctl
                .read(|st| st.cancel_reason().map_or(Ok(due(st)), Err))?;
            if Instant::now() >= at {
                return Ok(());
            }
            self.ctl.wait_until(at, |st| {
                (st.cancel_reason().is_some() || due(st) != at).then_some(())
            });
        }
    }

    /// `Err` when the terminal was dropped or the session ended.
    pub(crate) fn cancelled(&self) -> Step<()> {
        match self.ctl.read(|st| st.cancel_reason()) {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// `whenAgentReady`: until the agent is ready (`true`), or no agent runs (`false`) —
    /// not while its launch line is held back (the agent is not started yet). While the
    /// agent could have left unseen (`CtlState::could_have_left`) the foreground is read
    /// every [`LEFT_POLL`]: the shell in front (a trusted answer) ⇒ it left, marked gone
    /// (`false`). Nothing else would end the wait for an agent that exited from a dialog
    /// left to the user: the dialog stays on screen and keeps the gate parked.
    fn wait_agent_ready(&self, ticket: TicketId) -> Step<bool> {
        loop {
            let waited = self.ctl.wait_until(Instant::now() + LEFT_POLL, |st| {
                if let Some(reason) = st.cancel_reason() {
                    return Some(Err(reason));
                }
                if !st.gate.running() {
                    return Some(Ok(false));
                }
                (st.gate.ready() && !st.launch_held()).then_some(Ok(true))
            });
            if let Some(ready) = waited {
                return ready;
            }
            // Every Windows answer is a heuristic one: under `ExactOnly` none would count, and
            // each read is a process snapshot.
            let policy = self.ctl.cfg.foreground_policy;
            let untrusted = cfg!(windows) && policy == ForegroundPolicy::ExactOnly;
            if untrusted || !self.ctl.read(|st| st.could_have_left()) {
                continue;
            }
            let front = self.ctl.read_front();
            let left = self.ctl.update(|st, _| {
                st.observe(&front);
                let left = front.shell && st.could_have_left();
                if left {
                    st.agent_left();
                }
                left
            });
            if left {
                trace!(
                    self,
                    ticket, "the shell is in front: the agent left before it was ready"
                );
            }
        }
    }

    /// One bracketed-paste write (`ESC[200~` text `ESC[201~`, no newline); returns the
    /// chunks already fed immediately before it is queued. Sample before enqueueing so
    /// a fast echo cannot be included in the fence. Later output may still be unrelated;
    /// the submit path also requires recognising this text in the composer.
    pub(crate) fn paste(&self, text: &str) -> Step<u64> {
        let mut bytes = Vec::with_capacity(text.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        // It may end up on the shell's line: later launch lines interrupt it first.
        self.ctl.update(|st, _| st.note_paste());
        let before = self.ctl.term().output_chunks();
        self.write(&bytes)?;
        Ok(before)
    }

    /// `text` typed as is, without bracketed-paste markers — the shell fallback's single
    /// line where the shell does not take a bracketed paste
    /// ([`fresh_shell_line`](Self::fresh_shell_line)).
    fn type_plain(&self, text: &str) -> Step<()> {
        self.ctl.update(|st, _| st.note_paste());
        self.write(text.as_bytes())
    }

    /// A write; a failure is `WriteFailed` unless the delivery was cancelled meanwhile (a
    /// drop racing the teardown: `Disposed`).
    pub(crate) fn write(&self, bytes: &[u8]) -> Step<()> {
        self.cancelled()?;
        self.ctl
            .term()
            .write(bytes)
            .map_err(|e| self.write_failed(e))
    }

    /// The reason for a failed write (logged at `debug`).
    pub(crate) fn write_failed(&self, e: Error) -> AbortReason {
        let id = self.ctl.cfg.id;
        log::debug!("[agent-link:{id}] delivery write failed: {e}");
        self.ctl
            .read(|st| st.cancel_reason())
            .unwrap_or(AbortReason::WriteFailed)
    }

    /// The chunks fed and the time now: output after this point is fresh.
    pub(crate) fn fence(&self) -> Fence {
        Fence {
            serial: self.ctl.term().output_chunks(),
            at: Instant::now(),
        }
    }

    /// Waits for a lull per `params` (`settle::lull_ready`), reading the screen at each
    /// look.
    pub(super) fn wait_lull(
        &mut self,
        params: LullParams,
        reset: Option<&ResetBaseline>,
    ) -> Step<settle::Lull> {
        loop {
            let read = self.profile_read(reset, |profile, screen, leftover| {
                profile.classify(leftover.fresh(screen))
            })?;
            let view = if read.unseen {
                read.view
                    .with_unseen_output(self.ctl.term().output_chunks(), Instant::now())
            } else {
                read.view
            };
            let state = read.value.unwrap_or(ScreenState::Unknown);
            match settle::lull_ready(&view, state, Instant::now(), &params) {
                Settle::Done(why) => return Ok(why),
                Settle::Wait(at) => self.ctl.sleep_until(at)?,
            }
        }
    }

    /// Waits until the paste after `after_serial` (the fed count) landed
    /// (`settle::paste_landed`); no verdict while the reader is ahead of the view.
    fn wait_paste_landed(&self, after_serial: u64) -> Step<()> {
        let timing = &self.ctl.cfg.timing;
        let start = Instant::now();
        loop {
            let fed = self.ctl.term().output_chunks();
            let view = self.ctl.read(|st| st.view);
            let verdict = settle::paste_landed(
                &view,
                view.unseen(fed),
                Instant::now(),
                after_serial,
                timing.paste_echo_quiet,
                timing.paste_echo_max,
                start,
            );
            match verdict {
                Settle::Done(()) => return Ok(()),
                Settle::Wait(at) => self.ctl.sleep_until(at)?,
            }
        }
    }

    pub(super) fn observation(&self) -> Option<Observation> {
        let lifecycle = self.ctl.read(|st| st.lifecycle_revision());
        let (screen, serial, revision) = self.ctl.term().screen_observation()?;
        Some(Observation {
            screen,
            serial,
            revision,
            lifecycle,
        })
    }

    pub(super) fn observation_current(&self, obs: &Observation, st: &CtlState) -> bool {
        st.lifecycle_revision() == obs.lifecycle
            && st.view.output_serial == obs.serial
            && self
                .ctl
                .term()
                .screen_observation_current(obs.serial, obs.revision)
    }

    /// Evaluate callbacks outside the controller lock, then validate the render and
    /// lifecycle again. An explicit reset only admits observations after its first Enter.
    pub(super) fn profile_read<T>(
        &mut self,
        after_reset: Option<&ResetBaseline>,
        call: impl FnOnce(&dyn AgentProfile, &str, &Leftover) -> T,
    ) -> Step<ProfileRead<T>> {
        let observation = self.observation();
        self.read_observation(observation.as_ref(), after_reset, call)
    }

    pub(super) fn read_observation<T>(
        &mut self,
        observation: Option<&Observation>,
        after_reset: Option<&ResetBaseline>,
        call: impl FnOnce(&dyn AgentProfile, &str, &Leftover) -> T,
    ) -> Step<ProfileRead<T>> {
        self.cancelled()?;
        let value = observation.and_then(|obs| {
            let leftover = self.ctl.read(|st| {
                (self.observation_current(obs, st)
                    && after_reset.is_none_or(|reset| {
                        reset.lifecycle == obs.lifecycle && obs.serial > reset.fence.serial
                    }))
                .then(|| st.leftover().clone())
            })?;
            let text = obs.screen.text();
            self.profile_call(|profile| {
                if let Some(reset) = after_reset {
                    reset.check(profile, &text)?;
                }
                Ok(call(profile, &text, &leftover))
            })
        });
        self.ctl.read(|st| {
            if let Some(reason) = st.cancel_reason() {
                return Err(reason);
            }
            if after_reset.is_some_and(|reset| reset.lifecycle != st.lifecycle_revision()) {
                return Err(AbortReason::SubmitUnverified);
            }
            let fed = self.ctl.term().output_chunks();
            let current = observation.is_some_and(|obs| self.observation_current(obs, st));
            Ok(ProfileRead {
                value: value.filter(|_| current).transpose()?,
                view: st.view,
                unseen: st.view.unseen(fed),
            })
        })
    }

    /// Stop on a question or return the startup dialog from this observation. Reobserve
    /// stale or unavailable results within the verification window.
    pub(super) fn dialog_on_screen(
        &mut self,
        after_reset: Option<&ResetBaseline>,
        deadline: Instant,
    ) -> Step<(Option<Dialog>, Option<Observation>)> {
        if self.ctl.cfg.profile.is_none() {
            self.cancelled()?;
            return Ok((None, None));
        }
        loop {
            let observation = self.observation();
            let read = self.read_observation(
                observation.as_ref(),
                after_reset,
                |profile, screen, leftover| {
                    let fresh = leftover.fresh(screen);
                    check_question(profile, fresh)?;
                    Ok(profile.dialog(fresh))
                },
            )?;
            if let Some(dialog) = read.value.transpose()? {
                return Ok((dialog, observation));
            }
            self.retry_screen_check(deadline)?;
        }
    }

    pub(super) fn retry_screen_check(&self, deadline: Instant) -> Step<()> {
        let now = Instant::now();
        if now >= deadline {
            return Err(AbortReason::SubmitUnverified);
        }
        let poll = self
            .ctl
            .cfg
            .timing
            .submit_verify_poll
            .max(Duration::from_millis(1));
        self.ctl.sleep_until((now + poll).min(deadline))
    }

    /// Hands the current screen to the startup gate (it may un-ready and steer a dialog
    /// inside its post-ready watch); `true` when it is no longer ready and the job should
    /// wait for it again.
    pub(crate) fn recheck_gate(&self) -> bool {
        self.ctl.evaluate_gate(false);
        self.ctl
            .update(|st, _| st.gate.running() && !st.gate.ready())
    }

    /// Calls host profile code: `None` without a profile; a panic is logged once (`warn`)
    /// and the profile is not called from this thread again.
    fn profile_call<T>(&mut self, call: impl FnOnce(&dyn AgentProfile) -> T) -> Option<T> {
        if self.profile_failed {
            return None;
        }
        let profile = self.ctl.cfg.profile.as_deref()?;
        match catch_panic(|| call(profile)) {
            Ok(value) => Some(value),
            Err(_) => {
                self.profile_failed = true;
                let id = self.ctl.cfg.id;
                let profile = &self.ctl.cfg.profile_id;
                log::warn!(
                    "[agent-link:{id}] profile {profile} panicked during delivery. Further delivery screen checks are unavailable"
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_text_keeps_lines_and_tabs_and_drops_other_controls() {
        assert_eq!(paste_text("plain text"), ("plain text".into(), 0));
        assert_eq!(paste_text("a\n\tb"), ("a\n\tb".into(), 0));
        // CR LF and a lone CR become LF (not counted as dropped).
        assert_eq!(paste_text("a\r\nb\rc"), ("a\nb\nc".into(), 0));
        // An embedded end marker can no longer end the paste early.
        assert_eq!(
            paste_text("hello\u{1b}[201~\rworld\u{1b}[200~"),
            ("hello\nworld".into(), 12)
        );
        // ESC goes too, so no marker survives a split one; ^C, ^D, ^U, DEL and C1 go.
        assert_eq!(
            paste_text("a\u{1b}[20\u{1b}[201~1~b"),
            ("a[201~b".into(), 7)
        );
        assert_eq!(
            paste_text("x\u{3}\u{4}\u{15}\u{7f}\u{9b}y\u{0}"),
            ("xy".into(), 6)
        );
        assert_eq!(paste_text("ünï ❯"), ("ünï ❯".into(), 0));
    }

    /// Only a single line is left on a shell's line, and only where it can be cleared.
    #[test]
    fn the_shell_gets_one_clearable_line_at_most() {
        assert_eq!(inert_paste_refusal("echo hi", true), None);
        assert_eq!(inert_paste_refusal("", true), None);
        for text in ["a\nb", "a\n", "a\rb"] {
            assert!(inert_paste_refusal(text, true).is_some(), "{text:?}");
        }
        assert!(inert_paste_refusal("echo hi", false).is_some());
    }

    /// Strict and the handshake are accepted; refused are a text with nothing to paste and
    /// a job made refused (a reset without a reset command).
    #[test]
    fn nothing_to_paste_and_a_missing_reset_command_are_refused() {
        let refusal = |text: &str, opts| MessageJob::new(TicketId(1), text, opts).refusal();
        assert_eq!(refusal("hi", SendOptions::default()), None);
        let strict = SendOptions {
            strict: true,
            revalidate: crate::agent::Revalidate::Handshake {
                timeout: std::time::Duration::from_secs(1),
            },
            ..SendOptions::default()
        };
        assert_eq!(refusal("hi", strict), None);
        // Empty once sanitised: nothing, controls only, whitespace only.
        for text in ["", "\u{3}\u{1b}[201~\u{7f}", " \n\t\r\n "] {
            assert_eq!(
                refusal(text, SendOptions::default()),
                Some(AbortReason::EmptyText),
                "{text:?}"
            );
        }
        let refused = MessageJob::refused(TicketId(2), AbortReason::Unsupported);
        assert_eq!(refused.refusal(), Some(AbortReason::Unsupported));
        assert_eq!(refused.ticket, TicketId(2));
    }

    /// Only a trusted, named foreground program can be the agent, even for direct
    /// sessions. An armed launch never turns an unknown owner into permission to type.
    #[test]
    fn only_the_agent_s_own_program_is_the_agent() {
        use AgentProgram::{Known, Session, Unknown};
        let (agent, other) = (ProgramId(4300), ProgramId(4400));
        let expected = AgentProgram::Expected { shell_lines: 0 };
        for state in [Unknown, expected, Known(agent), Session] {
            for program in [None, Some(agent), Some(other)] {
                assert_eq!(job_owner(true, program, state), Owner::Shell);
            }
            assert_eq!(job_owner(false, None, state), Owner::Unproven);
        }
        assert_eq!(job_owner(false, Some(agent), Known(agent)), Owner::Agent);
        assert_eq!(job_owner(false, Some(other), Session), Owner::Agent);
        for state in [Unknown, expected, Known(agent)] {
            assert_eq!(job_owner(false, Some(other), state), Owner::Unproven);
        }
    }

    /// The shell fallback's paste: tabs would act as Tab keys where bracketed paste is off.
    #[test]
    fn the_shell_line_has_no_tabs() {
        assert_eq!(shell_line("echo a\tb\t\tc"), "echo a b  c");
        assert_eq!(shell_line("plain"), "plain");
    }
}
