//! Verified Enter — a port of the panel's `submitWithVerification`.
//!
//! Even a separate, delayed Enter can be swallowed: claude aggregates stdin for ~100 ms
//! after a paste and absorbs an Enter landing inside that window into the paste, leaving
//! the message entered but not sent while a fire-and-forget delivery reported success. So
//! the pasted text must first be visible in a readable composer. After Enter, fresh
//! output and a stable readable composer must show that the text left it. Continuous
//! output, silence and an unreadable composer are never receipts. A composer still
//! holding this message permits a bounded Enter retry only when the host opts in; the
//! default sends Enter once. A stale composer may outlive an accepted submit, so opt-in
//! retries can submit twice and do not provide exactly-once delivery. Uncertainty ends
//! `SubmitUnverified` without another Enter. A profile panic therefore cannot silently
//! confer receipt. Never into a dialog: one before any Enter goes back to the startup
//! gate and the delivery fails.
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

use std::time::Instant;

use super::controller::{MessageJob, Owner, Runner, Step, not_the_agent, trace};
use super::settle::{Settle, Submit, SubmitCheck, VerifyTiming};
use super::{AbortReason, TicketId};

impl Runner {
    /// Writes the Enter and verifies it took (see the module docs); `paste_fence` is the
    /// paste's fence. `on_submit_written` runs once, right after the first Enter was
    /// written.
    pub(crate) fn submit_with_verification(
        &mut self,
        job: &mut MessageJob,
        paste_fence: u64,
    ) -> Step<()> {
        let ticket = job.ticket;
        // A reset can keep its old composer while it is already executing. Repeating
        // it could clear the new conversation, so a reset is never retried.
        let retries = if self.ctl.cfg.reset_command == Some(job.text.as_str()) {
            0
        } else {
            self.ctl.cfg.timing.submit_retry_max
        };
        // A dialog drawn while the paste settled would take the Enter (claude's trust
        // dialog defaults to "No, exit").
        self.no_dialog_before_enter(ticket)?;
        self.wait_visible_paste(job, paste_fence)?;
        self.no_dialog_before_enter(ticket)?;
        self.agent_before_enter(ticket, job.opts.strict.then_some("initial submit"))?;
        let first_enter = self.fence();
        let mut enter_fence = first_enter.serial;
        self.write(b"\r")?;
        if let Some(on_submit_written) = job.on_submit_written.take() {
            on_submit_written(first_enter);
        }
        self.cancelled()?;
        let mut attempt = 0;
        loop {
            let verdict = self.wait_submit_outcome(&job.text, enter_fence)?;
            if job.opts.strict {
                self.strict_check(ticket, "submit verification")?;
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
            self.agent_before_enter(ticket, job.opts.strict.then_some("submit retry"))?;
            trace!(
                self,
                ticket, "the text is still in the composer: Enter again"
            );
            enter_fence = self.ctl.term().output_chunks();
            self.write(b"\r")?;
            self.cancelled()?;
            attempt += 1;
        }
    }

    /// Do not submit an unobserved paste: unrelated output over an unchanged empty
    /// composer otherwise looks like a receipt. Wait a bounded interval for the monitor
    /// to catch up and the profile to recognise this message before the first Enter.
    fn wait_visible_paste(&mut self, job: &MessageJob, paste_fence: u64) -> Step<()> {
        let timing = &self.ctl.cfg.timing;
        let deadline = Instant::now() + timing.submit_verify_window;
        let poll = timing
            .submit_verify_poll
            .max(std::time::Duration::from_millis(1));
        loop {
            self.cancelled()?;
            self.agent_before_enter(job.ticket, job.opts.strict.then_some("paste verification"))?;
            let read = self.profile_read(false, |profile, screen| {
                profile.pending_input(screen, &job.text)
            })?;
            if read.view.output_serial > paste_fence && read.value.flatten() == Some(true) {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(AbortReason::SubmitUnverified);
            }
            self.ctl.sleep_until((now + poll).min(deadline))?;
        }
    }

    /// A dialog on screen ⇒ handed to the startup gate and `TrustPromptBlocking`: an Enter
    /// is never typed into it.
    fn no_dialog_before_enter(&mut self, ticket: TicketId) -> Step<()> {
        let Some(dialog) = self.dialog_on_screen()? else {
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
    fn agent_before_enter(&self, ticket: TicketId, strict: Option<&str>) -> Step<()> {
        let (owner, front, agent) = self.owner(ticket)?;
        match owner {
            Owner::Agent => match strict {
                Some(point) => self.strict_verdict(ticket, point, &front, agent),
                None => Ok(()),
            },
            Owner::Shell => self.interrupt_the_shell(
                ticket,
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
        trace_line: &str,
    ) -> Step<()> {
        let fg = self.ctl.term().foreground();
        self.cancelled()?;
        if !self.shell_in_front(fg) {
            return Ok(());
        }
        self.interrupt_the_shell(ticket, trace_line)
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
    fn interrupt_the_shell(&self, ticket: TicketId, trace_line: &str) -> Step<()> {
        trace!(self, ticket, "{trace_line}");
        let written = self.ctl.interrupt();
        self.ctl.update(|st, _| {
            st.agent_left();
            if let Ok(after) = written {
                st.note_interrupt(after);
            }
        });
        written.map_err(|e| self.write_failed(e))?;
        Err(AbortReason::ShellOnly)
    }

    /// One Enter's verdict (`settle::SubmitCheck`), polling the screen.
    fn wait_submit_outcome(&mut self, text: &str, enter_fence: u64) -> Step<Submit> {
        let timing = &self.ctl.cfg.timing;
        let verify = VerifyTiming {
            quiet: timing.submit_verify_quiet,
            // Reset commands may execute silently before drawing a new composer. A
            // stale composer is neither a receipt nor permission to repeat the reset.
            window: if self.ctl.cfg.reset_command == Some(text) {
                timing.startup_max.max(timing.submit_verify_window)
            } else {
                timing.submit_verify_window
            },
            poll: timing.submit_verify_poll,
        };
        let mut check = SubmitCheck::new(Instant::now(), enter_fence);
        loop {
            self.cancelled()?;
            let read =
                self.profile_read(false, |profile, screen| profile.pending_input(screen, text))?;
            match check.poll(
                &read.view,
                read.value.flatten(),
                read.unseen,
                Instant::now(),
                &verify,
            ) {
                Settle::Done(outcome) => return Ok(outcome),
                Settle::Wait(at) => self.ctl.sleep_until(at)?,
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
