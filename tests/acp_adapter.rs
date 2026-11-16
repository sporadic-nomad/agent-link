//! Offline stable-v1 conformance over owned subprocess pipes, in isolated homes.
mod common;
use agent_link::acp::*;
use agent_link::agent::RpcLaunch;
use agent_link::portable::{CompletionStatus, EventKind};
use agent_link::rpc::RpcSpawnSpec;
use std::time::{Duration, Instant};
fn deadline() -> Instant {
    common::deadline()
}
fn spawn(scene: &str) -> (tempfile::TempDir, AcpClient, AcpEvents) {
    let temp = tempfile::tempdir().unwrap();
    let launch = RpcLaunch::new(
        common::fake_agent_path().to_str().unwrap(),
        &["--acp-peer", scene],
    );
    let mut spec = RpcSpawnSpec::new(launch, temp.path());
    spec.shell = Some("/bin/sh".into());
    spec.env.inherit = false;
    spec.env
        .set("HOME", temp.path())
        .set("USERPROFILE", temp.path())
        .set("PATH", std::env::var_os("PATH").unwrap_or_default());
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        spec.env.set("SystemRoot", root);
    }
    let (app, events) = AcpClient::spawn(spec, AcpLimits::default()).unwrap();
    (temp, app, events)
}
fn result(ticket: RequestTicket) {
    assert!(matches!(
        &*ticket.wait_until(deadline()).expect("settled request"),
        RequestOutcome::Response(_)
    ));
}
fn ready(scene: &str) -> (tempfile::TempDir, AcpClient, AcpEvents) {
    let (temp, app, events) = spawn(scene);
    result(app.initialize("fixture", "1", deadline()).unwrap());
    result(
        app.session_new(temp.path().to_str().unwrap(), deadline())
            .unwrap(),
    );
    (temp, app, events)
}
#[test]
fn strict_load_routes_history_before_response_and_prompt_preserves_late_events() {
    let (temp, app, events) = spawn("normal");
    result(app.initialize("fixture", "1", deadline()).unwrap());
    result(
        app.session_load("s", temp.path().to_str().unwrap(), deadline())
            .unwrap(),
    );
    result(app.prompt("hello", deadline()).unwrap());
    let mut history = false;
    let mut late = false;
    let mut terminal = false;
    while !late {
        let event = events.recv_until(deadline()).expect("ordered observations");
        match &event.portable.kind {
            EventKind::Text { text, .. } if text == "history" => history = true,
            EventKind::Text { text, .. } if text == "late" => {
                assert!(terminal);
                late = true;
            }
            EventKind::TurnCompleted {
                status: CompletionStatus::Completed,
            } => terminal = true,
            _ => {}
        }
    }
    assert!(history && terminal);
    assert!(!app.snapshot().prompting);
    app.close();
}
#[test]
fn permissions_are_explicit_and_cancellation_awaits_prompt_result() {
    for scene in ["permission", "cancel"] {
        let (_temp, app, events) = ready(scene);
        let prompt = app.prompt("hello", deadline()).unwrap();
        let p = loop {
            if let Some(p) = events
                .recv_until(deadline())
                .expect("permission")
                .permission
            {
                break p;
            }
        };
        if scene == "cancel" {
            let _write = app.cancel(deadline()).unwrap();
            assert!(p.select("yes").is_err());
        } else {
            assert!(p.select("unoffered").is_err());
            let _write = p.select("no").unwrap();
            assert!(p.select("yes").is_err());
        }
        result(prompt);
        assert!(!app.snapshot().prompting);
        app.close();
    }
}
#[test]
fn eof_and_deadline_keep_execution_unknown_and_events_do_not_own_tickets() {
    for scene in ["eof", "silent"] {
        let (_temp, app, events) = ready(scene);
        drop(events);
        let prompt = app
            .prompt("hello", Instant::now() + Duration::from_millis(200))
            .unwrap();
        assert!(matches!(
            &*prompt.wait_until(deadline()).expect("finite settlement"),
            RequestOutcome::OutcomeUnknown { .. }
        ));
        assert!(matches!(
            app.failure(),
            Some(Failure::Eof | Failure::DeadlineExceeded)
        ));
        app.close();
    }
}
#[test]
fn incompatible_protocol_is_terminal() {
    let (_temp, app, _events) = spawn("version");
    let init = app.initialize("fixture", "1", deadline()).unwrap();
    assert!(matches!(
        &*init.wait_until(deadline()).unwrap(),
        RequestOutcome::OutcomeUnknown {
            failure: Failure::Protocol,
            ..
        }
    ));
    assert_eq!(app.failure(), Some(Failure::Protocol));
}
