//! The built-in profiles' best-effort ambient sources on real PTYs: the fake agent is the
//! direct child of a terminal spawned with the built-in profile (Claude, Codex, Pi or
//! Cursor), the test plays that CLI's transcript writer under a temporary configuration
//! root, and `attach_ambient_turns` follows it through the profile's own source. The
//! transcripts replay the recorded fixtures under `tests/fixtures/transcripts/`. No real
//! agent CLI runs, and every configuration root is a temporary directory set through the
//! child's environment.
#![cfg(feature = "turns")]

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_link::agent::profiles::{
    ClaudeProfile, CodexProfile, CursorProfile, OpencodeProfile, PiProfile,
};
use agent_link::agent::turns::ambient::{
    AmbientClosed, AmbientEvidence, AmbientOptions, AmbientReceiver, AmbientStatus, AmbientTurn,
    AmbientUnavailable, AmbientWaiting,
};
use agent_link::agent::{AgentProfile, AgentSpec, AgentTerminal, ProfileRegistry};
use agent_link::{PtySize, SpawnSpec};
use common::agent::spawn_agent;
use common::fake_agent_path;
use tempfile::TempDir;

/// The CLIs' recorded project path in the fixtures.
const FIXTURE_CWD: &str = "/fixture/project";

/// A built-in profile's terminal whose direct child is the fake agent, in a temporary
/// project, with `vars` set to temporary directories (created) in its environment.
struct Agent {
    term: AgentTerminal,
    project: PathBuf,
    dirs: Vec<PathBuf>,
    _tmp: TempDir,
}

impl Agent {
    fn start(profile: Arc<dyn AgentProfile>, vars: &[(&str, &str)]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let project = std::fs::canonicalize(&project).unwrap();
        let mut spec = SpawnSpec::direct(
            fake_agent_path(),
            ["ready"],
            &project,
            PtySize { rows: 24, cols: 80 },
        );
        spec.env = common::test_env(&home);
        // Never this process's own configuration overrides: only what the test sets.
        for key in [
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "PI_CODING_AGENT_SESSION_DIR",
            "PI_CODING_AGENT_DIR",
            "CURSOR_DATA_DIR",
        ] {
            spec.env.remove(key);
        }
        let mut dirs = Vec::new();
        for (key, dir) in vars {
            let dir = tmp.path().join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            spec.env.remove.retain(|removed| removed != key);
            spec.env.set(*key, dir.as_os_str());
            dirs.push(dir);
        }
        // Agent events are not read here: the receiver is dropped.
        let (term, _) = spawn_agent(spec, AgentSpec::new(profile));
        Self {
            term,
            project,
            dirs,
            _tmp: tmp,
        }
    }

    fn attach(&self, options: AmbientOptions) -> AmbientReceiver {
        self.term.attach_ambient_turns(options).unwrap()
    }

    fn pid(&self) -> u32 {
        self.term.terminal().pid().expect("the direct child's pid")
    }

    /// The fixture `name`'s lines, recorded in this project.
    fn fixture(&self, name: &str) -> Vec<String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/transcripts")
            .join(name);
        let project = serde_json::to_string(&self.project).unwrap();
        let project = project.trim_matches('"');
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.replace(FIXTURE_CWD, project))
            .collect()
    }
}

fn options() -> AmbientOptions {
    let mut options = AmbientOptions::default();
    options.poll = Duration::from_millis(20);
    options.quiet = Duration::from_secs(600);
    options
}

fn append(path: &Path, lines: &[String]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
}

/// Replaces the file's content in place (same file, truncated), as cursor rewrites it.
fn rewrite(path: &Path, lines: &[String]) {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .unwrap();
    file.write_all((lines.join("\n") + "\n").as_bytes())
        .unwrap();
}

fn status_until(rx: &AmbientReceiver, what: &str, mut want: impl FnMut(AmbientStatus) -> bool) {
    let deadline = Instant::now() + common::SHELL_START_WAIT;
    loop {
        let status = rx.status();
        if want(status) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; status {status:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn unavailable(rx: &AmbientReceiver, reason: AmbientUnavailable) {
    status_until(rx, &format!("{reason:?}"), |s| {
        s == AmbientStatus::Waiting(AmbientWaiting::Unavailable(reason))
    });
}

fn active(rx: &AmbientReceiver) {
    status_until(rx, "an active reader", |s| {
        matches!(s, AmbientStatus::Active { .. })
    });
}

fn next_turn(rx: &AmbientReceiver) -> AmbientTurn {
    let deadline = Instant::now() + common::SHELL_START_WAIT;
    loop {
        if let Some(turn) = rx.try_recv() {
            return turn;
        }
        assert!(
            Instant::now() < deadline,
            "no turn; status {:?}",
            rx.status()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The next turn's reply text (`None` without one) and evidence.
fn next(rx: &AmbientReceiver) -> (Option<String>, AmbientEvidence) {
    let turn = next_turn(rx);
    (turn.reply.map(|reply| reply.text), turn.evidence)
}

fn explicit(text: &str) -> (Option<String>, AmbientEvidence) {
    (Some(text.into()), AmbientEvidence::ExplicitEnd)
}

/// No further turn arrives within a few wakes.
fn nothing_more(rx: &AmbientReceiver) {
    std::thread::sleep(Duration::from_millis(200));
    assert!(rx.try_recv().is_none(), "status {:?}", rx.status());
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The directory name claude and cursor build from a path: every char outside
/// `[A-Za-z0-9]` made `-`.
fn dashed(path: &Path) -> String {
    path.to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[test]
fn each_builtin_cli_profile_offers_an_ambient_source() {
    let registry = ProfileRegistry::builtin();
    for id in ["claude", "codex", "pi", "cursor"] {
        assert!(registry.get(id).unwrap().ambient_turns().is_some(), "{id}");
    }
    // opencode's turns come from its server, not a transcript file.
    assert!(OpencodeProfile.ambient_turns().is_none());
}

/// claude: the session named by the agent's pid file, explicit ends — the controlled
/// capture's turns, its Esc interruption included — and `/clear`'s new session.
#[test]
fn claude_turns_follow_the_agents_session_and_end_explicitly() {
    let agent = Agent::start(
        Arc::new(ClaudeProfile::new()),
        &[("CLAUDE_CONFIG_DIR", "claude")],
    );
    let root = agent.dirs[0].clone();
    let rx = agent.attach(options());
    unavailable(&rx, AmbientUnavailable::NotFound);
    let pid_file = root.join("sessions").join(format!("{}.json", agent.pid()));
    let session = |id: &str| {
        let record = serde_json::json!({
            "pid": agent.pid(), "sessionId": id, "cwd": agent.project, "startedAt": now_ms(),
            "kind": "interactive", "entrypoint": "cli",
        });
        std::fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
        std::fs::write(&pid_file, record.to_string()).unwrap();
    };
    session("00000000-0000-4000-8000-000000000001");
    // The project's directory appears with claude's first message.
    unavailable(&rx, AmbientUnavailable::MissingDirectory);
    let dir = root.join("projects").join(dashed(&agent.project));
    std::fs::create_dir_all(&dir).unwrap();
    active(&rx);
    let first = dir.join("00000000-0000-4000-8000-000000000001.jsonl");
    append(&first, &agent.fixture("claude-2.1.296/session-a.jsonl"));
    assert_eq!(next(&rx), explicit("Copper pebble"));
    assert_eq!(next(&rx), (None, AmbientEvidence::ExplicitEnd), "Esc");
    assert_eq!(next(&rx), explicit("Amber petal\n\nSecond petal"));
    // `/clear`: the pid file names a new session.
    session("00000000-0000-4000-8000-000000000003");
    let cleared = dir.join("00000000-0000-4000-8000-000000000003.jsonl");
    append(&cleared, &agent.fixture("claude-2.1.296/after-clear.jsonl"));
    assert_eq!(next(&rx), explicit("Lime button"));
    // The session left is not followed any more.
    append(&first, &agent.fixture("claude-2.1.296/session-b.jsonl"));
    nothing_more(&rx);
}

/// The UTC date directory (`YYYY/MM/DD`) of now (Howard Hinnant's `civil_from_days`).
fn today() -> PathBuf {
    let days = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    PathBuf::from(format!("{year:04}/{month:02}/{day:02}"))
}

/// codex: the project's interactive rollout of today, explicit ends; rollouts of a
/// subagent and of `codex exec` in the same project are never followed.
#[test]
fn codex_turns_come_from_the_interactive_rollout_only() {
    let agent = Agent::start(Arc::new(CodexProfile), &[("CODEX_HOME", "codex")]);
    let day = agent.dirs[0].join("sessions").join(today());
    let rx = agent.attach(options());
    unavailable(&rx, AmbientUnavailable::NotFound);
    let lines = agent.fixture("ambient/codex-0.154.0/turns.jsonl");
    let ours = day.join("rollout-2026-10-11T19-00-28-01a12c56.jsonl");
    append(&ours, &lines[..1]);
    active(&rx);
    // Others of the project, written later: a subagent's and `codex exec`'s.
    for (name, source, thread) in [
        (
            "sub",
            r#"{"subagent":{"thread_spawn":{"parent_thread_id":"p"}}}"#,
            "subagent",
        ),
        ("exec", r#""exec""#, "user"),
    ] {
        let meta = lines[0]
            .replacen(r#""source":"cli""#, &format!(r#""source":{source}"#), 1)
            .replacen(
                r#""thread_source":"user""#,
                &format!(r#""thread_source":"{thread}""#),
                1,
            );
        assert_ne!(meta, lines[0]);
        append(
            &day.join(format!("rollout-{name}.jsonl")),
            &[meta, lines[7].clone()],
        );
    }
    append(&ours, &lines[1..]);
    assert_eq!(next(&rx), explicit("first answer"));
    assert_eq!(next(&rx), (None, AmbientEvidence::ExplicitEnd), "Esc");
    nothing_more(&rx);
}

#[test]
fn a_replaced_rollout_cannot_publish_foreign_turns_on_switch() {
    let agent = Agent::start(Arc::new(CodexProfile), &[("CODEX_HOME", "codex")]);
    let day = agent.dirs[0].join("sessions").join(today());
    let rx = agent.attach(options());
    unavailable(&rx, AmbientUnavailable::NotFound);
    let path = day.join("rollout-old.jsonl");
    let lines = agent.fixture("ambient/codex-0.154.0/turns.jsonl");
    append(&path, &lines[..=7]);
    assert_eq!(next(&rx), explicit("first answer"));

    // Preserve the old answer and its predecessor so the rewrite anchor still matches.
    let mut foreign = lines[..=7].to_vec();
    let mut header: serde_json::Value = serde_json::from_str(&foreign[0]).unwrap();
    #[cfg(unix)]
    let other = agent.project.with_file_name(r"project\");
    #[cfg(windows)]
    let other = agent.project.with_file_name("other-project");
    std::fs::create_dir(&other).unwrap();
    header["payload"]["cwd"] = serde_json::json!(other);
    foreign[0] = header.to_string();
    foreign.push(lines[7].replace("first answer", "foreign reply"));
    let replacement = day.join("replacement");
    append(&replacement, &foreign);
    std::fs::rename(replacement, &path).unwrap();
    unavailable(&rx, AmbientUnavailable::NotFound);

    let next_path = day.join("rollout-new.jsonl");
    append(&next_path, &lines[..1]);
    active(&rx);
    append(
        &next_path,
        &[lines[7].replace("first answer", "own next reply")],
    );
    assert_eq!(next(&rx), explicit("own next reply"));
    nothing_more(&rx);
}

/// pi: no end record — a final message ends its turn once the session stays quiet (a tool
/// call within the window takes it back), or at the next prompt.
#[test]
fn pi_turns_are_inferred_after_quiet_or_at_the_next_prompt() {
    let agent = Agent::start(
        Arc::new(PiProfile::new()),
        &[("PI_CODING_AGENT_SESSION_DIR", "pi-sessions")],
    );
    let mut options = options();
    options.quiet = Duration::from_millis(400);
    let rx = agent.attach(options);
    unavailable(&rx, AmbientUnavailable::NotFound);
    let lines = agent.fixture("ambient/pi-0.85.1/turns.jsonl");
    let at = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
    let file = agent.dirs[0].join("2026-10-11T20-05-23-116Z_01a12c91.jsonl");
    // Turn one up to its final message, then a tool call within the window.
    let first = at(r#""text":"first answer""#);
    let written = Instant::now();
    append(&file, &lines[..=first]);
    active(&rx);
    let tool = at(r#""stopReason":"toolUse""#);
    append(&file, &lines[tool..=tool + 1]);
    // The final message after the tool, then quiet.
    append(&file, &lines[tool + 2..=tool + 2]);
    let (reply, evidence) = next(&rx);
    assert!(written.elapsed() >= options.quiet);
    assert_eq!(reply.as_deref(), Some("after tool"));
    assert_eq!(evidence, AmbientEvidence::InferredAfterQuiet);
    // A final message the next prompt follows within the window.
    let mut quick = agent.fixture("pi-1.5.2/turns.jsonl");
    quick.remove(0);
    append(&file, &quick[4..=6]);
    assert_eq!(
        next(&rx),
        (
            Some("pi-proposal-reply-1".into()),
            AmbientEvidence::InferredFromNextRecord
        )
    );
    assert_eq!(
        next(&rx),
        (
            Some("pi-proposal-reply-2".into()),
            AmbientEvidence::InferredAfterQuiet
        )
    );
    nothing_more(&rx);
}

#[test]
fn pi_finds_its_first_transcript_during_the_exit_drain() {
    let agent = Agent::start(
        Arc::new(PiProfile::new()),
        &[("PI_CODING_AGENT_SESSION_DIR", "pi-sessions")],
    );
    let mut options = options();
    options.poll = Duration::from_secs(60);
    let rx = agent.attach(options);
    unavailable(&rx, AmbientUnavailable::NotFound);

    let lines = agent.fixture("ambient/pi-0.85.1/turns.jsonl");
    let first = lines
        .iter()
        .position(|l| l.contains(r#""stopReason":"stop""#))
        .unwrap();
    let path = agent.dirs[0].join("first.jsonl");
    append(&path, &lines[..=first]);
    agent.term.interrupt().unwrap();
    common::wait_until("the direct child's exit", || {
        agent.term.terminal().try_wait().is_some()
    });
    drop(agent.term); // Wake the worker before its next scheduled lookup.
    status_until(&rx, "the exit drain", |s| s.is_closed());
    assert_eq!(
        rx.status(),
        AmbientStatus::Closed(AmbientClosed::TerminalEnded)
    );
    // A filesystem without birth times baselines newly found transcripts at EOF.
    if std::fs::metadata(path).unwrap().created().is_ok() {
        let turn = rx.try_recv().expect("first Pi reply");
        assert_eq!(turn.reply.unwrap().text, "first answer");
        assert_eq!(turn.evidence, AmbientEvidence::InferredAtTranscriptEnd);
    }
    assert!(rx.try_recv().is_none());
}

#[test]
fn pi_drains_the_known_session_then_the_session_selected_at_exit() {
    let agent = Agent::start(
        Arc::new(PiProfile::new()),
        &[("PI_CODING_AGENT_SESSION_DIR", "pi-sessions")],
    );
    let lines = agent.fixture("ambient/pi-0.85.1/turns.jsonl");
    let first = lines
        .iter()
        .position(|l| l.contains(r#""stopReason":"stop""#))
        .unwrap();
    let old = agent.dirs[0].join("a.jsonl");
    let new = agent.dirs[0].join("b.jsonl");
    append(&old, &lines[..1]);
    let mut options = options();
    options.poll = Duration::from_secs(60);
    let rx = agent.attach(options);
    active(&rx);
    append(&old, &lines[1..=first]);
    let switched: Vec<_> = lines[..=first]
        .iter()
        .map(|line| line.replace("first answer", "switched answer"))
        .collect();
    append(&new, &switched);
    drop(agent.term);
    status_until(&rx, "the exit drain", |s| s.is_closed());
    assert_eq!(
        rx.status(),
        AmbientStatus::Closed(AmbientClosed::TerminalEnded)
    );
    assert_eq!(
        next(&rx),
        (
            Some("first answer".into()),
            AmbientEvidence::InferredAtTranscriptEnd
        )
    );
    if std::fs::metadata(new).unwrap().created().is_ok() {
        assert_eq!(
            next(&rx),
            (
                Some("switched answer".into()),
                AmbientEvidence::InferredAtTranscriptEnd
            )
        );
    }
    assert!(rx.try_recv().is_none());
}

/// cursor: the conversation created since the launch, rewritten in place at every turn.
/// The profile's source reports completion only; the opt-in source the unseparated text.
/// Each turn is reported once across the rewrites; the aborted one not at all.
#[test]
fn cursor_turns_survive_rewrites_and_keep_text_only_when_opted_in() {
    let agent = Agent::start(Arc::new(CursorProfile), &[("CURSOR_DATA_DIR", "cursor")]);
    let completion = agent.attach(options());
    let texts = agent
        .term
        .attach_ambient_turns_with(
            CursorProfile.ambient_turns_with_unseparated_text(),
            options(),
        )
        .unwrap();
    unavailable(&completion, AmbientUnavailable::NotFound);
    let slug = dashed(&agent.project)
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let id = "0b5b8a0e-5f1f-4b7e-9a51-6d0d4f1f0c11";
    let file = agent.dirs[0]
        .join("projects")
        .join(slug)
        .join("agent-transcripts")
        .join(id)
        .join(format!("{id}.jsonl"));
    let image = |name: &str| agent.fixture(&format!("ambient/cursor-2026.10.01/{name}"));
    append(&file, &image("turns.jsonl"));
    assert_eq!(next(&texts), explicit("first answer"));
    assert_eq!(next(&completion), (None, AmbientEvidence::ExplicitEnd));
    rewrite(&file, &image("turns.rewritten.jsonl"));
    assert_eq!(next(&texts), explicit("second answer"));
    assert_eq!(next(&completion), (None, AmbientEvidence::ExplicitEnd));
    rewrite(&file, &image("turns.aborted.jsonl"));
    nothing_more(&texts);
    rewrite(&file, &image("turns.tool.jsonl"));
    assert_eq!(next(&texts), explicit("after tool"));
    assert_eq!(next(&completion), (None, AmbientEvidence::ExplicitEnd));
    nothing_more(&texts);
    nothing_more(&completion);
}
