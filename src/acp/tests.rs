//! Controlled native state and resource-bound tests, without a provider process.
use super::*;
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}
// These synthetic sessions validate the path but never access the filesystem.
fn cwd() -> &'static str {
    if cfg!(windows) {
        r"C:\workspace"
    } else {
        "/workspace"
    }
}
fn harness(limits: AcpLimits) -> (AcpClient, AcpEvents) {
    let (wake, _) = bounded(1);
    let shared = Arc::new(Shared {
        generation: SessionId::next(),
        limits,
        native_events: NativeEvents::Retain,
        state: Mutex::new(State::default()),
        events: Queue::new(limits.events, limits.event_bytes),
        wake,
    });
    (
        AcpClient {
            shared: Arc::clone(&shared),
        },
        AcpEvents(Arc::clone(&shared.events)),
    )
}
fn input(app: &AcpClient, value: Value) {
    if let Err(f) = ingress(&app.shared, 7, &value.to_string()) {
        app.shared.stop(f);
    }
}
fn response(app: &AcpClient, ticket: &RequestTicket, value: Value) {
    {
        let mut s = lock(&app.shared.state);
        let p = s
            .pending
            .iter_mut()
            .find(|p| p.id.as_deref() == Some(ticket.id()))
            .unwrap();
        p.dispatched = true;
        p.receipt = Some(crate::rpc::RpcWriteReceipt::finished_for_test(
            RpcWriteOutcome::Written,
        ));
        p.line = None;
    }
    input(
        app,
        json!({"jsonrpc":"2.0","id":ticket.id(),"result":value}),
    );
    let _ = maintenance(&app.shared, &mut false);
}
fn initialized(limits: AcpLimits) -> (AcpClient, AcpEvents) {
    let (app, events) = harness(limits);
    let init = app.initialize("test", "1", deadline()).unwrap();
    response(
        &app,
        &init,
        json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"resume":{}}},"future":{"preserved":true}}),
    );
    assert!(matches!(
        &*init.outcome().unwrap(),
        RequestOutcome::Response(_)
    ));
    (app, events)
}
fn ready(limits: AcpLimits) -> (AcpClient, AcpEvents) {
    let (app, events) = initialized(limits);
    let new = app.session_new(cwd(), deadline()).unwrap();
    response(&app, &new, json!({"sessionId":"s"}));
    (app, events)
}
fn permission(app: &AcpClient, events: &AcpEvents, id: Value) -> Permission {
    input(
        app,
        json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"tool","title":"Read"},"options":[{"optionId":"yes","name":"Allow once","kind":"allow_once"},{"optionId":"no","name":"Reject","kind":"reject_once"}]}}),
    );
    loop {
        if let Some(p) = events
            .recv_until(deadline())
            .expect("permission event")
            .permission
        {
            return p;
        }
    }
}
#[test]
fn handshake_restore_replay_and_exact_identity() {
    let (app, events) = initialized(AcpLimits::default());
    let load = app.session_load("s", cwd(), deadline()).unwrap();
    input(
        &app,
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"replay"}}}}),
    );
    assert_eq!(app.snapshot().session_id.as_deref(), Some("s"));
    assert!(matches!(
        app.prompt("too early", deadline()),
        Err(AcpError::Busy)
    ));
    response(&app, &load, json!({}));
    assert!(app.failure().is_none());
    assert!(events.recv_until(deadline()).is_some());
    let (other, _) = initialized(AcpLimits::default());
    let resume = other.session_resume("original", cwd(), deadline()).unwrap();
    response(&other, &resume, json!({"sessionId":"replacement"}));
    assert_eq!(other.failure(), Some(Failure::Protocol));
    assert_eq!(other.snapshot().session_id.as_deref(), Some("original"));
}
#[test]
fn incompatible_version_and_unadvertised_restore_are_refused() {
    let (app, _) = harness(AcpLimits::default());
    let init = app.initialize("test", "1", deadline()).unwrap();
    response(
        &app,
        &init,
        json!({"protocolVersion":2,"agentCapabilities":{}}),
    );
    assert_eq!(app.failure(), Some(Failure::Protocol));
    let (app, _) = harness(AcpLimits::default());
    let init = app.initialize("test", "1", deadline()).unwrap();
    response(
        &app,
        &init,
        json!({"protocolVersion":1,"agentCapabilities":{}}),
    );
    assert!(matches!(
        app.session_load("s", cwd(), deadline()),
        Err(AcpError::InvalidState)
    ));
    assert!(app.snapshot().session_id.is_none());
}
#[test]
fn permission_single_use_exact_options_cancel_and_connection_binding() {
    let (app, events) = ready(AcpLimits::default());
    let prompt = app.prompt("hello", deadline()).unwrap();
    let p = permission(&app, &events, json!(17));
    assert!(matches!(
        p.select("invented"),
        Err(AcpError::InvalidDecision)
    ));
    let clone = p.clone();
    p.select("yes").unwrap();
    assert!(matches!(clone.select("yes"), Err(AcpError::StaleApproval)));
    let p = permission(&app, &events, json!("17"));
    app.cancel(deadline()).unwrap();
    assert!(matches!(p.select("yes"), Err(AcpError::StaleApproval)));
    assert!(app.snapshot().prompting);
    response(&app, &prompt, json!({"stopReason":"cancelled"}));
    assert!(!app.snapshot().prompting);
    let (other, other_events) = ready(AcpLimits::default());
    let old = permission(&other, &other_events, json!(1));
    other.close();
    let (_replacement, _) = ready(AcpLimits::default());
    assert!(matches!(old.select("yes"), Err(AcpError::StaleApproval)));
}
#[test]
fn unknown_updates_preserve_native_fields_and_wrong_session_is_terminal() {
    let (app, events) = ready(AcpLimits::default());
    input(
        &app,
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"future_update","extension":[1,2]}}}),
    );
    let mut found = false;
    for _ in 0..3 {
        let e = events.recv_until(deadline()).unwrap();
        if e.native.as_ref().unwrap()["params"]["update"]["sessionUpdate"] == "future_update" {
            assert!(matches!(e.portable.kind, EventKind::Other));
            found = true;
        }
    }
    assert!(found);
    input(
        &app,
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"other","update":{"sessionUpdate":"future_update"}}}),
    );
    assert_eq!(app.failure(), Some(Failure::Protocol));
}
#[test]
fn unsupported_reverse_requests_use_reserved_control_lane() {
    let limits = AcpLimits {
        pending_requests: 1,
        ..AcpLimits::default()
    };
    let (app, _) = ready(limits);
    // Completed requests retain their capacity until their write receipt settles.
    input(
        &app,
        json!({"jsonrpc":"2.0","id":9,"method":"fs/read_text_file","params":{"sessionId":"s","path":"secret"}}),
    );
    assert!(app.failure().is_none());
    let state = lock(&app.shared.state);
    let line = state.pending.back().unwrap().line.as_ref().unwrap();
    let frame: Value = serde_json::from_str(line).unwrap();
    assert_eq!(frame["error"]["code"], -32601);
    assert!(state.permissions.is_empty());
}
#[test]
fn eof_settles_unwritten_and_dispatched_without_inventing_completion() {
    let (app, _) = ready(AcpLimits::default());
    let prompt = app.prompt("hello", deadline()).unwrap();
    {
        let mut s = lock(&app.shared.state);
        s.pending.back_mut().unwrap().dispatched = true;
    }
    app.shared.stop(Failure::Eof);
    assert!(matches!(
        &*prompt.outcome().unwrap(),
        RequestOutcome::OutcomeUnknown {
            failure: Failure::Eof,
            ..
        }
    ));
    let (app, _) = ready(AcpLimits::default());
    let prompt = app.prompt("hello", deadline()).unwrap();
    app.close();
    assert!(matches!(
        &*prompt.outcome().unwrap(),
        RequestOutcome::NotSent(Failure::Closed)
    ));
}
#[test]
fn saturation_is_terminal_but_native_response_is_authoritative() {
    let limits = AcpLimits {
        events: 2,
        ..AcpLimits::default()
    };
    let (app, _) = ready(limits);
    let prompt = app.prompt("hello", deadline()).unwrap();
    response(
        &app,
        &prompt,
        json!({"stopReason":"end_turn","future":true}),
    );
    assert_eq!(app.failure(), Some(Failure::ConsumerLagged));
    assert!(matches!(
        &*prompt.outcome().unwrap(),
        RequestOutcome::Response(_)
    ));
}
#[test]
fn failed_restore_does_not_create_a_session_and_late_ids_are_ignored() {
    let (app, _) = initialized(AcpLimits::default());
    let load = app.session_load("s", cwd(), deadline()).unwrap();
    {
        let mut s = lock(&app.shared.state);
        s.pending.back_mut().unwrap().dispatched = true;
    }
    input(
        &app,
        json!({"jsonrpc":"2.0","id":load.id(),"error":{"code":-32001,"message":"missing"}}),
    );
    assert!(matches!(
        &*load.outcome().unwrap(),
        RequestOutcome::RemoteError(_)
    ));
    assert!(app.snapshot().session_id.is_none());
    input(&app, json!({"jsonrpc":"2.0","id":"unknown","result":{}}));
    assert!(app.failure().is_none());
}
#[test]
fn expired_requests_and_permission_capacity_are_finite() {
    let (app, _) = ready(AcpLimits::default());
    let prompt = app.prompt("hello", deadline()).unwrap();
    {
        let mut s = lock(&app.shared.state);
        s.pending.back_mut().unwrap().deadline = Instant::now();
    }
    assert_eq!(
        maintenance(&app.shared, &mut false),
        Err(Failure::DeadlineExceeded)
    );
    app.shared.stop(Failure::DeadlineExceeded);
    assert!(prompt.outcome().is_some());
    let limits = AcpLimits {
        approvals: 1,
        ..AcpLimits::default()
    };
    let (app, events) = ready(limits);
    let p = permission(&app, &events, json!(1));
    input(
        &app,
        json!({"jsonrpc":"2.0","id":2,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{},"options":[{"optionId":"yes"}]}}),
    );
    assert_eq!(app.failure(), Some(Failure::Overloaded));
    assert!(matches!(p.select("yes"), Err(AcpError::StaleApproval)));
}
#[test]
fn host_input_and_limits_are_validated_before_admission() {
    let (app, _) = ready(AcpLimits::default());
    assert!(
        app.prompt(
            &"x".repeat(app.shared.limits.max_frame_bytes + 1),
            deadline()
        )
        .is_err()
    );
    assert!(!app.snapshot().prompting);
    let limits = AcpLimits {
        max_frame_bytes: usize::MAX,
        ..AcpLimits::default()
    };
    assert!(validate_limits(limits).is_err());
    assert!(app.prompt("x", Instant::now()).is_err());
}

#[test]
fn control_flush_is_transport_evidence_and_closed_admission_drains_native_response() {
    let (app, events) = ready(AcpLimits::default());
    let prompt = app.prompt("hello", deadline()).unwrap();
    let p = permission(&app, &events, json!(17));
    let reply = p.select("no").unwrap();
    {
        let mut state = lock(&app.shared.state);
        let ordinary = state
            .pending
            .iter_mut()
            .find(|p| p.id.as_deref() == Some(prompt.id()))
            .unwrap();
        ordinary.dispatched = true;
        ordinary.receipt = Some(crate::rpc::RpcWriteReceipt::finished_for_test(
            RpcWriteOutcome::Written,
        ));
    }
    let (tx, rx) = bounded(3);
    let (_wake, wakes) = bounded(1);
    tx.send(RpcEvent::Line {
        id: app.generation(),
        seq: 1,
        line: json!({"jsonrpc":"2.0","id":prompt.id(),"result":{"stopReason":"end_turn"}})
            .to_string(),
    })
    .unwrap();
    tx.send(RpcEvent::Exit {
        id: app.generation(),
        stderr_tail: String::new(),
    })
    .unwrap();
    run_loop(Arc::clone(&app.shared), rx, wakes, |_, _| {
        Err(Error::Closed)
    });
    assert!(matches!(
        &*prompt.outcome().unwrap(),
        RequestOutcome::Response(_)
    ));
    assert!(matches!(
        &*reply.outcome().unwrap(),
        RequestOutcome::NotSent(Failure::Eof)
    ));
    let (app, _) = ready(AcpLimits::default());
    let _prompt = app.prompt("hello", deadline()).unwrap();
    let cancel = app.cancel(deadline()).unwrap();
    {
        let mut s = lock(&app.shared.state);
        let p = s.pending.back_mut().unwrap();
        p.dispatched = true;
        p.receipt = Some(crate::rpc::RpcWriteReceipt::finished_for_test(
            RpcWriteOutcome::Written,
        ));
    }
    maintenance(&app.shared, &mut false).unwrap();
    assert!(matches!(
        &*cancel.outcome().unwrap(),
        RequestOutcome::Written
    ));
    assert!(app.snapshot().prompting);
}

#[test]
fn permission_expiry_and_control_saturation_never_grant() {
    let (app, events) = ready(AcpLimits::default());
    let p = permission(&app, &events, json!(17));
    {
        let mut s = lock(&app.shared.state);
        s.permissions.values_mut().next().unwrap().deadline = Instant::now();
    }
    assert!(matches!(p.select("yes"), Err(AcpError::StaleApproval)));
    assert_eq!(
        maintenance(&app.shared, &mut false),
        Err(Failure::ApprovalExpired)
    );
    let limits = AcpLimits {
        control_capacity: 2,
        ..AcpLimits::default()
    };
    let (app, events) = ready(limits);
    let _prompt = app.prompt("hello", deadline()).unwrap();
    let _p = permission(&app, &events, json!(17));
    app.cancel(deadline()).unwrap();
    assert!(matches!(
        app.cancel(deadline()),
        Err(AcpError::Transport(Error::QueueFull))
    ));
    assert!(app.failure().is_none());
}

#[test]
fn uncertain_write_preserves_native_response_until_ordered_eof() {
    let (app, _) = ready(AcpLimits::default());
    let prompt = app.prompt("hello", deadline()).unwrap();
    {
        let mut s = lock(&app.shared.state);
        let p = s.pending.back_mut().unwrap();
        p.dispatched = true;
        p.receipt = Some(crate::rpc::RpcWriteReceipt::finished_for_test(
            RpcWriteOutcome::PossiblyWritten(crate::rpc::RpcWriteFailure::Io(
                std::io::ErrorKind::BrokenPipe,
            )),
        ));
    }
    let (tx, incoming) = bounded(3);
    tx.send(RpcEvent::StdinError {
        id: app.generation(),
        kind: std::io::ErrorKind::BrokenPipe,
    })
    .unwrap();
    tx.send(RpcEvent::Line {
        id: app.generation(),
        seq: 7,
        line: json!({"jsonrpc":"2.0","id":prompt.id(),"result":{"stopReason":"end_turn"}})
            .to_string(),
    })
    .unwrap();
    tx.send(RpcEvent::Exit {
        id: app.generation(),
        stderr_tail: String::new(),
    })
    .unwrap();
    run_loop(
        Arc::clone(&app.shared),
        incoming,
        crossbeam_channel::never(),
        |_, _| panic!("failed stdin must not dispatch again"),
    );
    assert_eq!(app.failure(), Some(Failure::Eof));
    assert!(matches!(
        &*prompt.outcome().unwrap(),
        RequestOutcome::Response(_)
    ));
}

#[test]
fn omitted_native_events_keep_permission_details_and_latest_terminal_evidence() {
    let (mut app, events) = ready(AcpLimits::default());
    Arc::get_mut(&mut app.shared).unwrap().native_events = NativeEvents::Omit;
    let prompt = app.prompt("hello", deadline()).unwrap();
    input(
        &app,
        json!({"jsonrpc":"2.0","id":17,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"title":"private tool"},"options":[{"optionId":"yes"}]}}),
    );
    let permission = loop {
        let e = events.recv_until(deadline()).unwrap();
        if let Some(p) = e.permission {
            assert!(e.native.is_none());
            break p;
        }
    };
    assert_eq!(permission.request()["toolCall"]["title"], "private tool");
    response(
        &app,
        &prompt,
        json!({"stopReason":"end_turn","usage":{"inputTokens":3,"future":true}}),
    );
    assert!(matches!(
        permission.select("yes"),
        Err(AcpError::StaleApproval)
    ));
    let terminal = app.snapshot().last_prompt.unwrap();
    assert!(terminal.context.request_id == Some(RequestId::String(prompt.id().into())));
    if let RequestOutcome::Response(value) = terminal.outcome {
        assert_eq!(value["usage"]["inputTokens"], 3);
    } else {
        panic!("native terminal result");
    }
    let event = events.recv_until(deadline()).unwrap();
    assert!(event.native.is_none());
    assert!(matches!(
        event.portable.kind,
        EventKind::TurnCompleted { .. }
    ));
    let next = app.prompt("again", deadline()).unwrap();
    response(&app, &next, json!({"stopReason":"future_reason"}));
    assert!(
        app.snapshot().last_prompt.unwrap().context.request_id
            == Some(RequestId::String(next.id().into()))
    );
}

#[test]
fn response_publication_retains_admission_and_close_preserves_observed_result() {
    let limits = AcpLimits {
        pending_requests: 1,
        ..AcpLimits::default()
    };
    let (app, _) = ready(limits);
    let prompt = app.prompt("hello", deadline()).unwrap();
    {
        let mut state = lock(&app.shared.state);
        let p = state.pending.back_mut().unwrap();
        p.dispatched = true;
        p.receipt = Some(crate::rpc::RpcWriteReceipt::finished_for_test(
            RpcWriteOutcome::Written,
        ));
    }
    let (entered, release) = prompt.completion.pause_next_finish();
    let shared = Arc::clone(&app.shared);
    let line =
        json!({"jsonrpc":"2.0","id":prompt.id(),"result":{"stopReason":"end_turn"}}).to_string();
    let worker = std::thread::spawn(move || ingress(&shared, 1, &line));
    entered
        .recv_timeout(Duration::from_secs(5))
        .expect("publication reached");
    assert!(matches!(
        app.prompt("rejected", deadline()),
        Err(AcpError::Transport(Error::QueueFull))
    ));
    app.close();
    assert!(matches!(
        &*prompt.outcome().unwrap(),
        RequestOutcome::Response(_)
    ));
    drop(release);
    let _ = worker.join().unwrap();
    assert!(app.snapshot().last_prompt.is_some());
}

#[test]
fn extension_notifications_without_params_remain_unscoped_other_events() {
    let (app, events) = ready(AcpLimits::default());
    input(
        &app,
        json!({"jsonrpc":"2.0","method":"_vendor/pulse","future":true}),
    );
    for _ in 0..2 {
        events.recv_until(deadline()).unwrap();
    }
    let event = events.recv_until(deadline()).unwrap();
    assert_eq!(event.portable.native_method, "_vendor/pulse");
    assert!(event.portable.context.session_id.is_none());
    assert!(matches!(event.portable.kind, EventKind::Other));
    assert_eq!(event.native.unwrap()["future"], true);
}

#[test]
fn old_generation_response_cannot_settle_replacement_and_tool_ids_are_observed() {
    let (old, _) = harness(AcpLimits::default());
    let stale = old.initialize("test", "1", deadline()).unwrap();
    let (replacement, _) = harness(AcpLimits::default());
    let current = replacement.initialize("test", "1", deadline()).unwrap();
    assert_ne!(stale.id(), current.id());
    input(
        &replacement,
        json!({"jsonrpc":"2.0","id":stale.id(),"result":{"protocolVersion":1,"agentCapabilities":{}}}),
    );
    assert!(current.outcome().is_none());
    assert!(replacement.snapshot().initialized.is_none());
    response(
        &replacement,
        &current,
        json!({"protocolVersion":1,"agentCapabilities":{}}),
    );
    let (app, events) = ready(AcpLimits::default());
    input(
        &app,
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"native-tool","status":"pending"}}}),
    );
    for _ in 0..2 {
        events.recv_until(deadline()).unwrap();
    }
    assert_eq!(
        events
            .recv_until(deadline())
            .unwrap()
            .portable
            .context
            .item_id
            .as_deref(),
        Some("native-tool")
    );
}

#[cfg(feature = "approvals")]
#[test]
fn approval_callback_invalid_choice_and_panic_cancel_native_permission() {
    use crate::approvals::{AcpDecision, Executor, Limits, Outcome, Reply};
    let executor = Executor::new(Limits::default()).unwrap();
    for panic in [false, true] {
        let (app, events) = ready(AcpLimits::default());
        let token = permission(&app, &events, json!(17));
        let clone = token.clone();
        let ticket = executor
            .acp(token, deadline(), move |request| {
                assert_eq!(request.id, &json!(17));
                assert_eq!(request.params["toolCall"]["title"], "Read");
                if panic {
                    panic!("synthetic callback panic");
                }
                AcpDecision::Select("invented".into())
            })
            .unwrap();
        assert!(matches!(
            &*ticket.wait_until(deadline()).unwrap(),
            Outcome::Replied(Reply::Acp(Ok(_)))
        ));
        assert!(matches!(clone.select("yes"), Err(AcpError::StaleApproval)));
        let state = lock(&app.shared.state);
        let reply = state
            .pending
            .iter()
            .find(|p| matches!(p.action, Action::Control))
            .unwrap();
        let frame: Value = serde_json::from_str(reply.line.as_ref().unwrap()).unwrap();
        assert_eq!(frame["result"]["outcome"]["outcome"], "cancelled");
    }
}

#[cfg(feature = "approvals")]
#[test]
fn approval_callback_late_allow_cannot_authorize_reused_id_after_cancel() {
    use crate::approvals::{AcpDecision, Executor, Limits, Outcome, Reply};
    let executor = Executor::new(Limits::default()).unwrap();
    let (app, events) = ready(AcpLimits::default());
    app.prompt("hello", deadline()).unwrap();
    let token = permission(&app, &events, json!(17));
    let (entered, started) = bounded(1);
    let (release, resume) = bounded::<()>(1);
    let ticket = executor
        .acp(token, deadline(), move |_| {
            entered.send(()).unwrap();
            let _ = resume.recv_deadline(deadline());
            AcpDecision::Select("yes".into())
        })
        .unwrap();
    started.recv_deadline(deadline()).unwrap();
    app.cancel(deadline()).unwrap();
    let replacement = permission(&app, &events, json!(17));
    drop(release);
    assert!(matches!(
        &*ticket.wait_until(deadline()).unwrap(),
        Outcome::Replied(Reply::Acp(Err(AcpError::StaleApproval)))
    ));
    assert!(replacement.select("no").is_ok());
}

#[cfg(feature = "approvals")]
#[test]
fn approval_callback_ticket_cancellation_discards_late_allow_without_consuming_token() {
    use crate::approvals::{AcpDecision, Executor, Limits, Outcome};
    let executor = Executor::new(Limits {
        workers: 1,
        ..Limits::default()
    })
    .unwrap();
    let (app, events) = ready(AcpLimits::default());
    let token = permission(&app, &events, json!(17));
    let clone = token.clone();
    let (entered, started) = bounded(1);
    let (release, resume) = bounded::<()>(1);
    let ticket = executor
        .acp(token, deadline(), move |_| {
            entered.send(()).unwrap();
            let _ = resume.recv_deadline(deadline());
            AcpDecision::Select("yes".into())
        })
        .unwrap();
    started.recv_deadline(deadline()).unwrap();
    ticket.clone().cancel();
    drop(release);
    // Single worker FIFO makes this marker prove the earlier callback returned.
    let marker = permission(&app, &events, json!(18));
    let marker_ticket = executor
        .acp(marker, deadline(), |_| AcpDecision::Delegate)
        .unwrap();
    assert!(matches!(
        &*marker_ticket.wait_until(deadline()).unwrap(),
        Outcome::Delegated
    ));
    assert!(matches!(&*ticket.outcome().unwrap(), Outcome::Cancelled));
    assert!(
        !lock(&app.shared.state)
            .pending
            .iter()
            .any(|p| matches!(p.action, Action::Control))
    );
    assert!(clone.select("no").is_ok());
}

#[test]
fn input_closure_refuses_host_admissions_without_truncating_native_output() {
    use crate::rpc::{RpcWriteFailure, RpcWriteReceipt};
    // Event notification, immediate failed receipt, refused send, later receipt observation.
    for source in 0..4 {
        let (app, events) = ready(AcpLimits::default());
        let ticket = app.prompt("x", deadline()).unwrap();
        let token = permission(&app, &events, json!(77));
        if source == 2 {
            {
                let mut state = lock(&app.shared.state);
                let pending = state.pending.back_mut().unwrap();
                pending.dispatched = true;
                pending.receipt =
                    Some(RpcWriteReceipt::finished_for_test(RpcWriteOutcome::Written));
            }
            input(
                &app,
                json!({"jsonrpc":"2.0","id":66,"method":"unsupported/earlier","params":{}}),
            );
            events.recv_until(deadline()).unwrap();
        }
        if source == 3 {
            let mut state = lock(&app.shared.state);
            let pending = state.pending.back_mut().unwrap();
            pending.dispatched = true;
            pending.receipt = Some(RpcWriteReceipt::finished_for_test(
                RpcWriteOutcome::PossiblyWritten(RpcWriteFailure::Closed),
            ));
        }
        let (tx, incoming) = bounded(8);
        if source == 0 {
            tx.send(RpcEvent::StdinError {
                id: app.generation(),
                kind: std::io::ErrorKind::BrokenPipe,
            })
            .unwrap();
        }
        tx.send(RpcEvent::Line { id: app.generation(), seq: 100,
            line: json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"future_update"}}}).to_string() }).unwrap();
        std::thread::scope(|scope| {
            let app = &app;
            let ticket = &ticket;
            scope.spawn(move || {
                assert_eq!(
                    events.recv_until(deadline()).unwrap().portable.context.seq,
                    100
                );
                assert!(app.failure().is_none());
                let before = {
                    let state = lock(&app.shared.state);
                    (state.next, state.pending.iter().map(|p| (p.deadline, p.bytes)).collect::<Vec<_>>(), state.permissions.len())
                };
                assert!(matches!(
                    app.cancel(deadline()),
                    Err(AcpError::Transport(Error::Closed))
                ));
                assert!(matches!(
                    app.prompt("later", deadline()),
                    Err(AcpError::Transport(Error::Closed))
                ));
                assert!(matches!(token.select("yes"), Err(AcpError::StaleApproval)));
                assert!(matches!(token.cancel(), Err(AcpError::StaleApproval)));
                {
                    let state = lock(&app.shared.state);
                    assert_eq!(
                        (state.next, state.pending.iter().map(|p| (p.deadline, p.bytes)).collect::<Vec<_>>(), state.permissions.len()),
                        before
                    );
                }
                tx.send(RpcEvent::Line { id: app.generation(), seq: 101,
                    line: json!({"jsonrpc":"2.0","id":78,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"late"},"options":[{"optionId":"yes"}]}}).to_string() }).unwrap();
                let late = events.recv_until(deadline()).unwrap().permission.unwrap();
                let retained = lock(&app.shared.state).permissions.len();
                assert!(matches!(late.select("yes"), Err(AcpError::StaleApproval)));
                assert_eq!(lock(&app.shared.state).permissions.len(), retained);
                tx.send(RpcEvent::Line {
                    id: app.generation(),
                    seq: 102,
                    line:
                        json!({"jsonrpc":"2.0","id":88,"method":"unsupported/reverse","params":{}})
                            .to_string(),
                })
                .unwrap();
                tx.send(RpcEvent::Line {
                    id: app.generation(),
                    seq: 103,
                    line:
                        json!({"jsonrpc":"2.0","id":ticket.id(),"result":{"stopReason":"end_turn"}})
                            .to_string(),
                })
                .unwrap();
                let outcome = ticket.wait_until(deadline()).unwrap();
                assert!(
                    matches!(&*outcome, RequestOutcome::Response(_)),
                    "source {source}: {outcome:?}"
                );
                assert!(app.failure().is_none());
                assert!(matches!(app.prompt("after completion", deadline()), Err(AcpError::Transport(Error::Closed))));
                assert!(lock(&app.shared.state).pending.iter().filter(|p| p.control).count() <= usize::from(source == 2), "draining must not enqueue automatic controls");
                assert!(
                    lock(&app.shared.state).permissions.is_empty(),
                    "prompt completion revokes permission without unsendable cancellation"
                );
                tx.send(RpcEvent::Exit {
                    id: app.generation(),
                    stderr_tail: String::new(),
                })
                .unwrap();
            });
            run_loop(
                Arc::clone(&app.shared),
                incoming,
                crossbeam_channel::never(),
                |_, _| match source {
                    0 => Ok(RpcWriteReceipt::finished_for_test(RpcWriteOutcome::Written)),
                    1 => Ok(RpcWriteReceipt::finished_for_test(
                        RpcWriteOutcome::PossiblyWritten(RpcWriteFailure::Closed),
                    )),
                    2 => Err(Error::Closed),
                    _ => panic!("maintenance must stop dispatch"),
                },
            );
        });
        assert_eq!(app.failure(), Some(Failure::Eof));
    }
}

#[test]
fn input_closure_keeps_original_request_deadline() {
    let (app, _) = ready(AcpLimits::default());
    let ticket = app.prompt("x", deadline()).unwrap();
    {
        let mut state = lock(&app.shared.state);
        let pending = state.pending.back_mut().unwrap();
        pending.dispatched = true;
        pending.receipt = Some(crate::rpc::RpcWriteReceipt::finished_for_test(
            RpcWriteOutcome::PossiblyWritten(crate::rpc::RpcWriteFailure::Closed),
        ));
    }
    let mut draining = false;
    maintenance(&app.shared, &mut draining).unwrap();
    assert!(draining);
    assert!(app.failure().is_none());
    assert!(matches!(
        app.cancel(deadline()),
        Err(AcpError::Transport(Error::Closed))
    ));
    lock(&app.shared.state).pending.back_mut().unwrap().deadline = Instant::now();
    assert_eq!(
        maintenance(&app.shared, &mut draining),
        Err(Failure::DeadlineExceeded)
    );
    app.shared.stop(Failure::DeadlineExceeded);
    assert!(matches!(
        &*ticket.outcome().unwrap(),
        RequestOutcome::OutcomeUnknown {
            failure: Failure::DeadlineExceeded,
            ..
        }
    ));
}
