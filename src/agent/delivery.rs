//! The delivery API's types: what [`AgentTerminal::send`](super::AgentTerminal::send) (and
//! `reset`, `reset_and_send`) takes ([`SendOptions`]) and returns ([`Ticket`]), and how a
//! delivery ends ([`DeliveryOutcome`], also published as
//! [`Event::Delivery`](crate::Event::Delivery)).

use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A delivery's number: per [`AgentTerminal`](super::AgentTerminal), from 1, in `send`
/// order. [`Event::Delivery`](crate::Event::Delivery) names it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TicketId(pub u64);

impl fmt::Display for TicketId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// How a delivery ended. Only [`Delivered`](Self::Delivered) means the agent got the
/// message (and, with [`SendOptions::submit`], that the submit was seen to take).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// Pasted, and — for a submit — the Enter was verified on screen.
    Delivered,
    /// The agent was not in front (the shell was, after one relaunch), so the text — a
    /// single line — was put on the shell's input line without submitting it
    /// ([`ShellFallback::PasteInert`]); the host should tell the user. It stays on that line
    /// until the user acts or the next launch line the crate types clears it; it never runs
    /// by the crate's doing.
    PastedUnsubmitted,
    /// Delivery stopped. The reason describes which writes may already have occurred.
    Aborted(AbortReason),
}

/// Why a delivery was [`Aborted`](DeliveryOutcome::Aborted).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AbortReason {
    /// The session ended, or monitoring stopped after an internal error or event-queue
    /// overflow (logged at `error`). New requests are refused before admission.
    NotConnected,
    /// The shell stayed in front after one relaunch and [`ShellFallback::Fail`] (or
    /// [`SendOptions::strict`]) was asked for, or the text could not be left on its line safely (more than one line; a shell
    /// line no later launch line can clear; a line the host submitted to the shell while
    /// the fallback's Ctrl-C was being taken): nothing was pasted. Or the shell came to the
    /// front after the paste (the agent left — `/exit`, a crash — maybe before reading it):
    /// no Enter was written (or, for a multi-line paste without submit, the shell held it
    /// once it settled), and the shell's input line was interrupted (Ctrl-C) so a paste
    /// that landed there is discarded, not run. A strict delivery also ends here when one
    /// of its proofs finds the shell in front (the agent is then marked gone).
    ShellOnly,
    /// A program other than the shell owns the terminal, and it is not the agent's own (the
    /// one a launch line this terminal typed into the shell started): a nested shell,
    /// `ssh`, an editor, `cat` — started after the agent exited or with none launched — or
    /// an agent the user started by hand. Nothing was typed, no launch line either; found
    /// right before an Enter (the agent's program gave the terminal to another after the
    /// paste), no Enter was written; found after the shell fallback's Ctrl-C (started while
    /// the shell took it), nothing more was typed. Without a profile any such program. A
    /// strict delivery ([`SendOptions::strict`]) also ends here when a proof finds no
    /// answer the [`ForegroundPolicy`](super::ForegroundPolicy) trusts that names the
    /// agent's own program (an unknown owner, a heuristic one under `ExactOnly`, a program
    /// it cannot name).
    ForegroundUnproven,
    /// A startup dialog was on screen where the message would have gone (after readiness, or in
    /// place of the composer while verifying the submit); nothing was typed into it.
    TrustPromptBlocking,
    /// The profile recognized an active question. At the commit point this stops the
    /// paste, including a paste-only send. Before an Enter the paste is already queued,
    /// and a previous Enter may have been sent if retries were enabled. After a paste,
    /// this outcome requires a fresh foreground read naming the agent. Inspect the
    /// session before sending again.
    DialogBlocking,
    /// The profile recognized a fresh diagnostic refusing an explicit reset after Enter.
    /// A compound reset ends before the follow-up paste.
    ResetRejected,
    /// The commit-point check ([`SendOptions::revalidate`]) said no, or its handshake was
    /// not answered in time.
    Revalidate,
    /// The profile could not supply a current dialog check before input, or screen
    /// evidence could not confirm submission: the paste was not observed, the
    /// composer remained unchanged, or the profile could not read it reliably. No Enter
    /// is written before observing the paste; afterward the default writes it once.
    /// An explicit reset also ends here when a current refusal check is unavailable
    /// or a visible notice could predate its Enter. Its follow-up is withheld.
    /// Explicitly configured retries can write more. The agent may already have accepted
    /// the message or may accept it later: do not automatically resend this outcome.
    SubmitUnverified,
    /// The [`AgentTerminal`](super::AgentTerminal) was dropped. Its ticket is resolved
    /// independently of event delivery; final events during teardown are best effort.
    Disposed,
    /// Writing to the terminal failed.
    WriteFailed,
    /// The request is not supported by this terminal: a reset
    /// ([`reset`](super::AgentTerminal::reset),
    /// [`reset_and_send`](super::AgentTerminal::reset_and_send)) without a reset command —
    /// no profile, or a profile without one. Nothing was typed.
    Unsupported,
    /// The text holds nothing to paste: empty, or only whitespace and control characters
    /// (dropped before the paste). Nothing was typed — its Enter would submit whatever the
    /// user left in the composer — and a stopped agent was not launched for it.
    EmptyText,
    /// Admission refused: the raw message, reset command or composite exceeds the
    /// configured byte limits. The ticket resolves immediately; no event is published.
    InputTooLarge,
    /// Admission refused: queued/running jobs or unpublished completions exhausted the
    /// byte or job budget. The ticket resolves immediately; no event is published.
    QueueFull,
}

impl AbortReason {
    /// The reason in snake case (`not_connected`, `shell_only`, …) — for logs and hosts
    /// that pass it on as a string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotConnected => "not_connected",
            Self::ShellOnly => "shell_only",
            Self::ForegroundUnproven => "foreground_unproven",
            Self::TrustPromptBlocking => "trust_prompt_blocking",
            Self::ResetRejected => "reset_rejected",
            Self::DialogBlocking => "dialog_blocking",
            Self::Revalidate => "revalidate",
            Self::SubmitUnverified => "submit_unverified",
            Self::Disposed => "disposed",
            Self::WriteFailed => "write_failed",
            Self::Unsupported => "unsupported",
            Self::EmptyText => "empty_text",
            Self::InputTooLarge => "input_too_large",
            Self::QueueFull => "queue_full",
        }
    }
}

impl fmt::Display for AbortReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a send (a submit or a paste only) does when the shell, not the agent, is in front
/// after one relaunch of the agent. Either way the text never runs as a shell command.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ShellFallback {
    /// Put a single-line text on the shell's input line without Enter and end with
    /// [`DeliveryOutcome::PastedUnsubmitted`]: no newline, so nothing runs. A Ctrl-C first
    /// empties the line (once the crate typed into the session — before, the shell may
    /// still be reading its startup files): a draft or the last such text is discarded, not
    /// joined, and a line editor in vi mode starts the fresh line in insert mode. The text
    /// is a bracketed paste where the shell turned bracketed paste on after that Ctrl-C
    /// (bash 5.1+, zsh), else typed as is: a shell without it reads the paste marker's ESC
    /// as a key (bash 3.2 in vi mode: command mode, the text's letters vi commands). It
    /// stays there until the user acts: the controller never submits it, and every launch
    /// line it types later (a relaunch, [`launch`](super::AgentTerminal::launch)) clears the
    /// line first (a Ctrl-C with the shell in front, then
    /// [`ShellKind::clear_line`](super::ShellKind::clear_line)). Nothing is pasted —
    /// [`AbortReason::ShellOnly`] — for a text with more than one line (whatever the
    /// terminal's bracketed-paste mode says: it can be stale, a shell without it runs each
    /// line, and a multi-line buffer needs more than one line kill in zsh and fish), or
    /// where the line cannot be killed before a later launch line (`cmd.exe` with a
    /// profile).
    #[default]
    PasteInert,
    /// Paste nothing: [`AbortReason::ShellOnly`].
    Fail,
}

/// A check right before the paste (the commit point: after the input lull, before the
/// strict proofs and the dialog check), so a message that went stale while it waited is
/// never pasted. Asked every time the delivery reaches the commit point (it starts over
/// when the agent was relaunched or is no longer ready), and before an inert paste into the
/// shell too.
#[derive(Default)]
#[non_exhaustive]
pub enum Revalidate {
    /// No check.
    #[default]
    None,
    /// Called on the controller's job thread; `false` aborts with
    /// [`AbortReason::Revalidate`]. It must not wait on the agent or the terminal (the job
    /// thread is the one that serves them). A panic in it counts as `false`. For hosts
    /// that can answer synchronously (a Rust host).
    Sync(Box<dyn Fn() -> bool + Send + Sync>),
    /// For hosts that answer asynchronously (over IPC): publish
    /// [`Event::CommitPoint`](crate::Event::CommitPoint) with the ticket and wait up to
    /// `timeout` for [`AgentTerminal::confirm`](super::AgentTerminal::confirm) — `true`
    /// pastes, `false` (or no answer in time) aborts with [`AbortReason::Revalidate`]. The
    /// wait ends at once when the session ends or the terminal is dropped.
    /// [`Timing::revalidate_timeout`](super::Timing::revalidate_timeout) is a default
    /// for hosts with none of their own.
    Handshake {
        /// How long to wait for the answer.
        timeout: Duration,
    },
}

impl fmt::Debug for Revalidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Sync(_) => f.write_str("Sync(..)"),
            Self::Handshake { timeout } => f
                .debug_struct("Handshake")
                .field("timeout", timeout)
                .finish(),
        }
    }
}

/// How [`AgentTerminal::send`](super::AgentTerminal::send) delivers. [`Default`]: paste
/// only, not strict, [`ShellFallback::PasteInert`], no revalidation.
#[derive(Debug, Default)]
pub struct SendOptions {
    /// Observe the pasted text, press Enter once and verify a readable composer
    /// transition. Extra Enter attempts require opting into
    /// [`Timing::submit_retry_max`](super::Timing::submit_retry_max) and may submit twice
    /// when the agent redraws late. `false`: queue an editable paste and stop.
    pub submit: bool,
    /// Require proof that the agent owns the terminal at every irreversible step: before
    /// the lull, at the commit point and right before the paste, before every Enter and
    /// once the submit was verified. A proof is a foreground answer the
    /// [`ForegroundPolicy`](super::ForegroundPolicy) trusts (`ExactOnly`: unix only) naming
    /// the agent's own program; the shell in front ends the delivery
    /// [`AbortReason::ShellOnly`] (before the lull it relaunches the agent once first; it
    /// never takes the [`ShellFallback`]), anything else
    /// [`AbortReason::ForegroundUnproven`]. Non-strict delivery also refuses unknown or
    /// untrusted ownership; strict adds commit-point and post-submit proofs and forbids
    /// the inert shell fallback.
    pub strict: bool,
    /// What the send does when the shell stays in front after one relaunch.
    pub on_shell: ShellFallback,
    /// The commit-point check.
    pub revalidate: Revalidate,
}

/// A delivery request: its [`TicketId`] and authoritative outcome. Admission refusals
/// resolve immediately without an event. Accepted work normally also publishes
/// [`Event::Delivery`](crate::Event::Delivery); dropping the ticket does not cancel it.
/// Undrained event receivers can stall normal delivery progress. Teardown resolves
/// outstanding tickets independently of receivers or the job thread; its final events
/// are best effort.
pub struct Ticket {
    id: TicketId,
    slot: Arc<Slot>,
}

impl Ticket {
    /// The delivery's id.
    pub fn id(&self) -> TicketId {
        self.id
    }

    /// Waits up to `timeout` for the outcome; `None` if the delivery is still running.
    /// Once resolved, every call returns the same outcome at once.
    pub fn wait(&self, timeout: Duration) -> Option<DeliveryOutcome> {
        let deadline = Instant::now().checked_add(timeout);
        let mut outcome = self.slot.lock();
        loop {
            if let Some(outcome) = outcome.as_ref() {
                return Some(outcome.clone());
            }
            let wait = match deadline {
                Some(deadline) => deadline.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(3600),
            };
            if wait.is_zero() {
                return None;
            }
            outcome = self
                .slot
                .cv
                .wait_timeout(outcome, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// The outcome if the delivery ended, without waiting.
    pub fn try_outcome(&self) -> Option<DeliveryOutcome> {
        self.slot.lock().clone()
    }

    /// Woken (`notify_waiters`) every time the ticket is resolved, after the outcome is
    /// stored — for `Ticket::outcome` (`async_api`).
    #[cfg(feature = "tokio")]
    pub(crate) fn resolved(&self) -> &tokio::sync::Notify {
        &self.slot.resolved
    }
}

impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ticket")
            .field("id", &self.id)
            .field("outcome", &self.try_outcome())
            .finish()
    }
}

/// Where a ticket's outcome lands.
#[derive(Default)]
struct Slot {
    outcome: Mutex<Option<DeliveryOutcome>>,
    cv: Condvar,
    /// Wakes the async waiters (`Ticket::resolved`); no thread per ticket.
    #[cfg(feature = "tokio")]
    resolved: tokio::sync::Notify,
}

impl Slot {
    fn lock(&self) -> MutexGuard<'_, Option<DeliveryOutcome>> {
        self.outcome.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A passive handle to a ticket's outcome. Unlike the job's `Resolver`, dropping this
/// does not resolve anything; the controller retains it to cancel even a job blocked in
/// host code during teardown.
#[derive(Clone)]
pub(crate) struct Resolution {
    slot: Arc<Slot>,
}

impl Resolution {
    pub(crate) fn resolve(&self, outcome: DeliveryOutcome) {
        let mut slot = self.slot.lock();
        if slot.is_none() {
            *slot = Some(outcome);
        }
        drop(slot);
        self.slot.cv.notify_all();
        #[cfg(feature = "tokio")]
        self.slot.resolved.notify_waiters();
    }
}

/// The job's end of a ticket. Resolved once; dropped unresolved (the controller went away
/// without running the job) it resolves the ticket as [`AbortReason::Disposed`].
pub(crate) struct Resolver {
    slot: Arc<Slot>,
}

impl Resolver {
    /// A cancellation handle which never resolves on drop.
    pub(crate) fn resolution(&self) -> Resolution {
        Resolution {
            slot: Arc::clone(&self.slot),
        }
    }

    /// Stores the outcome and wakes every waiter (the first outcome wins).
    pub(crate) fn resolve(&self, outcome: DeliveryOutcome) {
        self.resolution().resolve(outcome);
    }
}

impl Drop for Resolver {
    fn drop(&mut self) {
        self.resolve(DeliveryOutcome::Aborted(AbortReason::Disposed));
    }
}

/// A new ticket and its resolver.
pub(crate) fn ticket(id: TicketId) -> (Ticket, Resolver) {
    let slot = Arc::new(Slot::default());
    (
        Ticket {
            id,
            slot: Arc::clone(&slot),
        },
        Resolver { slot },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_waits_then_keeps_its_outcome() {
        let (ticket, resolver) = ticket(TicketId(3));
        assert_eq!(ticket.id(), TicketId(3));
        assert_eq!(ticket.try_outcome(), None);
        assert_eq!(ticket.wait(Duration::from_millis(20)), None);
        let waiter = std::thread::spawn(move || {
            let outcome = ticket.wait(Duration::from_secs(10));
            (ticket, outcome)
        });
        resolver.resolve(DeliveryOutcome::PastedUnsubmitted);
        resolver.resolve(DeliveryOutcome::Delivered);
        let (ticket, outcome) = waiter.join().unwrap();
        assert_eq!(outcome, Some(DeliveryOutcome::PastedUnsubmitted));
        assert_eq!(ticket.wait(Duration::ZERO), outcome);
        assert_eq!(ticket.try_outcome(), outcome);
        drop(resolver);
        assert_eq!(ticket.try_outcome(), outcome, "the first outcome wins");
    }

    #[test]
    fn a_dropped_resolver_disposes_the_ticket() {
        let (ticket, resolver) = ticket(TicketId(1));
        drop(resolver);
        assert_eq!(
            ticket.wait(Duration::MAX),
            Some(DeliveryOutcome::Aborted(AbortReason::Disposed))
        );
    }

    #[test]
    fn reasons_have_snake_case_names() {
        assert_eq!(AbortReason::ResetRejected.as_str(), "reset_rejected");
        assert_eq!(AbortReason::DialogBlocking.as_str(), "dialog_blocking");
        assert_eq!(
            AbortReason::TrustPromptBlocking.as_str(),
            "trust_prompt_blocking"
        );
        assert_eq!(AbortReason::NotConnected.to_string(), "not_connected");
        assert_eq!(AbortReason::EmptyText.as_str(), "empty_text");
        let opts = SendOptions::default();
        assert!(!opts.submit && !opts.strict);
        assert_eq!(opts.on_shell, ShellFallback::PasteInert);
        assert!(matches!(opts.revalidate, Revalidate::None));
        assert_eq!(
            format!("{:?}", Revalidate::Sync(Box::new(|| true))),
            "Sync(..)"
        );
    }

    /// `Ticket::outcome` (feature `tokio`): woken by the resolve, no thread per ticket.
    #[cfg(feature = "tokio")]
    mod outcome {
        use super::*;

        const WAIT: Duration = Duration::from_secs(10);

        async fn outcome(ticket: &Ticket) -> DeliveryOutcome {
            tokio::time::timeout(WAIT, ticket.outcome())
                .await
                .expect("no outcome in time")
        }

        /// Two tasks wait on one ticket; the resolve wakes both. Afterwards it answers at
        /// once, and the blocking API agrees.
        #[tokio::test]
        async fn waiting_tasks_are_woken_by_the_resolve() {
            let (ticket, resolver) = ticket(TicketId(1));
            let ticket = Arc::new(ticket);
            let waiters: Vec<_> = (0..2)
                .map(|_| {
                    let ticket = Arc::clone(&ticket);
                    tokio::spawn(async move { outcome(&ticket).await })
                })
                .collect();
            // Current-thread runtime: the waiters run (and park on the slot) now.
            tokio::task::yield_now().await;
            assert_eq!(ticket.try_outcome(), None);
            resolver.resolve(DeliveryOutcome::Delivered);
            for waiter in waiters {
                assert_eq!(waiter.await.unwrap(), DeliveryOutcome::Delivered);
            }
            resolver.resolve(DeliveryOutcome::PastedUnsubmitted);
            assert_eq!(outcome(&ticket).await, DeliveryOutcome::Delivered);
            assert_eq!(
                ticket.wait(Duration::ZERO),
                Some(DeliveryOutcome::Delivered)
            );
        }

        /// A resolver dropped on another thread (the controller gone) ends the wait
        /// `Disposed`.
        #[tokio::test]
        async fn a_dropped_resolver_ends_the_wait_disposed() {
            let (ticket, resolver) = ticket(TicketId(2));
            let dropper = std::thread::spawn(move || drop(resolver));
            assert_eq!(
                outcome(&ticket).await,
                DeliveryOutcome::Aborted(AbortReason::Disposed)
            );
            dropper.join().unwrap();
        }

        #[test]
        fn the_future_is_send() {
            fn assert_send<T: Send>(_: &T) {}
            let (ticket, _resolver) = ticket(TicketId(3));
            assert_send(&ticket.outcome());
        }
    }
}
