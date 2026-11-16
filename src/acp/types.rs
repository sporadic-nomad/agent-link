use super::Shared;
use crate::protocol::{Completion, Queue};
use crate::rpc::{RpcTransportError, RpcWriteOutcome};
use serde_json::Value;
use std::fmt;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// Retention of native event envelopes; metadata and request results are independent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum NativeEvents {
    /// Preserve additive native fields (default).
    #[default]
    Retain,
    /// Keep portable observations only; permission capabilities retain their params.
    Omit,
}
/// Connection options; no host callbacks or automatic approval policy are installed.
#[derive(Clone, Copy, Debug, Default)]
#[non_exhaustive]
pub struct AcpOptions {
    /// Finite resource budgets.
    pub limits: AcpLimits,
    /// Native event retention; results and latest terminal metadata remain available.
    pub native_events: NativeEvents,
}

/// Finite adapter budgets, separate from the underlying transport's limits.
/// Byte budgets count serialized frames and extracted strings, with parsed JSON
/// bounded by frame size and serde_json's recursion limit. Host-retained events,
/// results and capability metadata are host-owned memory.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct AcpLimits {
    /// Maximum incoming/outgoing frame including newline. Default: 1 MiB.
    pub max_frame_bytes: usize,
    /// Ordinary requests awaiting a response or write completion. Default: 32.
    pub pending_requests: usize,
    /// Serialized ordinary requests retained through response and write completion. Default: 4 MiB.
    pub pending_bytes: usize,
    /// Reserved control operations (interrupts, replies and notifications). Default: 8.
    pub control_capacity: usize,
    /// Separate serialized control budget. Default: 2 MiB.
    pub control_bytes: usize,
    /// Pending host approvals. Default: 16.
    pub approvals: usize,
    /// Pending approval frame bytes plus extracted origin strings. Default: 2 MiB.
    pub approval_bytes: usize,
    /// Unread host events. Default: 64.
    pub events: usize,
    /// Unread frame bytes plus projection strings and permission params.
    /// Fixed overhead is bounded by events. Default: 4 MiB.
    pub event_bytes: usize,
    /// Time allowed for a host approval, measured from ingress. Default: 5 minutes.
    pub approval_timeout: Duration,
    /// Time allowed for internal control writes. Default: 10 seconds.
    pub control_timeout: Duration,
}

impl Default for AcpLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 1024 * 1024,
            pending_requests: 32,
            pending_bytes: 4 * 1024 * 1024,
            control_capacity: 8,
            control_bytes: 2 * 1024 * 1024,
            approvals: 16,
            approval_bytes: 2 * 1024 * 1024,
            events: 64,
            event_bytes: 4 * 1024 * 1024,
            approval_timeout: Duration::from_secs(300),
            control_timeout: Duration::from_secs(10),
        }
    }
}

/// Sticky connection termination, available even when the event consumer is stalled.
/// Diagnostics contain no prompts, command arguments, stderr or provider error text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Failure {
    /// The owner closed the adapter.
    #[error("ACP connection closed")]
    Closed,
    /// The child ended its stdout or the transport disconnected.
    #[error("ACP connection reached EOF")]
    Eof,
    /// A transport-level failure.
    #[error("ACP transport failed: {0}")]
    Transport(RpcTransportError),
    /// A write could not be admitted to the transport. Failed admitted receipts drain
    /// preceding native frames and terminate through EOF, a transport error or deadline.
    #[error("ACP write failed")]
    WriteFailed,
    /// An admitted request's total deadline expired. Execution may be unknown.
    #[error("ACP request deadline exceeded")]
    DeadlineExceeded,
    /// A host approval expired. No permission is granted by this outcome.
    #[error("ACP permission deadline exceeded")]
    ApprovalExpired,
    /// Host events could not be retained within their count/byte budgets.
    #[error("ACP event consumer lagged or disconnected")]
    ConsumerLagged,
    /// The finite server request/control registry filled.
    #[error("ACP control capacity exhausted")]
    Overloaded,
    /// Invalid JSON/envelope, missing required fields, or conflicting native identity.
    #[error("invalid ACP protocol message")]
    Protocol,
}

/// Local refusal, before an ordinary request is published. No automatic retry occurs.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AcpError {
    /// Spawn, transport preflight, or host input validation failed.
    #[error(transparent)]
    Transport(#[from] crate::Error),
    /// Adapter is terminal.
    #[error("ACP connection is closed: {0}")]
    Closed(Failure),
    /// Initialize first; this pilot owns at most one session per connection.
    #[error("operation is invalid in the current ACP state")]
    InvalidState,
    /// A prompt is active or a session mutation awaits acknowledgement.
    #[error("ACP session is busy")]
    Busy,
    /// The token was answered, cancelled, expired or belongs to a closed connection.
    #[error("ACP permission is no longer pending")]
    StaleApproval,
    /// An unoffered decision was refused without consuming the token or approving.
    #[error("ACP permission decision was not offered")]
    InvalidDecision,
}

/// Terminal request observation. A successful response acknowledges the native
/// method; it does not imply turn completion. Unknown outcomes must never be resent
/// automatically. Payloads are deliberately omitted from Debug.
#[derive(Clone)]
#[non_exhaustive]
pub enum RequestOutcome {
    /// Native method returned successfully, preserving additive result fields.
    Response(Arc<Value>),
    /// A notification or reverse reply was written and flushed. This is transport
    /// evidence only: no native response, approval effect or cancellation is inferred.
    Written,
    /// Native error, preserving its numeric code, message and optional data.
    RemoteError(Arc<Value>),
    /// No bytes were emitted: the request was not dispatched, or its receipt proves no emission.
    NotSent(Failure),
    /// No native result was observed; bytes may have reached the provider.
    OutcomeUnknown {
        /// Why observation ended.
        failure: Failure,
        /// Last terminal transport receipt, if available. Written is not native acceptance.
        write: Option<RpcWriteOutcome>,
    },
}

impl fmt::Debug for RequestOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Written => f.write_str("Written"),
            Self::Response(_) => f.write_str("Response(<redacted>)"),
            Self::RemoteError(_) => f.write_str("RemoteError(<redacted>)"),
            Self::NotSent(reason) => f.debug_tuple("NotSent").field(reason).finish(),
            Self::OutcomeUnknown { failure, write } => f
                .debug_struct("OutcomeUnknown")
                .field("failure", failure)
                .field("write", write)
                .finish(),
        }
    }
}

/// Generation-scoped request observation. Dropping it does not cancel remote work.
/// Completion is independent of event consumption, with no adapter completion history.
/// A native response observed before its request deadline remains authoritative even
/// when the transport settled its write receipt as uncertain before delivering that response.
#[derive(Clone)]
pub struct RequestTicket {
    pub(super) id: String,
    pub(super) completion: Arc<Completion<RequestOutcome>>,
}

impl RequestTicket {
    /// Generation-scoped monotonic wire ID; empty for notifications and reverse replies.
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Sticky outcome, if settled.
    pub fn outcome(&self) -> Option<Arc<RequestOutcome>> {
        self.completion.get()
    }
    /// Blocks until the observation deadline. None does not cancel the request; its
    /// admission deadline still applies. Use a blocking worker in async applications.
    pub fn wait_until(&self, deadline: Instant) -> Option<Arc<RequestOutcome>> {
        self.completion.wait_until(deadline)
    }
}

/// Current connection state. Native metadata is bounded by the frame budget.
#[derive(Clone, Default)]
pub struct Snapshot {
    /// Complete initialize result, including additive capability fields.
    pub initialized: Option<Arc<Value>>,
    /// Managed or provisionally restoring native session identity.
    pub session_id: Option<String>,
    /// A prompt awaits its native response; cancellation does not clear this.
    pub prompting: bool,
    /// Sticky terminal reason, independent of event consumption.
    pub failure: Option<Failure>,
    /// Latest native prompt result/error, committed before event publication.
    pub last_prompt: Option<PromptObservation>,
}

/// Latest terminal prompt observation, independent of event consumption.
/// Retains complete native result/error including additive usage or stop metadata.
#[derive(Clone)]
pub struct PromptObservation {
    /// Native session/request and transport ordering; ACP turn ID remains absent.
    pub context: crate::portable::EventContext,
    /// Only native Response or RemoteError; no text/write is interpreted as terminal.
    pub outcome: RequestOutcome,
}

/// Bounded arrival-ordered observation; payloads are sensitive and have no Debug.
#[derive(Clone)]
pub struct AcpEvent {
    /// Conservative versioned interpretation.
    pub portable: crate::portable::EventV1,
    /// Original envelope, including unknown fields.
    pub native: Option<Arc<Value>>,
    /// Only permission requests carry a reply capability.
    pub permission: Option<Permission>,
}

/// Single consumer; dropping it does not cancel admitted work.
pub struct AcpEvents(pub(super) Arc<Queue<AcpEvent>>);
impl AcpEvents {
    /// Waits on the current thread until an event, closure or deadline.
    pub fn recv_until(&self, deadline: Instant) -> Option<AcpEvent> {
        self.0.recv_until(deadline)
    }
}

/// Single-use permission capability bound to a connection, request and session.
/// Clones share the registry entry; no reply is inferred from a label or option kind.
#[derive(Clone)]
pub struct Permission {
    pub(super) id: Value,
    pub(super) deadline: Instant,
    pub(super) origin: (crate::SessionId, u64),
    pub(super) shared: Weak<Shared>,
    pub(super) key: String,
    pub(super) nonce: u64,
    /// Exact complete request params, including offered option IDs and tool details.
    pub(super) request: Arc<Value>,
}
impl Permission {
    /// Complete immutable original native parameters. Reading is not a freshness check.
    pub fn request(&self) -> &Value {
        &self.request
    }
    /// Exact immutable native reverse request identity.
    pub fn request_id(&self) -> &Value {
        &self.id
    }
    /// Absolute local capability deadline.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    /// Local generation and ingress sequence.
    pub fn origin(&self) -> (crate::SessionId, u64) {
        self.origin
    }
    /// Chooses an exactly offered option. Invalid choices never authorize execution.
    pub fn select(&self, option_id: &str) -> Result<RequestTicket, AcpError> {
        self.reply(Some(option_id))
    }
    /// Explicit native cancelled outcome; this is not an approval.
    pub fn cancel(&self) -> Result<RequestTicket, AcpError> {
        self.reply(None)
    }
    fn reply(&self, option: Option<&str>) -> Result<RequestTicket, AcpError> {
        let shared = self.shared.upgrade().ok_or(AcpError::StaleApproval)?;
        shared.permission_reply(&self.key, self.nonce, option)
    }
}
