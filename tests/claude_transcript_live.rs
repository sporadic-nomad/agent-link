//! Opt-in native PTYs. The Python runner supplies the pinned executable, isolated
//! filesystem, sandbox and local Messages API peer before invoking this test.

#![cfg(all(feature = "turns", target_os = "macos", target_arch = "aarch64"))]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_link::agent::profiles::ClaudeProfile;
use agent_link::agent::turns::{
    AttachError, ExclusiveClaudeRoot, TurnGap, TurnOptions, TurnReceiver, TurnStatus,
};
use agent_link::agent::{AgentSpec, AgentTerminal};
use agent_link::{Error, PtySize, SpawnSpec};
use serde_json::Value;

fn wait<T>(what: &str, mut poll: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(25);
    loop {
        if let Some(value) = poll() {
            return value;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::park_timeout(Duration::from_millis(20));
    }
}

struct Fixture {
    root: PathBuf,
    url: String,
}

impl Fixture {
    fn spawn(
        &self,
        flag: &str,
        id: &str,
        authority: Option<&ExclusiveClaudeRoot>,
    ) -> agent_link::Result<AgentTerminal> {
        let mut spec = SpawnSpec::direct(
            self.root.join("claude"),
            [
                flag,
                id,
                "--model",
                "claude-sonnet-4-6",
                "--strict-mcp-config",
                "--tools",
                "",
                "--settings",
                r#"{"showTurnDuration":false}"#,
            ],
            self.root.join("project"),
            PtySize {
                rows: 40,
                cols: 120,
            },
        );
        spec.env.inherit = false;
        spec.env
            .set("HOME", self.root.join("home"))
            .set("TMPDIR", self.root.join("tmp"))
            .set("CLAUDE_CONFIG_DIR", self.root.join("config"))
            .set("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .set("SHELL", "/bin/sh")
            .set("TERM", "xterm-256color")
            .set("LANG", "en_US.UTF-8")
            .set("ANTHROPIC_API_KEY", "fixture-dummy-no-credentials")
            .set("ANTHROPIC_BASE_URL", &self.url)
            .set("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .set("DISABLE_AUTOUPDATER", "1")
            .set("DISABLE_TELEMETRY", "1");
        let agent = AgentSpec::new(Arc::new(ClaudeProfile::default()));
        let (term, _events) = match authority {
            Some(root) => AgentTerminal::spawn_with_claude_turns(spec, agent, root),
            None => AgentTerminal::spawn(spec, agent),
        }?;
        Ok(term)
    }

    fn writer(&self, term: &AgentTerminal) -> Option<Value> {
        let pid = term.terminal().pid().unwrap();
        let data = fs::read(self.root.join(format!("config/sessions/{pid}.json"))).ok()?;
        let writer: Value = serde_json::from_slice(&data).ok()?;
        assert_eq!(writer["pid"], pid);
        assert_eq!(writer["cwd"].as_str(), self.root.join("project").to_str());
        assert_eq!(writer["version"], "2.1.296");
        Some(writer)
    }

    fn transcript(&self, id: &str) -> PathBuf {
        let slug: String = self
            .root
            .join("project")
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        self.root
            .join("config/projects")
            .join(slug)
            .join(format!("{id}.jsonl"))
    }
}

fn visible(term: &AgentTerminal, needle: &str) -> bool {
    term.terminal()
        .screen()
        .is_some_and(|s| s.text().contains(needle))
}

fn submit(term: &AgentTerminal, text: &str) {
    term.terminal().write(text.as_bytes()).unwrap();
    wait(&format!("composer to show {text}"), || {
        visible(term, text).then_some(())
    });
    term.terminal().write(b"\r").unwrap();
}

fn prompt(term: &AgentTerminal, name: &str) {
    submit(term, &format!("fixture prompt {name}"));
}

// Read only this controlled fixture's transcript to pair the on-screen reply with
// an actual assistant text record followed by the native completion marker.
fn rendered_turn(term: &AgentTerminal, file: &Path, name: &str) -> u64 {
    prompt(term, name);
    let expected = format!("Fixture response {name}");
    wait(
        &format!("rendered and recorded completion for {name}"),
        || {
            if !visible(term, &expected) {
                return None;
            }
            let data = fs::read_to_string(file).ok()?;
            let mut matching = false;
            for line in data.lines() {
                let row = serde_json::from_str::<Value>(line).ok()?;
                if row["type"] == "assistant" && row["message"]["content"][0]["text"] == expected {
                    matching = true;
                }
                if matching && row["type"] == "system" && row["subtype"] == "turn_duration" {
                    return Some(data.len() as u64);
                }
            }
            None
        },
    )
}

fn attach_after_history(term: &AgentTerminal, file: &Path, warm: &str) -> TurnReceiver {
    let history_bytes = rendered_turn(term, file, warm);
    let rx = wait("fresh reader", || {
        match term.attach_claude_turns(TurnOptions::default()) {
            Ok(rx) => Some(rx),
            Err(AttachError::Busy) => None,
            Err(other) => panic!("attachment failed: {other:?}"),
        }
    });
    wait("verified EOF after warm-up", || match rx.status() {
        TurnStatus::Waiting => None,
        TurnStatus::Active { start, .. } => {
            assert!(start >= history_bytes);
            Some(())
        }
        other => panic!("fresh reader failed: {other:?}"),
    });
    assert!(rx.try_recv().is_none());
    rx
}

fn expect_reply(term: &AgentTerminal, rx: &TurnReceiver, name: &str) {
    let expected = format!("Fixture response {name}");
    let reply = wait(&format!("native receiver reply for {name}"), || {
        assert!(
            matches!(rx.status(), TurnStatus::Active { .. }),
            "reader lost its binding: {:?}",
            rx.status()
        );
        rx.try_recv()
    });
    assert_eq!(reply.text, expected);
    assert!(!reply.truncated);
    wait(&format!("rendered reply for {name}"), || {
        visible(term, &expected).then_some(())
    });
    assert!(rx.try_recv().is_none());
}

#[test]
#[ignore = "python3 scripts/check_claude_transcripts.py supplies the isolated native CLI and peer"]
fn native_lease_turns_and_shared_resume() {
    let _spawn = common::spawn_lock();
    let f = Fixture {
        root: std::env::var_os("AGENT_LINK_CLAUDE_FIXTURE")
            .expect("run the Python harness")
            .into(),
        url: std::env::var("AGENT_LINK_CLAUDE_PEER").unwrap(),
    };
    let ids = fs::read_to_string(f.root.join("ids")).unwrap();
    let [id_a, id_b, id_overlap]: [&str; 3] = ids.lines().collect::<Vec<_>>().try_into().unwrap();
    assert_ne!(id_a, id_b);
    let lease = ExclusiveClaudeRoot::new(f.root.join("config")).unwrap();
    let a = f.spawn("--session-id", id_a, Some(&lease)).unwrap();
    let b = f.spawn("--session-id", id_b, Some(&lease)).unwrap();
    assert_ne!(a.terminal().pid(), b.terminal().pid());
    for (term, id) in [(&a, id_a), (&b, id_b)] {
        wait("native ready prompt", || visible(term, "❯").then_some(()));
        wait("launch UUID in the direct child's writer record", || {
            f.writer(term).filter(|writer| writer["sessionId"] == id)
        });
    }
    for flag in ["--session-id", "--resume"] {
        assert!(matches!(
            f.spawn(flag, id_a, Some(&lease)),
            Err(Error::InvalidInput(_))
        ));
    }

    let rx_a = attach_after_history(&a, &f.transcript(id_a), "warm-a");
    let rx_b = attach_after_history(&b, &f.transcript(id_b), "warm-b");

    // The local peer holds the first request in each pair until the other
    // terminal's rendered reply has reached its public receiver.
    for (slow, slow_rx, slow_name, fast, fast_rx, fast_name) in [
        (&a, &rx_a, "a-one", &b, &rx_b, "b-one"),
        (&b, &rx_b, "b-two", &a, &rx_a, "a-two"),
    ] {
        prompt(slow, slow_name);
        prompt(fast, fast_name);
        expect_reply(fast, fast_rx, fast_name);
        fs::write(f.root.join(format!("received-{fast_name}")), []).unwrap();
        expect_reply(slow, slow_rx, slow_name);
        assert!(!visible(slow, &format!("Fixture response {fast_name}")));
        assert!(!visible(fast, &format!("Fixture response {slow_name}")));
    }

    submit(&a, "/clear");
    wait("session-change gap after /clear", || match rx_a.status() {
        TurnStatus::Gap(TurnGap::SessionChanged) => Some(()),
        TurnStatus::Active { .. } => None,
        other => panic!("unexpected status after /clear: {other:?}"),
    });
    assert!(rx_a.try_recv().is_none());
    let next = wait("new UUID in the original PID's writer record", || {
        f.writer(&a).and_then(|w| {
            w["sessionId"]
                .as_str()
                .filter(|id| *id != id_a)
                .map(str::to_owned)
        })
    });
    assert_ne!(next, id_b);
    let fresh = attach_after_history(&a, &f.transcript(&next), "warm-clear");
    prompt(&a, "after-clear");
    expect_reply(&a, &fresh, "after-clear");
    assert_eq!(rx_a.status(), TurnStatus::Gap(TurnGap::SessionChanged));
    assert!(rx_a.try_recv().is_none());
    assert!(matches!(rx_b.status(), TurnStatus::Active { .. }));
    assert!(rx_b.try_recv().is_none());
    // The commit offset advances in the same batch as the pending reply.
    // Leave the reply unread until the host has withdrawn its writer guarantee.
    let file_b = f.transcript(id_b);
    let queued = rendered_turn(&b, &file_b, "before-revoke");
    wait("reader committed the unread reply", || {
        match rx_b.status() {
            TurnStatus::Active { offset, .. } => (offset >= queued).then_some(()),
            other => panic!("lost B before revocation: {other:?}"),
        }
    });
    lease.revoke();
    for rx in [&fresh, &rx_b] {
        assert!(rx.try_recv().is_none());
        assert_eq!(rx.status(), TurnStatus::Gap(TurnGap::AuthorityRevoked));
    }
    assert!(matches!(
        b.attach_claude_turns(TurnOptions::default()),
        Err(AttachError::AuthorityRevoked)
    ));
    assert!(matches!(
        f.spawn("--session-id", id_overlap, Some(&lease)),
        Err(Error::InvalidInput(_))
    ));
    rendered_turn(&b, &file_b, "after-revoke");
    assert!(rx_b.try_recv().is_none());
    drop((fresh, rx_a, rx_b, a, b, lease));

    assert_shared_resume_unavailable(&f, id_overlap);
}

fn assert_shared_resume_unavailable(f: &Fixture, id: &str) {
    // Ordinary launches can write the same session concurrently. Exercise the
    // actual resume behavior and the unavailable capability on both terminals.
    let original = f.spawn("--session-id", id, None).unwrap();
    let shared = f.transcript(id);
    wait("ordinary terminal ready", || {
        visible(&original, "❯").then_some(())
    });
    rendered_turn(&original, &shared, "overlap-start");
    let resumed = f.spawn("--resume", id, None).unwrap();
    assert_ne!(original.terminal().pid(), resumed.terminal().pid());
    for term in [&original, &resumed] {
        wait("both writer records select the same conversation", || {
            f.writer(term).filter(|writer| writer["sessionId"] == id)
        });
        assert!(matches!(
            term.attach_claude_turns(TurnOptions::default()),
            Err(AttachError::NoLaunch)
        ));
    }
    wait("resumed terminal ready", || {
        visible(&resumed, "❯").then_some(())
    });
    rendered_turn(&resumed, &shared, "overlap-resumed");
    rendered_turn(&original, &shared, "overlap-original");
    assert!(!visible(&original, "Fixture response overlap-resumed"));
    assert!(!visible(&resumed, "Fixture response overlap-original"));
}
