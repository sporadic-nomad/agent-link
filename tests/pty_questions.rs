//! Real foreground, PTY byte ordering, observer invalidation and blocked feed callbacks.
#![cfg(unix)]
mod common;

use agent_link::agent::questions::{
    Answer, AnswerError, ClosedBy, PendingQuestion, QuestionFeed, QuestionId, QuestionItem,
};
use agent_link::agent::{AgentSpec, AgentState, AgentTerminal, TrustPolicy};
use agent_link::{Event, SessionId};
use common::agent::states_until;
use common::delivery::{fake, long_window, shell};
use common::fake::{FakeProfile, open_question};
use common::{LAUNCH_WAIT, WAIT};
use crossbeam_channel::{Receiver, Sender, bounded};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Instant;

fn opened(events: &Receiver<Event>) -> (QuestionId, Arc<[QuestionItem]>) {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Event::Question { qid, questions, .. } =
            events.recv_deadline(deadline).expect("question opened")
        {
            return (qid, questions);
        }
    }
}
fn closed(events: &Receiver<Event>, id: QuestionId) -> ClosedBy {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Event::QuestionClosed { qid, by, .. } =
            events.recv_deadline(deadline).expect("question closed")
        {
            assert_eq!(qid, id);
            return by;
        }
    }
}
fn ready(term: &AgentTerminal, events: &Receiver<Event>) {
    term.launch().unwrap();
    states_until(term, events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
}

#[test]
fn screen_question_requires_a_fresh_appearance_and_only_the_current_ticket_can_write() {
    let (tx, reads) = bounded(2);
    let profile =
        FakeProfile::new("question-dialog", &["--input-log", "input.bin"]).with_question_reads(tx);
    let (term, events, home) = shell(AgentSpec {
        timing: long_window(),
        trust: TrustPolicy::unattended(),
        ..AgentSpec::new(Arc::new(profile))
    });
    ready(&term, &events);
    open_question(term.terminal());
    term.observe_questions().unwrap();
    for _ in 0..2 {
        reads.recv_timeout(WAIT).unwrap();
    }
    assert!(
        !events
            .try_iter()
            .any(|e| matches!(e, Event::Question { .. }))
    );
    common::fake::answer_question(term.terminal());
    // Four successive observer samples exceed the 300ms deliberate absence window.
    for _ in 0..6 {
        reads.recv_timeout(WAIT).unwrap();
    }
    open_question(term.terminal());
    let (qid, items) = opened(&events);
    assert_eq!(items[0].options, ["Continue"]);
    assert!(!format!("{items:?}").contains("Continue"));
    for (id, a, err) in [
        (
            QuestionId {
                session: SessionId(qid.session.0 + 1),
                serial: qid.serial,
            },
            Answer::Choice(0),
            AnswerError::NotOpen,
        ),
        (qid, Answer::Choice(1), AnswerError::Unsupported),
        (
            qid,
            Answer::Text("private draft".into()),
            AnswerError::Unsupported,
        ),
    ] {
        assert_eq!(
            term.answer_question(id, vec![a]).try_result(),
            Some(Err(err))
        );
    }
    let ticket = term.answer_question(qid, vec![Answer::Choice(0)]);
    assert_eq!(ticket.wait(WAIT), Some(Ok(())));
    assert_eq!(closed(&events, qid), ClosedBy::Host);
    assert_eq!(ticket.try_result(), Some(Ok(())));
    assert_eq!(
        term.answer_question(qid, vec![Answer::Choice(0)])
            .try_result(),
        Some(Err(AnswerError::NotOpen))
    );
    assert_eq!(
        std::fs::read(home.path().join("input.bin")).unwrap(),
        b"\x1b[A1\x1b[A1"
    );

    open_question(term.terminal());
    let (second, _) = opened(&events);
    assert_ne!(qid, second);
    common::fake::answer_question(term.terminal());
    assert_eq!(closed(&events, second), ClosedBy::Desk);
}

fn sample(id: &str) -> PendingQuestion {
    PendingQuestion::new(
        id,
        vec![
            QuestionItem::new("Color", "Select the secret color", vec!["Blue".into()])
                .with_free_text(true),
        ],
    )
}

struct Feed {
    questions: Mutex<Vec<PendingQuestion>>,
    writes: AtomicUsize,
    entered: Sender<()>,
    proceed: Receiver<()>,
    result: Result<(), AnswerError>,
}
impl QuestionFeed for Feed {
    fn pending(&self) -> Result<Vec<PendingQuestion>, AnswerError> {
        Ok(self.questions.lock().unwrap().clone())
    }
    fn answer(
        &self,
        _: &PendingQuestion,
        answers: &[Answer],
        current: &dyn Fn() -> bool,
    ) -> Result<(), AnswerError> {
        self.entered.send(()).unwrap();
        self.proceed.recv_timeout(WAIT).unwrap();
        if !current() {
            return Err(AnswerError::NotOpen);
        }
        assert_eq!(answers, [Answer::Text("literal\ttext\r\n".into())]);
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.result
    }
}
fn feed(result: Result<(), AnswerError>) -> (Arc<Feed>, Receiver<()>, Sender<()>) {
    let (tx, entered) = bounded(1);
    let (proceed, rx) = bounded(1);
    (
        Arc::new(Feed {
            questions: Mutex::new(vec![sample("first"), sample("second")]),
            writes: AtomicUsize::new(0),
            entered: tx,
            proceed: rx,
            result,
        }),
        entered,
        proceed,
    )
}
fn text() -> Vec<Answer> {
    vec![Answer::Text("literal\ttext\r\n".into())]
}

#[test]
fn feed_withdrawal_has_unknown_actor_and_an_uncertain_post_cannot_be_replayed() {
    let home = tempfile::tempdir().unwrap();
    let mut spec = agent_link::SpawnSpec::direct(
        common::fake_agent_path(),
        ["ready"],
        home.path(),
        agent_link::PtySize {
            rows: 24,
            cols: 100,
        },
    );
    spec.env = common::test_env(home.path());
    let (term, events) = common::agent::spawn_agent(
        spec,
        AgentSpec::new(Arc::new(FakeProfile::new("ready", &[]))),
    );
    states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
    let (feed, entered, proceed) = feed(Err(AnswerError::Unconfirmed));
    term.observe_feed_questions(feed.clone()).unwrap();
    let (first, _) = opened(&events);
    let ticket = term.answer_question(first, text());
    entered.recv_timeout(WAIT).unwrap();
    assert_eq!(
        term.answer_question(first, text()).try_result(),
        Some(Err(AnswerError::QueueFull))
    );
    proceed.send(()).unwrap();
    assert_eq!(ticket.wait(WAIT), Some(Err(AnswerError::Unconfirmed)));
    assert_eq!(
        term.answer_question(first, text()).wait(WAIT),
        Some(Err(AnswerError::Unconfirmed))
    );
    *feed.questions.lock().unwrap() = vec![sample("second")];
    assert_eq!(closed(&events, first), ClosedBy::Gone);
    let (second, _) = opened(&events);
    *feed.questions.lock().unwrap() = vec![sample("first")];
    assert_eq!(closed(&events, second), ClosedBy::Gone);
    assert_eq!(feed.writes.load(Ordering::SeqCst), 1);
}

#[test]
fn stopping_and_dropping_resolves_a_blocked_ticket_and_revokes_a_late_mutation() {
    let (term, events, _home) = fake("ready", &[]);
    ready(&term, &events);
    let (feed, entered, proceed) = feed(Ok(()));
    term.observe_feed_questions(feed.clone()).unwrap();
    let (qid, _) = opened(&events);
    let ticket = term.answer_question(qid, text());
    entered.recv_timeout(WAIT).unwrap();
    term.stop_questions();
    assert_eq!(ticket.wait(WAIT), Some(Err(AnswerError::NotOpen)));
    assert_eq!(closed(&events, qid), ClosedBy::Gone);
    drop(term);
    proceed.send(()).unwrap();
    common::wait_until("old source release", || Arc::strong_count(&feed) == 1);
    assert_eq!(feed.writes.load(Ordering::SeqCst), 0);
}

#[test]
fn relaunch_cannot_poll_the_previous_launch_feed() {
    let (term, events, _home) = fake("ready", &["--exit-on-input"]);
    ready(&term, &events);
    let (feed, _, _) = feed(Ok(()));
    term.observe_feed_questions(feed.clone()).unwrap();
    let (qid, _) = opened(&events);
    term.terminal().write(b"\r").unwrap();
    assert_eq!(closed(&events, qid), ClosedBy::Gone);
    common::wait_until("old source release", || Arc::strong_count(&feed) == 1);
    *feed.questions.lock().unwrap() = vec![sample("old-after-exit")];
    // A normal send observes the shell, launches a new agent and admits its own job.
    let next = term.send("new launch", Default::default());
    assert!(next.wait(LAUNCH_WAIT).is_some());
    assert!(
        !events
            .try_iter()
            .any(|e| matches!(e, Event::Question { .. }))
    );
}

struct ParkedPoll {
    entered: Sender<()>,
    release: Receiver<()>,
}
impl QuestionFeed for ParkedPoll {
    fn pending(&self) -> Result<Vec<PendingQuestion>, AnswerError> {
        self.entered.send(()).unwrap();
        self.release.recv_timeout(WAIT).unwrap();
        Ok(vec![PendingQuestion::new(
            "stale",
            vec![QuestionItem::new("stale", "stale", vec!["stale".into()])],
        )])
    }
    fn answer(
        &self,
        _: &PendingQuestion,
        _: &[Answer],
        _: &dyn Fn() -> bool,
    ) -> Result<(), AnswerError> {
        unreachable!()
    }
}

#[test]
fn replacing_a_source_discards_its_blocked_poll_before_publishing_the_next_source() {
    let (term, events, _home) = fake("ready", &[]);
    ready(&term, &events);
    let (tx, entered) = bounded(1);
    let (release, rx) = bounded(1);
    term.observe_feed_questions(Arc::new(ParkedPoll {
        entered: tx,
        release: rx,
    }))
    .unwrap();
    entered.recv_timeout(WAIT).unwrap();
    let (new, _, _) = feed(Ok(()));
    term.observe_feed_questions(new).unwrap();
    release.send(()).unwrap();
    let (_, items) = opened(&events);
    assert_eq!(items[0].header, "Color");
}

/// The built-in Cursor profile, telling each screen read of the question observer (as
/// `FakeProfile::with_question_reads` does): its first read with the agent in front is the
/// baseline, so the approval is drawn only after two.
struct ObservedCursor {
    cursor: agent_link::agent::profiles::CursorProfile,
    reads: Sender<()>,
}

impl agent_link::agent::AgentProfile for ObservedCursor {
    fn id(&self) -> &str {
        self.cursor.id()
    }
    fn dialog(&self, text: &str) -> Option<agent_link::agent::Dialog> {
        self.cursor.dialog(text)
    }
    fn input_ready(&self, screen: &str) -> bool {
        self.cursor.input_ready(screen)
    }
    fn question_dialog(&self, screen: &str) -> bool {
        let _ = self.reads.try_send(());
        self.cursor.question_dialog(screen)
    }
    fn question_on_screen(&self, screen: &str) -> Option<PendingQuestion> {
        self.cursor.question_on_screen(screen)
    }
    fn question_shown(&self, screen: &str, pending: &PendingQuestion) -> bool {
        self.cursor.question_shown(screen, pending)
    }
    fn answer_keys(
        &self,
        screen: &str,
        pending: &PendingQuestion,
        answers: &[Answer],
    ) -> Result<Vec<agent_link::agent::questions::KeyStep>, AnswerError> {
        self.cursor.answer_keys(screen, pending, answers)
    }
    fn launchers(&self) -> &'static [&'static str] {
        self.cursor.launchers()
    }
    fn launch(
        &self,
        perms: &agent_link::agent::Permissions,
        trust: &TrustPolicy,
        cwd: &std::path::Path,
        shell: agent_link::agent::ShellKind,
    ) -> agent_link::agent::LaunchPlan {
        self.cursor.launch(perms, trust, cwd, shell)
    }
}

/// The built-in Cursor profile reads its shell approval off the screen of a shell-launched
/// agent: a stand-in `agent` on `PATH` draws Cursor's ready composer, then the recorded
/// approval layout, and records the key it gets. Relayed, answered with `y`, closed `Host`.
#[test]
fn cursor_shell_approval_is_relayed_from_a_shell_launched_agent() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let screen = format!(
        "{}\r\n $  pwd in /sample\r\n\r\n Run this command?\r\n  \u{2192} Run (once) (y)\r\n    Skip & tell the agent what to do instead (esc or n)\r\n",
        "\u{2500}".repeat(40)
    );
    std::fs::write(home.path().join("screen.txt"), screen).unwrap();
    let agent = bin.join("agent");
    std::fs::write(
        &agent,
        "#!/bin/sh\n\
         stty raw -echo\n\
         printf 'Cursor Agent\\r\\n\\r\\n\\342\\206\\222 Plan, search, build anything\\r\\n'\n\
         dd bs=1 count=1 of=/dev/null 2>/dev/null\n\
         printf '\\033[2J\\033[H'\n\
         cat \"$HOME/screen.txt\"\n\
         dd bs=1 count=1 of=\"$HOME/received\" 2>/dev/null\n\
         printf '\\033[2J\\033[Hanswered\\r\\n'\n\
         dd bs=1 count=1 of=/dev/null 2>/dev/null\n",
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut spec = common::sh_spec(home.path());
    spec.env
        .set("PATH", format!("{}:/usr/bin:/bin", bin.to_str().unwrap()));
    let (tx, reads) = bounded(64);
    let profile = ObservedCursor {
        cursor: agent_link::agent::profiles::CursorProfile,
        reads: tx,
    };
    let (term, events) = common::agent::start_agent_shell_spec(
        spec,
        AgentSpec {
            timing: long_window(),
            trust: TrustPolicy::unattended(),
            ..AgentSpec::new(Arc::new(profile))
        },
    );
    term.observe_questions().unwrap();
    ready(&term, &events);
    while reads.try_recv().is_ok() {}
    for _ in 0..2 {
        reads.recv_timeout(WAIT).unwrap();
    }
    term.terminal().write(b"!").unwrap();
    let (qid, items) = opened(&events);
    assert_eq!(
        &items[..],
        [QuestionItem::new(
            "Run this command?",
            "pwd in /sample",
            vec![
                "Run (once) (y)".into(),
                "Skip & tell the agent what to do instead (esc or n)".into()
            ],
        )]
    );
    assert_eq!(
        term.answer_question(qid, vec![Answer::Choice(0)])
            .wait(WAIT),
        Some(Ok(()))
    );
    common::screen_contains(term.terminal(), "answered", WAIT);
    assert_eq!(closed(&events, qid), ClosedBy::Host);
    assert_eq!(std::fs::read(home.path().join("received")).unwrap(), b"y");
}
