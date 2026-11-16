//! Persistent Claude stream-json adapter (`claude` feature).
//!
//! This is a custom control envelope, not JSON-RPC. Results end turns, never stdin.
//! Raw envelopes preserve extension fields and plural user-message attribution. No
//! turn identity is manufactured. Idle state, background tasks and EOF are separate.
//! Finite event overflow, deadlines and malformed protocol fail the session; failure
//! and tickets remain readable without draining events. No automatic resend occurs.
//! See `docs/rpc-and-native-adapters.md#claude-stream-json-claude` in the public manual
//! for limitations and its validation-evidence section for pinned evidence.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::portable::{CompletionStatus, EventContext, EventKind, EventV1, Provider};
use crate::protocol::{Completion, Queue, lock};
use crate::rpc::{
    RpcChild, RpcEvent, RpcSpawnSpec, RpcUtf8Mode, RpcWriteOutcome, RpcWriteReceipt, RpcWriteStatus,
};
use crate::{Error, SessionId};

/// Persistent stream-json launch matching the pinned SDK's transport flags.
/// Defaults to unattended tools. Select AgentDefault/Custom explicitly for restrictions;
/// `stdio_approvals` adds permission routing, but bypass/rules may shadow ask prompts.
/// Uses no `--print`, SDK identity, empty system prompt or credentials override.
pub fn launch(
    permissions: &crate::agent::Permissions,
    stdio_approvals: bool,
) -> crate::agent::RpcLaunch {
    let mut launch = crate::agent::RpcLaunch::new(
        "claude",
        &[
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--verbose",
        ],
    )
    .with_permissions(
        permissions,
        &["--permission-mode", "bypassPermissions"],
        &[],
    );
    if stdio_approvals {
        launch
            .args
            .extend(["--permission-prompt-tool".into(), "stdio".into()]);
    }
    launch
        .env
        .push(("CLAUDE_CODE_SDK_READS_SESSION_STATE".into(), "1".into()));
    launch
}

/// SDK source revision used by synthetic offline fixtures; not negotiated capability.
pub const PROTOCOL_REVISION: &str = "4720d367f7db9d5df8a166c74d7f36a538ac8c34";

/// Offline evidence for this bounded subset; does not certify live CLI compatibility.
pub const COMPATIBILITY_EVIDENCE: &[crate::agent::compatibility::Evidence] = &[
    crate::agent::compatibility::Evidence {
        operation: "persistent stream-json initialize, user/results, interrupt and stdio permission requests",
        kind: crate::agent::compatibility::EvidenceKind::SyntheticFixture,
        cli_version: None,
        platform: None,
        source: "tests/fixtures/claude-stream-json/events.json",
    },
];

/// Finite budgets. Pending includes active writes and unpublished completions.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum serialized frame bytes, including newline.
    pub frame_bytes: usize,
    /// Ordinary queued/active operations.
    pub pending: usize,
    /// Ordinary pending serialized frames and IDs, including active writes.
    pub pending_bytes: usize,
    /// Reserved control pending serialized frames and IDs.
    pub control_bytes: usize,
    /// Outgoing control requests; one additional slot is reserved for replies.
    pub control_pending: usize,
    /// Outstanding approval IDs and request envelopes.
    pub approval_bytes: usize,
    /// Host events retained independently of transport events.
    pub events: usize,
    /// Serialized native bytes plus projected identity bytes retained in events.
    pub event_bytes: usize,
    /// Incoming unresolved approvals.
    pub approvals: usize,
    /// Maximum tracked finite task IDs.
    pub tasks: usize,
    /// Aggregate bytes in tracked finite task IDs.
    pub task_bytes: usize,
    /// Maximum time a permission capability remains usable.
    pub approval_timeout: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            frame_bytes: 1024 * 1024,
            pending: 16,
            pending_bytes: 4 * 1024 * 1024,
            control_bytes: 2 * 1024 * 1024,
            control_pending: 4,
            approval_bytes: 1024 * 1024,
            events: 64,
            event_bytes: 4 * 1024 * 1024,
            approvals: 8,
            tasks: 16,
            task_bytes: 1024 * 1024,
            approval_timeout: Duration::from_secs(60),
        }
    }
}
/// Native event retention policy. Snapshots, tickets and approval requests are independent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NativeEvents {
    /// Retain original event envelopes (default).
    #[default]
    Retain,
    /// Omit event envelopes while preserving conservative portable evidence.
    Omit,
}
/// Spawn options; native event retention defaults on.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Finite adapter budgets.
    pub limits: Limits,
    /// Only event envelopes are affected.
    pub native_events: NativeEvents,
}
/// Sticky metadata-only terminal failure. It never includes prompts or stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    /// Host closed the generation.
    #[error("Claude session closed")]
    Closed,
    /// Transport ended; this is independent of a preceding result.
    #[error("Claude transport ended")]
    Eof,
    /// Invalid envelope, duplicate request or mismatched resume identity.
    #[error("Claude protocol violation")]
    Protocol,
    /// An operation or approval exceeded its absolute deadline.
    #[error("Claude operation deadline exceeded")]
    Deadline,
    /// A configured queue or approval bound was exceeded.
    #[error("Claude adapter capacity exceeded")]
    Overload,
    /// Transport failed; mutation outcome can be unknown.
    #[error("Claude transport failed")]
    Transport,
}
/// Control completion. Error envelopes are retained exactly, without success inference.
#[derive(Clone)]
pub enum Outcome {
    /// Native control response, including explicit provider errors.
    Response(Value),
    /// User/approval frame flushed. This does not establish provider acceptance.
    Written,
    /// Session failed. A dispatched operation may have reached the provider.
    Failed {
        /// Sticky terminal cause.
        failure: Failure,
        /// Emission was dispatched; delivery may be partial or complete. Never resend automatically.
        may_have_been_written: bool,
    },
}
/// Independently readable operation completion. Dropping it does not cancel input.
pub struct Ticket(Arc<Completion<Outcome>>);
impl Ticket {
    /// Current terminal outcome, independent of the event receiver.
    pub fn outcome(&self) -> Option<Arc<Outcome>> {
        self.0.get()
    }
    /// Waits to the host's absolute deadline; a wait timeout does not cancel input.
    pub fn wait_until(&self, deadline: Instant) -> Option<Arc<Outcome>> {
        self.0.wait_until(deadline)
    }
}
/// Latest provider observations; absence never means idle or quiescent.
#[derive(Clone, Default)]
pub struct Snapshot {
    /// Sticky terminal failure, independent of event capacity.
    pub failure: Option<Failure>,
    /// Successful initialize control response observed.
    pub initialized: bool,
    /// Exact provider identity; never the local generation.
    pub session_id: Option<String>,
    /// Latest explicit session-state value. Unknown values remain unchanged strings.
    pub session_state: Option<String>,
    /// Number of tracked finite task IDs; excludes shell tasks. Not a quiescence proof.
    pub finite_tasks: usize,
    /// Latest result envelope, including usage and plural user-message UUIDs; committed before events.
    pub last_result: Option<Arc<Value>>,
    /// Native initialize response; no capability or CLI version inferred.
    pub initialize_response: Option<Arc<Value>>,
}
/// Raw native envelope and conservative versioned projection. Sensitive: no Debug.
pub struct Event {
    /// Exact decoded native object, including unknown fields and content blocks.
    pub native: Option<Value>,
    /// Provider evidence only; no synthetic turn ID.
    pub portable: EventV1,
    /// Present only for supported can_use_tool reverse requests.
    pub approval: Option<Approval>,
}
/// Single-consumer event stream; overflow fails the generation without blocking routing.
pub struct Events(Arc<Queue<Event>>);
impl Events {
    /// Waits to an absolute deadline. None means timeout or closed/drained stream.
    pub fn recv_until(&self, deadline: Instant) -> Option<Event> {
        self.0.recv_until(deadline)
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Initialize,
    Control,
    Write,
}
struct Pending {
    id: String,
    action: Action,
    line: Option<String>,
    receipt: Option<RpcWriteReceipt>,
    deadline: Instant,
    ticket: Arc<Completion<Outcome>>,
    committed: Option<Outcome>,
    control: bool,
    bytes: usize,
    approval_id: Option<String>,
    dispatched: bool,
}
struct Permission {
    bytes: usize,
    nonce: u64,
    deadline: Instant,
}
#[derive(Default)]
struct State {
    snapshot: Snapshot,
    next: u64,
    initializing: bool,
    draining: bool,
    pending: VecDeque<Pending>,
    approvals: HashMap<String, Permission>,
    tasks: HashMap<String, ()>,
}
struct Shared {
    generation: SessionId,
    limits: Limits,
    expected_resume: Option<String>,
    native_events: NativeEvents,
    state: Mutex<State>,
    events: Arc<Queue<Event>>,
}
impl Shared {
    fn stop(&self, failure: Failure) {
        let completions = {
            let mut state = lock(&self.state);
            if state.snapshot.failure.is_some() {
                return;
            }
            state.snapshot.failure = Some(failure);
            state.approvals.clear();
            state
                .pending
                .drain(..)
                .map(|p| {
                    (
                        p.ticket,
                        p.committed.unwrap_or(Outcome::Failed {
                            failure,
                            may_have_been_written: p.dispatched
                                && !p.receipt.as_ref().is_some_and(|r| {
                                    matches!(
                                        r.status(),
                                        RpcWriteStatus::Finished(RpcWriteOutcome::NotWritten(_))
                                    )
                                }),
                        }),
                    )
                })
                .collect::<Vec<_>>()
        };
        for (ticket, outcome) in completions {
            ticket.finish(outcome);
        }
        self.events.close();
    }
    fn enqueue(
        &self,
        action: Action,
        payload: Value,
        deadline: Instant,
        control: bool,
    ) -> crate::Result<Ticket> {
        if deadline <= Instant::now() {
            return Err(Error::InvalidInput("expired Claude deadline".into()));
        }
        let mut state = lock(&self.state);
        if state.snapshot.failure.is_some() || state.draining {
            return Err(Error::Closed);
        }
        if action == Action::Initialize {
            if state.snapshot.initialized || state.initializing {
                return Err(Error::InvalidInput(
                    "Claude already initialized or initializing".into(),
                ));
            }
        } else if !state.snapshot.initialized
            || self
                .expected_resume
                .as_ref()
                .is_some_and(|id| state.snapshot.session_id.as_ref() != Some(id))
        {
            return Err(Error::InvalidInput(
                "Claude initialization/resume identity not established".into(),
            ));
        }
        prune(&mut state);
        let count = state
            .pending
            .iter()
            .filter(|p| p.control == control)
            .count();
        if count
            >= if control {
                self.limits.control_pending
            } else {
                self.limits.pending
            }
        {
            return Err(Error::QueueFull);
        }
        let id = format!("agent-link-{}-{}", self.generation, state.next);
        let value = if action == Action::Write {
            payload
        } else {
            json!({"type":"control_request","request_id":id,"request":payload})
        };
        let line = encode(&value, self.limits.frame_bytes)?;
        if line.len() >= self.limits.frame_bytes {
            return Err(Error::InvalidInput("Claude frame exceeds limit".into()));
        }
        let bytes = line.len().saturating_add(id.len());
        if bytes
            > (if control {
                self.limits
                    .control_bytes
                    .saturating_sub(self.limits.frame_bytes)
            } else {
                self.limits.pending_bytes
            })
            .saturating_sub(
                state
                    .pending
                    .iter()
                    .filter(|p| p.control == control)
                    .map(|p| p.bytes)
                    .sum(),
            )
        {
            return Err(Error::QueueFull);
        }
        state.next = state.next.checked_add(1).ok_or(Error::Closed)?;
        if action == Action::Initialize {
            state.initializing = true;
        }
        let ticket = Completion::new();
        state.pending.push_back(Pending {
            id,
            action,
            line: Some(line),
            receipt: None,
            deadline,
            ticket: Arc::clone(&ticket),
            committed: None,
            control,
            bytes,
            approval_id: None,
            dispatched: false,
        });
        Ok(Ticket(ticket))
    }
}
/// Owned persistent connection. Drop settles tickets immediately; dispatcher then
/// kills/reaps the isolated process tree. No callback executor or async runtime.
pub struct ClaudeSession {
    shared: Arc<Shared>,
}
impl ClaudeSession {
    /// Spawn explicit owned stdio. For resume, supply an exact nonempty provider ID;
    /// appends `--resume=ID` and rejects conflicting resume/continue/session flags.
    /// Forces strict UTF-8 and bounded frame/write limits. Does not initialize yet.
    /// The supplied launch controls permissions; built-in profiles default unattended.
    pub fn spawn(
        spec: RpcSpawnSpec,
        limits: Limits,
        resume: Option<String>,
    ) -> crate::Result<(Self, Events)> {
        Self::spawn_with_options(
            spec,
            Options {
                limits,
                ..Options::default()
            },
            resume,
        )
    }
    /// Spawn with explicit event retention. Omission never erases snapshots, ticket
    /// outcomes or the immutable native details attached to an approval capability.
    pub fn spawn_with_options(
        mut spec: RpcSpawnSpec,
        options: Options,
        resume: Option<String>,
    ) -> crate::Result<(Self, Events)> {
        let limits = options.limits;
        if limits.frame_bytes == 0
            || limits.pending == 0
            || limits.events == 0
            || limits.event_bytes == 0
            || limits.pending_bytes == 0
            || limits.control_pending == 0
            || limits.control_bytes < limits.frame_bytes.saturating_mul(2)
            || limits.approval_bytes == 0
            || limits.approvals == 0
            || limits.tasks == 0
            || limits.task_bytes == 0
            || limits.approval_timeout.is_zero()
            || [
                limits.frame_bytes,
                limits.pending,
                limits.events,
                limits.event_bytes,
                limits.approvals,
                limits.pending_bytes,
                limits.control_bytes,
                limits.control_pending,
                limits.approval_bytes,
                limits.tasks,
                limits.task_bytes,
            ]
            .iter()
            .any(|n| *n > isize::MAX as usize / 64)
            || Instant::now()
                .checked_add(limits.approval_timeout)
                .is_none()
        {
            return Err(Error::InvalidInput("invalid Claude limits".into()));
        }
        if spec.launch.args.iter().any(|a| {
            [
                "--resume",
                "--continue",
                "--session-id",
                "--fork-session",
                "--resume-session-at",
                "-r",
                "-c",
            ]
            .iter()
            .any(|flag| a == flag || a.starts_with(&format!("{flag}=")))
        }) {
            return Err(Error::InvalidInput(
                "Claude identity flags must use explicit adapter resume".into(),
            ));
        }
        if let Some(id) = &resume {
            if id.is_empty() || id.len() >= limits.frame_bytes {
                return Err(Error::InvalidInput("invalid Claude resume ID".into()));
            }
            spec.launch.args.push(format!("--resume={id}"));
        }
        spec.stdout_utf8 = RpcUtf8Mode::Strict;
        spec.limits.max_stdout_line_bytes = limits.frame_bytes;
        spec.limits.max_stdin_line_bytes = limits.frame_bytes;
        spec.limits.max_pending_stdin_lines = 2;
        spec.limits.max_pending_stdin_bytes = limits.frame_bytes * 2;
        let (child, incoming) = RpcChild::spawn(spec)?;
        let shared = Arc::new(Shared {
            generation: child.id(),
            limits,
            expected_resume: resume,
            native_events: options.native_events,
            state: Mutex::new(State::default()),
            events: Queue::new(limits.events, limits.event_bytes),
        });
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name(format!("agent-link-claude-{}", child.id()))
            .spawn(move || run(worker, child, incoming))
            .map_err(|e| Error::Spawn(format!("Claude dispatcher: {e}")))?;
        Ok((
            Self {
                shared: Arc::clone(&shared),
            },
            Events(Arc::clone(&shared.events)),
        ))
    }
    /// Exact local transport generation; not a provider resume token.
    pub fn generation(&self) -> SessionId {
        self.shared.generation
    }
    /// Latest bounded state, committed before corresponding event publication.
    pub fn snapshot(&self) -> Snapshot {
        lock(&self.shared.state).snapshot.clone()
    }
    /// Sticky terminal reason independent of event queue capacity.
    pub fn failure(&self) -> Option<Failure> {
        self.snapshot().failure
    }
    /// Initialize the pinned minimal subset (no hooks, MCP or SDK imitation).
    pub fn initialize(&self, deadline: Instant) -> crate::Result<Ticket> {
        self.shared.enqueue(
            Action::Initialize,
            json!({"subtype":"initialize","hooks":null}),
            deadline,
            true,
        )
    }
    /// Send a user message after initialization. Optional UUID is caller supplied,
    /// preserved exactly, and is never treated as a generated turn identity.
    /// Written means flushed only; results may attribute multiple user messages.
    pub fn send(&self, text: &str, uuid: Option<&str>, deadline: Instant) -> crate::Result<Ticket> {
        if text.len().saturating_add(uuid.map_or(0, str::len)) >= self.shared.limits.frame_bytes {
            return Err(Error::InvalidInput("Claude message exceeds limit".into()));
        }
        let mut message = json!({"type":"user","message":{"role":"user","content":text},"parent_tool_use_id":null,"session_id":""});
        if let Some(uuid) = uuid {
            message["uuid"] = json!(uuid);
        }
        self.shared.enqueue(Action::Write, message, deadline, false)
    }
    /// Reserved control admission. A successful response is not turn completion.
    pub fn interrupt(&self, deadline: Instant) -> crate::Result<Ticket> {
        self.shared.enqueue(
            Action::Control,
            json!({"subtype":"interrupt"}),
            deadline,
            true,
        )
    }
    /// Settle all tickets and asynchronously tear down the owned process tree.
    pub fn close(&self) {
        self.shared.stop(Failure::Closed);
    }
}
impl Drop for ClaudeSession {
    fn drop(&mut self) {
        self.close();
    }
}
/// Generation-bound single-use capability. Cancellation, timeout and close revoke it.
/// No host code executes in the dispatcher. Dropping the token does not authorize.
#[derive(Clone)]
pub struct Approval {
    shared: Weak<Shared>,
    id: String,
    nonce: u64,
    deadline: Instant,
    request: Arc<Value>,
    seq: u64,
    generation: SessionId,
}
impl Approval {
    /// Full native request, including tool input and extension fields.
    pub fn request(&self) -> &Value {
        &self.request
    }
    /// Local generation and transport sequence that originated this capability.
    pub fn origin(&self) -> (SessionId, u64) {
        (self.generation, self.seq)
    }
    /// Opaque native control request ID, preserved exactly.
    pub fn request_id(&self) -> &str {
        &self.id
    }
    /// Absolute local capability deadline.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    /// Allow with provider-native updated input, or deny with a message. Responses
    /// use reserved control admission; a queue rejection leaves the token usable.
    /// `updated_input` is serialized and bounded before locking. No permissions added.
    pub fn respond(
        &self,
        updated_input: Option<&Value>,
        denial: Option<&str>,
    ) -> crate::Result<Ticket> {
        let shared = self.shared.upgrade().ok_or(Error::Closed)?;
        if updated_input.is_some() && denial.is_some()
            || updated_input.is_some_and(|v| !v.is_object())
        {
            return Err(Error::InvalidInput(
                "approval requires either object input or denial".into(),
            ));
        }
        if let Some(input) = updated_input {
            validate_json(input, shared.limits.frame_bytes)?;
        }
        if denial.is_some_and(|text| text.len() >= shared.limits.frame_bytes) {
            return Err(Error::InvalidInput("Claude denial exceeds limit".into()));
        }
        let response = if let Some(input) = updated_input {
            json!({"behavior":"allow","updatedInput":input})
        } else {
            json!({"behavior":"deny","message":denial.unwrap_or("Denied by host")})
        };
        let value = json!({"type":"control_response","response":{"subtype":"success","request_id":self.id,"response":response}});
        let line = encode(&value, shared.limits.frame_bytes)?;
        if line.len() >= shared.limits.frame_bytes {
            return Err(Error::InvalidInput("Claude approval exceeds limit".into()));
        }
        let mut state = lock(&shared.state);
        if state.snapshot.failure.is_some()
            || state.draining
            || !state
                .approvals
                .get(&self.id)
                .is_some_and(|p| p.nonce == self.nonce && p.deadline > Instant::now())
        {
            return Err(Error::Closed);
        }
        prune(&mut state);
        let bytes = line.len().saturating_add(64);
        if state.pending.iter().filter(|p| p.control).count() > shared.limits.control_pending
            || bytes
                > shared.limits.control_bytes.saturating_sub(
                    state
                        .pending
                        .iter()
                        .filter(|p| p.control)
                        .map(|p| p.bytes)
                        .sum(),
                )
        {
            return Err(Error::QueueFull);
        }
        state.approvals.remove(&self.id);
        let local_id = format!("agent-link-reply-{}-{}", shared.generation, state.next);
        state.next = state.next.checked_add(1).ok_or(Error::Closed)?;
        let ticket = Completion::new();
        state.pending.push_back(Pending {
            id: local_id,
            action: Action::Write,
            line: Some(line),
            receipt: None,
            deadline: self.deadline,
            ticket: Arc::clone(&ticket),
            committed: None,
            control: true,
            bytes,
            approval_id: Some(self.id.clone()),
            dispatched: false,
        });
        Ok(Ticket(ticket))
    }
}

fn prune(state: &mut State) {
    state.pending.retain(|p| {
        p.ticket.get().is_none()
            || !p
                .receipt
                .as_ref()
                .is_some_and(|r| matches!(r.status(), RpcWriteStatus::Finished(_)))
    });
}

fn run(shared: Arc<Shared>, child: RpcChild, incoming: crossbeam_channel::Receiver<RpcEvent>) {
    run_loop(shared, incoming, |line, deadline| {
        child.send_line_tracked(line, deadline)
    });
    drop(child);
}
fn run_loop(
    shared: Arc<Shared>,
    incoming: crossbeam_channel::Receiver<RpcEvent>,
    mut send: impl FnMut(&str, Instant) -> crate::Result<RpcWriteReceipt>,
) {
    let mut draining = false;
    loop {
        if lock(&shared.state).snapshot.failure.is_some() {
            break;
        }
        let (dispatch, complete, failure) = {
            let mut state = lock(&shared.state);
            let now = Instant::now();
            let mut failure = state
                .approvals
                .values()
                .any(|p| p.deadline <= now)
                .then_some(Failure::Deadline);
            let mut complete = Vec::new();
            for p in &mut state.pending {
                if p.deadline <= now
                    && (p.committed.is_none()
                        || !p.receipt.as_ref().is_some_and(|r| {
                            matches!(
                                r.status(),
                                RpcWriteStatus::Finished(RpcWriteOutcome::Written)
                            )
                        }))
                {
                    failure = Some(Failure::Deadline);
                }
                if let Some(receipt) = &p.receipt {
                    match receipt.status() {
                        RpcWriteStatus::Finished(RpcWriteOutcome::Written)
                            if p.action == Action::Write =>
                        {
                            p.committed = Some(Outcome::Written);
                        }
                        RpcWriteStatus::Finished(RpcWriteOutcome::Written) => {}
                        RpcWriteStatus::Finished(_) => {
                            draining = true;
                        }
                        _ => {}
                    }
                }
                if let Some(outcome) = p.committed.clone() {
                    complete.push((Arc::clone(&p.ticket), outcome));
                }
            }
            if draining {
                state.draining = true;
            }
            // Registration remains charged until publication on the next pass.
            state.pending.retain(|p| {
                p.ticket.get().is_none()
                    || !p
                        .receipt
                        .as_ref()
                        .is_some_and(|r| matches!(r.status(), RpcWriteStatus::Finished(_)))
            });
            let ordinary_active = state.pending.iter().any(|p| {
                !p.control
                    && p.line.is_none()
                    && !p.receipt.as_ref().is_some_and(|r| {
                        matches!(
                            r.status(),
                            RpcWriteStatus::Finished(RpcWriteOutcome::Written)
                        )
                    })
            });
            let control_active = state.pending.iter().any(|p| {
                p.control
                    && p.line.is_none()
                    && !p.receipt.as_ref().is_some_and(|r| {
                        matches!(
                            r.status(),
                            RpcWriteStatus::Finished(RpcWriteOutcome::Written)
                        )
                    })
            });
            let dispatch = state
                .pending
                .iter_mut()
                .filter(|p| {
                    failure.is_none()
                        && !draining
                        && p.line.is_some()
                        && if p.control {
                            !control_active
                        } else {
                            !ordinary_active
                        }
                })
                .min_by_key(|p| !p.control)
                .and_then(|p| {
                    p.line.take().map(|line| {
                        p.dispatched = true;
                        (p.id.clone(), line, p.deadline)
                    })
                });
            (dispatch, complete, failure)
        };
        for (ticket, outcome) in complete {
            ticket.finish(outcome);
        }
        if let Some(failure) = failure {
            shared.stop(failure);
            break;
        }
        if let Some((id, line, deadline)) = dispatch.filter(|_| !draining) {
            match send(&line, deadline) {
                Ok(receipt) => {
                    if let Some(p) = lock(&shared.state)
                        .pending
                        .iter_mut()
                        .find(|p| p.id == id && p.receipt.is_none())
                    {
                        p.receipt = Some(receipt);
                    }
                }
                Err(Error::Closed) => {
                    let mut state = lock(&shared.state);
                    state.draining = true;
                    if let Some(p) = state.pending.iter_mut().find(|p| p.id == id) {
                        p.dispatched = false;
                    }
                    draining = true;
                }
                Err(_) => {
                    shared.stop(Failure::Transport);
                    break;
                }
            }
        }
        match incoming.recv_timeout(Duration::from_millis(5)) {
            Ok(RpcEvent::Line { seq, line, .. }) => {
                if let Err(failure) = route(&shared, seq, &line) {
                    shared.stop(failure);
                }
            }
            Ok(RpcEvent::Exit { .. }) => shared.stop(Failure::Eof),
            Ok(RpcEvent::Error { .. }) => shared.stop(Failure::Transport),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => shared.stop(Failure::Eof),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
    }
}
fn route(shared: &Arc<Shared>, seq: u64, line: &str) -> Result<(), Failure> {
    let native: Value = serde_json::from_str(line).map_err(|_| Failure::Protocol)?;
    let kind = native
        .get("type")
        .and_then(Value::as_str)
        .ok_or(Failure::Protocol)?;
    let response_id = if kind == "control_response" {
        Some(
            native
                .pointer("/response/request_id")
                .and_then(Value::as_str)
                .ok_or(Failure::Protocol)?,
        )
    } else {
        None
    };
    if kind == "control_response"
        && lock(&shared.state)
            .pending
            .iter()
            .any(|p| Some(p.id.as_str()) == response_id && p.action != Action::Write)
    {
        let response = native.get("response").ok_or(Failure::Protocol)?;
        let id = response
            .get("request_id")
            .and_then(Value::as_str)
            .ok_or(Failure::Protocol)?;
        let completion = {
            let mut state = lock(&shared.state);
            let Some(p) = state
                .pending
                .iter_mut()
                .find(|p| p.id == id && p.action != Action::Write)
            else {
                return Ok(());
            };
            if p.line.is_some() || p.committed.is_some() {
                return Err(Failure::Protocol);
            }
            if p.deadline <= Instant::now() {
                return Err(Failure::Deadline);
            }
            let outcome = Outcome::Response(native.clone());
            p.committed = Some(outcome.clone());
            let initialize = p.action == Action::Initialize;
            let ticket = Arc::clone(&p.ticket);
            if initialize {
                state.snapshot.initialize_response = Some(Arc::new(native.clone()));
                state.initializing = false;
                if response.get("subtype").and_then(Value::as_str) == Some("success") {
                    state.snapshot.initialized = true;
                }
            }
            (ticket, outcome)
        };
        completion.0.finish(completion.1);
        return Ok(());
    }
    let mut cancelled = Vec::new();
    let mut approval = None;
    let mut projection = EventKind::Other;
    let session_id = native
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let request_id = native
        .get("request_id")
        .or_else(|| native.pointer("/response/request_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    {
        let mut state = lock(&shared.state);
        if let Some(id) = &session_id {
            if id.is_empty()
                || shared
                    .expected_resume
                    .as_ref()
                    .is_some_and(|expected| expected != id)
                || state
                    .snapshot
                    .session_id
                    .as_ref()
                    .is_some_and(|observed| observed != id)
            {
                return Err(Failure::Protocol);
            }
            state.snapshot.session_id = Some(id.clone());
        }
        match kind {
            "result" => {
                state.snapshot.last_result = Some(Arc::new(native.clone()));
                projection = EventKind::TurnCompleted {
                    status: match native.get("subtype").and_then(Value::as_str) {
                        Some("success")
                            if native.get("is_error").and_then(Value::as_bool) == Some(false) =>
                        {
                            CompletionStatus::Completed
                        }
                        Some(
                            "error_during_execution"
                            | "error_max_turns"
                            | "error_max_budget_usd"
                            | "error_max_structured_output_retries",
                        ) => CompletionStatus::Failed,
                        _ => CompletionStatus::Unknown,
                    },
                }
            }
            "assistant" => {
                if let Some(content) = native.pointer("/message/content").and_then(Value::as_array)
                {
                    if !content.is_empty()
                        && content.iter().all(|block| {
                            block.get("type").and_then(Value::as_str) == Some("text")
                                && block.get("text").and_then(Value::as_str).is_some()
                        })
                    {
                        let text = content
                            .iter()
                            .filter_map(|block| block.get("text").and_then(Value::as_str))
                            .collect::<String>();
                        projection = EventKind::Text {
                            text,
                            update: crate::portable::TextUpdate::Replace,
                            phase: crate::portable::TextPhase::Unknown,
                        };
                    }
                }
            }
            "stream_event"
                if native.pointer("/event/type").and_then(Value::as_str)
                    == Some("content_block_delta")
                    && native.pointer("/event/delta/type").and_then(Value::as_str)
                        == Some("text_delta") =>
            {
                if let Some(text) = native.pointer("/event/delta/text").and_then(Value::as_str) {
                    projection = EventKind::Text {
                        text: text.to_owned(),
                        update: crate::portable::TextUpdate::Delta,
                        phase: crate::portable::TextPhase::Unknown,
                    };
                }
            }
            "system" => match native.get("subtype").and_then(Value::as_str) {
                // init also occurs for resumed conversations: it is binding evidence,
                // not proof that a provider session was newly created.
                Some("init") => {}
                Some("session_state_changed") => {
                    state.snapshot.session_state = native
                        .get("state")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                Some("task_started") => {
                    if matches!(
                        native.get("task_type").and_then(Value::as_str),
                        Some("local_agent" | "local_workflow")
                    ) {
                        if let Some(id) = native.get("task_id").and_then(Value::as_str) {
                            if !state.tasks.contains_key(id)
                                && (state.tasks.len() >= shared.limits.tasks
                                    || id.len()
                                        > shared.limits.task_bytes.saturating_sub(
                                            state.tasks.keys().map(String::len).sum(),
                                        ))
                            {
                                return Err(Failure::Overload);
                            }
                            state.tasks.insert(id.to_owned(), ());
                        }
                    }
                }
                Some("task_updated")
                    if matches!(
                        native.pointer("/patch/status").and_then(Value::as_str),
                        Some("completed" | "failed" | "stopped" | "killed")
                    ) =>
                {
                    if let Some(id) = native.get("task_id").and_then(Value::as_str) {
                        state.tasks.remove(id);
                    }
                }
                Some("task_notification") => {
                    if let Some(id) = native.get("task_id").and_then(Value::as_str) {
                        state.tasks.remove(id);
                    }
                }
                _ => {}
            },
            "control_cancel_request" => {
                let id = request_id.as_ref().ok_or(Failure::Protocol)?;
                state.approvals.remove(id);
                state.pending.retain(|p| {
                    if p.approval_id.as_ref() == Some(id) && p.line.is_some() {
                        cancelled.push(Arc::clone(&p.ticket));
                        false
                    } else {
                        true
                    }
                });
            }
            "control_request" => {
                let id = request_id.as_ref().ok_or(Failure::Protocol)?;
                if native.pointer("/request/subtype").and_then(Value::as_str)
                    != Some("can_use_tool")
                {
                    return Err(Failure::Protocol);
                }
                prune(&mut state);
                if state.approvals.contains_key(id)
                    || state
                        .pending
                        .iter()
                        .any(|p| p.approval_id.as_ref() == Some(id))
                {
                    return Err(Failure::Protocol);
                }
                let bytes = line.len().saturating_add(id.len());
                if state.approvals.len() >= shared.limits.approvals
                    || bytes
                        > shared
                            .limits
                            .approval_bytes
                            .saturating_sub(state.approvals.values().map(|p| p.bytes).sum())
                {
                    return Err(Failure::Overload);
                }
                let nonce = state.next;
                state.next = state.next.checked_add(1).ok_or(Failure::Protocol)?;
                let deadline = Instant::now()
                    .checked_add(shared.limits.approval_timeout)
                    .ok_or(Failure::Deadline)?;
                state.approvals.insert(
                    id.clone(),
                    Permission {
                        nonce,
                        deadline,
                        bytes,
                    },
                );
                approval = Some(Approval {
                    shared: Arc::downgrade(shared),
                    id: id.clone(),
                    nonce,
                    deadline,
                    request: Arc::new(native.clone()),
                    seq,
                    generation: shared.generation,
                });
                projection = EventKind::ApprovalRequested;
            }
            _ => {}
        }
        state.snapshot.finite_tasks = state.tasks.len();
    }
    for ticket in cancelled {
        ticket.finish(Outcome::Failed {
            failure: Failure::Closed,
            may_have_been_written: false,
        });
    }
    let portable = EventV1 {
        context: EventContext {
            provider: Provider::Claude,
            generation: shared.generation,
            seq,
            session_id,
            turn_id: None,
            item_id: match kind {
                "assistant" => native.pointer("/message/id"),
                "stream_event" => native.pointer("/event/message/id"),
                _ => native.get("uuid"),
            }
            .and_then(Value::as_str)
            .map(str::to_owned),
            request_id: request_id.map(crate::portable::RequestId::String),
        },
        native_method: kind.to_owned(),
        kind: projection,
    };
    let bytes = portable
        .retained_bytes()
        .saturating_add(if shared.native_events == NativeEvents::Retain {
            line.len()
        } else {
            0
        })
        .saturating_add(if approval.is_some() { line.len() } else { 0 });
    let native = (shared.native_events == NativeEvents::Retain).then_some(native);
    if !shared.events.push(
        Event {
            native,
            portable,
            approval,
        },
        bytes,
    ) {
        return Err(Failure::Overload);
    }
    Ok(())
}

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
            "Claude JSON exceeds frame or nesting limits".into(),
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

#[cfg(test)]
mod tests;
