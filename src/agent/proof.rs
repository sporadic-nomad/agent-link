//! The checks a delivery makes before an irreversible step: the **strict proofs**
//! (`SendOptions::strict`) and the **commit-point revalidation** (`Revalidate`) — ports of
//! the panel's `revalidateStrictForegroundAgent` and its commit-point `revalidate` call.
//!
//! A strict delivery fails closed: it goes on only while a foreground answer the
//! [`ForegroundPolicy`] trusts names the agent's own program ([`strict_owner`]). It checks
//! that before the lull (where the shell in front still gets the one relaunch), at the
//! commit point (*pre-paste readiness*), right before the paste, before the first and every
//! retried Enter, and once the submit was verified (the agent may have exited while that
//! evidence settled: no receipt is claimed for it). The shell found in front ends it
//! `ShellOnly` and marks the agent gone — the startup gate's "running" is a launch
//! heuristic that outlives the agent, and must never turn an unproven delivery into one;
//! anything else ends it `ForegroundUnproven`.
//!
//! The revalidation runs at the commit point after the lull and the cancellation check,
//! before the strict proof and the dialog check (the panel's order), and before an inert
//! paste into the shell. `Sync` calls the host's closure on the job thread; `Handshake`
//! publishes `Event::CommitPoint` and waits for `AgentTerminal::confirm` (or its timeout,
//! or the session's end). The answer can take a round trip: after it the commit point waits
//! for the input lull once more, and the inert paste reads the shell's ownership again.

use std::time::{Duration, Instant};

use super::controller::{Runner, Step, trace};
use super::state::{AgentProgram, FrontRead};
use super::{AbortReason, ForegroundPolicy, Revalidate, TicketId, Timing};
use crate::screen::catch_panic;
use crate::{Confidence, Foreground, ForegroundOwner};

/// A strict delivery's foreground rule: the agent in front by an answer the policy trusts
/// (exact, or heuristic under `AcceptHeuristic`) ⇒ `Ok`; the shell by such an answer ⇒
/// `ShellOnly` (the caller marks the agent gone); anything else — an unknown owner, a
/// heuristic answer under `ExactOnly` — ⇒ `ForegroundUnproven`: an untrusted answer proves
/// nothing either way.
pub(crate) fn strict_proof(fg: Foreground, policy: ForegroundPolicy) -> Step<()> {
    let trusted = fg.confidence == Confidence::Exact || policy == ForegroundPolicy::AcceptHeuristic;
    match fg.owner {
        ForegroundOwner::Agent if trusted => Ok(()),
        ForegroundOwner::Shell if trusted => Err(AbortReason::ShellOnly),
        _ => Err(AbortReason::ForegroundUnproven),
    }
}

/// [`strict_proof`] on `front`, and the program it names must be the agent's own (`agent`,
/// after `CtlState::observe`): any program in a direct session, else the one a launch line
/// of this terminal started. An owner the platform does not name (a Windows shell running
/// several programs) proves nothing.
pub(crate) fn strict_owner(
    front: &FrontRead,
    policy: ForegroundPolicy,
    agent: AgentProgram,
) -> Step<()> {
    strict_proof(front.fg, policy)?;
    match agent {
        AgentProgram::Session => Ok(()),
        AgentProgram::Known(known) if front.program == Some(known) => Ok(()),
        _ => Err(AbortReason::ForegroundUnproven),
    }
}

impl Runner {
    /// A strict proof at `point` on a fresh read of the foreground ([`Runner::owner`]:
    /// cancellation is checked before and after it).
    pub(crate) fn strict_check(&self, ticket: TicketId, point: &str) -> Step<()> {
        let (_, front, agent) = self.owner(ticket)?;
        self.strict_verdict(ticket, point, &front, agent)
    }

    /// [`strict_owner`] on a read already made; a failure is traced, and the shell in front
    /// marks the agent gone (state `ShellOnly`).
    pub(crate) fn strict_verdict(
        &self,
        ticket: TicketId,
        point: &str,
        front: &FrontRead,
        agent: AgentProgram,
    ) -> Step<()> {
        let verdict = strict_owner(front, self.ctl.cfg.foreground_policy, agent);
        if let Err(reason) = verdict {
            trace!(
                self,
                ticket,
                "strict proof ({point}) failed: {reason} (foreground {:?}, {:?})",
                front.fg.owner,
                front.fg.confidence
            );
            if reason == AbortReason::ShellOnly {
                self.ctl.update(|st, _| st.agent_left());
            }
        }
        verdict
    }

    /// The commit-point check; `Err(Revalidate)` when the host says no (or does not answer
    /// a handshake in time).
    pub(crate) fn revalidate(&self, ticket: TicketId, revalidate: &Revalidate) -> Step<()> {
        let proceed = match revalidate {
            Revalidate::None => return Ok(()),
            Revalidate::Sync(check) => match catch_panic(check) {
                Ok(proceed) => proceed,
                Err(message) => {
                    let id = self.ctl.cfg.id;
                    log::warn!("[agent-link:{id}] the revalidation check panicked: {message}");
                    false
                }
            },
            Revalidate::Handshake { timeout } => self.handshake(ticket, *timeout)?,
        };
        if proceed {
            Ok(())
        } else {
            trace!(self, ticket, "the commit-point check said no: not pasted");
            Err(AbortReason::Revalidate)
        }
    }

    /// Publishes `Event::CommitPoint` for `ticket` and waits for the host's answer
    /// (`AgentTerminal::confirm`) up to `timeout` — no answer counts as no. Ends at once with
    /// the delivery's cancellation (session end, drop).
    fn handshake(&self, ticket: TicketId, timeout: Duration) -> Step<bool> {
        trace!(self, ticket, "commit point: asking the host");
        self.ctl.update(|st, _| st.await_commit(ticket));
        let deadline = Instant::now() + timeout.min(Timing::MAX_WAIT);
        let answer = self.ctl.wait_until(deadline, |st| {
            if let Some(reason) = st.cancel_reason() {
                return Some(Err(reason));
            }
            st.take_answer(ticket).map(Ok)
        });
        match answer {
            Some(Ok(proceed)) => Ok(proceed),
            Some(Err(reason)) => {
                self.ctl.update(|st, _| st.end_commit());
                Err(reason)
            }
            None => {
                // An answer that came with the deadline still counts.
                let late = self.ctl.update(|st, _| {
                    let answer = st.take_answer(ticket);
                    st.end_commit();
                    answer
                });
                if late.is_none() {
                    trace!(
                        self,
                        ticket,
                        "commit point: no answer within {} ms",
                        timeout.as_millis()
                    );
                }
                Ok(late.unwrap_or(false))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::ProgramId;

    fn fg(owner: ForegroundOwner, confidence: Confidence) -> Foreground {
        Foreground { owner, confidence }
    }

    /// The whole table: policy × owner × confidence.
    #[test]
    fn strict_proof_accepts_only_a_trusted_agent() {
        use AbortReason::{ForegroundUnproven, ShellOnly};
        use Confidence::{Exact, Heuristic};
        use ForegroundOwner::{Agent, Shell, Unknown};
        use ForegroundPolicy::{AcceptHeuristic, ExactOnly};
        let table = [
            (ExactOnly, Agent, Exact, Ok(())),
            (ExactOnly, Agent, Heuristic, Err(ForegroundUnproven)),
            (ExactOnly, Shell, Exact, Err(ShellOnly)),
            (ExactOnly, Shell, Heuristic, Err(ForegroundUnproven)),
            (ExactOnly, Unknown, Exact, Err(ForegroundUnproven)),
            (ExactOnly, Unknown, Heuristic, Err(ForegroundUnproven)),
            (AcceptHeuristic, Agent, Exact, Ok(())),
            (AcceptHeuristic, Agent, Heuristic, Ok(())),
            (AcceptHeuristic, Shell, Exact, Err(ShellOnly)),
            (AcceptHeuristic, Shell, Heuristic, Err(ShellOnly)),
            (AcceptHeuristic, Unknown, Exact, Err(ForegroundUnproven)),
            (AcceptHeuristic, Unknown, Heuristic, Err(ForegroundUnproven)),
        ];
        for (policy, owner, confidence, want) in table {
            assert_eq!(
                strict_proof(fg(owner, confidence), policy),
                want,
                "{policy:?} {owner:?} {confidence:?}"
            );
        }
    }

    /// On top of the table: only the agent's own program (or anything in a direct
    /// session) — never an unnamed owner, another program, or one before the agent is known.
    #[test]
    fn strict_owner_needs_the_agent_s_own_program() {
        let (agent, other) = (ProgramId(10), ProgramId(11));
        let read = |owner, program| FrontRead {
            fg: fg(owner, Confidence::Exact),
            shell: owner == ForegroundOwner::Shell,
            program,
            shell_lines: 0,
        };
        let policy = ForegroundPolicy::ExactOnly;
        let in_front = read(ForegroundOwner::Agent, Some(agent));
        assert_eq!(
            strict_owner(&in_front, policy, AgentProgram::Known(agent)),
            Ok(())
        );
        assert_eq!(
            strict_owner(&in_front, policy, AgentProgram::Session),
            Ok(())
        );
        for known in [
            AgentProgram::Known(other),
            AgentProgram::Unknown,
            AgentProgram::Expected { shell_lines: 0 },
        ] {
            assert_eq!(
                strict_owner(&in_front, policy, known),
                Err(AbortReason::ForegroundUnproven),
                "{known:?}"
            );
        }
        let unnamed = read(ForegroundOwner::Agent, None);
        assert_eq!(
            strict_owner(&unnamed, policy, AgentProgram::Known(agent)),
            Err(AbortReason::ForegroundUnproven)
        );
        let shell = read(ForegroundOwner::Shell, None);
        assert_eq!(
            strict_owner(&shell, policy, AgentProgram::Known(agent)),
            Err(AbortReason::ShellOnly)
        );
        // A heuristic answer under `ExactOnly` proves nothing, whatever it names.
        let heuristic = FrontRead {
            fg: fg(ForegroundOwner::Agent, Confidence::Heuristic),
            ..in_front
        };
        assert_eq!(
            strict_owner(&heuristic, policy, AgentProgram::Session),
            Err(AbortReason::ForegroundUnproven)
        );
        assert_eq!(
            strict_owner(
                &heuristic,
                ForegroundPolicy::AcceptHeuristic,
                AgentProgram::Known(agent)
            ),
            Ok(())
        );
    }
}
