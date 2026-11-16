//! Offline pinned synthetic protocol tests through the shared Rust fake-agent.
mod common;
use agent_link::agent::RpcLaunch;
use agent_link::claude::*;
use agent_link::portable::EventKind;
use agent_link::rpc::RpcSpawnSpec;
use std::time::Instant;
fn deadline() -> Instant {
    common::deadline()
}
fn spawn(scene: &str, native_events: NativeEvents) -> (tempfile::TempDir, ClaudeSession, Events) {
    let temp = tempfile::tempdir().unwrap();
    let launch = RpcLaunch::new(
        common::fake_agent_path().to_str().unwrap(),
        &["--claude-peer", scene],
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
    let (session, events) = ClaudeSession::spawn_with_options(
        spec,
        Options {
            native_events,
            ..Options::default()
        },
        None,
    )
    .unwrap();
    assert!(matches!(
        &*session
            .initialize(deadline())
            .unwrap()
            .wait_until(deadline())
            .unwrap(),
        Outcome::Response(_)
    ));
    events.recv_until(deadline()).unwrap();
    (temp, session, events)
}
#[test]
fn persistent_real_pipes_keep_permission_channel_after_result() {
    for retention in [NativeEvents::Retain, NativeEvents::Omit] {
        let (_temp, session, events) = spawn("persistent", retention);
        for uuid in ["first", "second"] {
            let sent = session.send("synthetic", Some(uuid), deadline()).unwrap();
            let result = events.recv_until(deadline()).unwrap();
            assert!(matches!(
                result.portable.kind,
                EventKind::TurnCompleted { .. }
            ));
            assert_eq!(result.native.is_some(), retention == NativeEvents::Retain);
            assert_eq!(
                session.snapshot().last_result.unwrap()["user_message_uuids"][0],
                uuid
            );
            assert!(matches!(
                &*sent.wait_until(deadline()).unwrap(),
                Outcome::Written
            ));
            events.recv_until(deadline()).unwrap();
            let approval = events.recv_until(deadline()).unwrap().approval.unwrap();
            assert_eq!(approval.request_id(), "permission-exact");
            assert_eq!(approval.request()["request"]["tool_name"], "Read");
            let reply = approval.respond(None, Some("synthetic denial")).unwrap();
            events.recv_until(deadline()).unwrap();
            assert!(matches!(
                &*reply.wait_until(deadline()).unwrap(),
                Outcome::Written
            ));
            assert!(session.failure().is_none());
        }
        assert!(matches!(
            &*session
                .interrupt(deadline())
                .unwrap()
                .wait_until(deadline())
                .unwrap(),
            Outcome::Response(_)
        ));
        session.close();
        assert_eq!(session.failure(), Some(Failure::Closed));
    }
}
#[test]
fn result_then_late_eof_keeps_terminal_usage_after_consumer_stops() {
    for retention in [NativeEvents::Retain, NativeEvents::Omit] {
        let (_temp, session, events) = spawn("eof", retention);
        session.send("synthetic", Some("one"), deadline()).unwrap();
        let result = events.recv_until(deadline()).unwrap();
        assert!(matches!(
            result.portable.kind,
            EventKind::TurnCompleted { .. }
        ));
        assert!(events.recv_until(deadline()).is_none());
        assert_eq!(session.failure(), Some(Failure::Eof));
        assert_eq!(
            session.snapshot().last_result.unwrap()["usage"]["input_tokens"],
            17
        );
    }
}

#[cfg(feature = "approvals")]
#[test]
fn approval_callbacks_delegation_panic_invalid_input_and_stale_clones_use_real_pipes() {
    use agent_link::approvals::{
        ClaudeDecision, Executor, Limits, Outcome as CallbackOutcome, Reply,
    };
    let executor = Executor::new(Limits::default()).unwrap();
    let (_temp, session, events) = spawn("persistent", NativeEvents::Omit);
    for mode in ["delegate", "panic", "invalid", "deny"] {
        session.send("synthetic", Some(mode), deadline()).unwrap();
        let approval = loop {
            if let Some(approval) = events.recv_until(deadline()).unwrap().approval {
                break approval;
            }
        };
        let clone = approval.clone();
        let ticket = executor
            .claude(approval, deadline(), move |request| {
                assert_eq!(request.id, "permission-exact");
                assert_eq!(request.native["request"]["tool_name"], "Read");
                match mode {
                    "delegate" => ClaudeDecision::Delegate,
                    "panic" => panic!("synthetic callback failure"),
                    "invalid" => ClaudeDecision::Allow(serde_json::json!([])),
                    _ => ClaudeDecision::Deny("fixture denial".into()),
                }
            })
            .unwrap();
        let outcome = ticket.wait_until(deadline()).unwrap();
        let written = match &*outcome {
            CallbackOutcome::Delegated => {
                assert_eq!(mode, "delegate");
                clone
                    .respond(None, Some("host denial after delegation"))
                    .unwrap()
                    .wait_until(deadline())
                    .unwrap()
            }
            CallbackOutcome::Replied(Reply::Claude(Ok(write))) => {
                write.wait_until(deadline()).unwrap()
            }
            _ => panic!("callback did not settle with native reply/delegation"),
        };
        assert!(matches!(&*written, Outcome::Written));
        assert!(clone.respond(None, Some("duplicate")).is_err());
        events.recv_until(deadline()).unwrap();
    }
    session.close();
}

#[test]
fn default_budgets_deliver_large_text_and_permission_envelopes() {
    for retention in [NativeEvents::Retain, NativeEvents::Omit] {
        let (_temp, session, events) = spawn("large", retention);
        let payload = "x".repeat(6 * 1024 * 1024);
        let sent = session.send(&payload, Some("large"), deadline()).unwrap();
        let event = events.recv_until(deadline()).unwrap();
        assert!(
            matches!(event.portable.kind, EventKind::Text { ref text, .. } if text == &payload)
        );
        assert_eq!(event.native.is_some(), retention == NativeEvents::Retain);
        let approval = events.recv_until(deadline()).unwrap().approval.unwrap();
        assert_eq!(approval.request()["request"]["input"]["file_path"], payload);
        let reply = approval
            .respond(Some(&serde_json::json!({"file_path": payload})), None)
            .unwrap();
        events.recv_until(deadline()).unwrap();
        assert!(matches!(
            &*sent.wait_until(deadline()).unwrap(),
            Outcome::Written
        ));
        assert!(matches!(
            &*reply.wait_until(deadline()).unwrap(),
            Outcome::Written
        ));
        assert!(session.failure().is_none());
        session.close();
    }
}
