//! Verified Enter — a port of the panel's `submitWithVerification`.
//!
//! Even a separate, delayed Enter can be swallowed: claude aggregates stdin for ~100 ms
//! after a paste and absorbs an Enter landing inside that window into the paste, leaving
//! the message entered but not sent while a fire-and-forget delivery reported success. So
//! the pasted text must first be visible in a readable composer. After Enter, fresh
//! output and a stable readable composer from the same launch must show that the text left it. Continuous
//! output, silence and an unreadable composer are never receipts. A composer still
//! holding this message permits a bounded Enter retry only when the host opts in; the
//! default sends Enter once. A stale composer may outlive an accepted submit, so opt-in
//! retries can submit twice and do not provide exactly-once delivery. Uncertainty ends
//! `SubmitUnverified` without another Enter. A profile panic therefore cannot silently
//! confer receipt. A recognized question stops the next Enter. The caller checks
//! ownership again before reporting `DialogBlocking` because the shell may now hold
//! the paste. A startup dialog goes back to the gate.
//!
//! Never into the shell either: the foreground is read right before every Enter (the
//! same rule as before the paste). The agent can leave in the quarter second or more
//! between the paste and its Enter (`/exit`, a crash) without reading the paste, which then
//! sits on the shell's input line — an Enter would run it. The shell in front ⇒ no Enter,
//! the shell's line interrupted (Ctrl-C), the agent marked gone and `ShellOnly`. Nor into
//! another program: one that is not the agent's own in front ⇒ no Enter (nothing else
//! typed either), `ForegroundUnproven`. A strict delivery also needs its proof on that read
//! (`proof::strict_owner`: an owner the policy cannot name gets no Enter either), and once
//! more after the screen showed the submit took — the agent may have exited while that
//! evidence settled, and a receipt is never claimed for it.
//!
//! **Unreadable composers (opt-in).** A profile that cannot read its composer
//! (`pending_input` is `None` on every current screen of the paste check) gets no Enter
//! by default. With `UnreadableComposer::EnterOnce`, and only after that whole window
//! ([`Readability`]: one readable screen keeps the verified path; stale reads and a
//! failed profile prove nothing), the paste's fresh output must go quiet
//! (`settle::Landed::Echoed`). The final screen must still have an unreadable composer
//! and pass the dialog and question checks. After checking ownership, delivery revalidates
//! that screen, its quiet interval and the paste's agent lifecycle before queueing Enter.
//! A host relaunch during verification withholds Enter. An unverified Enter ends `UnverifiedEnter`,
//! never `Delivered`. An explicit reset takes its baseline from a guarded screen right
//! before that Enter, still checks for a refusal while fresh output settles, and lets a
//! follow-up proceed only once fresh output after the Enter settled.

use std::time::Instant;

use super::controller::{
    MessageIntent, MessageJob, Observation, Owner, PasteFence, Runner, Step, check_question,
    not_the_agent, trace,
};
use super::leftover::Leftover;
use super::settle::{Fence, Landed, Lull, LullParams, Settle, Submit, SubmitCheck, VerifyTiming};
use super::state::CtlState;
use super::{AbortReason, AgentProfile, TicketId, UnreadableComposer};

/// What the paste check found before the first Enter.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Composer {
    /// A readable composer holds the pasted text: the verified path.
    Visible,
    /// No current screen of the window had a composer the profile could read, and the
    /// job opted into one unverified Enter.
    Unreadable,
}

/// The profile's current composer readings while deciding whether to write Enter.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct Readability {
    /// A current screen read `None`: no composer the profile can read.
    unreadable: bool,
    /// A current screen read `Some(_)`: the profile reads this composer.
    readable: bool,
}

impl Readability {
    /// `pending`: the profile's answer for a current screen, or `None` for a stale or
    /// unavailable read or a failed profile — no answer either way.
    fn note(&mut self, pending: Option<Option<bool>>) {
        match pending {
            Some(Some(_)) => self.readable = true,
            Some(None) => self.unreadable = true,
            None => {}
        }
    }

    /// The profile answered, and never read a composer.
    fn never_readable(self) -> bool {
        self.unreadable && !self.readable
    }
}

/// The paste-check window ended with `reason`: only its own deadline
/// (`retry_screen_check`'s `SubmitUnverified`) after nothing but unreadable answers
/// permits the job's opted-in unverified Enter.
fn enter_once_at_deadline(job: &MessageJob, reason: AbortReason, seen: Readability) -> bool {
    reason == AbortReason::SubmitUnverified
        && job.opts.unreadable_composer == UnreadableComposer::EnterOnce
        && seen.never_readable()
}

/// The screen and agent that received the explicit reset's single Enter.
pub(super) struct ResetBaseline {
    rows: Leftover,
    pub(super) lifecycle: u64,
    pub(super) fence: Fence,
}

impl ResetBaseline {
    pub(super) fn check(&self, profile: &dyn AgentProfile, screen: &str) -> Step<()> {
        if profile.reset_rejected(self.rows.fresh(screen)) {
            // A header redraw or reflow can expose an old notice in the suffix.
            if profile.reset_rejected(&self.rows.text()) {
                return Err(AbortReason::SubmitUnverified);
            }
            return Err(AbortReason::ResetRejected);
        }
        Ok(())
    }
}

impl Runner {
    /// Write a separate Enter and verify it. An explicit reset keeps its first-Enter
    /// evidence through verification, settling and any follow-up commit point.
    pub(super) fn submit_with_verification(
        &mut self,
        job: &mut MessageJob,
        paste_fence: PasteFence,
    ) -> Step<()> {
        let ticket = job.ticket;
        // A reset can keep its old composer while it is already executing. Repeating
        // it could clear the new conversation, so a reset is never retried.
        if matches!(job.intent, MessageIntent::Reset(_)) {
            let Some(reset) = self.write_reset_enter(job, paste_fence, Composer::Visible)? else {
                return self.unverified_reset(job, paste_fence);
            };
            let verdict = self.wait_submit_outcome(
                &job.text,
                reset.fence.serial,
                paste_fence.lifecycle,
                Some(&reset),
            )?;
            if verdict != Submit::Submitted {
                return Err(AbortReason::SubmitUnverified);
            }
            self.settle_reset(ticket, &reset)?;
            job.intent = MessageIntent::SubmittedReset(reset);
            return Ok(());
        }
        let retries = if self.ctl.cfg.reset_command == Some(job.text.as_str()) {
            0
        } else {
            self.ctl.cfg.timing.submit_retry_max
        };
        self.no_dialog_before_enter(ticket)?;
        if self.wait_visible_paste(job, paste_fence)? == Composer::Unreadable {
            return self.unverified_enter(job, paste_fence);
        }
        self.no_dialog_before_enter(ticket)?;
        self.agent_before_enter(
            ticket,
            paste_fence,
            job.opts.strict.then_some("initial submit"),
        )?;
        let mut enter_fence = self.fence().serial;
        self.write_after_paste_if(b"\r", paste_fence, |_| true)?;
        self.cancelled()?;
        let mut attempt = 0;
        loop {
            let verdict =
                self.wait_submit_outcome(&job.text, enter_fence, paste_fence.lifecycle, None)?;
            if job.opts.strict {
                self.strict_check(ticket, "submit verification", Some(paste_fence.lifecycle))?;
                if self.ctl.read(|st| st.lifecycle_revision()) != paste_fence.lifecycle {
                    return Err(AbortReason::SubmitUnverified);
                }
            }
            match verdict {
                Submit::Submitted => {
                    trace!(self, ticket, "submit verified after {attempt} retries");
                    return Ok(());
                }
                Submit::Unverified => {
                    trace!(
                        self,
                        ticket, "no stable readable composer proves the submit; no retry"
                    );
                    return Err(AbortReason::SubmitUnverified);
                }
                Submit::Pending if attempt >= retries => {
                    trace!(
                        self,
                        ticket, "the text is still in the composer after {attempt} retries"
                    );
                    return Err(AbortReason::SubmitUnverified);
                }
                Submit::Pending => {}
            }
            self.no_dialog_before_enter(ticket)?;
            self.agent_before_enter(
                ticket,
                paste_fence,
                job.opts.strict.then_some("submit retry"),
            )?;
            trace!(
                self,
                ticket, "the text is still in the composer: Enter again"
            );
            enter_fence = self.ctl.term().output_chunks();
            self.write_after_paste_if(b"\r", paste_fence, |_| true)?;
            self.cancelled()?;
            attempt += 1;
        }
    }

    /// A launch advances the lifecycle before queueing its keys. Holding the terminal
    /// writer lock across this check and enqueue orders Enter and shell cleanup with
    /// those keys. The controller lock is released before enqueueing.
    fn write_after_paste_if(
        &self,
        bytes: &[u8],
        paste: PasteFence,
        ready: impl FnOnce(&CtlState) -> bool,
    ) -> Step<bool> {
        let mut check = Ok(());
        let queued = self
            .ctl
            .term()
            .write_if(bytes, || {
                self.ctl.read(|st| {
                    check = st.check_delivery_lifecycle(Some(paste.lifecycle));
                    check.is_ok() && ready(st)
                })
            })
            .map_err(|e| self.write_failed(e))?;
        check.map(|()| queued)
    }

    /// Use the same owned rows for the visible-paste and dialog checks and for the
    /// reset baseline. Revalidate after the foreground read, immediately before queuing
    /// Enter. The screen observation and write are still separate operations.
    ///
    /// [`Composer::Visible`]: Enter once a readable composer holds the command. `Ok(None)`:
    /// the window ended without the profile reading a composer on any current screen and
    /// the job opted into one unverified Enter. [`Composer::Unreadable`] (that opt-in):
    /// Enter once the same guards pass on a screen whose composer the profile still cannot
    /// read; never `Ok(None)`.
    fn write_reset_enter(
        &mut self,
        job: &MessageJob,
        paste_fence: PasteFence,
        composer: Composer,
    ) -> Step<Option<ResetBaseline>> {
        let deadline = Instant::now() + self.ctl.cfg.timing.submit_verify_window;
        let mut seen = Readability::default();
        loop {
            if let Some((obs, fence)) =
                self.try_write_enter(job, paste_fence, composer, &mut seen)?
            {
                #[cfg(feature = "questions")]
                self.ctl.update(|st, _| st.invalidate_questions());
                return Ok(Some(ResetBaseline {
                    rows: Leftover::new(obs.screen.rows),
                    lifecycle: obs.lifecycle,
                    fence,
                }));
            }
            if let Err(reason) = self.retry_screen_check(deadline) {
                if composer == Composer::Visible && enter_once_at_deadline(job, reason, seen) {
                    trace!(
                        self,
                        job.ticket, "no readable composer during the reset check"
                    );
                    return Ok(None);
                }
                return Err(reason);
            }
        }
    }

    /// Check the question, dialog and composer on one observation. Write Enter only
    /// while that observation is still current after the foreground check.
    fn try_write_enter(
        &mut self,
        job: &MessageJob,
        paste_fence: PasteFence,
        composer: Composer,
        seen: &mut Readability,
    ) -> Step<Option<(Observation, Fence)>> {
        let observation = self.observation();
        let read =
            self.read_observation(observation.as_ref(), None, |profile, screen, launch| {
                let fresh = launch.fresh(screen);
                check_question(profile, fresh)?;
                let dialog = profile.dialog(fresh);
                let pending = dialog
                    .is_none()
                    .then(|| profile.pending_input(screen, &job.text));
                Ok((dialog, pending))
            })?;
        let guard = read.value.transpose()?;
        if let Some((Some(dialog), _)) = guard {
            trace!(
                self,
                job.ticket, "{:?} dialog on screen before the Enter", dialog.kind
            );
            self.recheck_gate();
            return Err(AbortReason::TrustPromptBlocking);
        }
        let pending = guard.and_then(|(_, pending)| pending);
        seen.note(pending);
        self.agent_before_enter(
            job.ticket,
            paste_fence,
            job.opts.strict.then_some("unverified submit"),
        )?;
        if composer == Composer::Unreadable && seen.readable {
            return Err(AbortReason::SubmitUnverified);
        }
        let wanted = match composer {
            Composer::Visible => Some(true),
            Composer::Unreadable => None,
        };
        if let Some(obs) = observation {
            let fence = self.fence();
            if self.write_after_paste_if(b"\r", paste_fence, |st| {
                pending == Some(wanted)
                    && fence.serial > paste_fence.serial
                    && fence.serial == obs.serial
                    && self.observation_current(&obs, st)
                    && (composer == Composer::Visible
                        || st.view.last_output_at.is_some_and(|last| {
                            fence.at.saturating_duration_since(last)
                                >= self.ctl.cfg.timing.paste_echo_quiet
                        }))
            })? {
                return Ok(Some((obs, fence)));
            }
        }
        Ok(None)
    }

    /// [`UnreadableComposer::EnterOnce`] for an explicit reset whose command the profile
    /// could not read: the paste's output must go quiet, then one Enter on a guarded
    /// screen ([`write_reset_enter`](Self::write_reset_enter)). The refusal check and the
    /// settle still run (a refusal ends `ResetRejected`). Only fresh output that settled
    /// after the Enter keeps the baseline a compound job's follow-up needs; without it
    /// the follow-up is withheld. Ends `UnverifiedEnter` once the Enter was written.
    fn unverified_reset(&mut self, job: &mut MessageJob, paste_fence: PasteFence) -> Step<()> {
        let ticket = job.ticket;
        self.paste_went_quiet(ticket, paste_fence.serial)?;
        let reset = self
            .write_reset_enter(job, paste_fence, Composer::Unreadable)?
            .ok_or(AbortReason::SubmitUnverified)?;
        trace!(
            self,
            ticket, "no readable composer: one unverified Enter for the reset, never retried"
        );
        match self.settle_reset(ticket, &reset) {
            Ok(why) if why != Lull::Max => job.intent = MessageIntent::SubmittedReset(reset),
            Ok(_) | Err(AbortReason::SubmitUnverified) => trace!(
                self,
                ticket, "no fresh output settled after the unverified reset"
            ),
            Err(reason) => return Err(reason),
        }
        Err(AbortReason::UnverifiedEnter)
    }

    /// An ordinary send with [`UnreadableComposer::EnterOnce`] needs an unreadable
    /// composer after the paste goes quiet. Retry stale observations within the
    /// verification window. A current readable composer rules out the unverified Enter.
    fn unverified_enter(&mut self, job: &MessageJob, paste_fence: PasteFence) -> Step<()> {
        self.paste_went_quiet(job.ticket, paste_fence.serial)?;
        let deadline = Instant::now() + self.ctl.cfg.timing.submit_verify_window;
        let mut seen = Readability::default();
        loop {
            if self
                .try_write_enter(job, paste_fence, Composer::Unreadable, &mut seen)?
                .is_some()
            {
                trace!(
                    self,
                    job.ticket, "no readable composer: one unverified Enter, never retried"
                );
                return Err(AbortReason::UnverifiedEnter);
            }
            self.retry_screen_check(deadline)?;
        }
    }

    /// The opt-in's only sign that the agent took the paste: counted output after the
    /// paste fence that then went quiet (`settle::paste_landed`, a fresh wait). No such
    /// output, output that keeps coming or a count that keeps lagging ⇒
    /// `SubmitUnverified`, no Enter.
    fn paste_went_quiet(&self, ticket: TicketId, paste_fence: u64) -> Step<()> {
        match self.wait_paste_landed(paste_fence)? {
            Landed::Echoed => Ok(()),
            Landed::Max => {
                trace!(
                    self,
                    ticket,
                    "no readable composer and no output after the paste that went quiet: no Enter"
                );
                Err(AbortReason::SubmitUnverified)
            }
        }
    }

    /// Settle fresh output after the reset's Enter, checking for a refusal throughout.
    /// Returns why the settle ended: [`Lull::Max`] means no fresh output settled.
    fn settle_reset(&mut self, ticket: TicketId, reset: &ResetBaseline) -> Step<Lull> {
        // The receipt just checked the refusal and the composer on one observation.
        let timing = &self.ctl.cfg.timing;
        let why = self.wait_lull(
            LullParams {
                quiet: timing.startup_quiet,
                max: timing.startup_max,
                after: Some(reset.fence),
                require_fresh: true,
                start: Instant::now(),
            },
            Some(reset),
        )?;
        trace!(self, ticket, "fresh output after the reset ({why:?})");
        let deadline = Instant::now() + self.ctl.cfg.timing.submit_verify_window;
        loop {
            if self
                .profile_read(Some(reset), |_, _, _| ())?
                .value
                .is_some()
            {
                return Ok(why);
            }
            self.retry_screen_check(deadline)?;
        }
    }

    /// Do not submit an unobserved paste: unrelated output over an unchanged empty
    /// composer otherwise looks like a receipt. Wait a bounded interval for the monitor
    /// to catch up and the profile to recognise this message before the first Enter.
    /// When the profile read no composer on any current screen of that interval
    /// ([`Readability`]) and the job opted into one unverified Enter, the interval ends
    /// [`Composer::Unreadable`]; otherwise it ends `SubmitUnverified`.
    fn wait_visible_paste(&mut self, job: &MessageJob, paste_fence: PasteFence) -> Step<Composer> {
        let deadline = Instant::now() + self.ctl.cfg.timing.submit_verify_window;
        let mut seen = Readability::default();
        loop {
            self.agent_before_enter(
                job.ticket,
                paste_fence,
                job.opts.strict.then_some("paste verification"),
            )?;
            let read = self.profile_read(None, |profile, screen, leftover| {
                check_question(profile, leftover.fresh(screen))?;
                Ok(profile.pending_input(screen, &job.text))
            })?;
            let pending = read.value.transpose()?;
            if pending == Some(Some(true)) && read.view.output_serial > paste_fence.serial {
                return Ok(Composer::Visible);
            }
            seen.note(pending);
            if let Err(reason) = self.retry_screen_check(deadline) {
                if enter_once_at_deadline(job, reason, seen) {
                    trace!(
                        self,
                        job.ticket, "no readable composer during the paste check"
                    );
                    return Ok(Composer::Unreadable);
                }
                return Err(reason);
            }
        }
    }

    /// Stop before Enter on a recognized question. Only a startup dialog rechecks the gate.
    fn no_dialog_before_enter(&mut self, ticket: TicketId) -> Step<()> {
        let deadline = Instant::now() + self.ctl.cfg.timing.submit_verify_window;
        let (Some(dialog), _) = self.dialog_on_screen(None, deadline)? else {
            return Ok(());
        };
        trace!(
            self,
            ticket, "{:?} dialog on screen before the Enter", dialog.kind
        );
        self.recheck_gate();
        Err(AbortReason::TrustPromptBlocking)
    }

    /// Only the agent's own program gets the Enter (`Runner::owner`): the shell in front ⇒
    /// its line interrupted, `ShellOnly`
    /// ([`interrupt_the_shell`](Self::interrupt_the_shell)); another program ⇒ no Enter,
    /// `ForegroundUnproven`. `strict` (the proof point's name): the strict proof on the
    /// same read too.
    pub(super) fn agent_before_enter(
        &self,
        ticket: TicketId,
        paste: PasteFence,
        strict: Option<&str>,
    ) -> Step<()> {
        let lifecycle = Some(paste.lifecycle);
        let (owner, front, agent) = self.owner(ticket, lifecycle)?;
        match owner {
            Owner::Agent => match strict {
                Some(point) => self.strict_verdict(ticket, point, &front, agent, lifecycle),
                None => Ok(()),
            },
            Owner::Shell => self.interrupt_the_shell(
                ticket,
                paste,
                "the shell is in front before the Enter: no Enter, its input line interrupted",
            ),
            Owner::Unproven => {
                trace!(
                    self,
                    ticket,
                    "{} before the Enter: no Enter",
                    not_the_agent(agent)
                );
                Err(AbortReason::ForegroundUnproven)
            }
        }
    }

    /// The shell in front (per the foreground policy, read now) ⇒
    /// [`interrupt_the_shell`](Self::interrupt_the_shell).
    pub(crate) fn no_shell_holding_the_paste(
        &self,
        ticket: TicketId,
        paste: PasteFence,
        trace_line: &str,
    ) -> Step<()> {
        let fg = self.ctl.term().foreground();
        self.ctl
            .read(|st| st.check_delivery_lifecycle(Some(paste.lifecycle)))?;
        if !self.shell_in_front(fg) {
            return Ok(());
        }
        self.interrupt_the_shell(ticket, paste, trace_line)
    }

    /// The shell was just read in front: its input line is interrupted, the agent marked
    /// gone (the next send launches it again) and the delivery ends `ShellOnly`;
    /// `trace_line` is traced. The paste may sit on that line —
    /// a multi-line one as a multi-line buffer that one line kill does not empty (zsh's
    /// `kill-whole-line` takes only the last line), so the next launch line would run the
    /// rest. Ctrl-C discards the whole line (zsh, bash; fish binds it to cancelling the
    /// command line — not tested), and the terminal flushes input the shell has not read
    /// yet. The shell was just read in front, so Ctrl-C interrupts nothing else. It is
    /// queued before the agent is marked gone: a launch line a concurrent `send` types for
    /// the stopped agent comes after it — and, while the shell still handles it, waits for
    /// it instead of interrupting again (`CtlState::note_interrupt`).
    fn interrupt_the_shell(
        &self,
        ticket: TicketId,
        paste: PasteFence,
        trace_line: &str,
    ) -> Step<()> {
        let written = self.write_after_paste_if(b"\x03", paste, |_| true);
        let after = (written == Ok(true)).then(|| self.ctl.interrupt_fence());
        self.ctl
            .update(|st, _| st.finish_paste_interrupt(paste.lifecycle, after))?;
        written?;
        trace!(self, ticket, "{trace_line}");
        Err(AbortReason::ShellOnly)
    }

    /// One Enter's verdict (`settle::SubmitCheck`), polling the screen.
    fn wait_submit_outcome(
        &mut self,
        text: &str,
        enter_fence: u64,
        lifecycle: u64,
        reset: Option<&ResetBaseline>,
    ) -> Step<Submit> {
        let timing = &self.ctl.cfg.timing;
        let verify = VerifyTiming {
            quiet: timing.submit_verify_quiet,
            // Reset commands may execute silently before drawing a new composer. A
            // stale composer is neither a receipt nor permission to repeat the reset.
            window: if reset.is_some() || self.ctl.cfg.reset_command == Some(text) {
                timing.startup_max.max(timing.submit_verify_window)
            } else {
                timing.submit_verify_window
            },
            poll: timing.submit_verify_poll,
        };
        let start = Instant::now();
        let deadline = start + verify.window;
        let mut check = SubmitCheck::new(start, enter_fence);
        loop {
            self.cancelled()?;
            let read = self.profile_read(reset, |profile, screen, _| {
                profile.pending_input(screen, text)
            })?;
            if read.lifecycle != lifecycle {
                return Err(AbortReason::SubmitUnverified);
            }
            let now = Instant::now();
            match check.poll(&read.view, read.value.flatten(), read.unseen, now, &verify) {
                Settle::Done(outcome) => return Ok(outcome),
                Settle::Wait(_) if reset.is_some() && now >= deadline => {
                    return Ok(Submit::Unverified);
                }
                Settle::Wait(at) if reset.is_some() => self.ctl.sleep_until(at.min(deadline))?,
                Settle::Wait(at) => self.ctl.sleep_until(at)?,
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
