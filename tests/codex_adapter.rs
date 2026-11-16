//! Offline native protocol conformance through real owned subprocess pipes.
mod common;
use agent_link::agent::RpcLaunch;
use agent_link::codex::*;
use agent_link::portable::{EventKind, Provider, TextPhase, TextUpdate};
use agent_link::rpc::{RpcSpawnSpec, RpcTransportError};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn deadline() -> Instant {
    common::deadline()
}
fn spawn(scene: &str, limits: CodexLimits) -> (tempfile::TempDir, CodexAppServer, CodexEvents) {
    let mut options = CodexOptions::default();
    options.limits = limits;
    spawn_with_options(scene, options)
}
fn spawn_with_options(
    scene: &str,
    options: CodexOptions,
) -> (tempfile::TempDir, CodexAppServer, CodexEvents) {
    spawn_with_stdout_limit(scene, options, None)
}
fn spawn_with_stdout_limit(
    scene: &str,
    options: CodexOptions,
    stdout_limit: Option<usize>,
) -> (tempfile::TempDir, CodexAppServer, CodexEvents) {
    let temp = tempfile::tempdir().unwrap();
    let launch = RpcLaunch::new(
        common::fake_agent_path().to_str().unwrap(),
        &["--codex-peer", scene],
    );
    let mut spec = RpcSpawnSpec::new(launch, temp.path());
    spec.shell = Some("/bin/sh".into());
    if let Some(limit) = stdout_limit {
        spec.limits.max_stdout_line_bytes = limit;
    }
    spec.env.inherit = false;
    spec.env
        .set("HOME", temp.path())
        .set("USERPROFILE", temp.path())
        .set("PATH", std::env::var_os("PATH").unwrap_or_default());
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        spec.env.set("SystemRoot", root);
    }
    let (app, events) = CodexAppServer::spawn_with_options(spec, options).unwrap();
    let init = app.initialize("agent-link-test", "1", deadline()).unwrap();
    result(&init);
    assert_eq!(
        events
            .recv_until(deadline())
            .unwrap()
            .portable
            .native_method,
        "fixture/ready"
    );
    (temp, app, events)
}
fn result(ticket: &RequestTicket) -> std::sync::Arc<Value> {
    match &*ticket.wait_until(deadline()).expect("request settled") {
        RequestOutcome::Response(value) => std::sync::Arc::clone(value),
        other => panic!("unexpected outcome: {other:?}"),
    }
}
fn start(app: &CodexAppServer) {
    result(&app.thread_start(json!({}), deadline()).unwrap());
}
fn next_approval(events: &CodexEvents) -> Approval {
    loop {
        if let Some(approval) = events
            .recv_until(deadline())
            .expect("approval event")
            .approval
        {
            return approval;
        }
    }
}

#[test]
fn portable_consumer_can_stop_at_terminal_with_native_retention_on_or_off() {
    for native_events in [NativeEvents::Retain, NativeEvents::Omit] {
        let mut options = CodexOptions::default();
        options.native_events = native_events;
        let (_temp, app, events) = spawn_with_options("projection", options);
        start(&app);
        let ticket = app.turn_start("fixture", deadline()).unwrap();
        let mut saw_final = false;
        let mut saw_usage = false;
        let mut previous = 0;
        let terminal_seq;
        loop {
            let event = events.recv_until(deadline()).expect("projected event");
            assert_eq!(
                event.native.is_some(),
                native_events == NativeEvents::Retain
            );
            let context = &event.portable.context;
            assert_eq!(context.provider, Provider::Codex);
            assert_eq!(context.generation, app.generation());
            assert_eq!(context.session_id.as_deref(), Some("t"));
            assert_eq!(context.turn_id.as_deref(), Some("a"));
            assert!(context.seq > previous);
            previous = context.seq;
            match event.portable.kind {
                EventKind::Text {
                    text,
                    update: TextUpdate::Replace,
                    phase: TextPhase::Final,
                } => {
                    assert_eq!(text, "hello 🌍, corrected");
                    assert_eq!(context.item_id.as_deref(), Some("i"));
                    saw_final = true;
                }
                EventKind::Usage(usage) => {
                    assert_eq!(usage.total.total_tokens, 30);
                    saw_usage = true;
                }
                EventKind::TurnCompleted {
                    status: TurnStatus::Completed,
                } => {
                    terminal_seq = context.seq;
                    break;
                }
                _ => {}
            }
        }
        // Snapshot reads require no further event consumption or ticket observation.
        drop(events);
        assert!(saw_final && saw_usage);
        let snapshot = app.snapshot();
        assert_eq!(snapshot.thread_id.as_deref(), Some("t"));
        let last = snapshot.last_turn.unwrap();
        assert_eq!(last.context.seq, terminal_seq);
        assert_eq!(last.context.generation, app.generation());
        let usage = last.usage.unwrap();
        assert_eq!(usage.usage.last.total_tokens, 15);
        assert_eq!(
            usage.native.is_some(),
            native_events == NativeEvents::Retain
        );
        if let Some(native) = usage.native {
            assert_eq!(native["params"]["tokenUsage"]["futureField"], true);
        }
        result(&ticket);
    }
}

#[test]
fn handshake_native_items_and_early_terminal_result_preserve_evidence() {
    let (_temp, app, events) = spawn("normal", CodexLimits::default());
    start(&app);
    let ticket = app.turn_start("hello", deadline()).unwrap();
    let response = result(&ticket);
    assert_eq!(response["turn"]["id"], "a");
    assert_eq!(
        app.snapshot().last_turn.unwrap().status,
        TurnStatus::Completed
    );
    assert!(app.snapshot().active_turn.is_none());
    let mut seq = None;
    loop {
        let event = events.recv_until(deadline()).unwrap();
        assert_eq!(event.generation, app.generation());
        if let Some(previous) = seq {
            assert!(event.seq > previous);
        }
        seq = Some(event.seq);
        if event.native.as_ref().unwrap()["method"] == "turn/completed" {
            break;
        }
    }
    drop(events);
    assert_eq!(app.snapshot().last_turn.unwrap().turn_id, "a");
}

#[test]
fn subagent_notifications_preserve_parent_requests_and_owned_connection() {
    for resume in [false, true] {
        let (_temp, app, events) = spawn("subagent", CodexLimits::default());
        if resume {
            result(&app.thread_resume("t", json!({}), deadline()).unwrap());
        } else {
            start(&app);
        }
        assert_eq!(app.snapshot().thread_id.as_deref(), Some("t"));
        assert!(app.snapshot().active_turn.is_none());
        assert!(app.snapshot().last_turn.is_none());
        result(&app.turn_start("parent", deadline()).unwrap());
        assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
        assert!(app.snapshot().last_turn.is_none());
        assert!(matches!(
            app.turn_start("busy", deadline()),
            Err(CodexError::Busy)
        ));
        result(&app.turn_steer("a", "more", deadline()).unwrap());
        result(&app.turn_interrupt("a", deadline()).unwrap());
        let mut seen = Vec::new();
        let mut previous = 0;
        loop {
            let event = events.recv_until(deadline()).expect("native event");
            assert_eq!(event.generation, app.generation());
            assert!(event.seq > previous);
            previous = event.seq;
            let method = event.native.as_ref().unwrap()["method"].as_str().unwrap();
            let params = &event.native.as_ref().unwrap()["params"];
            let thread = params["threadId"]
                .as_str()
                .or_else(|| params["thread"]["id"].as_str())
                .unwrap();
            seen.push((thread.to_owned(), method.to_owned()));
            if thread == "t" && method == "turn/completed" {
                break;
            }
        }
        let child = [
            "thread/started",
            "turn/started",
            "item/agentMessage/delta",
            "turn/completed",
            "serverRequest/resolved",
        ];
        let expected = child
            .iter()
            .map(|method| ("child", *method))
            .chain([("t", "turn/started"), ("t", "item/agentMessage/delta")])
            .chain(child.iter().map(|method| ("child", *method)))
            .chain([("t", "turn/completed")])
            .map(|(thread, method)| (thread.to_owned(), method.to_owned()))
            .collect::<Vec<_>>();
        assert_eq!(seen, expected);
        let outcome = app.snapshot().last_turn.unwrap();
        assert_eq!(outcome.thread_id, "t");
        assert_eq!(outcome.turn_id, "a");
        assert_eq!(outcome.status, TurnStatus::Interrupted);
        assert!(app.snapshot().active_turn.is_none());
        assert!(app.failure().is_none());
    }
}

#[test]
fn response_and_terminal_turn_survive_immediate_child_exit() {
    let (_temp, app, events) = spawn("response-exit", CodexLimits::default());
    start(&app);
    let ticket = app.turn_start("finish and exit", deadline()).unwrap();
    let native = result(&ticket);
    assert_eq!(native["turn"]["id"], "a");
    let mut methods = Vec::new();
    let until = deadline();
    while let Some(event) = events.recv_until(until) {
        methods.push(
            event.native.as_ref().unwrap()["method"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    assert_eq!(
        methods,
        ["turn/started", "item/agentMessage/delta", "turn/completed"]
    );
    assert_eq!(app.failure(), Some(Failure::Eof));
    assert!(app.snapshot().active_turn.is_none());
    assert_eq!(
        app.snapshot().last_turn.unwrap().status,
        TurnStatus::Completed
    );
    assert!(matches!(
        &*ticket.outcome().unwrap(),
        RequestOutcome::Response(_)
    ));
}

#[test]
fn approvals_and_unsupported_reverse_requests_do_not_block_response_dispatch() {
    for scene in ["approval", "file-approval", "unsupported"] {
        let (_temp, app, events) = spawn(scene, CodexLimits::default());
        start(&app);
        let turn = app.turn_start("approve", deadline()).unwrap();
        if scene != "unsupported" {
            let token = next_approval(&events);
            let reply = token.respond(json!("decline")).unwrap();
            assert_eq!(
                *reply.wait_until(deadline()).unwrap(),
                ReplyOutcome::Written
            );
            assert!(matches!(
                token.respond(json!("accept")),
                Err(CodexError::StaleApproval)
            ));
        }
        result(&turn); // Never requires draining the remaining ordinary events.
        assert_eq!(
            app.snapshot().last_turn.unwrap().status,
            TurnStatus::Completed
        );
    }
}

#[test]
fn exact_turn_steer_and_interrupt_are_separate_from_turn_completion() {
    let (_temp, app, _events) = spawn("interrupt", CodexLimits::default());
    start(&app);
    result(&app.turn_start("start", deadline()).unwrap());
    result(&app.turn_steer("a", "more", deadline()).unwrap());
    result(&app.turn_interrupt("a", deadline()).unwrap());
    assert_eq!(
        app.snapshot().last_turn.unwrap().status,
        TurnStatus::Interrupted
    );
    assert!(app.failure().is_none());
}

#[test]
fn replay_before_resume_response_and_missing_history_preserve_identity() {
    let (_temp, app, events) = spawn("resume", CodexLimits::default());
    let resume = app.thread_resume("t", json!({}), deadline()).unwrap();
    next_approval(&events).respond(json!("decline")).unwrap();
    result(&resume);
    assert_eq!(app.snapshot().thread_id.as_deref(), Some("t"));
    let (_temp, app, _events) = spawn("resume-missing", CodexLimits::default());
    let resume = app.thread_resume("missing", json!({}), deadline()).unwrap();
    assert!(matches!(
        &*resume.wait_until(deadline()).unwrap(),
        RequestOutcome::RemoteError(_)
    ));
    assert!(matches!(
        app.thread_start(json!({}), deadline()),
        Err(CodexError::InvalidState)
    ));
}

#[test]
fn metadata_only_resume_is_not_emitted_and_full_resume_requires_explicit_steering() {
    let (_temp, app, _events) = spawn("resume-active", CodexLimits::default());
    assert!(matches!(
        app.thread_resume("t", json!({"excludeTurns":true}), deadline()),
        Err(CodexError::Transport(agent_link::Error::InvalidInput(_)))
    ));
    assert!(app.snapshot().thread_id.is_none());
    let resume = app
        .thread_resume("t", json!({"excludeTurns":false}), deadline())
        .unwrap();
    let resumed = result(&resume);
    assert_eq!(
        resumed["fixtureThreadRequests"], 1,
        "refused resume must emit nothing"
    );
    assert_eq!(resumed["thread"]["status"]["type"], "active");
    assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
    assert!(matches!(
        app.turn_start("must not steer", deadline()),
        Err(CodexError::Busy)
    ));
    let steered = result(
        &app.turn_steer("a", "explicit steering", deadline())
            .unwrap(),
    );
    assert_eq!(
        steered["fixtureTurnStarts"], 0,
        "busy turn/start must emit nothing"
    );
    result(&app.turn_interrupt("a", deadline()).unwrap());
    assert!(app.snapshot().active_turn.is_none());
    assert!(app.failure().is_none());
}

#[test]
fn replayed_approval_blocks_implicit_steering_and_allows_exact_turn_controls() {
    for scene in ["resume", "resume-file-approval"] {
        let (_temp, app, events) = spawn(scene, CodexLimits::default());
        let resume = app.thread_resume("t", json!({}), deadline()).unwrap();
        let token = next_approval(&events);
        let resumed = result(&resume);
        assert_eq!(resumed["thread"]["turns"], json!([]));
        assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
        assert!(matches!(
            app.turn_start("must not steer", deadline()),
            Err(CodexError::Busy)
        ));

        let steer = result(
            &app.turn_steer("a", "explicit steering", deadline())
                .unwrap(),
        );
        assert_eq!(steer["turnId"], "a");
        assert_eq!(
            steer["fixtureTurnStarts"], 0,
            "refused turn/start must emit nothing"
        );

        let reply = token.respond(json!("decline")).unwrap();
        assert_eq!(
            *reply.wait_until(deadline()).unwrap(),
            ReplyOutcome::Written
        );
        assert_eq!(app.snapshot().active_turn.as_deref(), Some("a"));
        assert!(matches!(
            app.turn_start("still busy", deadline()),
            Err(CodexError::Busy)
        ));
        result(&app.turn_interrupt("a", deadline()).unwrap());
        assert!(app.snapshot().active_turn.is_none());
        assert_eq!(
            app.snapshot().last_turn.unwrap().status,
            TurnStatus::Interrupted
        );
        assert!(app.failure().is_none());
    }
}

#[test]
fn unread_events_are_terminal_without_starving_request_failure() {
    let mut limits = CodexLimits::default();
    limits.events = 1;
    let (_temp, app, _events) = spawn("flood", limits);
    start(&app);
    let ticket = app.turn_start("flood", deadline()).unwrap();
    assert!(matches!(
        &*ticket.wait_until(deadline()).unwrap(),
        RequestOutcome::OutcomeUnknown {
            failure: Failure::ConsumerLagged,
            ..
        }
    ));
    assert_eq!(app.failure(), Some(Failure::ConsumerLagged));
}

#[test]
fn eof_invalid_json_and_invalid_utf8_settle_native_waiters() {
    for (scene, failure) in [
        ("eof", Failure::Eof),
        ("invalid", Failure::Protocol),
        ("utf8", Failure::Transport(RpcTransportError::InvalidUtf8)),
    ] {
        let (_temp, app, _events) = spawn(scene, CodexLimits::default());
        start(&app);
        let ticket = app.turn_start("x", deadline()).unwrap();
        assert!(
            matches!(&*ticket.wait_until(deadline()).unwrap(), RequestOutcome::OutcomeUnknown { failure: f, .. } if *f == failure)
        );
        assert_eq!(app.failure(), Some(failure));
    }
}

#[test]
fn blocked_pipe_and_missing_response_have_total_deadlines_and_no_resend() {
    for scene in ["blocked", "hang"] {
        let (_temp, app, events) = spawn(scene, CodexLimits::default());
        start(&app);
        if scene == "blocked" {
            assert_eq!(
                events
                    .recv_until(deadline())
                    .unwrap()
                    .native
                    .as_ref()
                    .unwrap()["method"],
                "fixture/blocked"
            );
        }
        let ticket = app
            .turn_start(
                &"x".repeat(512 * 1024),
                Instant::now() + Duration::from_millis(300),
            )
            .unwrap();
        let outcome = ticket.wait_until(deadline()).unwrap();
        assert!(
            matches!(&*outcome, RequestOutcome::OutcomeUnknown { .. }),
            "{outcome:?}"
        );
        assert!(app.failure().is_some());
        assert!(matches!(
            app.turn_start("retry", deadline()),
            Err(CodexError::Closed(_))
        ));
    }
}

#[test]
fn dropping_owner_settles_tickets_and_invalidates_approval_handles() {
    let (_temp, app, events) = spawn("approval", CodexLimits::default());
    start(&app);
    let ticket = app.turn_start("x", deadline()).unwrap();
    let approval = next_approval(&events);
    drop(app);
    assert!(matches!(
        &*ticket.wait_until(deadline()).unwrap(),
        RequestOutcome::OutcomeUnknown {
            failure: Failure::Closed,
            ..
        }
    ));
    assert!(matches!(
        approval.respond(json!("accept")),
        Err(CodexError::StaleApproval)
    ));
}

#[test]
fn default_budgets_deliver_large_text_and_approval_envelopes() {
    for native_events in [NativeEvents::Retain, NativeEvents::Omit] {
        let mut options = CodexOptions::default();
        options.native_events = native_events;
        let (_temp, app, events) = spawn_with_options("large", options);
        start(&app);
        let payload = "x".repeat(6 * 1024 * 1024);
        let ticket = app.turn_start(&payload, deadline()).unwrap();
        let mut saw_text = false;
        loop {
            let event = events.recv_until(deadline()).unwrap();
            if let EventKind::Text { text, .. } = event.portable.kind {
                assert_eq!(text, payload);
                saw_text = true;
            }
            if let Some(approval) = event.approval {
                assert_eq!(approval.request().params["command"], payload);
                approval.respond(json!("decline")).unwrap();
                break;
            }
        }
        assert!(saw_text);
        result(&ticket);
        assert!(app.failure().is_none());
        app.close();
    }
}

#[test]
fn explicit_lower_transport_stdout_limit_remains_authoritative() {
    let limit = 1024 * 1024;
    let (_temp, app, _events) =
        spawn_with_stdout_limit("large", CodexOptions::default(), Some(limit));
    start(&app);
    let ticket = app
        .turn_start(&"x".repeat(6 * 1024 * 1024), deadline())
        .unwrap();
    assert!(matches!(
        &*ticket.wait_until(deadline()).unwrap(),
        RequestOutcome::OutcomeUnknown { failure: Failure::Transport(RpcTransportError::LineTooLong { limit: observed, .. }), .. }
            if *observed == limit
    ));
    app.close();
}
