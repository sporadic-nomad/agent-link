//! Observable evidence, retention and capability contracts for recommendation #4.
use super::*;
use crate::portable::{
    EventKind, EventV1, ItemState, Provider, RequestId, TextPhase, TextUpdate, ToolKind,
};

fn fixtures() -> Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/codex-app-server/v2.json"
    ))
    .unwrap()
}

#[test]
fn projection_preserves_origin_native_extensions_and_distinct_observations() {
    let fixture = fixtures();
    let (app, events) = ready(CodexLimits::default());
    for (index, key) in [
        "started",
        "delta",
        "commentary",
        "final",
        "tool",
        "usage",
        "extension",
        "completed",
    ]
    .into_iter()
    .enumerate()
    {
        let seq = 10 + index as u64 * 2;
        driver::ingress(&app.shared, seq, &fixture[key].to_string()).unwrap();
        let event = events.recv_until(deadline()).unwrap();
        assert_eq!(*event.native.unwrap(), fixture[key]);
        let portable = event.portable;
        assert_eq!(EventV1::VERSION, 1);
        assert_eq!(portable.context.provider, Provider::Codex);
        assert_eq!(portable.context.generation, app.generation());
        assert_eq!(portable.context.seq, event.seq);
        assert_eq!(event.seq, seq);
        assert_eq!(portable.context.session_id.as_deref(), Some("t"));
        assert_eq!(portable.context.turn_id.as_deref(), Some("a"));
        assert_eq!(portable.native_method, fixture[key]["method"]);
        match (key, &portable.kind) {
            ("started", EventKind::TurnStarted) => {}
            (
                "delta",
                EventKind::Text {
                    text,
                    update: TextUpdate::Delta,
                    phase: TextPhase::Unknown,
                },
            ) => assert_eq!(text, "hello 🌍"),
            (
                "commentary",
                EventKind::Text {
                    text,
                    update: TextUpdate::Replace,
                    phase: TextPhase::Commentary,
                },
            ) => assert_eq!(text, "Checking the result."),
            (
                "final",
                EventKind::Text {
                    text,
                    update: TextUpdate::Replace,
                    phase: TextPhase::Final,
                },
            ) => assert_eq!(text, "hello 🌍, corrected"),
            (
                "tool",
                EventKind::Tool {
                    tool: ToolKind::Command,
                    state: ItemState::Completed,
                },
            ) => {}
            ("usage", EventKind::Usage(usage)) => {
                assert_eq!(usage.total.total_tokens, 30);
                assert_eq!(usage.last.total_tokens, 15);
                assert_eq!(usage.last.cached_input_tokens, Some(3));
                assert_eq!(usage.model_context_window, Some(128000));
            }
            ("extension", EventKind::Other) => {}
            (
                "completed",
                EventKind::TurnCompleted {
                    status: TurnStatus::Completed,
                },
            ) => {}
            _ => panic!("wrong projection for {key}"),
        }
        if key != "completed" {
            assert!(
                app.snapshot().last_turn.is_none(),
                "{key} must not finish a turn"
            );
            assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
        }
    }
    // Breaking immediately at completion retains resume identity, native metadata and
    // the preceding usage sample, independently of another read/poll/response.
    drop(events);
    let snapshot = app.snapshot();
    assert_eq!(snapshot.thread_id.as_deref(), Some("t"));
    let terminal = snapshot.last_turn.unwrap();
    assert_eq!(*terminal.native, fixture["completed"]["params"]["turn"]);
    assert_eq!(terminal.usage.unwrap().usage.last.output_tokens, 5);
    assert_eq!(
        *snapshot.last_usage.unwrap().native.unwrap(),
        fixture["usage"]
    );
}

#[test]
fn incomplete_or_unknown_projection_never_invents_finality_or_identity() {
    let fixture = fixtures();
    let (app, events) = ready(CodexLimits::default());
    let mut cases = vec![json!({"method":"future/no-identity"})];
    let mut value = fixture["delta"].clone();
    value["params"]["delta"] = json!(123);
    cases.push(value);
    let mut value = fixture["delta"].clone();
    value["params"].as_object_mut().unwrap().remove("itemId");
    cases.push(value);
    let mut value = fixture["final"].clone();
    value["params"]["item"]["type"] = json!("futureTool");
    cases.push(value);
    let mut value = fixture["usage"].clone();
    value["params"]["tokenUsage"]["last"]["inputTokens"] = json!(-1);
    cases.push(value);
    let mut value = fixture["usage"].clone();
    value["params"]["tokenUsage"]["total"] = Value::Null;
    cases.push(value);
    for value in cases {
        ingress(&app, value.clone());
        let event = events.recv_until(deadline()).unwrap();
        assert!(matches!(event.portable.kind, EventKind::Other));
        assert_eq!(*event.native.unwrap(), value);
        if value.get("params").is_none() {
            assert!(event.portable.context.session_id.is_none());
            assert!(event.portable.context.turn_id.is_none());
        }
    }
    for phase in [Value::Null, json!("partial_answer"), json!("future")] {
        let mut value = fixture["final"].clone();
        value["params"]["item"]["phase"] = phase;
        ingress(&app, value);
        assert!(matches!(
            events.recv_until(deadline()).unwrap().portable.kind,
            EventKind::Text {
                phase: TextPhase::Unknown,
                update: TextUpdate::Replace,
                ..
            }
        ));
    }
    assert!(app.snapshot().last_turn.is_none());
    assert!(app.snapshot().last_usage.is_none());
    assert!(app.failure().is_none());
    turn(&app, "turn/completed", "a", "future");
    assert!(matches!(
        events.recv_until(deadline()).unwrap().portable.kind,
        EventKind::TurnCompleted {
            status: TurnStatus::Unknown
        }
    ));
}

#[test]
fn additive_ids_and_reverse_requests_cannot_change_projected_origin_or_lifecycle() {
    let fixture = fixtures();
    let (app, events) = ready(CodexLimits::default());
    for key in ["started", "final", "completed"] {
        let mut value = fixture[key].clone();
        value["params"]["turnId"] = json!("additive-turn");
        value["params"]["itemId"] = json!("additive-item");
        ingress(&app, value);
        let event = events.recv_until(deadline()).unwrap();
        if key == "final" {
            assert_eq!(event.portable.context.item_id.as_deref(), Some("i"));
        } else {
            assert_eq!(event.portable.context.turn_id.as_deref(), Some("a"));
        }
    }
    ingress(
        &app,
        json!({"method":"thread/started","params":{"threadId":"additive-thread","thread":{"id":"child"}}}),
    );
    assert_eq!(
        events
            .recv_until(deadline())
            .unwrap()
            .portable
            .context
            .session_id
            .as_deref(),
        Some("child")
    );

    let (app, events) = ready(CodexLimits::default());
    let token = approval(&app, &events, json!(41));
    for method in [
        "serverRequest/resolved",
        "turn/completed",
        "thread/tokenUsage/updated",
    ] {
        ingress(
            &app,
            json!({"id":123,"method":method,"params":{
                "threadId":"t","turnId":"a","requestId":41,
                "turn":{"id":"a","status":"completed"},
                "tokenUsage":fixture["usage"]["params"]["tokenUsage"]
            }}),
        );
        assert!(matches!(
            events.recv_until(deadline()).unwrap().portable.kind,
            EventKind::Other
        ));
        assert!(app.snapshot().last_usage.is_none());
        assert!(app.snapshot().last_turn.is_none());
    }
    assert!(token.respond(json!("decline")).is_ok());
}

#[test]
fn native_omission_retains_projection_terminal_state_and_approval_details() {
    let fixture = fixtures();
    let (app, events) = ready_with_options(CodexOptions {
        native_events: NativeEvents::Omit,
        ..CodexOptions::default()
    });
    for key in ["delta", "extension", "usage"] {
        ingress(&app, fixture[key].clone());
        let event = events.recv_until(deadline()).unwrap();
        assert!(event.native.is_none());
        assert_eq!(event.portable.native_method, fixture[key]["method"]);
        assert_eq!(
            event.portable.context.item_id.as_deref(),
            if key == "usage" { None } else { Some("i") }
        );
    }
    ingress(&app, fixture["approval"].clone());
    let event = events.recv_until(deadline()).unwrap();
    assert!(event.native.is_none());
    assert!(matches!(event.portable.kind, EventKind::ApprovalRequested));
    let token = event.approval.unwrap();
    assert_eq!(*token.request().params, fixture["approval"]["params"]);
    assert_eq!(token.request().id, 41);
    assert!(matches!(
        token.request().context.request_id,
        Some(RequestId::Number(41))
    ));
    assert!(token.request().context == event.portable.context);
    assert_eq!(
        token.request().decisions(),
        vec![json!("accept"), json!("decline"), json!("cancel")]
    );
    assert!(token.request().deadline > Instant::now());
    assert_eq!(
        format!("{:?}", token.request()),
        "ApprovalRequest(<redacted>)"
    );
    assert_eq!(format!("{:?}", event.portable), "EventV1(<redacted>)");
    ingress(&app, fixture["completed"].clone());
    let terminal = events.recv_until(deadline()).unwrap();
    assert!(terminal.native.is_none());
    drop(events);
    assert_eq!(app.snapshot().thread_id.as_deref(), Some("t"));
    assert!(app.snapshot().last_usage.unwrap().native.is_none());
    let last = app.snapshot().last_turn.unwrap();
    assert_eq!(*last.native, fixture["completed"]["params"]["turn"]);
    assert_eq!(last.usage.unwrap().usage.total.input_tokens, 20);
    assert!(matches!(
        token.respond(json!("accept")),
        Err(CodexError::StaleApproval)
    ));
    assert_eq!(
        token.request().id,
        41,
        "cancelled capability retains its original audit identity"
    );
}

#[test]
fn usage_is_committed_before_overflow_and_never_attaches_to_a_different_turn() {
    let fixture = fixtures();
    let (app, events) = ready(CodexLimits::default());
    ingress(&app, fixture["completed"].clone());
    ingress(&app, fixture["usage"].clone()); // late matching usage is still evidence
    assert_eq!(
        app.snapshot()
            .last_turn
            .unwrap()
            .usage
            .unwrap()
            .usage
            .last
            .output_tokens,
        5
    );
    let mut usage = fixture["usage"].clone();
    usage["params"]["turnId"] = json!("b");
    usage["params"]["tokenUsage"]["last"]["outputTokens"] = json!(999);
    ingress(&app, usage.clone());
    assert_eq!(
        app.snapshot()
            .last_usage
            .unwrap()
            .context
            .turn_id
            .as_deref(),
        Some("b")
    );
    assert_eq!(
        app.snapshot()
            .last_turn
            .unwrap()
            .usage
            .unwrap()
            .usage
            .last
            .output_tokens,
        5
    );
    usage["params"]["threadId"] = json!("child");
    usage["params"]["turnId"] = json!("a");
    ingress(&app, usage);
    assert_eq!(
        app.snapshot()
            .last_usage
            .unwrap()
            .context
            .turn_id
            .as_deref(),
        Some("b")
    );
    assert_eq!(
        app.snapshot()
            .last_turn
            .unwrap()
            .usage
            .unwrap()
            .usage
            .last
            .output_tokens,
        5
    );
    drop(events);

    let (app, _events) = ready(CodexLimits {
        events: 1,
        ..CodexLimits::default()
    });
    ingress(&app, fixture["usage"].clone());
    ingress(&app, fixture["completed"].clone());
    assert_eq!(app.failure(), Some(Failure::ConsumerLagged));
    assert_eq!(
        app.snapshot()
            .last_turn
            .unwrap()
            .usage
            .unwrap()
            .usage
            .last
            .output_tokens,
        5
    );
}

fn usage_sample(app: &CodexAppServer, turn: &str, seq: u64, output: u64) -> Value {
    let mut value = fixtures()["usage"].clone();
    value["params"]["turnId"] = json!(turn);
    value["params"]["tokenUsage"]["last"]["outputTokens"] = json!(output);
    driver::ingress(&app.shared, seq, &value.to_string()).unwrap();
    value
}

#[test]
fn usage_retention_survives_late_updates_until_terminal_notification_or_result() {
    for native_events in [NativeEvents::Retain, NativeEvents::Omit] {
        for terminal_result in [false, true] {
            let (app, events) = ready_with_options(CodexOptions {
                native_events,
                ..CodexOptions::default()
            });
            turn(&app, "turn/started", "a", "inProgress");
            turn(&app, "turn/completed", "a", "completed");
            let ticket = app.turn_start("successor", deadline()).unwrap();
            turn(&app, "turn/started", "b", "inProgress");
            usage_sample(&app, "b", 30, 6);
            let expected = usage_sample(&app, "b", 31, 7);
            usage_sample(&app, "a", 32, 8);
            // The arrival-ordered latest sample remains A; B needs independent retention.
            assert_eq!(
                app.snapshot()
                    .last_usage
                    .unwrap()
                    .context
                    .turn_id
                    .as_deref(),
                Some("a")
            );
            assert_eq!(
                app.snapshot()
                    .last_turn
                    .unwrap()
                    .usage
                    .unwrap()
                    .usage
                    .last
                    .output_tokens,
                8
            );
            if terminal_result {
                response(
                    &app,
                    &ticket,
                    json!({"turn":{"id":"b","status":"completed"}}),
                );
            } else {
                // Repeated activity after an unrelated sample must keep B's cache.
                turn(&app, "turn/started", "b", "inProgress");
                turn(&app, "turn/completed", "b", "completed");
                // A delayed acceptance must not resurrect B or erase its terminal usage.
                response(
                    &app,
                    &ticket,
                    json!({"turn":{"id":"b","status":"inProgress"}}),
                );
                loop {
                    let event = events
                        .recv_until(Instant::now())
                        .expect("terminal B event is already published");
                    if event.portable.context.turn_id.as_deref() == Some("b")
                        && matches!(event.portable.kind, EventKind::TurnCompleted { .. })
                    {
                        break;
                    }
                }
            }
            drop(events); // No extra observation is needed to commit the sample.
            let snapshot = app.snapshot();
            assert!(snapshot.active_turn.is_none());
            let terminal = snapshot.last_turn.unwrap();
            assert_eq!(terminal.turn_id, "b");
            let usage = terminal
                .usage
                .expect("observed B usage must survive late A usage");
            assert_eq!(usage.context.turn_id.as_deref(), Some("b"));
            assert_eq!(usage.context.seq, 31);
            assert_eq!(usage.usage.last.output_tokens, 7);
            assert_eq!(
                usage.native.is_some(),
                native_events == NativeEvents::Retain
            );
            if let Some(native) = usage.native {
                assert_eq!(*native, expected);
            }
            assert!(app.failure().is_none());
        }
    }
}

#[test]
fn usage_retention_preserves_reobserved_terminal_without_disturbing_active_turn() {
    for native_events in [NativeEvents::Retain, NativeEvents::Omit] {
        let (app, events) = ready_with_options(CodexOptions {
            native_events,
            ..CodexOptions::default()
        });
        turn(&app, "turn/started", "a", "inProgress");
        usage_sample(&app, "a", 30, 5);
        turn(&app, "turn/completed", "a", "completed");
        turn(&app, "turn/started", "b", "inProgress");
        usage_sample(&app, "b", 31, 6);
        turn(&app, "turn/completed", "a", "completed");
        let last = app.snapshot().last_turn.unwrap();
        assert_eq!(last.turn_id, "a");
        assert_eq!(
            last.usage
                .expect("reobserved A retains its usage")
                .usage
                .last
                .output_tokens,
            5
        );
        assert_eq!(app.snapshot().active_turn.as_deref(), Some("b"));

        usage_sample(&app, "a", 32, 7); // More recent evidence for the terminal outcome.
        usage_sample(&app, "b", 33, 8);
        turn(&app, "turn/completed", "a", "interrupted");
        let last = app.snapshot().last_turn.unwrap();
        assert_eq!(last.status, TurnStatus::Interrupted);
        let usage = last
            .usage
            .expect("latest A sample survives another observation");
        assert_eq!(usage.context.seq, 32);
        assert_eq!(usage.usage.last.output_tokens, 7);
        assert_eq!(app.snapshot().active_turn.as_deref(), Some("b"));
        // Reobserving A must also leave B's retained sample available for completion.
        usage_sample(&app, "unrelated", 34, 9);
        turn(&app, "turn/completed", "b", "completed");
        let last = app.snapshot().last_turn.unwrap();
        assert_eq!(last.turn_id, "b");
        assert_eq!(last.usage.unwrap().usage.last.output_tokens, 8);
        drop(events);
        assert!(app.failure().is_none());
    }
}

#[test]
fn usage_retention_seeds_each_activity_source_and_does_not_leak_to_a_successor() {
    for activation in ["notification", "start-result", "resume", "approval"] {
        let (app, events) = if activation == "resume" {
            let (app, events) = harness(CodexLimits::default());
            let init = app.initialize("test", "1", deadline()).unwrap();
            response(&app, &init, json!({"userAgent":"fixture"}));
            (app, events)
        } else {
            ready(CodexLimits::default())
        };
        let resume = (activation == "resume")
            .then(|| app.thread_resume("t", json!({}), deadline()).unwrap());
        usage_sample(&app, "a", 30, 5); // Latest sample precedes identifying activity.
        match activation {
            "notification" => turn(&app, "turn/started", "a", "inProgress"),
            "start-result" => {
                let start = app.turn_start("x", deadline()).unwrap();
                response(
                    &app,
                    &start,
                    json!({"turn":{"id":"a","status":"inProgress"}}),
                );
            }
            "resume" => response(
                &app,
                &resume.unwrap(),
                json!({
                    "thread":{"id":"t","turns":[{"id":"a","status":"inProgress"}]}
                }),
            ),
            "approval" => {
                approval(&app, &events, json!(41));
            }
            _ => unreachable!(),
        }
        assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
        usage_sample(&app, "unrelated", 31, 9);
        turn(&app, "turn/completed", "a", "completed");
        let last = app.snapshot().last_turn.unwrap();
        assert_eq!(last.usage.expect(activation).usage.last.output_tokens, 5);
        let start = app.turn_start("no usage yet", deadline()).unwrap();
        response(
            &app,
            &start,
            json!({"turn":{"id":"b","status":"inProgress"}}),
        );
        turn(&app, "turn/completed", "b", "completed");
        let last = app.snapshot().last_turn.unwrap();
        assert_eq!(last.turn_id, "b");
        assert!(last.usage.is_none(), "A's sample must not become B's usage");
        assert!(app.failure().is_none());
    }
}

#[test]
fn omitted_envelopes_do_not_bypass_byte_admission_or_hide_unknown_messages() {
    let mut delta = fixtures()["delta"].clone();
    delta["params"]["delta"] = json!("x".repeat(300));
    for native_events in [NativeEvents::Retain, NativeEvents::Omit] {
        // A raw frame alone fits; a frame plus the extracted text exceeds this budget.
        let (app, _events) = ready_with_options(CodexOptions {
            limits: CodexLimits {
                event_bytes: 600,
                ..CodexLimits::default()
            },
            native_events,
        });
        ingress(&app, delta.clone());
        assert_eq!(app.failure(), Some(Failure::ConsumerLagged));
        let (app, events) = ready_with_options(CodexOptions {
            limits: CodexLimits {
                event_bytes: 256,
                ..CodexLimits::default()
            },
            native_events,
        });
        ingress(
            &app,
            json!({"method":"future","params":{"blob":"x".repeat(300)}}),
        );
        assert_eq!(app.failure(), Some(Failure::ConsumerLagged));
        assert!(events.recv_until(Instant::now()).is_none());
    }
}

#[test]
fn approval_metadata_preserves_exact_choices_and_cannot_retarget_a_new_generation() {
    let amendment = json!({"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["echo"]}});
    let (app, events) = ready(CodexLimits::default());
    let mut request = fixtures()["approval"].clone();
    request["id"] = json!("41");
    request["params"]["approvalId"] = json!("native-callback");
    request["params"]["availableDecisions"] = json!(["decline", amendment, {"futureChoice":true}]);
    ingress(&app, request.clone());
    let token = events.recv_until(deadline()).unwrap().approval.unwrap();
    assert!(
        matches!(&token.request().context.request_id, Some(RequestId::String(id)) if id == "41")
    );
    assert_eq!(token.request().params["approvalId"], "native-callback");
    assert_eq!(
        token.request().decisions(),
        vec![json!("decline"), amendment.clone()]
    );
    assert_eq!(
        token.request().params["availableDecisions"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    token.respond(amendment.clone()).unwrap();
    let reply = lock(&app.shared.state)
        .pending
        .back()
        .unwrap()
        .line
        .clone()
        .unwrap();
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply, json!({"id":"41","result":{"decision":amendment}}));
    app.close();
    let (new_app, new_events) = ready(CodexLimits::default());
    ingress(&new_app, request);
    let replacement = new_events.recv_until(deadline()).unwrap().approval.unwrap();
    assert_ne!(
        token.request().context.generation,
        replacement.request().context.generation
    );
    assert!(matches!(
        token.respond(json!("decline")),
        Err(CodexError::StaleApproval)
    ));
    replacement.respond(json!("decline")).unwrap();

    let (app, events) = ready(CodexLimits::default());
    ingress(&app, fixtures()["fileApproval"].clone());
    let token = events.recv_until(deadline()).unwrap().approval.unwrap();
    assert_eq!(
        token.request().decisions(),
        vec![
            json!("accept"),
            json!("acceptForSession"),
            json!("decline"),
            json!("cancel")
        ]
    );
    ingress(
        &app,
        json!({"method":"serverRequest/resolved","params":{"threadId":"t","requestId":"files"}}),
    );
    let resolved = events.recv_until(deadline()).unwrap();
    assert!(matches!(
        resolved.portable.kind,
        EventKind::ApprovalResolved
    ));
    assert!(
        matches!(&resolved.portable.context.request_id, Some(RequestId::String(id)) if id == "files")
    );
    assert!(matches!(
        token.respond(json!("cancel")),
        Err(CodexError::StaleApproval)
    ));
}
