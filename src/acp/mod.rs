//! Bounded stable-v1 Agent Client Protocol over owned stdio (`acp` feature).
//!
//! Initialize before creating or strictly restoring one session. Prompts complete
//! only on their native response; updates, writes and cancellation are not completion.
//! All requests have absolute deadlines. EOF, protocol errors, event saturation and
//! timeout terminate the connection and settle tickets without automatic replay.
//! Filesystem/terminal reverse methods are unadvertised and rejected with -32601.
//! No SDK, async runtime, callbacks, persistence or automatic permission policy.
//! Profile-based launches retain their existing unattended permission defaults.
//! See `docs/rpc-and-native-adapters.md#agent-client-protocol-acp` in the public manual
//! for the supported subset; its validation-evidence section records provenance.
//!
//! Once input closure is observed, new ordinary/control requests return
//! `Transport(crate::Error::Closed)` and approval replies return `StaleApproval`
//! without consuming their tokens. This is input-only: `snapshot().failure` stays
//! unset while incoming native responses/events drain. Existing absolute request
//! and approval deadlines still apply; automatic replies are skipped when unsendable.

mod types;
pub use types::*;

use crate::portable::{
    EventContext, EventKind, EventV1, Provider, RequestId, TextPhase, TextUpdate,
};
use crate::protocol::{Completion, Queue, lock};
use crate::rpc::{RpcChild, RpcEvent, RpcSpawnSpec, RpcUtf8Mode, RpcWriteOutcome, RpcWriteStatus};
use crate::{Error, SessionId};
use crossbeam_channel::{Receiver, Sender, bounded};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Stable-v1 schema provenance; not a provider version allowlist.
pub const PROTOCOL_REVISION: &str = "0849001ca66fb975fc01111ddab9093bc8304e98";

/// Offline evidence for this bounded subset; not live provider interoperability.
pub const COMPATIBILITY_EVIDENCE: &[crate::agent::compatibility::Evidence] = &[
    crate::agent::compatibility::Evidence {
        operation: "ACP stable-v1 initialize, strict session restore, prompt/cancel and permissions",
        kind: crate::agent::compatibility::EvidenceKind::SyntheticFixture,
        cli_version: None,
        platform: None,
        source: "tests/fixtures/acp/stable-v1.json",
    },
];

#[derive(Clone)]
enum Action {
    Initialize,
    New,
    Restore { id: String, method: &'static str },
    Prompt,
    Control,
}
type Pending = crate::protocol::Pending<Action, RequestOutcome, RequestOutcome>;
struct Offered {
    nonce: u64,
    id: Value,
    params: Arc<Value>,
    deadline: Instant,
    bytes: usize,
}
#[derive(Default)]
struct State {
    snapshot: Snapshot,
    // Sticky input-only closure; output continues until its genuine terminal event.
    input_closed: bool,
    next: u64,
    mutation: bool,
    pending: VecDeque<Pending>,
    permissions: HashMap<String, Offered>,
}
struct Shared {
    generation: SessionId,
    limits: AcpLimits,
    native_events: NativeEvents,
    state: Mutex<State>,
    events: Arc<Queue<AcpEvent>>,
    wake: Sender<()>,
    #[cfg(feature = "tools")]
    mcp_servers: Value,
}

/// Owned connection. Drop settles tickets and asynchronously tears down the child.
/// Event consumers never own process lifetime. No host call waits on child I/O.
pub struct AcpClient {
    shared: Arc<Shared>,
}
impl AcpClient {
    /// Validates budgets before spawning, forces strict UTF-8 and finite framing,
    /// and reserves two transport slots for ordinary/control writes. No handshake
    /// is sent until [`Self::initialize`]. Other transport settings are preserved.
    pub fn spawn(spec: RpcSpawnSpec, limits: AcpLimits) -> crate::Result<(Self, AcpEvents)> {
        Self::spawn_with_options(
            spec,
            AcpOptions {
                limits,
                ..AcpOptions::default()
            },
        )
    }
    /// Spawns with explicit native-envelope retention. Omission does not remove
    /// permission metadata, native request outcomes or latest prompt evidence.
    pub fn spawn_with_options(
        mut spec: RpcSpawnSpec,
        options: AcpOptions,
    ) -> crate::Result<(Self, AcpEvents)> {
        let limits = options.limits;
        validate_limits(limits)?;
        #[cfg(feature = "tools")]
        let mcp_servers = {
            if let Some(tools) = &spec.launch.tools {
                tools.require_backend(crate::tools::McpBackend::Acp)?;
            }
            spec.launch
                .tools
                .as_ref()
                .and_then(crate::tools::ToolAttachment::acp_servers)
                .cloned()
                .unwrap_or_else(|| json!([]))
        };
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
            #[cfg(feature = "tools")]
            mcp_servers,
        });
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name(format!("agent-link-acp-{}", child.id()))
            .spawn(move || run(worker, child, incoming, wakes))
            .map_err(|e| Error::Spawn(format!("spawn ACP dispatcher: {e}")))?;
        Ok((
            Self {
                shared: Arc::clone(&shared),
            },
            AcpEvents(Arc::clone(&shared.events)),
        ))
    }
    /// Process-local transport generation, never a native resume token.
    pub fn generation(&self) -> SessionId {
        self.shared.generation
    }
    /// Current state, including provisional restore identity and sticky failure.
    pub fn snapshot(&self) -> Snapshot {
        lock(&self.shared.state).snapshot.clone()
    }
    /// Sticky termination reason, readable even when the event queue is full.
    pub fn failure(&self) -> Option<Failure> {
        self.snapshot().failure
    }
    /// Settles tickets before returning; child kill/reap occurs on the dispatcher.
    pub fn close(&self) {
        self.shared.stop(Failure::Closed);
    }
    /// Negotiates exactly protocol version 1 and advertises no delegated filesystem
    /// or terminal handlers. Application identity is explicit; authentication is not
    /// automated. Incompatible versions terminate the connection.
    pub fn initialize(
        &self,
        name: &str,
        version: &str,
        deadline: Instant,
    ) -> Result<RequestTicket, AcpError> {
        self.check_text(name)?;
        self.check_text(version)?;
        self.request(Action::Initialize, "initialize", json!({"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":name,"version":version}}), deadline)
    }
    /// Creates one native session. `cwd` is an absolute provider working directory;
    /// MCP connections prepared by the optional tools facade are supplied for this
    /// session; HTTP requires the agent's advertised capability. Native errors never
    /// trigger fallback. Without attachments the MCP list remains empty.
    pub fn session_new(&self, cwd: &str, deadline: Instant) -> Result<RequestTicket, AcpError> {
        self.check_text(cwd)?;
        self.request(
            Action::New,
            "session/new",
            json!({"cwd":cwd,"mcpServers":self.mcp_servers()}),
            deadline,
        )
    }
    /// Strictly loads exactly `session_id`, including replay before its response.
    /// Requires advertised `loadSession`; never creates a replacement session.
    pub fn session_load(
        &self,
        session_id: &str,
        cwd: &str,
        deadline: Instant,
    ) -> Result<RequestTicket, AcpError> {
        self.restore("session/load", session_id, cwd, deadline)
    }
    /// Strictly resumes exactly `session_id` without requesting replay. Requires
    /// `sessionCapabilities.resume`; never falls back to load or new.
    pub fn session_resume(
        &self,
        session_id: &str,
        cwd: &str,
        deadline: Instant,
    ) -> Result<RequestTicket, AcpError> {
        self.restore("session/resume", session_id, cwd, deadline)
    }
    fn restore(
        &self,
        method: &str,
        id: &str,
        cwd: &str,
        deadline: Instant,
    ) -> Result<RequestTicket, AcpError> {
        self.check_text(id)?;
        self.check_text(cwd)?;
        if id.is_empty() {
            return Err(AcpError::InvalidState);
        }
        self.request(
            Action::Restore {
                id: id.into(),
                method: if method == "session/load" {
                    "session/load"
                } else {
                    "session/resume"
                },
            },
            method,
            json!({"sessionId":id,"cwd":cwd,"mcpServers":self.mcp_servers()}),
            deadline,
        )
    }
    /// Sends text content only. Rich content is deliberately outside this subset;
    /// one prompt may be active. Ticket drop does not cancel or resend the prompt.
    pub fn prompt(&self, text: &str, deadline: Instant) -> Result<RequestTicket, AcpError> {
        self.check_text(text)?;
        self.request(
            Action::Prompt,
            "session/prompt",
            json!({"prompt":[{"type":"text","text":text}]}),
            deadline,
        )
    }
    /// Queues `session/cancel` and explicit cancelled outcomes for every outstanding
    /// permission. The returned ticket proves write completion only; the prompt
    /// remains active until its original response or connection termination.
    pub fn cancel(&self, deadline: Instant) -> Result<RequestTicket, AcpError> {
        self.shared.cancel(deadline)
    }
    fn check_text(&self, text: &str) -> Result<(), AcpError> {
        if text.len() > self.shared.limits.max_frame_bytes {
            return Err(Error::InputTooLarge {
                size: text.len(),
                limit: self.shared.limits.max_frame_bytes,
            }
            .into());
        }
        Ok(())
    }
    fn mcp_servers(&self) -> Value {
        #[cfg(feature = "tools")]
        {
            self.shared.mcp_servers.clone()
        }
        #[cfg(not(feature = "tools"))]
        {
            json!([])
        }
    }

    fn request(
        &self,
        action: Action,
        method: &str,
        mut params: Value,
        deadline: Instant,
    ) -> Result<RequestTicket, AcpError> {
        validate_json(&params, self.shared.limits.max_frame_bytes)?;
        let mut state = lock(&self.shared.state);
        self.shared.check_open(&state, deadline)?;
        match &action {
            Action::Initialize if state.snapshot.initialized.is_some() || state.mutation => {
                return Err(AcpError::InvalidState);
            }
            Action::New | Action::Restore { .. } => {
                if state.snapshot.initialized.is_none()
                    || state.snapshot.session_id.is_some()
                    || state.mutation
                {
                    return Err(AcpError::InvalidState);
                }
                if !params["cwd"]
                    .as_str()
                    .is_some_and(|cwd| std::path::Path::new(cwd).is_absolute())
                {
                    return Err(AcpError::InvalidState);
                }
                if matches!(action, Action::Restore { .. }) {
                    let caps = &state.snapshot.initialized.as_ref().unwrap()["agentCapabilities"];
                    let supported = if method == "session/load" {
                        caps["loadSession"].as_bool() == Some(true)
                    } else {
                        caps["sessionCapabilities"]["resume"].is_object()
                    };
                    if !supported {
                        return Err(AcpError::InvalidState);
                    }
                }
                if params["mcpServers"]
                    .as_array()
                    .is_some_and(|servers| servers.iter().any(|s| s["type"] == "http"))
                    && state
                        .snapshot
                        .initialized
                        .as_ref()
                        .is_none_or(|v| v["agentCapabilities"]["mcpCapabilities"]["http"] != true)
                {
                    return Err(Error::Unsupported(
                        "ACP agent did not advertise HTTP MCP support".into(),
                    )
                    .into());
                }
            }
            Action::Prompt => {
                if state.snapshot.prompting || state.mutation {
                    return Err(AcpError::Busy);
                }
                let id = state
                    .snapshot
                    .session_id
                    .as_ref()
                    .ok_or(AcpError::InvalidState)?;
                params["sessionId"] = json!(id);
            }
            _ => {}
        }
        let id = next_id(&mut state, self.shared.generation)?;
        let ticket = self.shared.enqueue(
            &mut state,
            Some(id.clone()),
            action.clone(),
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            false,
            deadline,
        )?;
        match action {
            Action::Initialize | Action::New => state.mutation = true,
            Action::Restore { id, .. } => {
                state.mutation = true;
                state.snapshot.session_id = Some(id);
            }
            Action::Prompt => state.snapshot.prompting = true,
            Action::Control => {}
        }
        drop(state);
        self.shared.wake();
        Ok(ticket)
    }
}
impl Drop for AcpClient {
    fn drop(&mut self) {
        self.close();
    }
}

impl Shared {
    fn wake(&self) {
        let _ = self.wake.try_send(());
    }
    fn check_open(&self, state: &State, deadline: Instant) -> Result<(), AcpError> {
        if let Some(failure) = &state.snapshot.failure {
            return Err(AcpError::Closed(failure.clone()));
        }
        if state.input_closed {
            return Err(Error::Closed.into());
        }
        if deadline <= Instant::now() {
            return Err(Error::InvalidInput("ACP deadline already expired".into()).into());
        }
        Ok(())
    }
    fn enqueue(
        &self,
        state: &mut State,
        id: Option<String>,
        action: Action,
        value: Value,
        control: bool,
        deadline: Instant,
    ) -> Result<RequestTicket, AcpError> {
        if state.input_closed {
            return Err(Error::Closed.into());
        }
        let line = encode(&value, self.limits.max_frame_bytes)?;
        let bytes = line.len() + 1;
        crate::protocol::admit(
            &state.pending,
            bytes,
            control,
            if control {
                self.limits.control_capacity
            } else {
                self.limits.pending_requests
            },
            if control {
                self.limits.control_bytes
            } else {
                self.limits.pending_bytes
            },
        )?;
        let completion = Completion::new();
        let ticket = RequestTicket {
            id: id.clone().unwrap_or_default(),
            completion: Arc::clone(&completion),
        };
        state.pending.push_back(Pending {
            id,
            action,
            line: Some(line),
            dispatched: false,
            receipt: None,
            deadline,
            bytes,
            control,
            request: Some(completion),
            request_outcome: None,
            reply: None,
            responded: false,
        });
        Ok(ticket)
    }
    fn permission_reply(
        &self,
        key: &str,
        nonce: u64,
        option: Option<&str>,
    ) -> Result<RequestTicket, AcpError> {
        let mut state = lock(&self.state);
        let offered = state.permissions.get(key).ok_or(AcpError::StaleApproval)?;
        if offered.nonce != nonce
            || offered.deadline <= Instant::now()
            || state.snapshot.failure.is_some()
            || state.input_closed
        {
            return Err(AcpError::StaleApproval);
        }
        if option.is_some_and(|id| {
            !offered.params["options"]
                .as_array()
                .is_some_and(|opts| opts.iter().any(|opt| opt["optionId"].as_str() == Some(id)))
        }) {
            return Err(AcpError::InvalidDecision);
        }
        let outcome = match option {
            Some(id) => json!({"outcome":"selected","optionId":id}),
            None => json!({"outcome":"cancelled"}),
        };
        let frame = json!({"jsonrpc":"2.0","id":offered.id,"result":{"outcome":outcome}});
        let deadline = offered
            .deadline
            .min(Instant::now() + self.limits.control_timeout);
        let ticket = self.enqueue(&mut state, None, Action::Control, frame, true, deadline)?;
        state.permissions.remove(key);
        drop(state);
        self.wake();
        Ok(ticket)
    }
    fn cancel(&self, deadline: Instant) -> Result<RequestTicket, AcpError> {
        let mut state = lock(&self.state);
        self.check_open(&state, deadline)?;
        if !state.snapshot.prompting {
            return Err(AcpError::InvalidState);
        }
        let id = state
            .snapshot
            .session_id
            .as_ref()
            .ok_or(AcpError::InvalidState)?;
        // Preflight the entire cancellation batch so refusal has no wire side effects.
        let mut frames =
            vec![json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}})];
        frames.extend(state.permissions.values().map(
            |p| json!({"jsonrpc":"2.0","id":p.id,"result":{"outcome":{"outcome":"cancelled"}}}),
        ));
        let lines: Vec<_> = frames
            .iter()
            .map(|v| encode(v, self.limits.max_frame_bytes))
            .collect::<crate::Result<_>>()?;
        let (count, bytes) = state
            .pending
            .iter()
            .filter(|p| p.control)
            .fold((0, 0), |(n, b), p| (n + 1, b + p.bytes));
        if lines.len() > self.limits.control_capacity.saturating_sub(count)
            || lines.iter().map(|s| s.len() + 1).sum::<usize>()
                > self.limits.control_bytes.saturating_sub(bytes)
        {
            return Err(Error::QueueFull.into());
        }
        let mut ticket = None;
        for frame in frames {
            let current = self.enqueue(&mut state, None, Action::Control, frame, true, deadline)?;
            if ticket.is_none() {
                ticket = Some(current);
            }
        }
        state.permissions.clear();
        drop(state);
        self.wake();
        ticket.ok_or(AcpError::InvalidState)
    }
    fn stop(&self, failure: Failure) {
        let pending = {
            let mut state = lock(&self.state);
            if state.snapshot.failure.is_some() {
                return;
            }
            state.snapshot.failure = Some(failure.clone());
            state.permissions.clear();
            std::mem::take(&mut state.pending)
        };
        for p in pending {
            let outcome = p.request_outcome.unwrap_or_else(|| {
                let write = p.receipt.as_ref().and_then(|r| match r.status() {
                    RpcWriteStatus::Finished(w) => Some(w),
                    _ => None,
                });
                if p.id.is_none() && write == Some(RpcWriteOutcome::Written) {
                    RequestOutcome::Written
                } else if !p.dispatched || matches!(write, Some(RpcWriteOutcome::NotWritten(_))) {
                    RequestOutcome::NotSent(failure.clone())
                } else {
                    RequestOutcome::OutcomeUnknown {
                        failure: failure.clone(),
                        write,
                    }
                }
            });
            if let Some(c) = p.request {
                c.finish(outcome.clone());
            }
            if let Some(c) = p.reply {
                c.finish(outcome);
            }
        }
        self.events.close();
        self.wake();
    }
}
fn next_nonce(state: &mut State) -> Result<u64, AcpError> {
    state.next = state.next.checked_add(1).ok_or(AcpError::InvalidState)?;
    Ok(state.next)
}
fn next_id(state: &mut State, generation: SessionId) -> Result<String, AcpError> {
    Ok(format!("acp-{generation}-{}", next_nonce(state)?))
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
            "ACP JSON exceeds frame or nesting limits".into(),
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

fn validate_limits(l: AcpLimits) -> crate::Result<()> {
    if [
        l.max_frame_bytes,
        l.pending_requests,
        l.pending_bytes,
        l.control_capacity,
        l.control_bytes,
        l.approvals,
        l.approval_bytes,
        l.events,
        l.event_bytes,
    ]
    .iter()
    .any(|&n| n == 0 || n > isize::MAX as usize / 32)
        || [
            (l.pending_requests, std::mem::size_of::<Pending>()),
            (l.control_capacity, std::mem::size_of::<Pending>()),
            (l.events, std::mem::size_of::<AcpEvent>()),
            (l.approvals, std::mem::size_of::<Offered>()),
        ]
        .iter()
        .any(|(count, size)| {
            count
                .checked_mul(*size)
                .is_none_or(|n| n > isize::MAX as usize)
        })
        || l.max_frame_bytes < 128
        || l.control_capacity < 2
        || l.control_bytes < l.max_frame_bytes * 2
        || l.pending_bytes < l.max_frame_bytes
        || l.approval_timeout.is_zero()
        || l.control_timeout.is_zero()
        || Instant::now().checked_add(l.approval_timeout).is_none()
        || Instant::now().checked_add(l.control_timeout).is_none()
    {
        return Err(Error::InvalidInput("invalid ACP lifecycle limits".into()));
    }
    Ok(())
}
fn run(shared: Arc<Shared>, child: RpcChild, incoming: Receiver<RpcEvent>, wakes: Receiver<()>) {
    run_loop(shared, incoming, wakes, |line, deadline| {
        child.send_line_tracked(line, deadline)
    });
    drop(child);
}
fn run_loop(
    shared: Arc<Shared>,
    incoming: Receiver<RpcEvent>,
    wakes: Receiver<()>,
    mut send: impl FnMut(&str, Instant) -> crate::Result<crate::rpc::RpcWriteReceipt>,
) {
    let mut draining = false;
    loop {
        if let Err(f) = maintenance(&shared, &mut draining) {
            shared.stop(f);
        }
        if lock(&shared.state).snapshot.failure.is_some() {
            break;
        }
        if !draining {
            for control in [true, false] {
                let outgoing = {
                    let mut state = lock(&shared.state);
                    if state.pending.iter().any(|p| {
                        p.control == control
                            && p.dispatched
                            && !matches!(
                                p.receipt.as_ref().map(|r| r.status()),
                                Some(RpcWriteStatus::Finished(_))
                            )
                    }) {
                        None
                    } else {
                        state
                            .pending
                            .iter_mut()
                            .find(|p| p.control == control && !p.dispatched)
                            .map(|p| {
                                p.dispatched = true;
                                (p.request.clone(), p.line.take().unwrap(), p.deadline)
                            })
                    }
                };
                if let Some((completion, line, deadline)) = outgoing {
                    match send(&line, deadline) {
                        Ok(receipt) => {
                            let mut state = lock(&shared.state);
                            if matches!(receipt.status(), RpcWriteStatus::Finished(o) if o != RpcWriteOutcome::Written)
                            {
                                state.input_closed = true;
                                draining = true;
                            }
                            if let Some(p) =
                                state
                                    .pending
                                    .iter_mut()
                                    .find(|p| match (&p.request, &completion) {
                                        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                                        _ => false,
                                    })
                            {
                                p.receipt = Some(receipt);
                            }
                            if draining {
                                break;
                            }
                        }
                        Err(error) => {
                            // Rejected admission emits no bytes; Closed can precede
                            // buffered native responses and the ordered EOF event.
                            {
                                let mut state = lock(&shared.state);
                                if matches!(error, Error::Closed) {
                                    state.input_closed = true;
                                }
                                if let Some(p) = state.pending.iter_mut().find(|p| {
                                    match (&p.request, &completion) {
                                        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                                        _ => false,
                                    }
                                }) {
                                    p.dispatched = false;
                                }
                            }
                            if matches!(error, Error::Closed) {
                                draining = true;
                            } else {
                                shared.stop(Failure::WriteFailed);
                            }
                            break;
                        }
                    }
                }
            }
        }
        crossbeam_channel::select! {
            recv(incoming) -> event => {
                let result = match event {
                    Ok(RpcEvent::Line { seq, line, .. }) => ingress(&shared, seq, &line),
                    Ok(RpcEvent::StdinError { .. }) => { lock(&shared.state).input_closed = true; draining = true; Ok(()) },
                    Ok(RpcEvent::Error { error, .. }) => Err(Failure::Transport(error)),
                    Ok(RpcEvent::Exit { .. }) | Err(_) => Err(Failure::Eof),
                };
                if let Err(f) = result { shared.stop(f); }
            }
            recv(wakes) -> _ => {}
            default(Duration::from_millis(5)) => {}
        }
    }
}
fn maintenance(shared: &Shared, draining: &mut bool) -> Result<(), Failure> {
    let mut finish = Vec::new();
    {
        let mut state = lock(&shared.state);
        if state.snapshot.failure.is_some() {
            return Ok(());
        }
        if state
            .permissions
            .values()
            .any(|p| p.deadline <= Instant::now())
        {
            return Err(Failure::ApprovalExpired);
        }
        *draining |= state.input_closed;
        let mut observed_failure = false;
        for p in &mut state.pending {
            if p.deadline <= Instant::now() && !p.responded {
                return Err(Failure::DeadlineExceeded);
            }
            match p.receipt.as_ref().map(|r| r.status()) {
                Some(RpcWriteStatus::Finished(RpcWriteOutcome::Written))
                    if p.id.is_none() && !p.responded =>
                {
                    p.responded = true;
                    p.request_outcome = Some(RequestOutcome::Written);
                    if let Some(c) = &p.request {
                        finish.push((Arc::clone(c), p.request_outcome.clone().unwrap()));
                    }
                }
                Some(RpcWriteStatus::Finished(RpcWriteOutcome::Written)) => {}
                Some(RpcWriteStatus::Finished(_)) => {
                    *draining = true;
                    observed_failure = true;
                    if p.deadline <= Instant::now() {
                        return Err(Failure::WriteFailed);
                    }
                }
                _ => {
                    if p.deadline <= Instant::now() {
                        return Err(Failure::DeadlineExceeded);
                    }
                }
            }
        }
        state.input_closed |= observed_failure;
    }
    for (c, o) in finish {
        c.finish(o);
    }
    lock(&shared.state).pending.retain(|p| {
        !(p.responded
            && p.request.as_ref().is_none_or(|c| c.get().is_some())
            && matches!(
                p.receipt.as_ref().map(|r| r.status()),
                Some(RpcWriteStatus::Finished(RpcWriteOutcome::Written))
            ))
    });
    Ok(())
}
fn ingress(shared: &Arc<Shared>, seq: u64, line: &str) -> Result<(), Failure> {
    if line.len() + 1 > shared.limits.max_frame_bytes {
        return Err(Failure::Protocol);
    }
    let frame: Value = serde_json::from_str(line).map_err(|_| Failure::Protocol)?;
    if !frame.is_object() || frame["jsonrpc"] != "2.0" {
        return Err(Failure::Protocol);
    }
    if let Some(method) = frame.get("method") {
        let method = method
            .as_str()
            .filter(|m| !m.is_empty())
            .ok_or(Failure::Protocol)?;
        if frame.get("result").is_some() || frame.get("error").is_some() {
            return Err(Failure::Protocol);
        }
        let params = frame.get("params").unwrap_or(&Value::Null);
        if !params.is_null() && !params.is_object() && !params.is_array() {
            return Err(Failure::Protocol);
        }
        if matches!(method, "session/update" | "session/request_permission") && !params.is_object()
        {
            return Err(Failure::Protocol);
        }
        let mut permission = None;
        let mut kind = EventKind::Other;
        if let Some(id) = frame.get("id") {
            let key = request_key(id)?;
            let mut state = lock(&shared.state);
            if state.snapshot.failure.is_some() {
                return Ok(());
            }
            if method == "session/request_permission"
                && params["sessionId"].as_str() == state.snapshot.session_id.as_deref()
                && state.snapshot.session_id.is_some()
            {
                let options = params["options"].as_array().ok_or(Failure::Protocol)?;
                if options.is_empty()
                    || !params["toolCall"].is_object()
                    || options
                        .iter()
                        .any(|o| !o["optionId"].as_str().is_some_and(|id| !id.is_empty()))
                {
                    return Err(Failure::Protocol);
                }
                let bytes = line.len() + key.len();
                if state.permissions.contains_key(&key) {
                    return Err(Failure::Protocol);
                }
                if state.permissions.len() >= shared.limits.approvals
                    || bytes
                        > shared
                            .limits
                            .approval_bytes
                            .saturating_sub(state.permissions.values().map(|p| p.bytes).sum())
                {
                    return Err(Failure::Overloaded);
                }
                let nonce = next_nonce(&mut state).map_err(|_| Failure::Overloaded)?;
                let params = Arc::new(params.clone());
                let deadline = Instant::now() + shared.limits.approval_timeout;
                state.permissions.insert(
                    key.clone(),
                    Offered {
                        nonce,
                        id: id.clone(),
                        params: Arc::clone(&params),
                        deadline,
                        bytes,
                    },
                );
                permission = Some(Permission {
                    id: id.clone(),
                    deadline,
                    origin: (shared.generation, seq),
                    shared: Arc::downgrade(shared),
                    key,
                    nonce,
                    request: params,
                });
                kind = EventKind::ApprovalRequested;
            } else if !state.input_closed {
                shared.enqueue(&mut state,None,Action::Control,json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unsupported reverse method"}}),true,Instant::now()+shared.limits.control_timeout).map_err(|_|Failure::Overloaded)?;
            }
        } else if method == "session/update" {
            let state = lock(&shared.state);
            if params["sessionId"].as_str() != state.snapshot.session_id.as_deref()
                || state.snapshot.session_id.is_none()
            {
                return Err(Failure::Protocol);
            }
            let update = &params["update"];
            let tag = update["sessionUpdate"].as_str().ok_or(Failure::Protocol)?;
            if tag == "agent_message_chunk" && update["content"]["type"] == "text" {
                kind = EventKind::Text {
                    text: update["content"]["text"]
                        .as_str()
                        .ok_or(Failure::Protocol)?
                        .into(),
                    update: TextUpdate::Delta,
                    phase: TextPhase::Unknown,
                };
            }
        }
        publish(shared, seq, method, &frame, kind, permission)?;
        return Ok(());
    }
    let id = frame.get("id").ok_or(Failure::Protocol)?;
    request_key(id)?;
    let result = frame.get("result");
    let error = frame.get("error");
    if result.is_some() == error.is_some() {
        return Err(Failure::Protocol);
    }
    if error.is_some_and(|e| !e["code"].is_i64() || !e["message"].is_string()) {
        return Err(Failure::Protocol);
    }
    let (completion, outcome, method, kind, cleanup_failed) = {
        let mut state = lock(&shared.state);
        let Some(index) = state
            .pending
            .iter()
            .position(|p| p.id.is_some() && p.id.as_deref() == id.as_str())
        else {
            return Ok(());
        };
        if !state.pending[index].dispatched {
            return Err(Failure::Protocol);
        }
        if state.pending[index].responded {
            return Ok(());
        }
        if state.pending[index].deadline <= Instant::now() {
            return Err(Failure::DeadlineExceeded);
        }
        let action = state.pending[index].action.clone();
        let mut kind = EventKind::Other;
        let method = match &action {
            Action::Initialize => "initialize",
            Action::New => "session/new",
            Action::Restore { method, .. } => method,
            Action::Prompt => "session/prompt",
            Action::Control => "control",
        };
        if let Some(result) = result {
            if !result.is_object() {
                return Err(Failure::Protocol);
            }
            match &action {
                Action::Initialize => {
                    if result["protocolVersion"].as_u64() != Some(1)
                        || !result["agentCapabilities"].is_object()
                    {
                        return Err(Failure::Protocol);
                    }
                    state.snapshot.initialized = Some(Arc::new(result.clone()));
                    state.mutation = false;
                }
                Action::New => {
                    let id = result["sessionId"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or(Failure::Protocol)?;
                    state.snapshot.session_id = Some(id.into());
                    state.mutation = false;
                    kind = EventKind::SessionStarted;
                }
                Action::Restore { id: expected, .. } => {
                    if result
                        .get("sessionId")
                        .is_some_and(|id| id.as_str() != Some(expected))
                    {
                        return Err(Failure::Protocol);
                    }
                    state.mutation = false;
                }
                Action::Prompt => {
                    let reason = result["stopReason"].as_str().ok_or(Failure::Protocol)?;
                    kind = EventKind::TurnCompleted {
                        status: match reason {
                            "end_turn" => crate::portable::CompletionStatus::Completed,
                            "cancelled" => crate::portable::CompletionStatus::Interrupted,
                            _ => crate::portable::CompletionStatus::Unknown,
                        },
                    };
                    state.snapshot.prompting = false;
                }
                Action::Control => {}
            }
        } else {
            match &action {
                Action::Initialize | Action::New => state.mutation = false,
                Action::Restore { .. } => {
                    state.mutation = false;
                    state.snapshot.session_id = None;
                }
                Action::Prompt => state.snapshot.prompting = false,
                Action::Control => {}
            }
        }
        let outcome = if let Some(result) = result {
            RequestOutcome::Response(Arc::new(result.clone()))
        } else {
            RequestOutcome::RemoteError(Arc::new(error.unwrap().clone()))
        };
        if matches!(action, Action::Prompt) {
            state.snapshot.last_prompt = Some(PromptObservation {
                context: EventContext {
                    provider: Provider::Acp,
                    generation: shared.generation,
                    seq,
                    session_id: state.snapshot.session_id.clone(),
                    turn_id: None,
                    item_id: None,
                    request_id: id.as_str().map(|s| RequestId::String(s.into())),
                },
                outcome: outcome.clone(),
            });
        }
        let pending = &mut state.pending[index];
        pending.responded = true;
        pending.request_outcome = Some(outcome.clone());
        let completion = pending.request.clone();
        let mut cleanup_failed = false;
        if matches!(action, Action::Prompt) {
            // Terminal prompt observation revokes permission authority before the
            // host can start another prompt. Reply cancelled, never implicitly allow.
            for (_, permission) in std::mem::take(&mut state.permissions) {
                if state.input_closed {
                    continue;
                }
                let frame = json!({"jsonrpc":"2.0","id":permission.id,"result":{"outcome":{"outcome":"cancelled"}}});
                if shared
                    .enqueue(
                        &mut state,
                        None,
                        Action::Control,
                        frame,
                        true,
                        Instant::now() + shared.limits.control_timeout,
                    )
                    .is_err()
                {
                    cleanup_failed = true;
                }
            }
        }
        (completion, outcome, method, kind, cleanup_failed)
    };
    // Native outcomes commit before wakeups/event publication; a racing close sees them.
    if let Some(c) = completion {
        c.finish(outcome);
    }
    if cleanup_failed {
        return Err(Failure::Overloaded);
    }
    publish(shared, seq, method, &frame, kind, None)
}
fn request_key(id: &Value) -> Result<String, Failure> {
    match id {
        Value::String(s) => Ok(format!("s:{s}")),
        Value::Number(n) if n.is_i64() || n.is_u64() => Ok(format!("n:{n}")),
        _ => Err(Failure::Protocol),
    }
}
fn publish(
    shared: &Arc<Shared>,
    seq: u64,
    method: &str,
    frame: &Value,
    kind: EventKind,
    permission: Option<Permission>,
) -> Result<(), Failure> {
    let session = frame["params"]["sessionId"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            if frame.get("method").is_none() {
                lock(&shared.state).snapshot.session_id.clone()
            } else {
                None
            }
        });
    let portable = EventV1 {
        context: EventContext {
            provider: Provider::Acp,
            generation: shared.generation,
            seq,
            session_id: session,
            turn_id: None,
            item_id: frame["params"]["update"]["toolCallId"]
                .as_str()
                .or_else(|| frame["params"]["toolCall"]["toolCallId"].as_str())
                .map(str::to_owned),
            request_id: match frame.get("id") {
                Some(Value::String(s)) => Some(RequestId::String(s.clone())),
                Some(Value::Number(n)) => n.as_i64().map(RequestId::Number),
                _ => None,
            },
        },
        native_method: method.into(),
        kind,
    };
    let bytes = frame
        .to_string()
        .len()
        .saturating_mul(if permission.is_some() { 2 } else { 1 })
        .saturating_add(portable.retained_bytes());
    if !shared.events.push(
        AcpEvent {
            portable,
            native: (shared.native_events == NativeEvents::Retain).then(|| Arc::new(frame.clone())),
            permission,
        },
        bytes,
    ) {
        return Err(Failure::ConsumerLagged);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
