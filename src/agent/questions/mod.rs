//! Opt-in PTY question observation and explicitly requested answers.
//!
//! Enable observation on one terminal with [`AgentTerminal::observe_questions`](super::AgentTerminal::observe_questions).
//! Drain its ordinary PTY events. A [`QuestionId`] belongs to one interactive terminal and
//! one appearance of a question. Answer writes and message delivery run on the same job thread.
//! Native stdio permissions use their adapter's own approval capabilities.
//!
//! Sources, one per terminal: the profile's screen parser
//! ([`observe_questions`](super::AgentTerminal::observe_questions)), a controlled transcript
//! ([`observe_transcript_questions`](super::AgentTerminal::observe_transcript_questions),
//! [`observe_provider_questions`](super::AgentTerminal::observe_provider_questions)), a host
//! feed ([`observe_feed_questions`](super::AgentTerminal::observe_feed_questions), such as
//! [`opencode::OpenCodeQuestions`]), and — best effort, with weaker evidence — an ambient
//! transcript attachment corroborated on screen
//! ([`observe_ambient_transcript_questions`](super::AgentTerminal::observe_ambient_transcript_questions))
//! or an OpenCode TUI's own server ([`opencode::OpenCodeAmbientQuestions`]).

mod book;
mod dispatch;
pub mod opencode;
pub(in crate::agent) mod providers;
#[cfg(test)]
mod tests;
mod transcript;
pub(in crate::agent) use transcript::{TranscriptChange, provider_record};

use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::SessionId;
pub(in crate::agent) use book::{Book, Source};
pub(super) use dispatch::AnswerJob;

const MAX_QUESTIONS: usize = 16;
const MAX_OPTIONS: usize = 32;
const MAX_FIELD: usize = 8 * 1024;
const MAX_QUESTION_BYTES: usize = 64 * 1024;
const MAX_FEED_QUESTIONS: usize = 64;
const MAX_STEPS: usize = 64;

/// A terminal-local appearance of a question. Its serial is never reused on reattachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct QuestionId {
    /// The terminal that observed this question.
    pub session: SessionId,
    /// Monotonic appearance number in that terminal.
    pub serial: u64,
}

/// One prompt and the exact, ordered option labels shown to the user.
/// Debug formatting omits all content. A dialog exceeding the documented bounds is withheld whole.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct QuestionItem {
    /// Short title or tab label.
    pub header: String,
    /// The question or requested command.
    pub prompt: String,
    /// Choices in display order. Indices in an answer are zero based.
    pub options: Vec<String>,
    /// Whether multiple options can be selected.
    pub multi_select: bool,
    /// Whether the provider accepts an explicit text answer.
    pub free_text: bool,
}

impl QuestionItem {
    /// Construct a single-choice question.
    pub fn new(header: impl Into<String>, prompt: impl Into<String>, options: Vec<String>) -> Self {
        Self {
            header: header.into(),
            prompt: prompt.into(),
            options,
            multi_select: false,
            free_text: false,
        }
    }
    /// Expose multiple choices for this item.
    pub fn with_multi_select(mut self, on: bool) -> Self {
        self.multi_select = on;
        self
    }
    /// Expose the provider's text field.
    pub fn with_free_text(mut self, on: bool) -> Self {
        self.free_text = on;
        self
    }
}

impl fmt::Debug for QuestionItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuestionItem")
            .field("options", &self.options.len())
            .field("multi_select", &self.multi_select)
            .field("free_text", &self.free_text)
            .finish_non_exhaustive()
    }
}

/// A complete provider question. The provider ID is compared at dispatch and omitted from Debug.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PendingQuestion {
    /// An opaque provider request identifier. A screen source may leave it empty.
    pub id: String,
    /// The complete ordered set of items.
    pub questions: Vec<QuestionItem>,
}

impl PendingQuestion {
    /// Construct a pending question for a profile or feed.
    pub fn new(id: impl Into<String>, questions: Vec<QuestionItem>) -> Self {
        Self {
            id: id.into(),
            questions,
        }
    }

    pub(in crate::agent) fn valid(&self) -> bool {
        if self.id.len() > 128
            || self.id.chars().any(char::is_control)
            || !(1..=MAX_QUESTIONS).contains(&self.questions.len())
        {
            return false;
        }
        let mut bytes = self.id.len();
        self.questions.iter().all(|q| {
            if q.prompt.len() > MAX_FIELD
                || q.prompt.trim().is_empty()
                || q.options.len() > MAX_OPTIONS
                || (q.options.is_empty() && !q.free_text)
                || q.options.iter().enumerate().any(|(i, s)| {
                    s.len() > MAX_FIELD || s.trim().is_empty() || q.options[..i].contains(s)
                })
            {
                return false;
            }
            [&q.header, &q.prompt]
                .into_iter()
                .chain(&q.options)
                .all(|s| {
                    bytes += s.len().min(MAX_QUESTION_BYTES + 1);
                    s.len() <= MAX_FIELD
                        && bytes <= MAX_QUESTION_BYTES
                        && !s
                            .chars()
                            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
                })
        })
    }
}

impl fmt::Debug for PendingQuestion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingQuestion")
            .field("items", &self.questions.len())
            .finish_non_exhaustive()
    }
}

/// One explicit host answer per item, in question order.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Answer {
    /// The index of a single choice.
    Choice(usize),
    /// Distinct indices selected for a multiple-choice item.
    Choices(Vec<usize>),
    /// Literal user text.
    Text(String),
}
impl fmt::Debug for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Choice(_) => f.write_str("Choice(..)"),
            Self::Choices(v) => f.debug_tuple("Choices").field(&v.len()).finish(),
            Self::Text(s) => f.debug_tuple("TextBytes").field(&s.len()).finish(),
        }
    }
}

/// How observation of one question ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClosedBy {
    /// The question was dismissed, answered at the terminal or replaced.
    Desk,
    /// This host's answer was dispatched before the question disappeared.
    Host,
    /// Observation stopped, the agent left or attribution became unavailable.
    Gone,
}

/// Outcome of an answer request. A partially dispatched sequence is never retried automatically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AnswerError {
    /// The terminal, observer or question closed.
    #[error("the question is no longer open")]
    NotOpen,
    /// The current provider request or live screen no longer matches.
    #[error("the question changed")]
    Changed,
    /// The provider cannot encode this answer or a required field is unavailable.
    #[error("unsupported answer")]
    Unsupported,
    /// An input write or provider query failed before dispatch.
    #[error("question input or source failed")]
    Io,
    /// Another answer is outstanding or the shared delivery budget is full.
    #[error("question answer capacity is full")]
    QueueFull,
    /// An answer, decoded question or key sequence exceeds a configured bound.
    #[error("question input exceeds the configured limit")]
    InputTooLarge,
    /// Part of the answer may have reached the CLI. Inspect the terminal before acting.
    #[error("answer dispatch is unconfirmed")]
    Unconfirmed,
}

/// One profile-supplied input write and its maximum settle delay before the next write.
/// The job revalidates the question and foreground again before every write.
#[derive(Clone)]
#[non_exhaustive]
pub struct KeyStep {
    /// Trusted profile-generated bytes, possibly including bracketed user text.
    pub bytes: Vec<u8>,
    /// Time allowed for the CLI to redraw after this step, up to five seconds.
    pub settle: Duration,
    guard: Option<Arc<ScreenGuard>>,
}
type ScreenGuard = dyn Fn(&str) -> bool + Send + Sync;
impl KeyStep {
    /// Construct a single write.
    pub fn new(bytes: impl Into<Vec<u8>>, settle: Duration) -> Self {
        Self {
            bytes: bytes.into(),
            settle,
            guard: None,
        }
    }
}
impl KeyStep {
    /// Require a profile-specific screen state immediately before this write. A stale
    /// render discards the callback result. This lets each navigation step establish
    /// the expected active page or selection before queuing a confirming Enter.
    pub fn when(mut self, predicate: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        self.guard = Some(Arc::new(predicate));
        self
    }
}
impl fmt::Debug for KeyStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyStep")
            .field("bytes", &self.bytes.len())
            .field("settle", &self.settle)
            .finish()
    }
}

/// A host-attested question transport bound to a single interactive terminal and conversation.
/// Implementations must authenticate and recheck that binding before each read and answer.
/// Calls run outside controller locks on dedicated threads. An uncertain mutation returns
/// [`AnswerError::Unconfirmed`]. The library admits one answer at a time and never retries it.
pub trait QuestionFeed: Send + Sync + 'static {
    /// Return a complete, ordered list of outstanding requests for this session only.
    /// Up to 64 entries are accepted. A failed or incomplete sample cannot authorize an answer.
    fn pending(&self) -> Result<Vec<PendingQuestion>, AnswerError>;
    /// Revalidate the complete provider request, then call `current` immediately before
    /// mutating provider state. If it returns false, return [`AnswerError::NotOpen`].
    /// A successful check cannot be atomic with a remote write. Return
    /// [`AnswerError::Unconfirmed`] if the provider might have accepted a failed mutation.
    fn answer(
        &self,
        pending: &PendingQuestion,
        answers: &[Answer],
        current: &dyn Fn() -> bool,
    ) -> Result<(), AnswerError>;
}

/// Repeatably readable answer completion. Dropping a ticket does not cancel admitted work.
#[derive(Clone)]
pub struct AnswerTicket {
    id: QuestionId,
    result: Arc<Completion>,
}
struct Completion {
    value: OnceLock<Result<(), AnswerError>>,
    sender: Mutex<Option<crossbeam_channel::Sender<()>>>,
    closed: crossbeam_channel::Receiver<()>,
    #[cfg(feature = "tokio")]
    wake: tokio::sync::Notify,
}
impl AnswerTicket {
    fn new(id: QuestionId) -> Self {
        let (sender, closed) = crossbeam_channel::bounded(0);
        Self {
            id,
            result: Arc::new(Completion {
                value: OnceLock::new(),
                sender: Mutex::new(Some(sender)),
                closed,
                #[cfg(feature = "tokio")]
                wake: tokio::sync::Notify::new(),
            }),
        }
    }
    pub(super) fn finish(&self, result: Result<(), AnswerError>) {
        if self.result.value.set(result).is_ok() {
            self.result
                .sender
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            #[cfg(feature = "tokio")]
            self.result.wake.notify_waiters();
        }
    }
    /// The requested question.
    pub fn qid(&self) -> QuestionId {
        self.id
    }
    /// The authoritative result if dispatch finished.
    pub fn try_result(&self) -> Option<Result<(), AnswerError>> {
        self.result.value.get().copied()
    }
    /// Wait up to the supplied timeout. Success means all keys were queued or the feed accepted the request.
    pub fn wait(&self, timeout: Duration) -> Option<Result<(), AnswerError>> {
        if self.try_result().is_none() {
            let _ = self.result.closed.recv_timeout(timeout);
        }
        self.try_result()
    }
    /// Await this ticket without occupying a blocking thread.
    #[cfg(feature = "tokio")]
    pub async fn result(&self) -> Result<(), AnswerError> {
        loop {
            let wake = self.result.wake.notified();
            if let Some(result) = self.try_result() {
                return result;
            }
            wake.await;
        }
    }
}
impl fmt::Debug for AnswerTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnswerTicket")
            .field("id", &self.id)
            .field("result", &self.try_result())
            .finish()
    }
}
