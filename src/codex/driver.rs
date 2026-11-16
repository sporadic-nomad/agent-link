//! Codex's wire dialect and native state transitions. No application code runs here.
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde_json::{Value, json};

use crate::portable::{EventContext, EventKind, Provider};
use crate::protocol::lock;
use crate::rpc::{RpcChild, RpcEvent, RpcWriteOutcome, RpcWriteStatus};

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WriteState {
    Open,
    Draining,
}

pub(super) fn run(
    shared: Arc<Shared>,
    child: RpcChild,
    incoming: Receiver<RpcEvent>,
    wakes: Receiver<()>,
) {
    run_loop(Arc::clone(&shared), incoming, wakes, |line, deadline| {
        child.send_line_tracked(line, deadline)
    });
    // No controller locks or user callbacks during kill/reap.
    drop(child);
    shared
        .dispatcher_finished
        .store(true, std::sync::atomic::Ordering::Release);
}

// The production sender only admits to RpcChild; controlled tests can supply pipe
// observations without racing real writer and reader threads.
pub(super) fn run_loop(
    shared: Arc<Shared>,
    incoming: Receiver<RpcEvent>,
    wakes: Receiver<()>,
    mut send: impl FnMut(&str, Instant) -> crate::Result<RpcWriteReceipt>,
) {
    let mut writes = WriteState::Open;
    loop {
        match maintenance(&shared) {
            Ok(WriteState::Draining) => writes = WriteState::Draining,
            Ok(WriteState::Open) => {}
            Err(failure) => shared.stop(failure),
        }
        if lock(&shared.state).snapshot.failure.is_some() {
            break;
        }
        // At most one active write per lane. Reserved control capacity is independent
        // of ordinary admissions; the pipe itself remains ordered and cannot preempt
        // an active frame. Its absolute deadline bounds that unavoidable wait.
        if writes == WriteState::Open {
            for control in [true, false] {
                match dispatch(&shared, &mut send, control) {
                    Ok(WriteState::Open) => {}
                    Ok(WriteState::Draining) => {
                        writes = WriteState::Draining;
                        break;
                    }
                    Err(failure) => {
                        shared.stop(failure);
                        break;
                    }
                }
            }
        }
        if lock(&shared.state).snapshot.failure.is_some() {
            break;
        }
        crossbeam_channel::select! {
            recv(incoming) -> event => {
                let result = match event {
                    Ok(RpcEvent::Line { seq, line, .. }) => ingress(&shared, seq, &line),
                    Ok(RpcEvent::StdinError { .. }) => { lock(&shared.state).input_closed = true; writes = WriteState::Draining; Ok(()) },
                    Ok(RpcEvent::Error { error, .. }) => Err(Failure::Transport(error)),
                    Ok(RpcEvent::Exit { .. }) | Err(_) => Err(Failure::Eof),
                };
                if let Err(failure) = result { shared.stop(failure); }
            }
            recv(wakes) -> _ => {}
            // RpcWriteReceipt has no dispatcher wake callback. Bounded polling keeps
            // pipe progress/deadlines independent of incoming traffic and host events.
            default(Duration::from_millis(5)) => {}
        }
    }
}

pub(super) fn maintenance(shared: &Shared) -> Result<WriteState, Failure> {
    let now = Instant::now();
    let mut completed = Vec::new();
    let mut writes = WriteState::Open;
    let failure = {
        let mut state = lock(&shared.state);
        if state.snapshot.failure.is_some() {
            return Ok(WriteState::Draining);
        }
        if state.input_closed {
            writes = WriteState::Draining;
        }
        let mut failure = None;
        state.pending.retain_mut(|p| {
            let status = p.receipt.as_ref().map(RpcWriteReceipt::status);
            match status {
                Some(RpcWriteStatus::Finished(RpcWriteOutcome::Written)) if p.responded => {
                    if let Some(reply) = &p.reply {
                        completed.push(Arc::clone(reply));
                        true // retain admission through publication outside the lock
                    } else {
                        p.request
                            .as_ref()
                            .is_some_and(|request| request.get().is_none())
                    }
                }
                Some(RpcWriteStatus::Finished(RpcWriteOutcome::Written)) => {
                    if p.deadline <= now {
                        failure = Some(Failure::DeadlineExceeded);
                    }
                    true
                }
                Some(RpcWriteStatus::Finished(_)) => {
                    // Input settles before RpcChild publishes its ordered terminal
                    // event (EOF may still be draining stderr). Preserve preceding
                    // native frames, but never extend an admitted request's deadline.
                    writes = WriteState::Draining;
                    if p.deadline <= now {
                        failure = Some(Failure::DeadlineExceeded);
                    }
                    true
                }
                _ => {
                    if p.deadline <= now {
                        failure = Some(Failure::DeadlineExceeded);
                    }
                    true
                }
            }
        });
        if writes == WriteState::Draining {
            state.input_closed = true;
        }
        if state.approvals.values().any(|a| a.request.deadline <= now) {
            failure = Some(Failure::ApprovalExpired);
        }
        failure
    };
    for completion in &completed {
        completion.finish(ReplyOutcome::Written);
    }
    if !completed.is_empty() {
        lock(&shared.state)
            .pending
            .retain(|p| p.reply.as_ref().is_none_or(|reply| reply.get().is_none()));
    }
    failure.map_or(Ok(writes), Err)
}

fn dispatch(
    shared: &Shared,
    send: &mut impl FnMut(&str, Instant) -> crate::Result<RpcWriteReceipt>,
    control: bool,
) -> Result<WriteState, Failure> {
    let next = {
        let mut state = lock(&shared.state);
        if state.snapshot.failure.is_some() {
            return Ok(WriteState::Draining);
        }
        if state.input_closed {
            return Ok(WriteState::Draining);
        }
        let active = state.pending.iter().any(|p| {
            p.control == control
                && p.dispatched
                && !matches!(
                    p.receipt.as_ref().map(RpcWriteReceipt::status),
                    Some(RpcWriteStatus::Finished(_))
                )
        });
        if active || (!control && state.pending.iter().any(|p| p.control && !p.dispatched)) {
            return Ok(WriteState::Open);
        }
        state
            .pending
            .iter_mut()
            .enumerate()
            .find(|(_, p)| p.control == control && !p.dispatched)
            .map(|(index, p)| {
                p.dispatched = true;
                (
                    index,
                    p.line.take().expect("registered unsent frame"),
                    p.deadline,
                )
            })
    };
    if let Some((index, line, deadline)) = next {
        let receipt = send(&line, deadline);
        let mut state = lock(&shared.state);
        if state.snapshot.failure.is_some() {
            return Ok(WriteState::Draining);
        }
        // Only this dispatcher removes pending entries; host threads only append.
        let pending = &mut state.pending[index];
        match receipt {
            Ok(receipt) => {
                let failed = matches!(receipt.status(), RpcWriteStatus::Finished(outcome) if outcome != RpcWriteOutcome::Written);
                pending.receipt = Some(receipt);
                if failed {
                    state.input_closed = true;
                    return Ok(WriteState::Draining);
                }
            }
            Err(Error::Closed) => {
                // This can precede the EOF/error event even if previous writes all
                // completed successfully. Keep reading instead of truncating output.
                pending.dispatched = false;
                state.input_closed = true;
                return Ok(WriteState::Draining);
            }
            Err(_) => {
                pending.dispatched = false;
                return Err(Failure::WriteFailed);
            }
        }
    }
    Ok(WriteState::Open)
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, Failure> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(Failure::Protocol)
}

fn wire_id(id: &Value) -> Result<String, Failure> {
    if id.as_str().is_some() || id.as_i64().is_some() {
        Ok(id.to_string())
    } else {
        Err(Failure::Protocol)
    }
}

pub(super) fn ingress(shared: &Arc<Shared>, seq: u64, line: &str) -> Result<(), Failure> {
    let arrived = Instant::now();
    let value: Value = serde_json::from_str(line).map_err(|_| Failure::Protocol)?;
    if !value.is_object() {
        return Err(Failure::Protocol);
    }
    let value = Arc::new(value);
    let bytes = line.len() + 1;
    if let Some(method) = value.get("method") {
        let method = method
            .as_str()
            .filter(|m| !m.is_empty())
            .ok_or(Failure::Protocol)?;
        if value.get("result").is_some() || value.get("error").is_some() {
            return Err(Failure::Protocol);
        }
        let params = value.get("params").unwrap_or(&Value::Null);
        let portable = projection::project(shared.generation, seq, &value);
        let mut approval = None;
        if let Some(id) = value.get("id") {
            wire_id(id)?;
            match server_request(shared, &value, &portable.context, bytes, arrived)? {
                ServerRequest::Publish(token) => approval = token,
                // Exact duplicate pending approvals do not generate a second capability.
                ServerRequest::Duplicate => return Ok(()),
            }
        } else {
            notification(shared, seq, method, params)?;
        }
        let native = (shared.native_events == NativeEvents::Retain).then(|| Arc::clone(&value));
        if let EventKind::Usage(usage) = portable.kind {
            let mut state = lock(&shared.state);
            if state.snapshot.failure.is_none()
                && state.snapshot.thread_id.is_some()
                && state.snapshot.thread_id == portable.context.session_id
            {
                let observation = UsageObservation {
                    context: portable.context.clone(),
                    usage,
                    native: native.clone(),
                };
                if state.snapshot.active_turn == observation.context.turn_id {
                    state.active_usage = Some(observation.clone());
                }
                if let Some(last) = state.snapshot.last_turn.as_mut() {
                    if Some(&last.turn_id) == observation.context.turn_id.as_ref() {
                        last.usage = Some(observation.clone());
                    }
                }
                state.snapshot.last_usage = Some(observation);
            }
        }
        // Capabilities keep their own parameters/context after leaving the registry.
        // Charge those too, even if omission makes this a conservative overestimate.
        let capability_bytes = approval.as_ref().map_or(0, |a| {
            bytes.saturating_add(a.request.context.retained_bytes())
        });
        let bytes = bytes
            .saturating_add(portable.retained_bytes())
            .saturating_add(capability_bytes);
        if !shared.events.push(
            CodexEvent {
                generation: shared.generation,
                seq,
                portable,
                native,
                approval,
            },
            bytes,
        ) {
            return Err(Failure::ConsumerLagged);
        }
        Ok(())
    } else {
        let id = value.get("id").ok_or(Failure::Protocol)?;
        wire_id(id)?;
        if value.get("result").is_some() == value.get("error").is_some() {
            return Err(Failure::Protocol);
        }
        if let Some(error) = value.get("error") {
            if error.get("code").and_then(Value::as_i64).is_none()
                || error.get("message").and_then(Value::as_str).is_none()
            {
                return Err(Failure::Protocol);
            }
        }
        response(shared, seq, id, &value)
    }
}

fn response(shared: &Shared, seq: u64, id: &Value, value: &Value) -> Result<(), Failure> {
    let (completion, outcome) = {
        let mut state = lock(&shared.state);
        if state.snapshot.failure.is_some() {
            return Ok(());
        }
        let Some(index) = state
            .pending
            .iter()
            .position(|p| p.id.as_deref() == id.as_str() && p.id.is_some())
        else {
            // No completion cache: unknown, old-generation and duplicate replies cannot
            // settle a new operation. Native request IDs never repeat on this connection.
            return Ok(());
        };
        let p = &state.pending[index];
        if !p.dispatched {
            return Err(Failure::Protocol);
        }
        if p.responded {
            return Ok(());
        }
        if p.deadline <= Instant::now() {
            return Err(Failure::DeadlineExceeded);
        }
        let action = p.action.clone();
        let outcome = if let Some(result) = value.get("result") {
            apply_result(shared, &mut state, seq, &action, result)?;
            RequestOutcome::Response(Arc::new(result.clone()))
        } else {
            // A resume refusal never changes identity or falls back to thread/start.
            // The provisional identity remains unusable without a successful thread result.
            match action {
                Action::Initialize => state.initialized_pending = false,
                Action::ThreadStart | Action::ThreadResume(_) => {
                    state.thread_pending = false;
                    state.resume_activity_observed = false;
                    state.approvals.clear();
                }
                Action::TurnStart | Action::Steer(_) => state.mutation_pending = false,
                _ => {}
            }
            RequestOutcome::RemoteError(Arc::new(value["error"].clone()))
        };
        let p = &mut state.pending[index];
        p.responded = true;
        // Shutdown can publish this same outcome if it races the finish below.
        p.request_outcome = Some(outcome.clone());
        (p.request.clone(), outcome)
    };
    if let Some(completion) = completion {
        completion.finish(outcome);
    }
    Ok(())
}

fn bind_thread(state: &mut State, id: &str) -> Result<(), Failure> {
    if state
        .snapshot
        .thread_id
        .as_deref()
        .is_some_and(|old| old != id)
    {
        return Err(Failure::Protocol);
    }
    state.snapshot.thread_id = Some(id.into());
    Ok(())
}

fn apply_result(
    shared: &Shared,
    state: &mut State,
    seq: u64,
    action: &Action,
    result: &Value,
) -> Result<(), Failure> {
    if !result.is_object() {
        return Err(Failure::Protocol);
    }
    match action {
        Action::Initialize => {
            required(result, "userAgent")?;
            shared.control(state, json!({"method":"initialized"}), None)?;
            state.initialized_pending = false;
            state.snapshot.initialized = true;
            state.snapshot.server = Some(Arc::new(result.clone()));
        }
        Action::ThreadStart | Action::ThreadResume(_) => {
            let thread = result.get("thread").ok_or(Failure::Protocol)?;
            let id = required(thread, "id")?;
            bind_thread(state, id)?;
            if let Action::ThreadResume(expected) = action {
                if expected != id {
                    return Err(Failure::Protocol);
                }
            }
            // Resume snapshots can include a live turn without a turn/started event.
            // Earlier notifications are newer than the returned snapshot; do not
            // resurrect a turn already observed terminal before this response.
            let mut has_activity_evidence =
                state.resume_activity_observed || state.snapshot.active_turn.is_some();
            if let Some(turns) = thread.get("turns").and_then(Value::as_array) {
                for turn in turns {
                    if turn.get("status").and_then(Value::as_str) == Some("inProgress") {
                        observe_turn(shared, state, seq, id, turn, false)?;
                        has_activity_evidence = true;
                    }
                }
            }
            // Warm resume can acknowledge activity before replay supplies its ID,
            // even with turn history enabled. Commit the busy guard before settling
            // the ticket. An identified snapshot turn already observed terminal
            // remains terminal, including when identifying replay finished before
            // an empty snapshot arrived. Unrelated completions cannot resolve an
            // unknown ID.
            state.unidentified_active_turn = thread.pointer("/status/type").and_then(Value::as_str)
                == Some("active")
                && !has_activity_evidence;
            state.thread_pending = false;
            state.resume_activity_observed = false;
            state.snapshot.thread = Some(Arc::new(result.clone()));
        }
        Action::TurnStart => {
            let thread = state.snapshot.thread_id.clone().ok_or(Failure::Protocol)?;
            let turn = result.get("turn").ok_or(Failure::Protocol)?;
            observe_turn(shared, state, seq, &thread, turn, false)?;
            state.mutation_pending = false;
        }
        Action::Steer(expected) => {
            if required(result, "turnId")? != expected {
                return Err(Failure::Protocol);
            }
            state.mutation_pending = false;
        }
        Action::Interrupt(_) | Action::Control => {}
    }
    Ok(())
}

fn notification(shared: &Shared, seq: u64, method: &str, params: &Value) -> Result<(), Failure> {
    let mut state = lock(&shared.state);
    if state.snapshot.failure.is_some() {
        return Ok(());
    }
    match method {
        "thread/started" => {
            required(params.get("thread").ok_or(Failure::Protocol)?, "id")?;
            // Initialized connections also receive newly created subagent threads.
            // Only a correlated thread/start result chooses the root; resume pins
            // its requested identity at admission so early replay remains routable.
        }
        "turn/started" | "turn/completed" => {
            let thread = required(params, "threadId")?;
            let turn = params.get("turn").ok_or(Failure::Protocol)?;
            let terminal = method == "turn/completed";
            validate_turn(turn, terminal)?;
            if state.snapshot.thread_id.as_deref() == Some(thread) {
                observe_turn(shared, &mut state, seq, thread, turn, terminal)?;
            }
            // Other threads retain their native envelopes in the bounded event
            // queue, without consuming root snapshot or terminal-history capacity.
        }
        "serverRequest/resolved" => {
            // Native resolution invalidates the captured capability, even if the UI
            // still holds it. Numeric and string IDs retain their distinct type.
            let thread = required(params, "threadId")?;
            let id = params.get("requestId").ok_or(Failure::Protocol)?;
            let key = wire_id(id)?;
            if state.snapshot.thread_id.as_deref() == Some(thread) {
                state.approvals.remove(&key);
            }
        }
        _ => {} // Item updates and unknown extensions remain intact in native events.
    }
    Ok(())
}

fn validate_turn(turn: &Value, terminal: bool) -> Result<(&str, &str), Failure> {
    let id = required(turn, "id")?;
    let status = required(turn, "status")?;
    if terminal && status == "inProgress" {
        return Err(Failure::Protocol);
    }
    Ok((id, status))
}

fn observe_turn(
    shared: &Shared,
    state: &mut State,
    seq: u64,
    thread: &str,
    turn: &Value,
    terminal: bool,
) -> Result<(), Failure> {
    let (id, status) = validate_turn(turn, terminal)?;
    if terminal {
        if !state.terminal_turns.contains(id) {
            let bytes: usize = state.terminal_turns.iter().map(String::len).sum();
            if state.terminal_turns.len() >= shared.limits.events
                || id.len() > shared.limits.max_frame_bytes.saturating_sub(bytes)
            {
                return Err(Failure::Overloaded);
            }
            state.terminal_turns.insert(id.into());
        }
        // Results and notifications establish the same terminal state. Invalidate
        // only this turn's capabilities before exposing its terminal snapshot.
        state
            .approvals
            .retain(|_, a| a.request.params.get("turnId").and_then(Value::as_str) != Some(id));
        // last_usage is newest by arrival, but can belong to another root turn.
        // Preserve the active sample or an already attached matching terminal sample
        // when unrelated usage has arrived since either was observed.
        let usage = state
            .snapshot
            .last_usage
            .as_ref()
            .filter(|usage| usage.context.turn_id.as_deref() == Some(id))
            .or_else(|| {
                state
                    .active_usage
                    .as_ref()
                    .filter(|usage| usage.context.turn_id.as_deref() == Some(id))
            })
            .or_else(|| {
                state
                    .snapshot
                    .last_turn
                    .as_ref()
                    .filter(|last| last.turn_id == id)
                    .and_then(|last| last.usage.as_ref())
            })
            .cloned();
        if state.snapshot.active_turn.as_deref() == Some(id) {
            state.snapshot.active_turn = None;
            state.active_usage = None;
        }
        let status = projection::status(status);
        state.snapshot.last_turn = Some(TurnOutcome {
            context: EventContext {
                provider: Provider::Codex,
                generation: shared.generation,
                seq,
                session_id: Some(thread.into()),
                turn_id: Some(id.into()),
                item_id: None,
                request_id: None,
            },
            thread_id: thread.into(),
            turn_id: id.into(),
            status,
            native: Arc::new(turn.clone()),
            usage,
        });
    } else if status == "inProgress" {
        if state.terminal_turns.contains(id) {
            return Ok(());
        }
        if state
            .snapshot
            .active_turn
            .as_deref()
            .is_some_and(|active| active != id)
        {
            return Err(Failure::Protocol);
        }
        activate_turn(state, id);
    } else {
        // A start response can race the terminal event; preserve explicit terminal
        // status without inventing a transcript or a completed active successor.
        observe_turn(shared, state, seq, thread, turn, true)?;
    }
    Ok(())
}

// Both native turns and approval replay can identify activity. Seed from a matching
// preceding sample once, then keep it across repeated activity and unrelated usage.
fn activate_turn(state: &mut State, id: &str) {
    if state.snapshot.active_turn.as_deref() != Some(id) {
        state.active_usage = state
            .snapshot
            .last_usage
            .as_ref()
            .filter(|usage| usage.context.turn_id.as_deref() == Some(id))
            .cloned();
    }
    state.snapshot.active_turn = Some(id.into());
    state.unidentified_active_turn = false;
    state.resume_activity_observed |= state.thread_pending;
}

fn is_approval(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
    )
}

enum ServerRequest {
    Publish(Option<Approval>),
    Duplicate,
}

fn server_request(
    shared: &Arc<Shared>,
    value: &Value,
    context: &EventContext,
    bytes: usize,
    arrived: Instant,
) -> Result<ServerRequest, Failure> {
    let method = required(value, "method")?;
    let id = value.get("id").ok_or(Failure::Protocol)?;
    let params = value.get("params").unwrap_or(&Value::Null);
    let mut state = lock(&shared.state);
    if state.snapshot.failure.is_some() {
        return Ok(ServerRequest::Duplicate);
    }
    if is_approval(method) {
        required(params, "threadId")?;
        required(params, "turnId")?;
        required(params, "itemId")?;
    }
    let key = wire_id(id)?;
    if let Some(old) = state.approvals.get(&key) {
        if old.request.method != method || *old.request.params != *params {
            return Err(Failure::Protocol);
        }
        return Ok(ServerRequest::Duplicate);
    }
    if !is_approval(method)
        || state.snapshot.thread_id.as_deref() != params.get("threadId").and_then(Value::as_str)
    {
        // Child-thread approvals are outside this root-only API. Explicitly reject
        // them through reserved control capacity, then forward the native request
        // without a capability. Never bind or terminate the root just for its scope.
        shared.control(
            &mut state,
            json!({"id": id, "error":{"code":-32601,"message":"Unsupported client method"}}),
            None,
        )?;
        return Ok(ServerRequest::Publish(None));
    }
    let turn = required(params, "turnId")?;
    if state
        .snapshot
        .active_turn
        .as_deref()
        .is_some_and(|active| active != turn)
        || state.terminal_turns.contains(turn)
        || state
            .snapshot
            .last_turn
            .as_ref()
            .is_some_and(|last| last.turn_id == turn)
    {
        return Err(Failure::Protocol);
    }
    let used: usize = state.approvals.values().map(|a| a.bytes).sum();
    let bytes = bytes.saturating_add(context.retained_bytes());
    if state.approvals.len() >= shared.limits.approvals
        || bytes > shared.limits.approval_bytes.saturating_sub(used)
    {
        return Err(Failure::Overloaded);
    }
    state.next_id = state.next_id.checked_add(1).ok_or(Failure::Overloaded)?;
    let nonce = state.next_id;
    // An approval proves this turn is live even without turn/started or an
    // inProgress entry in the resume snapshot. Commit that evidence before host
    // publication; answering/resolving the approval does not establish turn idle.
    activate_turn(&mut state, turn);
    let request = Arc::new(ApprovalRequest {
        context: context.clone(),
        id: id.clone(),
        method: method.into(),
        params: Arc::new(params.clone()),
        deadline: arrived + shared.limits.approval_timeout,
    });
    state.approvals.insert(
        key.clone(),
        PendingApproval {
            nonce,
            request: Arc::clone(&request),
            bytes,
        },
    );
    Ok(ServerRequest::Publish(Some(Approval {
        shared: Arc::downgrade(shared),
        key,
        nonce,
        request,
    })))
}

pub(super) fn decision_allowed(approval: &ApprovalRequest, decision: &Value) -> bool {
    if !decision_supported(approval, decision) {
        return false;
    }
    if let Some(offered) = approval
        .params
        .get("availableDecisions")
        .filter(|v| !v.is_null())
    {
        return offered
            .as_array()
            .is_some_and(|choices| choices.contains(decision));
    }
    // Stable subset. Policy amendments are accepted only when explicitly offered.
    decision.is_string()
}

pub(super) fn decision_supported(approval: &ApprovalRequest, decision: &Value) -> bool {
    let simple = matches!(
        decision.as_str(),
        Some("accept" | "acceptForSession" | "decline" | "cancel")
    );
    let amendment = approval.method == "item/commandExecution/requestApproval"
        && decision.as_object().is_some_and(|obj| {
            obj.len() == 1
                && (obj.contains_key("acceptWithExecpolicyAmendment")
                    || obj.contains_key("applyNetworkPolicyAmendment"))
        });
    simple || amendment
}
