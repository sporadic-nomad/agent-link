//! Observation types for a dedicated Codex, Pi or Cursor transcript namespace.

use super::{TurnReceiver, TurnReply, TurnStatus};

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
        let reply = self.receiver.try_recv()?;
        Some(match self.source {
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
        })
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
