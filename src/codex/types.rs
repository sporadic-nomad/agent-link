use std::fmt;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::SessionId;
use crate::portable::{EventContext, EventV1, Usage};
use crate::protocol::{Completion, Queue};
use crate::rpc::{RpcTransportError, RpcWriteOutcome};

use super::Shared;

/// Retention of complete native envelopes in events and usage observations.
/// Snapshot server/thread/terminal results and pending approval details are still
/// retained within their separate bounds. This does not control transport logging.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum NativeEvents {
    /// Preserve native envelopes and additive fields (the compatibility default).
    #[default]
    Retain,
    /// Keep portable observations only; unprojected fields are deliberately discarded.
    Omit,
}

/// Adapter configuration. No callbacks or automatic approval policy are installed.
#[derive(Clone, Copy, Debug, Default)]
#[non_exhaustive]
pub struct CodexOptions {
    /// Finite resource budgets and deadlines.
    pub limits: CodexLimits,
    /// Native event retention; defaults to [`NativeEvents::Retain`].
    pub native_events: NativeEvents,
}

/// Finite adapter budgets, separate from the underlying transport's limits.
/// Defaults are library resource policy, not vendor protocol maxima.
/// Byte budgets count serialized frames and extracted strings, with parsed JSON
/// bounded by frame size and serde_json's recursion limit. Host-retained events,
/// results and capability metadata are host-owned memory.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct CodexLimits {
    /// Maximum incoming/outgoing frame including newline. Default: 8 MiB.
    pub max_frame_bytes: usize,
    /// Ordinary requests awaiting a response or write completion. Default: 32.
    pub pending_requests: usize,
    /// Serialized ordinary requests retained through response and write completion. Default: 32 MiB.
    pub pending_bytes: usize,
    /// Reserved control operations (interrupts, replies and notifications). Default: 8.
    pub control_capacity: usize,
    /// Separate serialized control budget. Default: 32 MiB.
    pub control_bytes: usize,
    /// Pending host approvals. Default: 16.
    pub approvals: usize,
    /// Pending approval frame bytes plus extracted origin strings. Default: 16 MiB.
    pub approval_bytes: usize,
    /// Unread host events. Default: 64.
    pub events: usize,
    /// Unread frame bytes plus projection strings and attached approval data, even
    /// when native envelopes are omitted. Fixed overhead is bounded by events. Default: 32 MiB.
    pub event_bytes: usize,
    /// Time allowed for a host approval, measured from ingress. Default: 5 minutes.
    pub approval_timeout: Duration,
    /// Time allowed for internal control writes. Default: 10 seconds.
    pub control_timeout: Duration,
}

impl Default for CodexLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 8 * 1024 * 1024,
            pending_requests: 32,
            pending_bytes: 32 * 1024 * 1024,
            control_capacity: 8,
            control_bytes: 32 * 1024 * 1024,
            approvals: 16,
            approval_bytes: 16 * 1024 * 1024,
            events: 64,
            event_bytes: 32 * 1024 * 1024,
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
    #[error("Codex connection closed")]
    Closed,
    /// The child ended its stdout or the transport disconnected.
    #[error("Codex connection reached EOF")]
    Eof,
    /// A transport-level failure.
    #[error("Codex transport failed: {0}")]
    Transport(RpcTransportError),
    /// A write could not be admitted to the transport. Failed admitted receipts drain
    /// preceding native frames and terminate through EOF, a transport error or deadline.
    #[error("Codex write failed")]
    WriteFailed,
    /// An admitted request's total deadline expired. Execution may be unknown.
    #[error("Codex request deadline exceeded")]
    DeadlineExceeded,
    /// A host approval expired. No permission is granted by this outcome.
    #[error("Codex approval deadline exceeded")]
    ApprovalExpired,
    /// Host events could not be retained within their count/byte budgets.
    #[error("Codex event consumer lagged or disconnected")]
    ConsumerLagged,
    /// The finite server request/control registry filled.
    #[error("Codex control capacity exhausted")]
    Overloaded,
    /// Invalid JSON/envelope, missing required fields, or conflicting native identity.
    #[error("invalid Codex protocol message")]
    Protocol,
}

/// Local refusal, before an ordinary request is published. No automatic retry occurs.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CodexError {
    /// Spawn, transport preflight, or host input validation failed.
    /// `crate::Error::Closed` also refuses new requests after observed input closure
    /// while the adapter can still drain native output without a terminal failure.
    #[error(transparent)]
    Transport(#[from] crate::Error),
    /// Adapter is terminal.
    #[error("Codex connection is closed: {0}")]
    Closed(Failure),
    /// Initialize first; this pilot owns at most one thread per connection.
    #[error("operation is invalid in the current Codex state")]
    InvalidState,
    /// A turn is active (possibly awaiting its ID) or a mutation awaits acknowledgement.
    #[error("Codex thread is busy; use explicit expected-turn steering")]
    Busy,
    /// The supplied turn does not match the observed active turn.
    #[error("Codex active turn does not match")]
    TurnMismatch,
    /// The token was answered, cancelled, expired or belongs to a closed connection.
    /// Observed input-only closure also refuses replies without consuming the token.
    #[error("Codex approval is no longer pending")]
    StaleApproval,
    /// An unoffered decision was refused and the connection closed without approving.
    #[error("Codex approval decision was not offered")]
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
    /// Wire ID, minted from the connection generation and a monotonic counter.
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

/// Native terminal turn state, separate from transport exit and request acceptance.
pub use crate::portable::CompletionStatus as TurnStatus;

/// Latest usage observation with its own native identity/order. Committed before
/// event publication. This is a sample, not proof that final usage has arrived.
#[derive(Clone)]
#[non_exhaustive]
pub struct UsageObservation {
    /// Origin of the usage sample.
    pub context: EventContext,
    /// Provider counters; do not add cumulative updates together.
    pub usage: Usage,
    /// Complete bounded notification, governed by [`CodexOptions::native_events`].
    pub native: Option<Arc<Value>>,
}

/// Latest observed terminal turn. The payload may contain summary items only.
#[derive(Clone)]
pub struct TurnOutcome {
    /// Origin and transport order of the terminal response or notification.
    /// Session/turn IDs match thread_id/turn_id; this is not a persistence cursor.
    pub context: EventContext,
    /// Native conversation ID.
    pub thread_id: String,
    /// Native turn ID.
    pub turn_id: String,
    /// Native outcome classification.
    pub status: TurnStatus,
    /// Complete bounded turn object, including errors and unknown fields.
    pub native: Arc<Value>,
    /// Most recent usage observed for this turn, if any. Late matching usage can
    /// update this field; absence is not zero usage or proof of complete reporting.
    /// Other-turn updates do not discard the active turn's sample or usage already
    /// attached to this same latest terminal outcome when it is reobserved.
    pub usage: Option<UsageObservation>,
}

/// Small authoritative snapshot, committed before request/event publication.
/// This pilot retains the root thread and its latest terminal turn, not a transcript.
/// Other threads' notifications are forwarded without modifying this snapshot.
#[derive(Clone, Default)]
pub struct Snapshot {
    /// Initialize succeeded and initialized was queued ahead of subsequent work.
    pub initialized: bool,
    /// Complete initialize result (server identity/platform; not schema negotiation).
    pub server: Option<Arc<Value>>,
    /// Root identity from the correlated start result or explicit resume admission.
    /// Unsolicited notifications cannot establish or change it.
    pub thread_id: Option<String>,
    /// Effective thread/start or thread/resume response, including effective policy.
    pub thread: Option<Arc<Value>>,
    /// Observed active turn, including evidence from command/file approvals replayed
    /// during resume. Answering or resolving an approval does not clear this identity;
    /// a matching terminal turn does. Absence is not proof of idle during a pending
    /// start or after resume reports activity without a turn ID; turn_start stays busy.
    pub active_turn: Option<String>,
    /// Latest terminal turn observation; retained when consumers stop reading.
    pub last_turn: Option<TurnOutcome>,
    /// Latest root usage sample, tagged with its own turn and transport order.
    /// Ordered by arrival; it may describe a different turn from active_turn or last_turn.
    /// Child usage never changes this or the terminal turn's usage.
    pub last_usage: Option<UsageObservation>,
    /// Sticky connection failure, independent of event capacity.
    pub failure: Option<Failure>,
}

/// Origin-bound, single-use approval capability. Clones share the same claim.
/// No callbacks run on the dispatcher. Dropping every clone sends nothing; expiry
/// terminates the owned connection. Host decisions are not tool-execution receipts.
/// A terminal result or notification for its turn invalidates a pending token,
/// as do native request resolution and connection shutdown.
#[derive(Clone)]
pub struct Approval {
    pub(super) shared: Weak<Shared>,
    pub(super) key: String,
    pub(super) nonce: u64,
    pub(super) request: Arc<ApprovalRequest>,
}

/// Immutable native request details carried by an approval capability. These remain
/// readable after cancellation/expiry for host audit, without keeping the child alive.
/// Details are sensitive and omitted from Debug. Token clones share this allocation.
#[derive(Clone)]
#[non_exhaustive]
pub struct ApprovalRequest {
    /// Provider, generation, sequence and native session/turn/item/request identities.
    pub context: EventContext,
    /// Original numeric/string wire request ID.
    pub id: Value,
    /// Native approval method.
    pub method: String,
    /// Complete request parameters, including native approval IDs, details and choices.
    /// Retained even with native event omission so hosts can make informed decisions.
    pub params: Arc<Value>,
    /// Absolute local deadline, measured from request ingress.
    pub deadline: Instant,
}

impl fmt::Debug for ApprovalRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApprovalRequest(<redacted>)")
    }
}

impl ApprovalRequest {
    /// Decisions supported by this adapter and offered by this request. Preserves
    /// exact native amendment objects. If native choices are absent/null, returns
    /// the four stable choices. Unknown choices remain in params but cannot approve.
    /// The returned allocation belongs to the caller; this does not claim the token.
    pub fn decisions(&self) -> Vec<Value> {
        if let Some(offered) = self
            .params
            .get("availableDecisions")
            .filter(|v| !v.is_null())
        {
            return offered
                .as_array()
                .into_iter()
                .flatten()
                .filter(|choice| super::driver::decision_supported(self, choice))
                .cloned()
                .collect();
        }
        ["accept", "acceptForSession", "decline", "cancel"]
            .into_iter()
            .map(Value::from)
            .collect()
    }
}

impl fmt::Debug for Approval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Approval(<redacted>)")
    }
}

impl Approval {
    /// Immutable origin, deadline and native details. Reading these is not a
    /// freshness check; only respond atomically claims the live capability.
    pub fn request(&self) -> &ApprovalRequest {
        &self.request
    }

    /// Answers with a native decision: accept, acceptForSession, decline, cancel,
    /// or an exact offered amendment object. An invalid choice closes the connection
    /// without granting permission. Return value observes only writing the reply.
    /// While input remains open, expired tokens return [`CodexError::StaleApproval`]
    /// and terminate the connection with [`Failure::ApprovalExpired`], even if
    /// dispatcher maintenance has not run. Observed input closure instead refuses
    /// the reply as stale without consuming its token or changing terminal state.
    pub fn respond(&self, decision: Value) -> Result<ReplyTicket, CodexError> {
        super::approval_reply(self, decision)
    }
}

/// Write observation for an approval reply, not provider acceptance or execution.
#[derive(Clone)]
pub struct ReplyTicket(pub(super) Arc<Completion<ReplyOutcome>>);
impl ReplyTicket {
    /// Reads the sticky reply write outcome.
    pub fn outcome(&self) -> Option<Arc<ReplyOutcome>> {
        self.0.get()
    }
    /// Blocks until the supplied observation deadline without cancelling the reply.
    pub fn wait_until(&self, deadline: Instant) -> Option<Arc<ReplyOutcome>> {
        self.0.wait_until(deadline)
    }
}

/// Outcome of an admitted control response.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplyOutcome {
    /// Transport finished writing and flushing; the provider has not acknowledged it.
    Written,
    /// Reply was not confirmed; execution cannot be inferred.
    Unknown(Failure),
}

/// Versioned portable projection plus optional bounded native notification/request.
/// Raw JSON is preserved by default, including unknown extensions.
/// Includes other threads' notifications (such as subagents), which do not update
/// the root snapshot. Hosts must inspect native thread identities when routing events.
#[non_exhaustive]
pub struct CodexEvent {
    /// Connection generation, never a provider session ID.
    pub generation: SessionId,
    /// Monotonic transport line sequence (responses also consume sequence numbers).
    pub seq: u64,
    /// Version 1 projection. Context repeats generation/seq for independent routing.
    pub portable: EventV1,
    /// Complete native envelope, retained by default. Sensitive; logging requires
    /// explicit RpcSpawnSpec::trace. Configure omission via CodexOptions.
    pub native: Option<Arc<Value>>,
    /// Present for supported command/file approval requests on the root thread only.
    /// Other threads' requests receive -32601 and are forwarded without a token.
    pub approval: Option<Approval>,
}

/// A single bounded host event consumer. Dropping it causes terminal ConsumerLagged
/// at the next publication. Failure remains readable through the adapter snapshot.
pub struct CodexEvents(pub(super) Arc<Queue<CodexEvent>>);
impl CodexEvents {
    /// Waits until an event, closure or deadline. None means timeout or drained closure;
    /// inspect the adapter's failure() to distinguish terminal state.
    pub fn recv_until(&self, deadline: Instant) -> Option<CodexEvent> {
        self.0.recv_until(deadline)
    }
}
impl Drop for CodexEvents {
    fn drop(&mut self) {
        self.0.close();
    }
}
