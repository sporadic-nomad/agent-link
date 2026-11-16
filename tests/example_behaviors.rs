//! Offline regressions for the actual event and recovery helpers used by examples.
#[allow(dead_code)]
#[path = "../examples/acp_prompt.rs"]
mod acp_prompt;
#[allow(dead_code)]
#[path = "../examples/claude_session.rs"]
mod claude_session;
mod common;
#[allow(dead_code)]
#[path = "../examples/pty_recovery_observers.rs"]
mod pty_recovery_observers;
#[cfg(feature = "tokio")]
#[allow(dead_code)]
#[path = "../examples/tokio_shell.rs"]
mod tokio_shell;

use agent_link::acp::{AcpClient, AcpLimits, RequestOutcome};
use agent_link::agent::RpcLaunch;
use agent_link::claude::{ClaudeSession, Failure, Limits, Outcome};
use agent_link::portable::EventKind;
use agent_link::rpc::RpcSpawnSpec;
use std::time::Instant;

#[cfg(feature = "tokio")]
#[tokio::test]
async fn tokio_example_matches_output_split_across_chunks() {
    use agent_link::{Event, EventStream, SessionId};
    use std::time::Duration;

    let (sender, receiver) = crossbeam_channel::bounded(3);
    let mut noisy_prefix = vec![b'x'; 16 * 1024];
    noisy_prefix.extend_from_slice(b"he");
    for (seq, bytes) in [noisy_prefix, b"l".to_vec(), b"lo\r\n".to_vec()]
        .into_iter()
        .enumerate()
    {
        sender
            .send(Event::Output {
                id: SessionId(1),
                seq: seq as u64,
                bytes,
            })
            .unwrap();
    }
    let mut events = EventStream::new(receiver).unwrap();
    // Keep the sender alive: matching output must finish without EOF.
    tokio_shell::wait_for_hello(&mut events, Duration::from_secs(5))
        .await
        .unwrap();
    drop(sender);
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn tokio_example_reports_exit_disconnection_and_deadline() {
    use agent_link::{Event, EventStream, SessionId};
    use std::time::Duration;

    let (sender, receiver) = crossbeam_channel::bounded(1);
    sender
        .send(Event::Exit {
            id: SessionId(1),
            status: Some(0),
        })
        .unwrap();
    let mut events = EventStream::new(receiver).unwrap();
    let error = tokio_shell::wait_for_hello(&mut events, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("shell exited before output"));

    let (sender, receiver) = crossbeam_channel::bounded(1);
    drop(sender);
    let mut events = EventStream::new(receiver).unwrap();
    let error = tokio_shell::wait_for_hello(&mut events, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("stream ended before output"));

    let (_sender, receiver) = crossbeam_channel::bounded(1);
    let mut events = EventStream::new(receiver).unwrap();
    let error = tokio_shell::wait_for_hello(&mut events, Duration::ZERO)
        .await
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
    );
}

fn peer_spec(home: &std::path::Path, protocol: &str, scene: &str) -> RpcSpawnSpec {
    let launch = RpcLaunch::new(
        common::fake_agent_path().to_str().unwrap(),
        &[protocol, scene],
    );
    let mut spec = RpcSpawnSpec::new(launch, home);
    spec.shell = Some("/bin/sh".into());
    spec.env.inherit = false;
    spec.env
        .set("HOME", home)
        .set("USERPROFILE", home)
        .set("PATH", std::env::var_os("PATH").unwrap_or_default());
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        spec.env.set("SystemRoot", root);
    }
    spec
}

#[test]
fn acp_example_drains_queued_text_even_when_ticket_is_already_complete() {
    let home = tempfile::tempdir().unwrap();
    let (client, events) = AcpClient::spawn(
        peer_spec(home.path(), "--acp-peer", "normal"),
        AcpLimits::default(),
    )
    .unwrap();
    let initialized = client
        .initialize("example-test", "1", common::deadline())
        .unwrap();
    assert!(matches!(
        initialized.wait_until(common::deadline()).as_deref(),
        Some(RequestOutcome::Response(_))
    ));
    let session = client
        .session_new(home.path().to_str().unwrap(), common::deadline())
        .unwrap();
    assert!(matches!(
        session.wait_until(common::deadline()).as_deref(),
        Some(RequestOutcome::Response(_))
    ));
    let prompt = client.prompt("hello", common::deadline()).unwrap();
    assert!(matches!(
        prompt.wait_until(common::deadline()).as_deref(),
        Some(RequestOutcome::Response(_))
    ));
    // Initialization and session events are still ahead of the answer in the queue.
    let mut output = Vec::new();
    acp_prompt::write_prompt(&events, &prompt, common::deadline(), &mut output).unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "hello");
    // A later update is outside this prompt's terminal boundary and is not consumed.
    let late = events.recv_until(common::deadline()).unwrap();
    assert!(matches!(&late.portable.kind, EventKind::Text { text, .. } if text == "late"));
}

#[test]
fn acp_example_reports_remote_error_without_waiting_for_turn_completed() {
    let home = tempfile::tempdir().unwrap();
    let (client, events) = AcpClient::spawn(
        peer_spec(home.path(), "--acp-peer", "remote-error"),
        AcpLimits::default(),
    )
    .unwrap();
    client
        .initialize("example-test", "1", common::deadline())
        .unwrap()
        .wait_until(common::deadline())
        .unwrap();
    client
        .session_new(home.path().to_str().unwrap(), common::deadline())
        .unwrap()
        .wait_until(common::deadline())
        .unwrap();
    let prompt = client.prompt("hello", common::deadline()).unwrap();
    let error = acp_prompt::write_prompt(&events, &prompt, common::deadline(), &mut Vec::new())
        .unwrap_err();
    assert!(error.to_string().contains("ACP prompt failed"));
    assert!(matches!(
        prompt.outcome().as_deref(),
        Some(RequestOutcome::RemoteError(_))
    ));
    assert!(
        client.failure().is_none(),
        "a remote error does not close the session"
    );
}

#[test]
fn claude_example_requires_a_successful_result() {
    for (scene, expected) in [
        ("silent", "timed out"),
        ("before-result-eof", "ended before a successful result"),
        ("failed-result", "did not succeed: Failed"),
        ("unknown-result", "did not succeed: Unknown"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let (session, events) = ClaudeSession::spawn(
            peer_spec(home.path(), "--claude-peer", scene),
            Limits::default(),
            None,
        )
        .unwrap();
        assert!(matches!(
            session
                .initialize(common::deadline())
                .unwrap()
                .wait_until(common::deadline())
                .as_deref(),
            Some(Outcome::Response(_))
        ));
        let sent = session.send("hello", None, common::deadline()).unwrap();
        assert!(sent.wait_until(common::deadline()).is_some());
        let end = if scene == "silent" {
            Instant::now()
        } else {
            common::deadline()
        };
        let error =
            claude_session::write_turn(&session, &events, end, &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains(expected), "{scene}: {error}");
        if scene == "silent" {
            assert!(matches!(sent.outcome().as_deref(), Some(Outcome::Written)));
            assert!(session.snapshot().last_result.is_none());
        }
    }
}

#[test]
fn claude_example_keeps_successful_result_before_late_eof() {
    let home = tempfile::tempdir().unwrap();
    let (session, events) = ClaudeSession::spawn(
        peer_spec(home.path(), "--claude-peer", "eof"),
        Limits::default(),
        None,
    )
    .unwrap();
    session
        .initialize(common::deadline())
        .unwrap()
        .wait_until(common::deadline())
        .unwrap();
    session.send("hello", None, common::deadline()).unwrap();
    common::wait_until("EOF after the native result", || {
        session.failure() == Some(Failure::Eof)
    });
    let mut output = Vec::new();
    claude_session::write_turn(&session, &events, common::deadline(), &mut output).unwrap();
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("Turn completed: Completed")
    );
    assert!(session.snapshot().last_result.is_some());
}

#[cfg(unix)]
#[test]
fn recovery_example_uses_the_captured_display_fence_and_preserves_loss() {
    use agent_link::SessionLogConfig;
    use agent_link::transcript::Origin;

    for log in [SessionLogConfig::default(), SessionLogConfig::Off] {
        let home = tempfile::tempdir().unwrap();
        let mut spec = common::sh_spec(home.path());
        spec.log = log;
        let (term, events) = common::spawn(spec).unwrap();
        drop(events);
        term.write(b"echo before-$((40+2))\n").unwrap();
        common::screen_contains(&term, "before-42", common::SHELL_START_WAIT);
        let (recovery, receiver) = term.attach_recovery(None).unwrap();
        drop(receiver);
        term.write(b"echo after-$((40+3))\n").unwrap();
        common::screen_contains(&term, "after-43", common::SHELL_START_WAIT);

        let projected = pty_recovery_observers::project_recovery(&recovery).unwrap();
        assert_eq!(projected.incomplete, !recovery.gaps.is_empty());
        let message = projected.messages.first().unwrap();
        assert!(message.text.contains("before-42"));
        assert!(
            !message.text.contains("after-43"),
            "later output cannot move a captured display"
        );
        let Origin::Screen(span) = &message.origin else {
            panic!("screen provenance")
        };
        assert_eq!(span.generation, recovery.boundary.session);
        assert_eq!(span.start, 0);
        assert_eq!(span.end, recovery.boundary.offset);
        assert_eq!(span.first_seq, 0);
        assert_eq!(span.last_seq + 1, recovery.next_output_seq);
    }
}
