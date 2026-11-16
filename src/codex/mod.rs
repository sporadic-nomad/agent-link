//! Optional Codex app-server v2 pilot over owned, strict UTF-8 stdio (`codex` feature).
//!
//! One dispatcher drains responses, notifications and reverse requests independently
//! of the host event consumer. Finite event overflow is terminal: inspect
//! [`CodexAppServer::failure`] even when events are full. Every admitted request has an
//! absolute deadline covering queueing, writing and response observation. EOF, invalid
//! protocol, timeout and shutdown settle all tickets; unknown mutations are never resent.
//! Failed write receipts stop further dispatch while preceding native frames drain to
//! the ordered transport EOF/error. Request/approval deadlines and event limits still
//! apply during draining; receipts cannot replace an on-time native response.
//! Host JSON receives a bounded structural check before controller locking so
//! cleanup of oversized input cannot hold the lock and delay shutdown.
//! Events pair [`crate::portable::EventV1`] with an optional native envelope (retained
//! by default; configure [`CodexOptions`]). Text mutations, phases, usage and terminal
//! observations stay distinct. Root identity, terminal metadata and observed usage
//! commit before publication. [`Approval::request`] exposes origin, deadline and choices.
//!
//! This pilot manages one root thread per connection. Other threads' notifications
//! (including subagents) share the bounded event stream without changing the root
//! snapshot. Their approval requests receive -32601 without a host approval token.
//! Initialize, then start or resume the root. Busy starts are refused; steering and
//! interruption require the observed nonempty turn ID. Resume never falls back to
//! creating a new conversation. Command and file approvals use [`Approval`];
//! unsupported reverse methods receive -32601.
//! No host callbacks, runtime, vendor SDK, reconnection or persistence are installed.
//!
//! [`CodexAppServer::spawn`] accepts an explicit [`RpcSpawnSpec`]. Profile launches keep
//! their YOLO/unattended default; select `Permissions::AgentDefault` or `Custom` in
//! `RpcLaunchOptions` to request a different policy. Native thread settings can override
//! launch defaults; inspect [`Snapshot::thread`] for the returned effective settings.
//! Socket/borrowed endpoints, ACP and Claude are separate adapters, not JSON-line modes
//! of this API. See `docs/rpc-and-native-adapters.md#codex-app-server-codex` in the
//! public manual for limits and its validation-evidence section for provenance.

/// Upstream schema revision used by this adapter's synthetic conformance fixtures.
/// Descriptive provenance, not a CLI version allowlist or negotiated capability.
pub const PROTOCOL_REVISION: &str = "b1573f102413e96a6481087922bf425231222dee";

/// Evidence for the adapter subset. A mock pass does not certify live interoperability;
/// initialize retains server identity but does not negotiate this schema revision.
pub const COMPATIBILITY_EVIDENCE: &[crate::agent::compatibility::Evidence] = &[
    crate::agent::compatibility::Evidence {
        operation: "app-server v2 initialize, root thread/turn lifecycle, events and command/file approvals",
        kind: crate::agent::compatibility::EvidenceKind::SyntheticFixture,
        cli_version: None,
        platform: None,
        source: "tests/fixtures/codex-app-server/v2.json",
    },
];

mod driver;
mod projection;
#[cfg(test)]
mod tests;
mod types;

pub use types::*;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::{Sender, bounded};
use serde_json::{Value, json};

use crate::protocol::{Completion, Queue, lock};
use crate::rpc::{RpcChild, RpcSpawnSpec, RpcUtf8Mode, RpcWriteReceipt};
use crate::{Error, SessionId};

#[derive(Clone)]
enum Action {
    Initialize,
    ThreadStart,
    ThreadResume(String),
    TurnStart,
    Steer(String),
    Interrupt(String),
    Control,
}

type Pending = crate::protocol::Pending<Action, RequestOutcome, ReplyOutcome>;

struct PendingApproval {
    nonce: u64,
    request: Arc<ApprovalRequest>,
    bytes: usize,
}

#[derive(Default)]
struct State {
    snapshot: Snapshot,
    // One root sample for the observed active turn, independent of arrival-ordered
    // last_usage. Cleared on matching completion; this is not a per-turn history.
    active_usage: Option<UsageObservation>,
    next_id: u64,
    initialized_pending: bool,
    thread_pending: bool,
    mutation_pending: bool,
    // Explicit activity without a turn ID is busy until replay identifies it.
    unidentified_active_turn: bool,
    // Identifying activity during pending resume outlives its matching completion;
    // a delayed identity-less snapshot must not restore the unknown-activity guard.
    resume_activity_observed: bool,
    // Terminal identities since the latest thread/turn start admission. Bounded by
    // one frame of ID bytes and the event count; protects response/event reordering.
    terminal_turns: HashSet<String>,
    pending: VecDeque<Pending>,
    approvals: HashMap<String, PendingApproval>,
}

struct Shared {
    generation: SessionId,
    limits: CodexLimits,
    native_events: NativeEvents,
    state: Mutex<State>,
    events: Arc<Queue<CodexEvent>>,
    wake: Sender<()>,
    cleanup: Mutex<Option<crate::rpc::RpcReapObservation>>,
    dispatcher_finished: std::sync::atomic::AtomicBool,
}

/// Owned Codex app-server connection. Send + Sync; no async runtime required.
/// Drop/close settle waiters immediately and wake the dispatcher to kill/reap its
/// child using RpcChild's bounded teardown. They do not wait for that asynchronous
/// reap. The dispatcher retains ownership until teardown finishes.
pub struct CodexAppServer {
    shared: Arc<Shared>,
}

impl CodexAppServer {
    /// Validates every adapter/transport budget before spawning. Forces strict UTF-8,
    /// caps stdout at max_frame_bytes, and uses two maximum-sized stdin slots: one
    /// ordinary write and one reserved control write. Remaining transport settings
    /// (stderr limits, trace, environment, launch) are honored. Adds one dispatcher
    /// thread; spawn failure tears down the child. No protocol request is sent yet.
    pub fn spawn(spec: RpcSpawnSpec, limits: CodexLimits) -> crate::Result<(Self, CodexEvents)> {
        Self::spawn_with_options(
            spec,
            CodexOptions {
                limits,
                ..CodexOptions::default()
            },
        )
    }

    /// Spawns with explicit native-event retention and finite limits. Same validation,
    /// ownership and threading as [`Self::spawn`]. Native omission affects event/usage
    /// envelopes only; request outcomes, terminal state and approval details remain.
    /// RpcSpawnSpec::trace independently opts into sensitive transport logging.
    pub fn spawn_with_options(
        mut spec: RpcSpawnSpec,
        options: CodexOptions,
    ) -> crate::Result<(Self, CodexEvents)> {
        let limits = options.limits;
        validate_limits(limits)?;
        spec.stdout_utf8 = RpcUtf8Mode::Strict;
        spec.limits.max_stdout_line_bytes = spec
            .limits
            .max_stdout_line_bytes
            .min(limits.max_frame_bytes);
        spec.limits.max_stdin_line_bytes = limits.max_frame_bytes;
        spec.limits.max_pending_stdin_lines = 2;
        spec.limits.max_pending_stdin_bytes = limits.max_frame_bytes * 2;
        let (child, incoming) = RpcChild::spawn(spec)?;
        let (wake, wakes) = bounded(1);
        let shared = Arc::new(Shared {
            generation: child.id(),
            limits,
            native_events: options.native_events,
            state: Mutex::new(State::default()),
            events: Queue::new(limits.events, limits.event_bytes),
            wake,
            cleanup: Mutex::new(Some(child.reap_observation())),
            dispatcher_finished: std::sync::atomic::AtomicBool::new(false),
        });
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name(format!("agent-link-codex-{}", child.id()))
            .spawn(move || driver::run(worker, child, incoming, wakes))
            .map_err(|e| Error::Spawn(format!("spawn Codex dispatcher: {e}")))?;
        let events = CodexEvents(Arc::clone(&shared.events));
        Ok((Self { shared }, events))
    }

    /// Actual direct-child reap plus dispatcher completion. Pipe pump threads are
    /// detached; escaped descendants are outside this evidence.
    pub fn cleanup_status(&self) -> Option<crate::rpc::RpcReapStatus> {
        if !self
            .shared
            .dispatcher_finished
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        lock(&self.shared.cleanup).as_ref().map(|o| o.status())
    }

    /// Connection generation; never reused by this adapter or by a reply token.
    pub fn generation(&self) -> SessionId {
        self.shared.generation
    }
    /// Current bounded native state, including independently readable failure.
    pub fn snapshot(&self) -> Snapshot {
        lock(&self.shared.state).snapshot.clone()
    }
    /// Sticky terminal reason, independent of event consumption.
    pub fn failure(&self) -> Option<Failure> {
        lock(&self.shared.state).snapshot.failure.clone()
    }
    /// Ends observation and asynchronously kills/reaps owned stdio. Idempotent.
    /// Tickets settle before returning, including concurrent response/shutdown
    /// publications. Already observed native outcomes are preserved. This does not
    /// claim automatic remote turn cancellation.
    pub fn close(&self) {
        self.shared.stop(Failure::Closed);
    }

    /// Performs initialize with real application identity and no experimental opt-in.
    /// The result retains server identity. The dispatcher queues initialized before
    /// allowing ordinary requests. Repeated or concurrent initialization is refused.
    pub fn initialize(
        &self,
        name: &str,
        version: &str,
        deadline: Instant,
    ) -> Result<RequestTicket, CodexError> {
        self.check_text(name)?;
        self.check_text(version)?;
        self.request(
            Action::Initialize,
            json!({
                "clientInfo": {"name": name, "version": version},
                "capabilities": {"experimentalApi": false}
            }),
            deadline,
        )
    }

    /// Starts the connection's single thread. Params are a bounded native object
    /// (e.g. model/cwd/approvalPolicy); additive fields pass through to the provider.
    /// A second thread is refused. Native errors never trigger an automatic retry.
    pub fn thread_start(
        &self,
        params: Value,
        deadline: Instant,
    ) -> Result<RequestTicket, CodexError> {
        self.request(Action::ThreadStart, params, deadline)
    }

    /// Resumes the exact thread, preserving warm/cold provider semantics. Params may
    /// contain native settings but must not replace threadId, path or history.
    /// `excludeTurns: true` and invalid non-boolean values are refused with
    /// [`Error::InvalidInput`] before admission: this adapter needs turn history to
    /// guard against implicit steering. Omit excludeTurns or use false.
    /// A missing/mismatched history never silently creates a fresh thread.
    /// Replayed approvals establish the active turn even if the resume result omits
    /// it; after resume succeeds, use its exact ID for steering or interruption.
    /// An explicitly active response without a turn ID keeps new starts busy until
    /// a replayed approval or turn observation identifies the turn and it terminates.
    /// Identifying replay and its completion take precedence over a delayed active
    /// response even when that response omits the turn from history.
    pub fn thread_resume(
        &self,
        thread_id: &str,
        mut params: Value,
        deadline: Instant,
    ) -> Result<RequestTicket, CodexError> {
        self.check_text(thread_id)?;
        let object = params.as_object_mut().ok_or_else(invalid_params)?;
        if ["threadId", "path", "history"]
            .iter()
            .any(|key| object.contains_key(*key))
        {
            return Err(invalid_params());
        }
        // A metadata-only response can report an active thread without identifying
        // its turn. Require history; explicit activity without an ID still remains
        // busy when the result is applied, pending identifying replay.
        if !matches!(object.get("excludeTurns"), None | Some(Value::Bool(false))) {
            return Err(Error::InvalidInput(
                "Codex resume requires turn history; omit excludeTurns or set it to false".into(),
            )
            .into());
        }
        object.insert("threadId".into(), Value::String(thread_id.into()));
        self.request(Action::ThreadResume(thread_id.into()), params, deadline)
    }

    /// Starts a text turn only while locally observed idle. Refuses a concurrent or
    /// active turn, including unidentified activity reported by resume. Use turn_steer
    /// explicitly once the active ID is observed. This local guard cannot make the
    /// provider's start-or-steer operation atomic against other native clients.
    pub fn turn_start(&self, text: &str, deadline: Instant) -> Result<RequestTicket, CodexError> {
        self.check_text(text)?;
        self.request(Action::TurnStart, text_input(text), deadline)
    }

    /// Adds text to exactly the expected observed active turn using turn/steer.
    pub fn turn_steer(
        &self,
        expected_turn_id: &str,
        text: &str,
        deadline: Instant,
    ) -> Result<RequestTicket, CodexError> {
        self.check_text(expected_turn_id)?;
        self.check_text(text)?;
        self.request(
            Action::Steer(expected_turn_id.into()),
            text_input(text),
            deadline,
        )
    }

    /// Interrupts exactly the expected active turn through reserved control admission.
    /// Empty/startup wildcard IDs are refused. A reply and a terminal turn event are
    /// separate observations; neither implies process exit or durable persistence.
    pub fn turn_interrupt(
        &self,
        expected_turn_id: &str,
        deadline: Instant,
    ) -> Result<RequestTicket, CodexError> {
        self.check_text(expected_turn_id)?;
        self.request(
            Action::Interrupt(expected_turn_id.into()),
            json!({}),
            deadline,
        )
    }

    fn check_text(&self, text: &str) -> Result<(), CodexError> {
        if text.is_empty() || text.len() > self.shared.limits.max_frame_bytes {
            return Err(invalid_params());
        }
        Ok(())
    }

    fn request(
        &self,
        action: Action,
        mut params: Value,
        deadline: Instant,
    ) -> Result<RequestTicket, CodexError> {
        if deadline <= Instant::now() {
            return Err(invalid_params());
        }
        // Reject unbounded host JSON before taking the controller lock: even a
        // bounded validation failure can leave arbitrarily many values to drop.
        // Envelope encoding below still enforces the final wire size/depth limits.
        validate_json(&params, self.shared.limits.max_frame_bytes)?;
        let object = params.as_object_mut().ok_or_else(invalid_params)?;
        let mut state = lock(&self.shared.state);
        if let Some(failure) = &state.snapshot.failure {
            return Err(CodexError::Closed(failure.clone()));
        }
        let method = match &action {
            Action::Initialize => {
                if state.snapshot.initialized || state.initialized_pending {
                    return Err(CodexError::InvalidState);
                }
                "initialize"
            }
            _ if !state.snapshot.initialized => return Err(CodexError::InvalidState),
            Action::ThreadStart | Action::ThreadResume(_) => {
                if state.snapshot.thread_id.is_some() || state.thread_pending {
                    return Err(CodexError::InvalidState);
                }
                if matches!(action, Action::ThreadStart) {
                    "thread/start"
                } else {
                    "thread/resume"
                }
            }
            Action::TurnStart | Action::Steer(_) | Action::Interrupt(_) => {
                let thread = state
                    .snapshot
                    .thread_id
                    .as_ref()
                    .ok_or(CodexError::InvalidState)?;
                if state.thread_pending || state.snapshot.thread.is_none() {
                    return Err(CodexError::InvalidState);
                }
                if !matches!(action, Action::Interrupt(_)) && state.mutation_pending {
                    return Err(CodexError::Busy);
                }
                object.insert("threadId".into(), Value::String(thread.clone()));
                match &action {
                    Action::TurnStart => {
                        if state.snapshot.active_turn.is_some() || state.unidentified_active_turn {
                            return Err(CodexError::Busy);
                        }
                        "turn/start"
                    }
                    Action::Steer(expected) | Action::Interrupt(expected) => {
                        if state.snapshot.active_turn.as_ref() != Some(expected) {
                            return Err(CodexError::TurnMismatch);
                        }
                        if matches!(action, Action::Steer(_)) {
                            object.insert("expectedTurnId".into(), Value::String(expected.clone()));
                            "turn/steer"
                        } else {
                            if state
                                .pending
                                .iter()
                                .any(|p| matches!(p.action, Action::Interrupt(_)))
                            {
                                return Err(CodexError::Busy);
                            }
                            object.insert("turnId".into(), Value::String(expected.clone()));
                            "turn/interrupt"
                        }
                    }
                    _ => unreachable!(),
                }
            }
            Action::Control => unreachable!(),
        };
        let next = state
            .next_id
            .checked_add(1)
            .ok_or(CodexError::InvalidState)?;
        let id = format!("{}:{next}", self.shared.generation);
        // Move host JSON into the envelope; json! would recursively clone it before
        // our size/depth validator had a chance to reject oversized input.
        let envelope = Value::Object(serde_json::Map::from_iter([
            ("id".into(), Value::String(id.clone())),
            ("method".into(), Value::String(method.into())),
            ("params".into(), params),
        ]));
        let line = encode(&envelope, self.shared.limits.max_frame_bytes)?;
        let bytes = line.len() + 1;
        let control = matches!(action, Action::Interrupt(_));
        self.shared.admit(&state, bytes, control, control)?;
        let completion = Completion::new();
        match &action {
            Action::Initialize => state.initialized_pending = true,
            Action::ThreadStart => {
                state.thread_pending = true;
                state.terminal_turns.clear();
            }
            Action::ThreadResume(thread_id) => {
                state.thread_pending = true;
                state.resume_activity_observed = false;
                state.terminal_turns.clear();
                state.snapshot.thread_id = Some(thread_id.clone());
            }
            Action::TurnStart => {
                state.mutation_pending = true;
                state.terminal_turns.clear();
            }
            Action::Steer(_) => state.mutation_pending = true,
            _ => {}
        }
        state.next_id = next;
        state.pending.push_back(Pending {
            id: Some(id.clone()),
            action,
            line: Some(line),
            dispatched: false,
            receipt: None,
            deadline,
            bytes,
            control,
            request: Some(Arc::clone(&completion)),
            request_outcome: None,
            reply: None,
            responded: false,
        });
        drop(state);
        let _ = self.shared.wake.try_send(());
        Ok(RequestTicket { id, completion })
    }
}

impl Drop for CodexAppServer {
    fn drop(&mut self) {
        self.close();
    }
}

fn text_input(text: &str) -> Value {
    json!({"input": [{"type":"text", "text":text, "text_elements": []}]})
}
fn invalid_params() -> CodexError {
    Error::InvalidInput("invalid Codex request parameters or deadline".into()).into()
}

fn validate_limits(limits: CodexLimits) -> crate::Result<()> {
    let values = [
        limits.max_frame_bytes,
        limits.pending_requests,
        limits.pending_bytes,
        limits.control_capacity,
        limits.control_bytes,
        limits.approvals,
        limits.approval_bytes,
        limits.events,
        limits.event_bytes,
    ];
    if values
        .iter()
        .any(|&n| n == 0 || n > isize::MAX as usize / 16)
        || [
            (limits.pending_requests, std::mem::size_of::<Pending>()),
            (limits.control_capacity, std::mem::size_of::<Pending>()),
            (limits.events, std::mem::size_of::<CodexEvent>()),
            (limits.approvals, std::mem::size_of::<PendingApproval>()),
        ]
        .iter()
        .any(|(count, size)| {
            count
                .checked_mul(*size)
                .is_none_or(|bytes| bytes > isize::MAX as usize)
        })
        || limits.control_capacity < 2
        || limits.max_frame_bytes < 128
        || limits.control_bytes < limits.max_frame_bytes * 2
        || limits.pending_bytes < limits.max_frame_bytes
        || limits.approval_timeout.is_zero()
        || limits.control_timeout.is_zero()
        || Instant::now()
            .checked_add(limits.approval_timeout)
            .is_none()
        || Instant::now().checked_add(limits.control_timeout).is_none()
    {
        return Err(Error::InvalidInput("invalid Codex lifecycle limits".into()));
    }
    Ok(())
}

/// Bounds traversal and destruction work before serialization or controller locking.
fn validate_json(value: &Value, max: usize) -> crate::Result<()> {
    // Validate nesting before serde's recursive serializer. This traversal does not
    // allocate and stops after a frame's worth of minimum encoded bytes.
    fn validate(value: &Value, depth: usize, remaining: &mut usize) -> bool {
        if depth > 64 {
            return false;
        }
        let cost = match value {
            Value::String(text) => text.len().saturating_add(2),
            _ => 1,
        };
        if cost > *remaining {
            return false;
        }
        *remaining -= cost;
        match value {
            Value::Array(values) => values.iter().all(|v| validate(v, depth + 1, remaining)),
            Value::Object(values) => values.iter().all(|(key, v)| {
                let cost = key.len().saturating_add(3);
                if cost > *remaining {
                    return false;
                }
                *remaining -= cost;
                validate(v, depth + 1, remaining)
            }),
            _ => true,
        }
    }
    if !validate(value, 0, &mut (max - 1)) {
        return Err(Error::InvalidInput(
            "Codex JSON exceeds frame or nesting limits".into(),
        ));
    }
    Ok(())
}

/// Serializes without allocating beyond the frame budget, even for oversized input.
fn encode(value: &Value, max: usize) -> crate::Result<String> {
    validate_json(value, max)?;
    struct Capped {
        bytes: Vec<u8>,
        max: usize,
    }
    impl Write for Capped {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.max - self.bytes.len() {
                return Err(std::io::ErrorKind::FileTooLarge.into());
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Capped {
        bytes: Vec::new(),
        max: max - 1,
    };
    serde_json::to_writer(&mut sink, value).map_err(|_| Error::InputTooLarge {
        size: max.saturating_add(1),
        limit: max,
    })?;
    String::from_utf8(sink.bytes).map_err(|_| Error::InvalidInput("invalid JSON encoding".into()))
}

impl Shared {
    fn admit(
        &self,
        state: &State,
        bytes: usize,
        control: bool,
        reserve_reply: bool,
    ) -> crate::Result<()> {
        let (max_count, max_bytes) = if control {
            (
                self.limits.control_capacity - usize::from(reserve_reply),
                self.limits.control_bytes
                    - if reserve_reply {
                        self.limits.max_frame_bytes
                    } else {
                        0
                    },
            )
        } else {
            (self.limits.pending_requests, self.limits.pending_bytes)
        };
        crate::protocol::admit(&state.pending, bytes, control, max_count, max_bytes)
    }

    fn control(
        &self,
        state: &mut State,
        value: Value,
        reply: Option<Arc<Completion<ReplyOutcome>>>,
    ) -> Result<(), Failure> {
        let line = encode(&value, self.limits.max_frame_bytes).map_err(|_| Failure::Overloaded)?;
        let bytes = line.len() + 1;
        self.admit(state, bytes, true, false)
            .map_err(|_| Failure::Overloaded)?;
        state.pending.push_back(Pending {
            id: None,
            action: Action::Control,
            line: Some(line),
            dispatched: false,
            receipt: None,
            deadline: Instant::now() + self.limits.control_timeout,
            bytes,
            control: true,
            request: None,
            request_outcome: None,
            reply,
            responded: true,
        });
        Ok(())
    }

    fn stop(&self, failure: Failure) {
        let (requests, replies) = {
            let mut state = lock(&self.state);
            let failure = state.snapshot.failure.get_or_insert(failure).clone();
            state.approvals.clear();
            let mut requests = Vec::new();
            let mut replies = Vec::new();
            // Keep every completion registered until publication finishes. Another
            // close must be able to help settle tickets even after failure is set.
            for p in &mut state.pending {
                if let Some(completion) = &p.request {
                    let outcome = p.request_outcome.get_or_insert_with(|| {
                        let write = p.receipt.as_ref().and_then(|r| match r.status() {
                            crate::rpc::RpcWriteStatus::Finished(outcome) => Some(outcome),
                            _ => None,
                        });
                        if !p.dispatched
                            || matches!(write, Some(crate::rpc::RpcWriteOutcome::NotWritten(_)))
                        {
                            RequestOutcome::NotSent(failure.clone())
                        } else {
                            RequestOutcome::OutcomeUnknown {
                                failure: failure.clone(),
                                write,
                            }
                        }
                    });
                    requests.push((Arc::clone(completion), outcome.clone()));
                }
                if let Some(completion) = &p.reply {
                    let outcome = if matches!(
                        p.receipt.as_ref().map(RpcWriteReceipt::status),
                        Some(crate::rpc::RpcWriteStatus::Finished(
                            crate::rpc::RpcWriteOutcome::Written
                        ))
                    ) {
                        ReplyOutcome::Written
                    } else {
                        ReplyOutcome::Unknown(failure.clone())
                    };
                    replies.push((Arc::clone(completion), outcome));
                }
            }
            (requests, replies)
        };
        // Never settle/wake tickets or publish events under the controller lock.
        for (completion, outcome) in requests {
            completion.finish(outcome);
        }
        for (completion, outcome) in replies {
            completion.finish(outcome);
        }
        // Failure prevents further admissions; every retained ticket is now settled.
        let pending = std::mem::take(&mut lock(&self.state).pending);
        drop(pending);
        self.events.close();
        let _ = self.wake.try_send(());
    }
}

fn approval_reply(token: &Approval, decision: Value) -> Result<ReplyTicket, CodexError> {
    let shared = token.shared.upgrade().ok_or(CodexError::StaleApproval)?;
    let mut state = lock(&shared.state);
    if state.snapshot.failure.is_some() {
        return Err(CodexError::StaleApproval);
    }
    let approval = &state
        .approvals
        .get(&token.key)
        .filter(|a| a.nonce == token.nonce)
        .ok_or(CodexError::StaleApproval)?
        .request;
    let refusal = if approval.deadline <= Instant::now() {
        Some((Failure::ApprovalExpired, CodexError::StaleApproval))
    } else if !driver::decision_allowed(approval, &decision) {
        Some((Failure::Protocol, CodexError::InvalidDecision))
    } else {
        None
    };
    if let Some((failure, error)) = refusal {
        state.approvals.remove(&token.key); // consume before releasing the claim lock
        drop(state);
        shared.stop(failure);
        return Err(error);
    }
    let id = approval.id.clone();
    let completion = Completion::new();
    let result = shared.control(
        &mut state,
        json!({"id":id,"result":{"decision":decision}}),
        Some(Arc::clone(&completion)),
    );
    // All token clones are consumed together. No retargeting, even on write failure.
    state.approvals.remove(&token.key);
    drop(state);
    if let Err(failure) = result {
        shared.stop(failure.clone());
        return Err(CodexError::Closed(failure));
    }
    let _ = shared.wake.try_send(());
    Ok(ReplyTicket(completion))
}
