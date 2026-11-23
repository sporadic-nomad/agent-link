//! Observation types for a dedicated Codex, Pi or Cursor transcript namespace.

use std::time::Duration;

use super::{TurnReceiver, TurnRecvError, TurnReply, TurnStatus};

/// Which interactive transcript to observe. Cursor's recorded text stream can
/// combine reasoning with its displayed answer. Explicitly opt in to that text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TranscriptSource {
    /// The final message in a completed Codex rollout event.
    Codex,
    /// A Pi assistant stop inferred to end a turn after quiet, another prompt or shell input.
    Pi,
    /// Report completion without retaining Cursor's mixed assistant text.
    Cursor,
    /// Include Cursor's unseparated assistant text after its last tool call.
    CursorWithUnseparatedText,
}

impl TranscriptSource {
    pub(in crate::agent) fn profile(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::Cursor | Self::CursorWithUnseparatedText => "cursor",
        }
    }
}

/// The meaning and sensitivity of one newly observed turn.
#[derive(Debug)]
#[non_exhaustive]
pub enum TranscriptTurn {
    /// The interactive Codex rollout explicitly completed its turn.
    CodexFinal(TurnReply),
    /// Pi persisted an assistant stop, followed by three quiet seconds, a new prompt or shell input.
    /// A later tool invocation or branch can still change the interpretation.
    PiInferred(TurnReply),
    /// Cursor explicitly ended a successful turn. Its text was withheld.
    CursorCompleted,
    /// An additional user prompt followed the candidate before an explicit end marker.
    CursorNextPrompt,
    /// Cursor explicitly ended successfully. This contains unseparated model
    /// output, which can include reasoning and must be treated accordingly.
    CursorUnseparatedText(TurnReply),
    /// A next-prompt inference carrying the unseparated transcript text requested by the host.
    CursorUnseparatedTextAtNextPrompt(TurnReply),
}

/// One bounded observer of a controlled, isolated PTY. Drop or stop cancels it.
pub struct TranscriptReceiver {
    pub(in crate::agent) source: TranscriptSource,
    pub(in crate::agent) receiver: std::sync::Arc<TurnReceiver>,
}

impl TranscriptReceiver {
    /// Coalesced status. Loss and closure discard queued output.
    pub fn status(&self) -> TurnStatus {
        self.receiver.status()
    }

    /// Pull the next observation. A nonempty pull performs bounded synchronous
    /// local file and process checks. An empty queue returns immediately.
    pub fn try_recv(&self) -> Option<TranscriptTurn> {
        self.receiver.try_recv().map(|reply| self.turn(reply))
    }

    /// Wait up to `timeout` for the next observation instead of polling: an observation,
    /// the gap or closure that ended observation, or [`TurnRecvError::Timeout`]. Blocks
    /// the calling thread. Wakeups, latency and pull checks are those of
    /// [`TurnReceiver::recv_timeout`].
    pub fn recv_timeout(&self, timeout: Duration) -> Result<TranscriptTurn, TurnRecvError> {
        self.receiver
            .recv_timeout(timeout)
            .map(|reply| self.turn(reply))
    }

    /// Wait up to `timeout` for a status other than `seen`, as
    /// [`TurnReceiver::wait_status_change`] does. `None` on timeout.
    pub fn wait_status_change(&self, seen: TurnStatus, timeout: Duration) -> Option<TurnStatus> {
        self.receiver.wait_status_change(seen, timeout)
    }

    /// Await the next observation (feature `tokio`), as [`TurnReceiver::recv`] does.
    /// Never times out; cancel-safe.
    #[cfg(feature = "tokio")]
    pub async fn recv(&self) -> Result<TranscriptTurn, TurnRecvError> {
        self.receiver.recv().await.map(|reply| self.turn(reply))
    }

    /// Await a status other than `seen` (feature `tokio`), as
    /// [`TurnReceiver::status_change`] does. Cancel-safe.
    #[cfg(feature = "tokio")]
    pub async fn status_change(&self, seen: TurnStatus) -> TurnStatus {
        self.receiver.status_change(seen).await
    }

    fn turn(&self, reply: TurnReply) -> TranscriptTurn {
        match self.source {
            TranscriptSource::Codex => TranscriptTurn::CodexFinal(reply),
            TranscriptSource::Pi => TranscriptTurn::PiInferred(reply),
            TranscriptSource::Cursor if reply.inferred => TranscriptTurn::CursorNextPrompt,
            TranscriptSource::Cursor => TranscriptTurn::CursorCompleted,
            TranscriptSource::CursorWithUnseparatedText if reply.inferred => {
                TranscriptTurn::CursorUnseparatedTextAtNextPrompt(reply)
            }
            TranscriptSource::CursorWithUnseparatedText => {
                TranscriptTurn::CursorUnseparatedText(reply)
            }
        }
    }

    /// Stop observation and discard queued text. The terminal keeps running.
    pub fn stop(&self) {
        self.receiver.stop();
    }
}

impl Drop for TranscriptReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Host-controlled directory dedicated to one interactive PTY's transcript writers.
/// Configure isolated credentials and settings as needed. Start with an empty provider
/// transcript namespace. The host must exclude other writers, remote/shared backends,
/// imported history, independent agents and sessions resumed outside this namespace.
/// Keep the directory at its canonical name and prevent replacement, renaming or
/// retargeting of that name or its ancestors for the entire lease.
/// Revoke before relaxing isolation. Dropping this value revokes it as well.
#[must_use = "keep the lease while the host enforces writer isolation"]
pub struct IsolatedTranscriptRoot(pub(in crate::agent) super::ExclusiveClaudeRoot);

impl IsolatedTranscriptRoot {
    /// Pin an existing directory on macOS arm64. Access errors are synchronous.
    /// Keep the canonical directory name stable for the full lease.
    pub fn new(root: impl AsRef<std::path::Path>) -> crate::Result<Self> {
        super::ExclusiveClaudeRoot::named(root.as_ref()).map(Self)
    }

    /// Withhold queued observations before releasing the host's isolation.
    /// Waits for any admitted process creation. The running PTY stays alive.
    pub fn revoke(&self) {
        self.0.revoke();
    }
}
