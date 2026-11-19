use super::*;
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
fn shared(limits: Limits, resume: Option<&str>) -> Arc<Shared> {
    Arc::new(Shared {
        generation: SessionId::next(),
        limits,
        expected_resume: resume.map(str::to_owned),
        native_events: NativeEvents::Retain,
        state: Mutex::new(State::default()),
        events: Queue::new(limits.events, limits.event_bytes),
    })
}
#[test]
fn native_event_preserves_literal_json_keys_in_extensions() {
    let shared = shared(Limits::default(), None);
    for (seq, value) in [json!("[]"), json!(42)].into_iter().enumerate() {
        let extension = json!({ "$serde_json::private::RawValue": value });
        let line = json!({ "type": "future", "extension": [extension.clone()] }).to_string();
        route(&shared, seq as u64, &line).unwrap();
        let native = shared
            .events
            .recv_until(deadline())
            .unwrap()
            .native
            .unwrap();
        assert_eq!(native["extension"][0], extension);
    }
}

#[test]
fn result_idle_tasks_unknown_identity_and_eof_are_separate() {
    let shared = shared(Limits::default(), None);
    route(
        &shared,
        0,
        r#"{"type":"system","subtype":"task_started","task_id":"t","task_type":"local_agent"}"#,
    )
    .unwrap();
    route(&shared, 1, r#"{"type":"result","subtype":"future","session_id":"s","user_message_uuids":["u1","u2"],"extra":42}"#).unwrap();
    assert_eq!(lock(&shared.state).snapshot.finite_tasks, 1);
    assert!(lock(&shared.state).snapshot.session_state.is_none());
    shared.events.recv_until(deadline()).unwrap();
    let result = shared.events.recv_until(deadline()).unwrap();
    assert!(matches!(
        result.portable.kind,
        EventKind::TurnCompleted {
            status: CompletionStatus::Unknown
        }
    ));
    assert!(result.portable.context.turn_id.is_none());
    assert_eq!(
        result.native.as_ref().unwrap()["user_message_uuids"],
        json!(["u1", "u2"])
    );
    route(
        &shared,
        2,
        r#"{"type":"system","subtype":"session_state_changed","state":"idle"}"#,
    )
    .unwrap();
    assert_eq!(lock(&shared.state).snapshot.finite_tasks, 1);
    assert_eq!(
        lock(&shared.state).snapshot.session_state.as_deref(),
        Some("idle")
    );
    assert!(lock(&shared.state).snapshot.failure.is_none());
    shared.stop(Failure::Eof);
    assert_eq!(lock(&shared.state).snapshot.failure, Some(Failure::Eof));
}
#[test]
fn capability_cancel_reuse_and_single_use() {
    let shared = shared(Limits::default(), None);
    let request = r#"{"type":"control_request","request_id":"opaque","request":{"subtype":"can_use_tool","input":{"a":1}}}"#;
    route(&shared, 0, request).unwrap();
    let old = shared
        .events
        .recv_until(deadline())
        .unwrap()
        .approval
        .unwrap();
    route(
        &shared,
        1,
        r#"{"type":"control_cancel_request","request_id":"opaque"}"#,
    )
    .unwrap();
    route(&shared, 2, request).unwrap();
    assert!(matches!(old.respond(None, None), Err(Error::Closed)));
    shared.events.recv_until(deadline()).unwrap();
    let current = shared
        .events
        .recv_until(deadline())
        .unwrap()
        .approval
        .unwrap();
    let clone = current.clone();
    current.respond(Some(&json!({"a":2})), None).unwrap();
    assert!(matches!(clone.respond(None, None), Err(Error::Closed)));
    assert_eq!(lock(&shared.state).pending.len(), 1);
}
#[test]
fn resume_mismatch_overflow_and_registration_cleanup() {
    let shared = shared(
        Limits {
            events: 1,
            ..Limits::default()
        },
        Some("wanted"),
    );
    assert_eq!(
        route(&shared, 0, r#"{"type":"system","session_id":"other"}"#),
        Err(Failure::Protocol)
    );
    assert!(lock(&shared.state).snapshot.session_id.is_none());
    let ticket = shared
        .enqueue(
            Action::Initialize,
            json!({"subtype":"initialize"}),
            deadline(),
            true,
        )
        .unwrap();
    route(&shared, 1, r#"{"type":"future","session_id":"wanted"}"#).unwrap();
    let failure = route(&shared, 2, r#"{"type":"future"}"#).unwrap_err();
    shared.stop(failure);
    assert!(matches!(
        &*ticket.outcome().unwrap(),
        Outcome::Failed {
            failure: Failure::Overload,
            may_have_been_written: false
        }
    ));
    assert!(lock(&shared.state).pending.is_empty());
}
#[test]
fn bounded_input_and_control_reserve() {
    let shared = shared(
        Limits {
            pending: 1,
            frame_bytes: 256,
            ..Limits::default()
        },
        None,
    );
    lock(&shared.state).snapshot.initialized = true;
    shared
        .enqueue(Action::Write, json!({"type":"user"}), deadline(), false)
        .unwrap();
    assert!(matches!(
        shared.enqueue(Action::Write, json!({}), deadline(), false),
        Err(Error::QueueFull)
    ));
    shared
        .enqueue(
            Action::Control,
            json!({"subtype":"interrupt"}),
            deadline(),
            true,
        )
        .unwrap();
    assert!(encode(&json!("\n".repeat(256)), 256).is_err());
    assert_eq!(lock(&shared.state).pending.len(), 2);
    shared.stop(Failure::Closed);
}

#[test]
fn refused_write_drains_already_queued_response_before_eof() {
    let shared = shared(Limits::default(), None);
    let ticket = shared
        .enqueue(
            Action::Initialize,
            json!({"subtype":"initialize"}),
            deadline(),
            true,
        )
        .unwrap();
    let id = lock(&shared.state).pending[0].id.clone();
    let (tx, rx) = crossbeam_channel::bounded(3);
    tx.send(RpcEvent::Line { id: shared.generation, seq: 0, line: json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":{"retained":true}}}).to_string() }).unwrap();
    tx.send(RpcEvent::Line { id: shared.generation, seq: 1, line: json!({"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":12}}).to_string() }).unwrap();
    tx.send(RpcEvent::Exit {
        id: shared.generation,
        stderr_tail: String::new(),
    })
    .unwrap();
    run_loop(Arc::clone(&shared), rx, |_, _| Err(Error::Closed));
    assert!(
        matches!(&*ticket.outcome().unwrap(), Outcome::Response(value) if value["response"]["response"]["retained"] == true)
    );
    assert_eq!(lock(&shared.state).snapshot.failure, Some(Failure::Eof));
    assert_eq!(
        lock(&shared.state).snapshot.last_result.as_ref().unwrap()["usage"]["input_tokens"],
        12
    );
}
#[test]
fn failed_write_and_stdin_error_drain_native_response_before_eof() {
    let shared = shared(Limits::default(), None);
    let ticket = shared
        .enqueue(
            Action::Initialize,
            json!({"subtype":"initialize"}),
            deadline(),
            true,
        )
        .unwrap();
    let id = lock(&shared.state).pending[0].id.clone();
    let (tx, rx) = crossbeam_channel::bounded(4);
    tx.send(RpcEvent::StdinError {
        id: shared.generation,
        kind: std::io::ErrorKind::BrokenPipe,
    })
    .unwrap();
    tx.send(RpcEvent::Line { id: shared.generation, seq: 0, line: json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":{"retained":true}}}).to_string() }).unwrap();
    tx.send(RpcEvent::Line { id: shared.generation, seq: 1, line: json!({"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":12}}).to_string() }).unwrap();
    tx.send(RpcEvent::Exit {
        id: shared.generation,
        stderr_tail: String::new(),
    })
    .unwrap();
    let mut writes = 0;
    run_loop(Arc::clone(&shared), rx, |_, _| {
        writes += 1;
        assert_eq!(writes, 1, "failed stdin must not dispatch again");
        Ok(RpcWriteReceipt::finished_for_test(
            RpcWriteOutcome::PossiblyWritten(crate::rpc::RpcWriteFailure::Io(
                std::io::ErrorKind::BrokenPipe,
            )),
        ))
    });
    assert!(
        matches!(&*ticket.outcome().unwrap(), Outcome::Response(value) if value["response"]["response"]["retained"] == true)
    );
    assert_eq!(lock(&shared.state).snapshot.failure, Some(Failure::Eof));
    assert_eq!(
        lock(&shared.state).snapshot.last_result.as_ref().unwrap()["usage"]["input_tokens"],
        12
    );
}
#[test]
fn cancellation_revokes_queued_reply_and_deadlines_settle_independently() {
    let shared = shared(Limits::default(), None);
    route(
        &shared,
        0,
        r#"{"type":"control_request","request_id":"opaque","request":{"subtype":"can_use_tool"}}"#,
    )
    .unwrap();
    let token = shared
        .events
        .recv_until(deadline())
        .unwrap()
        .approval
        .unwrap();
    let ticket = token.respond(None, None).unwrap();
    route(
        &shared,
        1,
        r#"{"type":"control_cancel_request","request_id":"opaque"}"#,
    )
    .unwrap();
    assert!(matches!(
        &*ticket.outcome().unwrap(),
        Outcome::Failed {
            may_have_been_written: false,
            ..
        }
    ));
    assert!(lock(&shared.state).pending.is_empty());
    let init = shared
        .enqueue(
            Action::Initialize,
            json!({"subtype":"initialize"}),
            deadline(),
            true,
        )
        .unwrap();
    lock(&shared.state).pending[0].deadline = Instant::now();
    let (_tx, rx) = crossbeam_channel::bounded(1);
    run_loop(Arc::clone(&shared), rx, |_, _| {
        panic!("expired operation must not write")
    });
    assert!(matches!(
        &*init.outcome().unwrap(),
        Outcome::Failed {
            failure: Failure::Deadline,
            may_have_been_written: false
        }
    ));
}
#[test]
fn launch_permissions_are_explicit_and_unknown_envelopes_survive() {
    use crate::agent::Permissions;
    let unattended = launch(&Permissions::Unattended, false);
    assert!(unattended.args.iter().any(|a| a == "bypassPermissions"));
    let native = launch(&Permissions::AgentDefault, true);
    assert!(!native.args.iter().any(|a| a == "bypassPermissions"));
    assert!(native.args.iter().any(|a| a == "--permission-prompt-tool"));
    let shared = shared(Limits::default(), None);
    route(&shared, 0, r#"{"type":"control_response","response":{"subtype":"future","request_id":"unknown","extra":42}}"#).unwrap();
    let event = shared.events.recv_until(deadline()).unwrap();
    assert!(matches!(event.portable.kind, EventKind::Other));
    assert_eq!(event.native.as_ref().unwrap()["response"]["extra"], 42);
    assert_eq!(route(&shared, 1, "{broken"), Err(Failure::Protocol));
}

#[test]
fn reversed_control_responses_commit_exact_ids_and_unknown_errors() {
    let shared = shared(Limits::default(), None);
    lock(&shared.state).snapshot.initialized = true;
    let a = shared
        .enqueue(
            Action::Control,
            json!({"subtype":"interrupt"}),
            deadline(),
            true,
        )
        .unwrap();
    let b = shared
        .enqueue(
            Action::Control,
            json!({"subtype":"interrupt"}),
            deadline(),
            true,
        )
        .unwrap();
    let ids = {
        let mut state = lock(&shared.state);
        state
            .pending
            .iter_mut()
            .map(|p| {
                p.line = None;
                p.id.clone()
            })
            .collect::<Vec<_>>()
    };
    assert!(ids[0].contains(&shared.generation.to_string()));
    route(&shared, 0, &json!({"type":"control_response","response":{"request_id":ids[1],"subtype":"error","error":"future vendor error"}}).to_string()).unwrap();
    assert!(a.outcome().is_none());
    assert!(
        matches!(&*b.outcome().unwrap(), Outcome::Response(value) if value["response"]["subtype"] == "error")
    );
    route(&shared, 1, &json!({"type":"control_response","response":{"request_id":ids[0],"subtype":"success","response":{}}}).to_string()).unwrap();
    assert!(
        matches!(&*a.outcome().unwrap(), Outcome::Response(value) if value["response"]["request_id"] == ids[0])
    );
}

#[test]
fn native_omission_preserves_early_exit_result_and_approval_details() {
    for retention in [NativeEvents::Retain, NativeEvents::Omit] {
        let mut shared = shared(Limits::default(), None);
        Arc::get_mut(&mut shared).unwrap().native_events = retention;
        route(&shared, 0, r#"{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":21},"user_message_uuids":["one","two"]}"#).unwrap();
        let result = shared.events.recv_until(deadline()).unwrap();
        assert_eq!(result.native.is_some(), retention == NativeEvents::Retain);
        drop(result); // an event-only consumer stops at the result
        let snapshot = lock(&shared.state).snapshot.clone();
        assert_eq!(
            snapshot.last_result.as_ref().unwrap()["usage"]["input_tokens"],
            21
        );
        assert_eq!(
            snapshot.last_result.as_ref().unwrap()["user_message_uuids"],
            json!(["one", "two"])
        );
        route(&shared, 1, r#"{"type":"control_request","request_id":"permission","request":{"subtype":"can_use_tool","tool_name":"Read","input":{"path":"fixture"}}}"#).unwrap();
        let event = shared.events.recv_until(deadline()).unwrap();
        let approval = event.approval.unwrap();
        assert_eq!(approval.request()["request"]["input"]["path"], "fixture");
        assert_eq!(approval.origin(), (shared.generation, 1));
    }
}

#[test]
fn assistant_text_projection_never_infers_finality_or_flattens_unknown_blocks() {
    let shared = shared(Limits::default(), None);
    route(&shared, 0, r#"{"type":"assistant","uuid":"transcript-exact","message":{"id":"message-exact","content":[{"type":"text","text":"observed"},{"type":"text","text":" continuation"}]}}"#).unwrap();
    let text = shared.events.recv_until(deadline()).unwrap();
    assert_eq!(
        text.portable.context.item_id.as_deref(),
        Some("message-exact")
    );
    assert!(text.portable.context.turn_id.is_none());
    assert!(matches!(
        text.portable.kind,
        EventKind::Text {
            phase: crate::portable::TextPhase::Unknown,
            update: crate::portable::TextUpdate::Replace,
            ..
        }
    ));
    route(&shared, 1, r#"{"type":"assistant","message":{"content":[{"type":"text","text":"one"},{"type":"future","kept":true}]}}"#).unwrap();
    let unknown = shared.events.recv_until(deadline()).unwrap();
    assert!(matches!(unknown.portable.kind, EventKind::Other));
    assert_eq!(
        unknown.native.unwrap()["message"]["content"][1]["kept"],
        true
    );
}

#[test]
fn strict_resume_requires_observed_exact_binding_before_any_user_admission() {
    let shared = shared(Limits::default(), Some("exact-provider-id"));
    lock(&shared.state).snapshot.initialized = true;
    assert!(matches!(
        shared.enqueue(Action::Write, json!({"type":"user"}), deadline(), false),
        Err(Error::InvalidInput(_))
    ));
    assert!(lock(&shared.state).pending.is_empty());
    route(
        &shared,
        0,
        r#"{"type":"system","subtype":"init","session_id":"exact-provider-id"}"#,
    )
    .unwrap();
    shared
        .enqueue(Action::Write, json!({"type":"user"}), deadline(), false)
        .unwrap();
    assert_eq!(lock(&shared.state).pending.len(), 1);
}

#[test]
fn provider_cannot_rebind_an_id_while_its_reply_is_queued() {
    let shared = shared(Limits::default(), None);
    let request =
        r#"{"type":"control_request","request_id":"same","request":{"subtype":"can_use_tool"}}"#;
    route(&shared, 0, request).unwrap();
    let approval = shared
        .events
        .recv_until(deadline())
        .unwrap()
        .approval
        .unwrap();
    approval.respond(Some(&json!({})), None).unwrap();
    assert_eq!(route(&shared, 1, request), Err(Failure::Protocol));
}

#[test]
fn explicit_text_deltas_do_not_invent_item_or_turn_identity() {
    let shared = shared(Limits::default(), None);
    route(&shared, 0, r#"{"type":"stream_event","uuid":"stream-envelope","session_id":"s","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"fragment"}}}"#).unwrap();
    let event = shared.events.recv_until(deadline()).unwrap();
    assert!(event.portable.context.item_id.is_none());
    assert!(event.portable.context.turn_id.is_none());
    assert!(
        matches!(event.portable.kind, EventKind::Text { ref text, update: crate::portable::TextUpdate::Delta, phase: crate::portable::TextPhase::Unknown } if text == "fragment")
    );
}
