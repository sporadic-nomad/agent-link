//! Versioned observations for native adapters and transcript projections.
//!
//! These are projections of provider evidence, not a universal wire protocol or a
//! transcript. Missing identity/phase stays unknown. Item completion, usage, final
//! text and turn completion are different observations. Keep the optional native
//! envelope when additional provider fields matter. No projection is logged here.

use std::fmt;

use crate::SessionId;

/// Provider that supplied the evidence. New adapters may add variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Provider {
    /// Codex app-server.
    Codex,
    /// Agent Client Protocol stable v1.
    Acp,
    /// Claude stream-json.
    Claude,
}

/// Native request identity; numeric and string IDs remain distinct.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RequestId {
    /// Native string ID.
    String(String),
    /// Native integer ID.
    Number(i64),
}

/// Origin and ordering of one observation. IDs are opaque and provider-scoped.
/// No root/session/turn identity is inferred for an event that omits it.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventContext {
    /// Provider protocol, not model vendor.
    pub provider: Provider,
    /// Local transport generation; never a resume token.
    pub generation: SessionId,
    /// Transport line sequence. Responses consume numbers too; gaps are not loss.
    pub seq: u64,
    /// Native session identity (Codex thread ID or Claude session ID); use with its provider for resume.
    pub session_id: Option<String>,
    /// Native turn identity.
    pub turn_id: Option<String>,
    /// Native item identity.
    pub item_id: Option<String>,
    /// Reverse request or resolved request identity, when supplied.
    pub request_id: Option<RequestId>,
}

impl EventContext {
    #[cfg(any(feature = "codex", feature = "acp", feature = "claude"))]
    pub(crate) fn retained_bytes(&self) -> usize {
        self.session_id.as_ref().map_or(0, String::len)
            + self.turn_id.as_ref().map_or(0, String::len)
            + self.item_id.as_ref().map_or(0, String::len)
            + match &self.request_id {
                Some(RequestId::String(id)) => id.len(),
                _ => 0,
            }
    }
}

/// Version 1 of the portable event semantics. Additions are non-exhaustive;
/// incompatible interpretations require a new projection version/type.
/// Sensitive fields are omitted from Debug; explicit access is host-controlled.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventV1 {
    /// Native origin and transport position.
    pub context: EventContext,
    /// Original method, also retained for unprojected/unknown messages.
    pub native_method: String,
    /// Small, conservative interpretation of the native observation.
    pub kind: EventKind,
}

impl EventV1 {
    /// Semantic projection version, unrelated to the provider protocol version.
    pub const VERSION: u16 = 1;

    #[cfg(any(feature = "codex", feature = "acp", feature = "claude"))]
    pub(crate) fn retained_bytes(&self) -> usize {
        self.context.retained_bytes()
            + self.native_method.len()
            + match &self.kind {
                EventKind::Text { text, .. } => text.len(),
                _ => 0,
            }
    }
}

impl fmt::Debug for EventV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EventV1(<redacted>)")
    }
}

/// Projected observations. Unknown or incompletely understood payloads stay Other;
/// they never imply success. Native envelopes retain unprojected fields when enabled.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventKind {
    /// Native session creation notification; not necessarily the managed root.
    SessionStarted,
    /// Native turn started.
    TurnStarted,
    /// Native terminal turn notification; not process exit or session idle.
    TurnCompleted {
        /// Explicit native terminal classification.
        status: CompletionStatus,
    },
    /// Agent message text, scoped to its item. Final text is not turn completion.
    Text {
        /// Delta to append or complete text replacing the item's previous text.
        text: String,
        /// How this text changes the item.
        update: TextUpdate,
        /// Explicit native phase, without inferring final output from item completion.
        phase: TextPhase,
    },
    /// Tool item lifecycle only; completion does not establish execution success.
    Tool {
        /// Supported tool category; details remain in the native envelope.
        tool: ToolKind,
        /// Lifecycle notification, not an approval decision or tool result.
        state: ItemState,
    },
    /// A usage snapshot; never a terminal-turn signal or an incremental bill.
    Usage(Usage),
    /// Native command/file approval request. Only an attached capability can reply.
    ApprovalRequested,
    /// Provider resolved a reverse request; this is not execution evidence.
    ApprovalResolved,
    /// Unknown method, unsupported item, or insufficient fields for a projection.
    Other,
}

/// Native terminal classification shared by projections and adapter snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompletionStatus {
    /// Provider reported completion.
    Completed,
    /// Provider reported interruption.
    Interrupted,
    /// Provider reported failure.
    Failed,
    /// Future status; never interpreted as success.
    Unknown,
}

/// Text mutation semantics within one item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextUpdate {
    /// Append this fragment.
    Delta,
    /// Replace the item's accumulated text with this complete snapshot.
    Replace,
}

/// Native message phase. Absence or a new value is never assumed final.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextPhase {
    /// Explicit commentary.
    Commentary,
    /// Explicit final answer; the turn may still be active.
    Final,
    /// Phase absent or unsupported (including Codex's phase-less text deltas).
    Unknown,
}

/// Tool categories projected by the current adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolKind {
    /// Command execution item.
    Command,
    /// File change item.
    FileChange,
}

/// Item lifecycle, separate from the containing turn's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ItemState {
    /// Item started.
    Started,
    /// Item completed; inspect native status for success, failure or denial.
    Completed,
}

/// Provider token counters; optional details are absent rather than invented zeros.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TokenUsage {
    /// Provider-reported total (not recomputed).
    pub total_tokens: u64,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cached input tokens, if supplied.
    pub cached_input_tokens: Option<u64>,
    /// Cache write input tokens, if supplied.
    pub cache_write_input_tokens: Option<u64>,
    /// Reasoning output tokens, if supplied.
    pub reasoning_output_tokens: Option<u64>,
}

/// Native usage snapshot. `last` is the provider's last usage sample, which need
/// not be the whole turn; `total` is cumulative for the session. Do not sum updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Usage {
    /// Cumulative session counters.
    pub total: TokenUsage,
    /// Last usage sample.
    pub last: TokenUsage,
    /// Model context window when reported.
    pub model_context_window: Option<u64>,
}
