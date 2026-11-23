//! Best-effort ambient questions on real PTYs. The fake agent's `question-dialog` scene
//! draws a question on demand (its profile reads it off the screen); the test plays the
//! CLI's transcript writer under a temporary configuration root, and the built-in Claude
//! or Codex ambient source reads it. A question is relayed only while the screen shows
//! the transcript's question; an answer types the profile's keys into the PTY. For
//! OpenCode the fake agent serves the synthetic OpenCode-like API (questions included)
//! from the launched process, and the ambient question feed reads its TUI server. No
//! real agent CLI runs, and no user configuration directory is read.
#![cfg(feature = "questions")]

mod common;
#[path = "common/http_stub.rs"]
mod http_stub;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_link::agent::profiles::{ClaudeProfile, CodexProfile, OpencodeProfile};
use agent_link::agent::questions::opencode::OpenCodeAmbientQuestions;
use agent_link::agent::questions::{Answer, AnswerError, ClosedBy, QuestionId, QuestionItem};
use agent_link::agent::turns::ambient::{
    AmbientEndpoint, AmbientOptions, AmbientReceiver, AmbientSource, AmbientStatus, ambient_workers,
};
use agent_link::agent::{
    AgentProfile, AgentSpec, AgentState, AgentTerminal, Dialog, LaunchPlan, Permissions, ShellKind,
    TrustPolicy,
};
use agent_link::{Event, PtySize, SpawnSpec};
use common::agent::{spawn_agent, states_until};
use common::fake::{FakeProfile, answer_question, open_question};
use common::{LAUNCH_WAIT, WAIT, fake_agent_path, wait_until};
use crossbeam_channel::{Receiver, bounded};
use serde_json::json;
use tempfile::TempDir;

/// The question the `question-dialog` scene draws.
const PROMPT: &str = "Continue the fixture?";

/// One test at a time, each after the previous test's ambient workers retired.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    wait_until("ambient workers to retire", || ambient_workers() == 0);
    guard
}

fn options() -> AmbientOptions {
    let mut options = AmbientOptions::default();
    options.poll = Duration::from_millis(20);
    options.quiet = Duration::from_secs(600);
    options
}

fn opened(events: &Receiver<Event>) -> (QuestionId, Arc<[QuestionItem]>) {
    let deadline = Instant::now() + LAUNCH_WAIT;
    loop {
        if let Event::Question { qid, questions, .. } =
            events.recv_deadline(deadline).expect("a question opened")
        {
            return (qid, questions);
        }
    }
}

fn closed(events: &Receiver<Event>, id: QuestionId) -> ClosedBy {
    let deadline = Instant::now() + LAUNCH_WAIT;
    loop {
        match events.recv_deadline(deadline).expect("the question closed") {
            Event::QuestionClosed { qid, by, .. } => {
                assert_eq!(qid, id);
                return by;
            }
            Event::Question { .. } => panic!("another question opened first"),
            _ => {}
        }
    }
}

/// The observer reads the screen `n` more times (it does only while the transcript shows a
/// question) and opens nothing meanwhile.
fn withheld(events: &Receiver<Event>, reads: &Receiver<()>, n: usize) {
    while reads.try_recv().is_ok() {}
    for _ in 0..n {
        reads.recv_timeout(WAIT).expect("a screen read");
    }
    assert!(
        !events
            .try_iter()
            .any(|e| matches!(e, Event::Question { .. })),
        "a question opened"
    );
}

fn the_question(items: &[QuestionItem], free_text: bool) {
    assert_eq!(
        items,
        [QuestionItem::new("Fake", PROMPT, vec!["Continue".into()]).with_free_text(free_text)]
    );
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

fn next_reply(rx: &AmbientReceiver) -> Option<String> {
    let deadline = Instant::now() + common::SHELL_START_WAIT;
    loop {
        if let Some(turn) = rx.try_recv() {
            return turn.reply.map(|reply| reply.text);
        }
        assert!(
            Instant::now() < deadline,
            "no turn; status {:?}",
            rx.status()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn active(rx: &AmbientReceiver) {
    status_until(rx, "an active reader", |s| {
        matches!(s, AmbientStatus::Active { .. })
    });
}

/// The fake agent's terminal, in a temporary project, with a configuration root.
struct Agent {
    term: AgentTerminal,
    events: Receiver<Event>,
    reads: Receiver<()>,
    project: PathBuf,
    root: PathBuf,
    input: PathBuf,
    _tmp: TempDir,
}

/// Configuration overrides of this process never reach the agent; only `var` is set.
fn isolated_env(spec: &mut SpawnSpec, home: &Path, var: &str, root: &Path) {
    spec.env = common::test_env(home);
    for key in ["CLAUDE_CONFIG_DIR", "CODEX_HOME"] {
        spec.env.remove(key);
    }
    spec.env.remove.retain(|removed| removed != var);
    spec.env.set(var, root.as_os_str());
}

impl Agent {
    fn dirs() -> (TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let home = tmp.path().join("home");
        let root = tmp.path().join("config");
        for dir in [&project, &home, &root] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let project = std::fs::canonicalize(&project).unwrap();
        (tmp, project, home, root)
    }

    /// The `question-dialog` scene as the direct child of a terminal read by `fake`, with
    /// `var` (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`) set to a temporary root.
    fn direct(fake: FakeProfile, var: &str) -> Self {
        let (tmp, project, home, root) = Self::dirs();
        let input = tmp.path().join("input.bin");
        let (tx, reads) = bounded(256);
        let mut spec = SpawnSpec::direct(
            fake_agent_path(),
            ["question-dialog", "--input-log", input.to_str().unwrap()],
            &project,
            PtySize { rows: 24, cols: 80 },
        );
        isolated_env(&mut spec, &home, var, &root);
        let profile = fake.with_question_reads(tx);
        let (term, events) = spawn_agent(
            spec,
            AgentSpec {
                trust: TrustPolicy::unattended(),
                ..AgentSpec::new(Arc::new(profile))
            },
        );
        states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
        Self {
            term,
            events,
            reads,
            project,
            root,
            input,
            _tmp: tmp,
        }
    }

    fn attach(&self, source: Arc<dyn AmbientSource>) -> AmbientReceiver {
        self.term
            .attach_ambient_turns_with(source, options())
            .unwrap()
    }

    fn input(&self) -> Vec<u8> {
        std::fs::read(&self.input).unwrap_or_default()
    }
}

fn fake() -> FakeProfile {
    FakeProfile::new("question-dialog", &[])
}

// --- Codex ------------------------------------------------------------------------------

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

/// A Codex TUI rollout of `project` under `codex_home`, its `session_meta` written.
fn rollout(codex_home: &Path, project: &Path) -> PathBuf {
    let path = codex_home
        .join("sessions")
        .join(today())
        .join("rollout-2026-11-19T10-00-00-fixture.jsonl");
    let meta = json!({"type": "session_meta", "payload": {
        "id": "fixture", "cwd": project, "originator": "codex-tui", "cli_version": "0.154.0",
        "source": "cli", "thread_source": "user"}});
    append(&path, &[meta.to_string()]);
    path
}

fn codex_event(kind: &str) -> String {
    let mut payload = json!({"type": kind, "turn_id": "turn"});
    if kind == "task_complete" {
        payload["last_agent_message"] = json!("Done.");
    }
    json!({"type": "event_msg", "payload": payload}).to_string()
}

/// A Plan-mode `request_user_input` call asking `prompt` with the one choice "Continue"
/// and no notes, as the scene's dialog shows it.
fn codex_call(id: &str, prompt: &str) -> String {
    let arguments = json!({"questions": [{"header": "Fake", "id": "q", "question": prompt,
        "isOther": false, "options": [{"label": "Continue", "description": "Go on."}]}]});
    json!({"type": "response_item", "payload": {"type": "function_call",
        "name": "request_user_input", "arguments": arguments.to_string(), "call_id": id}})
    .to_string()
}

fn codex_output(id: &str) -> String {
    json!({"type": "response_item", "payload": {"type": "function_call_output",
        "call_id": id, "output": "{\"answers\":{\"q\":{\"answers\":[\"Continue\"]}}}"}})
    .to_string()
}

fn codex() -> Arc<dyn AmbientSource> {
    CodexProfile.ambient_turns().unwrap()
}

/// A transcript question is relayed only once the screen shows the same question; the
/// host's answer types the profile's keys into the PTY, and the question closes `Host`
/// once the dialog left. Nothing reached the PTY but the test's Up and the answer.
#[test]
fn a_codex_question_is_relayed_while_the_screen_agrees_and_answered_with_keys() {
    let _serial = serial();
    let agent = Agent::direct(fake(), "CODEX_HOME");
    let rx = agent.attach(codex());
    let path = rollout(&agent.root, &agent.project);
    active(&rx);
    agent
        .term
        .observe_ambient_transcript_questions(&rx)
        .unwrap();
    append(
        &path,
        &[codex_event("task_started"), codex_call("call_1", PROMPT)],
    );
    // The transcript asks; the screen shows no dialog yet.
    withheld(&agent.events, &agent.reads, 3);
    open_question(agent.term.terminal());
    let (qid, items) = opened(&agent.events);
    the_question(&items, false);
    let ticket = agent.term.answer_question(qid, vec![Answer::Choice(0)]);
    assert_eq!(ticket.wait(WAIT), Some(Ok(())));
    common::screen_contains(agent.term.terminal(), "question answered", WAIT);
    append(
        &path,
        &[codex_output("call_1"), codex_event("task_complete")],
    );
    assert_eq!(closed(&agent.events, qid), ClosedBy::Host);
    assert_eq!(agent.input(), b"\x1b[A1");
    // The attachment's replies go on as before.
    assert_eq!(next_reply(&rx).as_deref(), Some("Done."));
}

/// A transcript question the screen does not show (another prompt) is never relayed. Once
/// the transcript asks the question on screen it is; answered at the terminal, it closes
/// `Desk`.
#[test]
fn a_question_the_screen_does_not_show_is_withheld_and_a_desk_answer_closes_it() {
    let _serial = serial();
    let agent = Agent::direct(fake(), "CODEX_HOME");
    let rx = agent.attach(codex());
    let path = rollout(&agent.root, &agent.project);
    active(&rx);
    agent
        .term
        .observe_ambient_transcript_questions(&rx)
        .unwrap();
    open_question(agent.term.terminal());
    append(
        &path,
        &[
            codex_event("task_started"),
            codex_call("call_1", "Something else?"),
        ],
    );
    withheld(&agent.events, &agent.reads, 5);
    // Answered, then asked again with the question on screen.
    append(
        &path,
        &[
            codex_output("call_1"),
            codex_event("task_complete"),
            codex_event("task_started"),
            codex_call("call_2", PROMPT),
        ],
    );
    let (qid, items) = opened(&agent.events);
    the_question(&items, false);
    answer_question(agent.term.terminal());
    assert_eq!(closed(&agent.events, qid), ClosedBy::Desk);
    assert_eq!(
        agent
            .term
            .answer_question(qid, vec![Answer::Choice(0)])
            .try_result(),
        Some(Err(AnswerError::NotOpen))
    );
}

/// Stopping (or dropping) the receiver ends the source's authority: its question closes
/// `Gone`. A new attachment starts from now: the call it did not read is not asked until
/// asked again. The agent's exit closes the question `Gone` too.
#[test]
fn stopping_the_receiver_or_the_agent_s_exit_closes_the_question_gone() {
    let _serial = serial();
    let agent = Agent::direct(fake(), "CODEX_HOME");
    let rx = agent.attach(codex());
    let path = rollout(&agent.root, &agent.project);
    active(&rx);
    agent
        .term
        .observe_ambient_transcript_questions(&rx)
        .unwrap();
    open_question(agent.term.terminal());
    append(
        &path,
        &[codex_event("task_started"), codex_call("call_1", PROMPT)],
    );
    let (first, _) = opened(&agent.events);
    rx.stop();
    assert_eq!(closed(&agent.events, first), ClosedBy::Gone);
    assert_eq!(
        agent
            .term
            .answer_question(first, vec![Answer::Choice(0)])
            .try_result(),
        Some(Err(AnswerError::NotOpen))
    );
    assert!(matches!(
        agent.term.observe_ambient_transcript_questions(&rx),
        Err(agent_link::Error::InvalidInput(_))
    ));

    let rx = agent.attach(codex());
    active(&rx);
    agent
        .term
        .observe_ambient_transcript_questions(&rx)
        .unwrap();
    append(
        &path,
        &[
            codex_output("call_1"),
            codex_event("task_started"),
            codex_call("call_2", PROMPT),
        ],
    );
    let (second, _) = opened(&agent.events);
    assert_ne!(first, second);
    // ^C: the fake exits.
    agent.term.terminal().write(b"\x03").unwrap();
    assert_eq!(closed(&agent.events, second), ClosedBy::Gone);
    drop(rx);
}

/// Receivers of another terminal, closed ones and feed attachments are refused.
#[test]
fn only_a_live_transcript_attachment_of_this_terminal_is_taken() {
    let _serial = serial();
    let agent = Agent::direct(fake(), "CODEX_HOME");
    let other = Agent::direct(fake(), "CODEX_HOME");
    let rx = other.attach(codex());
    assert!(matches!(
        agent.term.observe_ambient_transcript_questions(&rx),
        Err(agent_link::Error::InvalidInput(_))
    ));
    let feed = agent
        .term
        .attach_ambient_feed_with(OpencodeProfile.ambient_feed().unwrap(), options())
        .unwrap();
    assert!(matches!(
        agent.term.observe_ambient_transcript_questions(&feed),
        Err(agent_link::Error::Unsupported(_))
    ));
    // Spawned without an ambient feed: no OpenCode question feed either.
    assert!(matches!(
        OpenCodeAmbientQuestions::new(&agent.term, "ses_stub"),
        Err(agent_link::Error::InvalidInput(_))
    ));
}

// --- Claude -----------------------------------------------------------------------------

fn dashed(path: &Path) -> String {
    path.to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn claude_line(kind: &str, content: serde_json::Value) -> String {
    json!({"type": kind, "isSidechain": false, "message": {"role": kind, "content": content}})
        .to_string()
}

/// claude: the session its pid file names, a prompt, then an `AskUserQuestion` call; the
/// scene's question offers a typed answer like claude's single-choice dialog. Relayed,
/// answered with keys, closed `Host` after the result.
#[test]
fn a_claude_question_of_the_agent_s_session_is_relayed_and_answered() {
    let _serial = serial();
    let agent = Agent::direct(fake().with_question_free_text(), "CLAUDE_CONFIG_DIR");
    let rx = agent.attach(ClaudeProfile::new().ambient_turns().unwrap());
    let pid = agent.term.terminal().pid().expect("the direct child's pid");
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let session = "00000000-0000-4000-8000-000000000001";
    let pid_file = agent.root.join("sessions").join(format!("{pid}.json"));
    std::fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    std::fs::write(
        &pid_file,
        json!({"pid": pid, "sessionId": session, "cwd": agent.project, "startedAt": started,
            "kind": "interactive", "entrypoint": "cli"})
        .to_string(),
    )
    .unwrap();
    let dir = agent.root.join("projects").join(dashed(&agent.project));
    std::fs::create_dir_all(&dir).unwrap();
    active(&rx);
    agent
        .term
        .observe_ambient_transcript_questions(&rx)
        .unwrap();
    open_question(agent.term.terminal());
    let transcript = dir.join(format!("{session}.jsonl"));
    let ask = json!([{"type": "tool_use", "id": "toolu_1", "name": "AskUserQuestion",
        "input": {"questions": [{"header": "Fake", "question": PROMPT, "multiSelect": false,
            "options": [{"label": "Continue", "description": "Go on."}]}]}}]);
    append(
        &transcript,
        &[
            claude_line("user", json!("survey")),
            claude_line("assistant", ask),
        ],
    );
    let (qid, items) = opened(&agent.events);
    the_question(&items, true);
    let ticket = agent.term.answer_question(qid, vec![Answer::Choice(0)]);
    assert_eq!(ticket.wait(WAIT), Some(Ok(())));
    common::screen_contains(agent.term.terminal(), "question answered", WAIT);
    append(
        &transcript,
        &[claude_line(
            "user",
            json!([{"type": "tool_result", "tool_use_id": "toolu_1", "content": "Continue"}]),
        )],
    );
    assert_eq!(closed(&agent.events, qid), ClosedBy::Host);
    assert_eq!(agent.input(), b"\x1b[A1");
}

// --- A shell-launched agent -------------------------------------------------------------

/// The same on a shell terminal: the agent's program is the shell's foreground job, the
/// rollout is found through the shell's environment, and the keys go to that program.
#[cfg(unix)]
#[test]
fn a_shell_launched_agent_s_question_is_relayed_and_answered() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap().join("home");
    let root = tmp.path().join("codex");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let input = tmp.path().join("input.bin");
    let mut spec = common::sh_spec(&home);
    isolated_env(&mut spec, &home, "CODEX_HOME", &root);
    let (tx, reads) = bounded(256);
    let profile = FakeProfile::new("question-dialog", &["--input-log", input.to_str().unwrap()])
        .with_question_reads(tx);
    let (term, events) = common::agent::start_agent_shell_spec(
        spec,
        AgentSpec {
            timing: common::delivery::long_window(),
            trust: TrustPolicy::unattended(),
            ..AgentSpec::new(Arc::new(profile))
        },
    );
    let rx = term.attach_ambient_turns_with(codex(), options()).unwrap();
    term.launch().unwrap();
    states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
    let path = rollout(&root, &home);
    active(&rx);
    term.observe_ambient_transcript_questions(&rx).unwrap();
    append(
        &path,
        &[codex_event("task_started"), codex_call("call_1", PROMPT)],
    );
    withheld(&events, &reads, 2);
    open_question(term.terminal());
    let (qid, items) = opened(&events);
    the_question(&items, false);
    assert_eq!(
        term.answer_question(qid, vec![Answer::Choice(0)])
            .wait(WAIT),
        Some(Ok(()))
    );
    common::screen_contains(term.terminal(), "question answered", WAIT);
    append(&path, &[codex_output("call_1")]);
    assert_eq!(closed(&events, qid), ClosedBy::Host);
    assert_eq!(std::fs::read(&input).unwrap(), b"\x1b[A1");
}

// --- OpenCode ---------------------------------------------------------------------------

/// The fake agent's screens with the built-in OpenCode launch hook — or, with `endpoint`,
/// a launch hook that adds nothing and points the feed there.
struct FakeOpencode {
    fake: FakeProfile,
    endpoint: Option<AmbientEndpoint>,
}

impl AgentProfile for FakeOpencode {
    fn id(&self) -> &str {
        "fake-opencode"
    }
    fn dialog(&self, text: &str) -> Option<Dialog> {
        self.fake.dialog(text)
    }
    fn input_ready(&self, screen: &str) -> bool {
        self.fake.input_ready(screen)
    }
    fn launchers(&self) -> &'static [&'static str] {
        self.fake.launchers()
    }
    fn launch(
        &self,
        perms: &Permissions,
        trust: &TrustPolicy,
        cwd: &Path,
        shell: ShellKind,
    ) -> LaunchPlan {
        self.fake.launch(perms, trust, cwd, shell)
    }
    fn ambient_feed_launch(
        &self,
        plan: &mut LaunchPlan,
    ) -> agent_link::Result<Option<AmbientEndpoint>> {
        match &self.endpoint {
            Some(endpoint) => Ok(Some(endpoint.clone())),
            None => OpencodeProfile.ambient_feed_launch(plan),
        }
    }
}

/// The fake agent with `flags` as the direct child, launched to serve its feed.
fn opencode(
    dir: &Path,
    endpoint: Option<AmbientEndpoint>,
    flags: &[&str],
) -> (AgentTerminal, Receiver<Event>) {
    let args: Vec<&str> = std::iter::once("ready")
        .chain(flags.iter().copied())
        .collect();
    let mut spec = SpawnSpec::direct(fake_agent_path(), args, dir, PtySize { rows: 24, cols: 80 });
    spec.env = common::test_env(dir);
    let profile = Arc::new(FakeOpencode {
        fake: FakeProfile::new("ready", &[]),
        endpoint,
    });
    let (term, events) = {
        let _guard = common::spawn_lock();
        AgentTerminal::spawn_with_ambient_feed(spec, AgentSpec::new(profile))
            .expect("spawn with an ambient feed")
    };
    states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
    (term, events)
}

/// A pending request of the TUI's conversation is listed from the launched agent's own
/// server and relayed; the answer is posted there with the selected label, and the
/// question closes `Host` once the server no longer lists it.
#[test]
fn an_opencode_question_is_listed_and_answered_through_the_tui_server() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let replies = dir.path().join("replies.txt");
    let (term, events) = opencode(
        dir.path(),
        None,
        &[
            "--serve-question",
            "--serve-replies",
            replies.to_str().unwrap(),
        ],
    );
    for invalid in ["", "ses_", "ses_../other"] {
        assert!(matches!(
            OpenCodeAmbientQuestions::new(&term, invalid),
            Err(agent_link::Error::InvalidInput(_))
        ));
    }
    let feed = Arc::new(OpenCodeAmbientQuestions::new(&term, "ses_stub").unwrap());
    assert!(!format!("{feed:?}").contains("password="));
    term.observe_feed_questions(feed).unwrap();
    let (qid, items) = opened(&events);
    the_question(&items, true);
    assert_eq!(
        term.answer_question(qid, vec![Answer::Choice(0)])
            .wait(WAIT),
        Some(Ok(()))
    );
    assert_eq!(closed(&events, qid), ClosedBy::Host);
    assert_eq!(
        std::fs::read_to_string(&replies).unwrap(),
        "que_fake {\"answers\":[[\"Continue\"]]}\n"
    );
}

/// A port squatter serving the launch's endpoint is never sent anything, the answer's
/// connection included: the owner check finds another process than the launched agent.
#[cfg(unix)]
#[test]
fn an_opencode_port_squatter_is_sent_nothing() {
    use agent_link::agent::questions::QuestionFeed;

    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let squatter = http_stub::Stub::start(|_| http_stub::Reply::status(401));
    let endpoint = AmbientEndpoint::new(squatter.addr(), "opencode", "secret-of-the-launch");
    let (term, _events) = opencode(dir.path(), endpoint, &[]);
    let feed = OpenCodeAmbientQuestions::new(&term, "ses_stub").unwrap();
    assert_eq!(feed.pending(), Err(AnswerError::NotOpen));
    let pending = agent_link::agent::questions::PendingQuestion::new(
        "que_1",
        vec![QuestionItem::new("Fake", PROMPT, vec!["Continue".into()]).with_free_text(true)],
    );
    assert_eq!(
        feed.answer(&pending, &[Answer::Choice(0)], &|| true),
        Err(AnswerError::NotOpen)
    );
    assert!(squatter.requests().is_empty(), "{:?}", squatter.requests());
}
