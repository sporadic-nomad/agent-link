//! Questions an ambient attachment's transcript shows asked (feature `questions`): what a
//! parser tells of them ([`AmbientQuestion`], from
//! [`AmbientParser::question`](super::AmbientParser::question)), what the worker keeps per
//! transcript ([`Asking`]) and publishes for the transcript it follows ([`Slot`]), and the
//! question observer's source over it ([`Questions`], for
//! [`AgentTerminal::observe_ambient_transcript_questions`](crate::agent::AgentTerminal::observe_ambient_transcript_questions)).
//!
//! The observer keeps only what a transcript's own lines tell: a line of copied history,
//! a rewritten file's lines read again, and a line over the line budget withdraw the
//! question instead. The slot belongs to one agent launch (the controller's lifecycle
//! revision the worker read it in); a source asking in another launch gets no answer
//! until the worker read that launch's transcript.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use super::{AmbientParser, AmbientReceiver, Shared};
use crate::agent::questions::{AnswerError, PendingQuestion};
use crate::agent::state::Controller;

/// What one transcript line tells of a question the agent asks the user and waits on, from
/// [`AmbientParser::question`](super::AmbientParser::question) (feature `questions`).
/// `Debug` shows no prompt, choice or provider id.
#[non_exhaustive]
pub enum AmbientQuestion {
    /// The agent asks this complete question and waits for its answer: a question tool's
    /// call written before its result. Its [`id`](PendingQuestion::id) is the tool call's
    /// (nonempty). It replaces any question asked before; one outside the documented
    /// bounds withdraws it instead.
    Asked(PendingQuestion),
    /// The question asked with this provider id got its result (answered or dismissed).
    Answered(String),
    /// No question stands any more: a prompt, the turn's end, or a question record that
    /// cannot be read.
    Withdrawn,
}

impl fmt::Debug for AmbientQuestion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Asked(question) => f.debug_tuple("Asked").field(question).finish(),
            Self::Answered(_) => f.write_str("Answered(..)"),
            Self::Withdrawn => f.write_str("Withdrawn"),
        }
    }
}

/// The question one transcript shows asked, as far as its own lines tell.
#[derive(Default)]
pub(super) struct Asking {
    /// Reading the contents of a rewritten file through EOF, even without a reply anchor.
    pub(super) replaying: bool,
    question: Option<Arc<PendingQuestion>>,
}

impl Asking {
    /// A line just passed to `parser`'s `record`: what it tells of questions counts only
    /// when `own` (not copied history, not a rewritten file read again); any other line
    /// withdraws the question.
    pub(super) fn line(&mut self, parser: &mut dyn AmbientParser, line: &[u8], own: bool) {
        if !own || self.replaying {
            parser.reset_question_context();
            self.question = None;
            return;
        }
        match parser.question(line) {
            None => {}
            Some(AmbientQuestion::Asked(question)) => {
                // Copy only validated elements and string contents. A custom parser can
                // return small fields backed by arbitrarily large reusable buffers.
                self.question = (!question.id.is_empty() && question.valid())
                    .then(|| Arc::new(question.clone()));
            }
            Some(AmbientQuestion::Answered(id)) => {
                if self.question.as_ref().is_some_and(|q| q.id == id) {
                    self.question = None;
                }
            }
            Some(AmbientQuestion::Withdrawn) => self.question = None,
        }
    }

    /// A line nobody read (over the line budget), or a rewrite: whatever it told is
    /// unknown.
    pub(super) fn clear(&mut self) {
        self.question = None;
    }

    pub(super) fn asked(&self) -> Option<Arc<PendingQuestion>> {
        self.question.clone()
    }
}

/// What the worker last read of the followed transcript's question.
pub(super) struct Slot(Mutex<SlotState>);

struct SlotState {
    /// The terminal (set at attach).
    ctl: Weak<Controller>,
    /// The attachment tails a transcript (not a feed).
    transcript: bool,
    /// The controller's lifecycle revision of the launch read.
    lifecycle: u64,
    question: Option<Arc<PendingQuestion>>,
    /// The read ended at the transcript's end on a complete line, with nothing pending:
    /// what it shows is the transcript's current state.
    settled: bool,
}

impl Default for Slot {
    fn default() -> Self {
        Self(Mutex::new(SlotState {
            ctl: Weak::new(),
            transcript: false,
            lifecycle: 0,
            question: None,
            settled: true,
        }))
    }
}

impl Slot {
    fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// At attach: the terminal, its launch, and whether a transcript is read.
    pub(super) fn bind(&self, ctl: &Arc<Controller>, lifecycle: u64, transcript: bool) {
        let mut state = self.lock();
        state.ctl = Arc::downgrade(ctl);
        state.lifecycle = lifecycle;
        state.transcript = transcript;
    }

    /// A new generation (launch, relaunch, departure): nothing asked in it yet.
    pub(super) fn restart(&self, lifecycle: u64) {
        let mut state = self.lock();
        state.lifecycle = lifecycle;
        state.question = None;
        state.settled = true;
    }

    /// What the followed transcript shows asked after a read.
    pub(super) fn set(&self, question: Option<Arc<PendingQuestion>>, settled: bool) {
        let mut state = self.lock();
        state.question = question;
        state.settled = settled;
    }

    /// No agent runs: nothing is asked.
    pub(super) fn clear(&self) {
        self.set(None, true);
    }

    /// The transcript cannot be read now: what it shows is unknown until it is again.
    pub(super) fn unsettle(&self) {
        self.lock().settled = false;
    }
}

impl fmt::Debug for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("Slot")
            .field("asked", &state.question.is_some())
            .field("settled", &state.settled)
            .finish_non_exhaustive()
    }
}

/// The question observer's source over one ambient attachment. Holds the attachment's
/// shared state only: stopping or dropping its [`AmbientReceiver`] ends its authority.
#[derive(Clone)]
pub(in crate::agent) struct Questions {
    shared: Arc<Shared>,
}

impl Questions {
    /// The attachment observes `ctl`'s terminal and is still live.
    pub(in crate::agent) fn belongs_to(&self, ctl: &Arc<Controller>) -> bool {
        self.shared.live()
            && self
                .shared
                .question
                .lock()
                .ctl
                .upgrade()
                .is_some_and(|own| Arc::ptr_eq(&own, ctl))
    }

    /// The attachment tails a transcript (a feed's tells no questions here).
    pub(in crate::agent) fn reads_transcript(&self) -> bool {
        self.shared.question.lock().transcript
    }

    /// The question the followed transcript shows asked now, as the worker last read it.
    /// [`AnswerError::NotOpen`]: the attachment closed or its terminal is gone;
    /// [`AnswerError::Changed`]: not known now (the transcript is unreadable, a read
    /// stopped part-way or on a partial line, or the worker has not read this launch's
    /// transcript yet). Reads no file; samples the controller's lifecycle revision.
    pub(in crate::agent) fn pending_question(
        &self,
    ) -> Result<Option<Arc<PendingQuestion>>, AnswerError> {
        if !self.shared.live() {
            return Err(AnswerError::NotOpen);
        }
        let (ctl, lifecycle, question, settled) = {
            let state = self.shared.question.lock();
            (
                state.ctl.upgrade(),
                state.lifecycle,
                state.question.clone(),
                state.settled,
            )
        };
        let ctl = ctl.ok_or(AnswerError::NotOpen)?;
        if ctl.read(|st| st.lifecycle_revision()) != lifecycle || !settled {
            return Err(AnswerError::Changed);
        }
        Ok(question)
    }
}

impl fmt::Debug for Questions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Questions")
            .field("slot", &self.shared.question)
            .finish()
    }
}

impl AmbientReceiver {
    /// This attachment as a question source.
    pub(in crate::agent) fn questions(&self) -> Questions {
        Questions {
            shared: Arc::clone(&self.shared),
        }
    }
}

#[cfg(test)]
mod tests;
