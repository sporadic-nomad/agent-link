//! Best-effort ambient turns from a feed on real PTYs. The fake agent plays an OpenCode TUI
//! with its server: launched through `spawn_with_ambient_feed` with the built-in OpenCode
//! feed and launch hook (its screens read by the fake's profile), it serves the synthetic
//! OpenCode-like API of `tests/common/opencode_stub.rs` on the port the launch gave it,
//! with the password the launch put in its environment, from its own process — so the
//! feed's check of who serves the endpoint has a real launched process to find. Covers
//! from-now, replies, a new port and password per spawn, shell launches and relaunches,
//! a port squatter, an unprotected server, refused credentials, a launch that serves
//! nothing, stop during a poll and queue overflow. No real OpenCode or user configuration
//! is used.
#![cfg(feature = "turns")]

mod common;
#[path = "common/http_stub.rs"]
mod http_stub;
#[path = "common/opencode_stub.rs"]
mod opencode_stub;

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

#[cfg(unix)]
use agent_link::Event;
use agent_link::agent::profiles::OpencodeProfile;
use agent_link::agent::turns::ambient::{
    AmbientClosed, AmbientContext, AmbientEndpoint, AmbientEvidence, AmbientFeed,
    AmbientFeedBudget, AmbientFeedEvent, AmbientFeedPoll, AmbientFeedReader, AmbientFeedState,
    AmbientFeedUnavailable, AmbientOptions, AmbientReceiver, AmbientStatus, AmbientTurn,
    AmbientUnavailable, AmbientWaiting, ambient_workers,
};
use agent_link::agent::turns::{TurnOptions, TurnReply};
use agent_link::agent::{
    AgentProfile, AgentSpec, AgentTerminal, Dialog, LaunchPlan, Permissions, ShellKind, TrustPolicy,
};
use agent_link::{PtySize, SpawnSpec};
use common::fake::FakeProfile;
use common::{fake_agent_path, wait_until};
use crossbeam_channel::Receiver;
use http_stub::{Reply, Stub};

/// The tests share the process-wide worker budget: one at a time, each after the
/// previous test's workers retired.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    wait_until("ambient workers to retire", || ambient_workers() == 0);
    guard
}

/// The fake agent's screens with the built-in OpenCode feed and launch hook — or, with
/// `endpoint`, a launch hook that adds nothing and points the feed there.
struct FakeOpencode {
    fake: FakeProfile,
    endpoint: Option<AmbientEndpoint>,
}

impl FakeOpencode {
    fn new(flags: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            fake: FakeProfile::new("ready", flags),
            endpoint: None,
        })
    }
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
    fn ambient_feed(&self) -> Option<Arc<dyn AmbientFeed>> {
        OpencodeProfile.ambient_feed()
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

fn options() -> AmbientOptions {
    let mut options = AmbientOptions::default();
    options.poll = Duration::from_millis(20);
    options
}

/// The fake agent with `flags` as the direct child, launched to serve its feed.
fn direct(dir: &Path, profile: Arc<dyn AgentProfile>, flags: &[&str]) -> AgentTerminal {
    let args: Vec<&str> = std::iter::once("ready")
        .chain(flags.iter().copied())
        .collect();
    let mut spec = SpawnSpec::direct(fake_agent_path(), args, dir, PtySize { rows: 24, cols: 80 });
    spec.env = common::test_env(dir);
    let _guard = common::spawn_lock();
    AgentTerminal::spawn_with_ambient_feed(spec, AgentSpec::new(profile))
        .expect("spawn with an ambient feed")
        .0
}

fn status_until(
    rx: &AmbientReceiver,
    what: &str,
    mut want: impl FnMut(AmbientStatus) -> bool,
) -> AmbientStatus {
    let deadline = Instant::now() + common::SHELL_START_WAIT;
    loop {
        let status = rx.status();
        if want(status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; status {status:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn active(rx: &AmbientReceiver) {
    status_until(rx, "an active feed", |s| {
        matches!(s, AmbientStatus::Active { .. })
    });
}

fn unavailable(reason: AmbientFeedUnavailable) -> AmbientStatus {
    AmbientStatus::Waiting(AmbientWaiting::Unavailable(AmbientUnavailable::Feed(
        reason,
    )))
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

fn text(turn: &AmbientTurn) -> &str {
    turn.reply.as_ref().map_or("", |reply| reply.text.as_str())
}

/// Types a prompt into the fake's composer and submits it: its reply follows.
fn prompt(term: &AgentTerminal, text: &str) {
    common::screen_contains(term.terminal(), "fake-agent ready", common::LAUNCH_WAIT);
    term.terminal()
        .write(format!("{text}\r").as_bytes())
        .unwrap();
}

/// `port=` and `password=` as the fake wrote them (`--serve-info`).
fn served(path: &Path) -> (u16, String) {
    wait_until("the fake's server info", || path.exists());
    let info = std::fs::read_to_string(path).unwrap();
    let value = |key: &str| {
        info.lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap()
            .to_string()
    };
    (value("port=").parse().unwrap(), value("password="))
}

/// A turn that ended before the attachment (told again by the server on connecting) is
/// not reported; a reply after it is, once, with explicit evidence.
#[test]
fn a_direct_launch_serves_its_feed_and_replies_are_relayed_from_now() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let term = direct(dir.path(), FakeOpencode::new(&[]), &["--serve-history"]);
    let rx = term.attach_ambient_turns(options()).unwrap();
    active(&rx);
    prompt(&term, "hello");
    let turn = next_turn(&rx);
    assert_eq!(text(&turn), "ok #1");
    assert_eq!(turn.evidence, AmbientEvidence::ExplicitEnd);
    assert!(!turn.reply.as_ref().unwrap().truncated);
    prompt(&term, "again");
    assert_eq!(text(&next_turn(&rx)), "ok #2");
    std::thread::sleep(Duration::from_millis(300));
    assert!(rx.try_recv().is_none(), "each turn once");
    assert!(matches!(
        rx.status(),
        AmbientStatus::Active {
            unrecognised: false,
            ..
        }
    ));
    drop(term);
    status_until(&rx, "the end", |s| {
        s == AmbientStatus::Closed(AmbientClosed::TerminalEnded)
    });
}

#[cfg(windows)]
#[test]
fn a_windows_launch_can_serve_the_feed_from_its_child() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(fake_agent_path(), dir.path().join("fake.exe")).unwrap();
    let mut spec = SpawnSpec::direct(
        agent_link::proc::system32_program("cmd"),
        ["/d", "/c", r".\fake.exe", "ready"],
        dir.path(),
        PtySize { rows: 24, cols: 80 },
    );
    spec.env = common::test_env(dir.path());
    let (term, _events) = {
        let _guard = common::spawn_lock();
        AgentTerminal::spawn_with_ambient_feed(spec, AgentSpec::new(FakeOpencode::new(&[])))
            .unwrap()
    };
    let rx = term.attach_ambient_turns(options()).unwrap();
    active(&rx);
}

/// Every spawn gets its own port and password; the password never shows in the agent's
/// argv, and a terminal spawned the ordinary way serves nothing.
#[test]
fn each_spawn_serves_a_new_port_and_password_and_ordinary_spawns_none() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut seen = Vec::new();
    for n in 0..2 {
        let info = dir.path().join(format!("served-{n}"));
        let info_arg = info.to_str().unwrap().to_string();
        let term = direct(
            dir.path(),
            FakeOpencode::new(&[]),
            &["--serve-info", &info_arg],
        );
        let (port, password) = served(&info);
        assert_eq!(password.len(), 32, "{password:?}");
        let rx = term.attach_ambient_turns(options()).unwrap();
        active(&rx);
        seen.push((port, password));
    }
    assert_ne!(seen[0].0, seen[1].0, "a new port");
    assert_ne!(seen[0].1, seen[1].1, "a new password");

    let mut spec = SpawnSpec::direct(
        fake_agent_path(),
        ["ready"],
        dir.path(),
        PtySize { rows: 24, cols: 80 },
    );
    spec.env = common::test_env(dir.path());
    let ordinary = common::agent::spawn_agent(spec, AgentSpec::new(FakeOpencode::new(&[]))).0;
    let rx = ordinary.attach_ambient_turns(options()).unwrap();
    status_until(&rx, "nothing served", |s| {
        s == unavailable(AmbientFeedUnavailable::NotServed)
    });
}

#[test]
fn a_feed_launch_is_refused_when_it_cannot_reach_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let size = PtySize { rows: 24, cols: 80 };
    // The policy would strip the password: the server would run unprotected.
    let mut spec = SpawnSpec::direct(fake_agent_path(), ["ready"], dir.path(), size);
    spec.env = common::test_env(dir.path());
    spec.env.remove("OPENCODE_SERVER_PASSWORD");
    let refused =
        AgentTerminal::spawn_with_ambient_feed(spec, AgentSpec::new(FakeOpencode::new(&[])));
    assert!(
        matches!(refused, Err(agent_link::Error::InvalidInput(_))),
        "{refused:?}"
    );
    // The server flags would become positional arguments after this marker.
    let spec = SpawnSpec::direct(
        fake_agent_path(),
        ["ready", "--", "project"],
        dir.path(),
        size,
    );
    let refused =
        AgentTerminal::spawn_with_ambient_feed(spec, AgentSpec::new(FakeOpencode::new(&[])));
    assert!(
        matches!(refused, Err(agent_link::Error::InvalidInput(_))),
        "{refused:?}"
    );
    // Mini mode rejects both network flags. A missing executable distinguishes
    // preflight rejection from a spawn attempt.
    for args in [
        &["--mini"][..],
        &["--mini=true"],
        &[r#"--mini="true""#],
        &["--mini", "true"],
    ] {
        let spec = SpawnSpec::direct(dir.path().join("absent"), args, dir.path(), size);
        let refused =
            AgentTerminal::spawn_with_ambient_feed(spec, AgentSpec::new(FakeOpencode::new(&[])));
        assert!(
            matches!(refused, Err(agent_link::Error::InvalidInput(_))),
            "{refused:?}"
        );
    }
    // A profile that serves no feed.
    let spec = SpawnSpec::direct(fake_agent_path(), ["ready"], dir.path(), size);
    let plain = common::fake::fake_profile("ready", &[]);
    let refused = AgentTerminal::spawn_with_ambient_feed(spec, AgentSpec::new(plain));
    assert!(
        matches!(refused, Err(agent_link::Error::Unsupported(_))),
        "{refused:?}"
    );
    // No profile at all.
    let spec = SpawnSpec::direct(fake_agent_path(), ["ready"], dir.path(), size);
    let refused = AgentTerminal::spawn_with_ambient_feed(spec, AgentSpec::default());
    assert!(
        matches!(refused, Err(agent_link::Error::InvalidInput(_))),
        "{refused:?}"
    );
}

/// A port squatter — another process serving the launch's endpoint, answering `401` as a
/// protected server would — is sent nothing at all. A proven mismatch ends the feed.
#[test]
fn a_port_squatter_never_gets_the_password() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let squatter = Stub::start(|_| Reply::status(401));
    let profile = Arc::new(FakeOpencode {
        fake: FakeProfile::new("ready", &[]),
        endpoint: AmbientEndpoint::new(squatter.addr(), "opencode", "secret-of-the-launch"),
    });
    let term = direct(dir.path(), profile, &[]);
    let rx = term.attach_ambient_turns(options()).unwrap();
    let status = status_until(&rx, "an ownership refusal", |s| {
        s == unavailable(AmbientFeedUnavailable::OwnerMismatch)
            || (cfg!(windows) && s == unavailable(AmbientFeedUnavailable::OwnerUnverified))
    });
    if status == unavailable(AmbientFeedUnavailable::OwnerMismatch) {
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(rx.status(), status, "final");
    }
    assert!(squatter.requests().is_empty(), "{:?}", squatter.requests());
}

/// The launched agent's own server answering without the password, or refusing it: the
/// feed ends for the launch with the typed reason.
#[test]
fn an_unprotected_or_refusing_server_ends_the_launch_s_feed() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    for (flags, reason) in [
        (&["--serve-open"][..], AmbientFeedUnavailable::Unprotected),
        (
            &["--serve-password", "not-the-launch-s"][..],
            AmbientFeedUnavailable::CredentialsRefused,
        ),
    ] {
        let term = direct(dir.path(), FakeOpencode::new(&[]), flags);
        let rx = term.attach_ambient_turns(options()).unwrap();
        status_until(&rx, "the refusal", |s| s == unavailable(reason));
    }
}

/// `start_agent_shell_spec`'s prompt wait, for a terminal spawned with a feed.
#[cfg(unix)]
fn feed_shell(home: &Path, profile: Arc<dyn AgentProfile>) -> (AgentTerminal, Receiver<Event>) {
    let (term, events) = {
        let _guard = common::spawn_lock();
        let agent = AgentSpec {
            trace: Some(true),
            ..AgentSpec::new(profile)
        };
        AgentTerminal::spawn_with_ambient_feed(common::sh_spec(home), agent)
            .expect("spawn a feed shell")
    };
    term.terminal().write(b"set -m\n").unwrap();
    term.terminal().write(b"echo shell-$((20+1))\n").unwrap();
    let deadline = Instant::now() + common::SHELL_START_WAIT;
    let mut text = String::new();
    loop {
        match events.recv_deadline(deadline) {
            Ok(Event::Output { bytes, .. }) => {
                text.push_str(&String::from_utf8_lossy(&bytes));
                if let Some(at) = text.find("shell-21") {
                    if text[at..].contains("$ ") {
                        return (term, events);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => panic!("no prompt ({e}); output so far: {text:?}"),
        }
    }
}

/// A shell terminal: the launch line carries the port, the shell's environment the
/// password (never on screen, never logged); the feed verifies the launched program, and
/// after the agent left and was launched again it verifies the new one (same terminal:
/// same port and password) and reports its turns from its own launch on.
#[cfg(unix)]
#[test]
fn a_shell_launch_is_served_by_the_launched_program_and_a_relaunch_is_followed() {
    use agent_link::ForegroundOwner;
    use agent_link::agent::AgentState;
    use common::agent::states_until;

    let _serial = serial();
    common::logger::install();
    let home = tempfile::tempdir().unwrap();
    let info = home.path().join("served");
    let profile = FakeOpencode::new(&["--serve-info", info.to_str().unwrap()]);
    let (term, events) = feed_shell(home.path(), profile);
    let ready_again = |events: &Receiver<Event>| {
        let mut starting = false;
        states_until(&term, events, common::LAUNCH_WAIT, |s, _| {
            starting |= s == AgentState::Starting;
            starting && s == AgentState::Ready
        });
    };
    let rx = term.attach_ambient_turns(options()).unwrap();
    status_until(&rx, "no agent yet", |s| {
        s == AmbientStatus::Waiting(AmbientWaiting::NoAgent)
    });
    term.launch().unwrap();
    ready_again(&events);
    active(&rx);
    let (port, password) = served(&info);
    prompt(&term, "one");
    assert_eq!(text(&next_turn(&rx)), "ok #1");
    let screen = term.terminal().screen().unwrap().text();
    assert!(!screen.contains(&password), "the password is not on screen");

    term.interrupt().unwrap();
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    status_until(&rx, "the departure", |s| {
        s == AmbientStatus::Waiting(AmbientWaiting::NoAgent)
    });
    std::fs::remove_file(&info).unwrap();
    term.launch().unwrap();
    ready_again(&events);
    let (again, same) = served(&info);
    assert_eq!(
        (again, &same),
        (port, &password),
        "one port and password per terminal"
    );
    active(&rx);
    prompt(&term, "two");
    // The new process numbers its replies afresh: a new launch, a new turn.
    assert_eq!(text(&next_turn(&rx)), "ok #1");
    assert!(rx.try_recv().is_none());
    let id = term.terminal().id();
    assert!(
        !common::logger::lines_for(id).is_empty(),
        "the session traced"
    );
    let encoded = http_stub::basic("opencode", &password);
    for line in common::logger::lines() {
        assert!(!line.contains(&password), "{line}");
        assert!(!line.contains(&encoded["Basic ".len()..]), "{line}");
    }
    assert!(!format!("{rx:?}").contains(&password));
    drop(events);
}

/// A host feed whose poll is held: `stop` returns at once and closes the receiver; the
/// turn the poll returns after it is never published.
#[test]
fn stop_during_a_poll_returns_at_once_and_publishes_nothing_after() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let (release, held) = crossbeam_channel::bounded::<()>(0);
    let (entered, polling) = crossbeam_channel::bounded::<()>(1);
    let feed = Arc::new(Scripted::held(held, entered));
    let term = direct(dir.path(), FakeOpencode::new(&[]), &[]);
    let rx = term.attach_ambient_feed_with(feed, options()).unwrap();
    polling.recv_timeout(common::LAUNCH_WAIT).expect("a poll");
    let started = Instant::now();
    rx.stop();
    assert!(started.elapsed() < Duration::from_millis(100));
    assert_eq!(rx.status(), AmbientStatus::Closed(AmbientClosed::Stopped));
    release.send(()).unwrap();
    wait_until("the worker to retire", || ambient_workers() == 0);
    assert!(rx.try_recv().is_none());
}

/// More turns than the queue takes close the attachment with an overflow; those that fit
/// stay drainable. Turns that ended before the launch are dropped first.
#[test]
fn a_feed_s_turns_beyond_the_queue_close_it_with_an_overflow() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let now = SystemTime::now() + Duration::from_secs(5);
    let old = SystemTime::now() - Duration::from_secs(60);
    let feed = Arc::new(Scripted::once(vec![
        ("old", old, "before the launch"),
        ("a", now, "one"),
        ("b", now, "two"),
        ("c", now, "three"),
    ]));
    let term = direct(dir.path(), FakeOpencode::new(&[]), &[]);
    let mut options = options();
    options.poll = Duration::from_secs(60);
    options.limits.queue_items = 2;
    let rx = term.attach_ambient_feed_with(feed, options).unwrap();
    status_until(&rx, "the overflow", |s| {
        s == AmbientStatus::Closed(AmbientClosed::Overflow)
    });
    // The worker releases its slot before the next poll, even with turns left to drain.
    let deadline = Instant::now() + common::WAIT;
    while ambient_workers() != 0 {
        assert!(Instant::now() < deadline, "overflowed worker kept its slot");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(text(&rx.try_recv().unwrap()), "one");
    assert_eq!(text(&rx.try_recv().unwrap()), "two");
    assert!(rx.try_recv().is_none());
}

/// This feed releases its pages only on departure. It can finish, exhaust the
/// deadline, or end with an outstanding page when its request socket closes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FinalRead {
    Complete,
    Timeout,
    Disconnected,
}

#[derive(Clone)]
struct FinalPages {
    outcome: FinalRead,
    first_deadline: Option<Instant>,
}

impl AmbientFeed for FinalPages {
    fn open(&self, _: &AmbientContext, _: Option<&AmbientEndpoint>) -> Box<dyn AmbientFeedReader> {
        Box::new(self.clone())
    }
}

impl AmbientFeedReader for FinalPages {
    fn poll(&mut self, _: &AmbientContext, budget: &AmbientFeedBudget) -> AmbientFeedPoll {
        let mut poll = AmbientFeedPoll::new(AmbientFeedState::Ready);
        if budget.last {
            assert_eq!(budget.events, 1);
            poll.more = self.first_deadline.is_none();
            assert_eq!(
                *self.first_deadline.get_or_insert(budget.deadline),
                budget.deadline
            );
            if self.outcome == FinalRead::Disconnected {
                poll.state = AmbientFeedState::Ended(AmbientUnavailable::Io(
                    std::io::ErrorKind::NotConnected,
                ));
            }
            if self.outcome == FinalRead::Timeout {
                std::thread::sleep(budget.deadline.saturating_duration_since(Instant::now()));
            }
            let text = if poll.more { "first" } else { "second" };
            let mut reply = TurnReply::default();
            reply.text = text.into();
            poll.events.push(AmbientFeedEvent::Ended {
                key: text.into(),
                ended: SystemTime::now(),
                reply: Some(reply),
            });
        }
        poll
    }
}

#[test]
fn departure_drains_feed_pages_or_reports_the_incomplete_drain() {
    let _serial = serial();
    for outcome in [
        FinalRead::Complete,
        FinalRead::Timeout,
        FinalRead::Disconnected,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let term = direct(dir.path(), FakeOpencode::new(&[]), &[]);
        let mut options = options();
        options.limits.records = 1;
        let feed = FinalPages {
            outcome,
            first_deadline: None,
        };
        let rx = term
            .attach_ambient_feed_with(Arc::new(feed), options)
            .unwrap();
        active(&rx);
        drop(term);
        let closed = status_until(&rx, "departure", |s| matches!(s, AmbientStatus::Closed(_)));
        let expected = if outcome == FinalRead::Complete {
            AmbientClosed::TerminalEnded
        } else {
            AmbientClosed::IncompleteDrain
        };
        assert_eq!(closed, AmbientStatus::Closed(expected));
        assert_eq!(text(&rx.try_recv().unwrap()), "first");
        if outcome == FinalRead::Complete {
            assert_eq!(text(&rx.try_recv().unwrap()), "second");
        }
        assert!(rx.try_recv().is_none());
    }
}

/// A host feed: turns told once, or a poll held until released.
struct Scripted {
    turns: Mutex<Vec<(&'static str, SystemTime, &'static str)>>,
    hold: Option<(Receiver<()>, crossbeam_channel::Sender<()>)>,
}

impl Scripted {
    fn once(turns: Vec<(&'static str, SystemTime, &'static str)>) -> Self {
        Self {
            turns: Mutex::new(turns),
            hold: None,
        }
    }

    fn held(release: Receiver<()>, entered: crossbeam_channel::Sender<()>) -> Self {
        Self {
            turns: Mutex::new(vec![(
                "late",
                SystemTime::now() + Duration::from_secs(60),
                "late",
            )]),
            hold: Some((release, entered)),
        }
    }
}

struct ScriptedReader(Arc<Scripted>);

impl AmbientFeed for Scripted {
    fn open(&self, _: &AmbientContext, _: Option<&AmbientEndpoint>) -> Box<dyn AmbientFeedReader> {
        // The feed is shared by reference count: the reader keeps a handle on the script.
        let script = Scripted {
            turns: Mutex::new(std::mem::take(&mut *self.turns.lock().unwrap())),
            hold: self.hold.clone(),
        };
        Box::new(ScriptedReader(Arc::new(script)))
    }
}

impl AmbientFeedReader for ScriptedReader {
    fn poll(&mut self, _: &AmbientContext, budget: &AmbientFeedBudget) -> AmbientFeedPoll {
        if let Some((release, entered)) = &self.0.hold {
            let _ = entered.try_send(());
            let _ = release.recv();
        }
        let mut poll = AmbientFeedPoll::new(AmbientFeedState::Ready);
        for (key, ended, text) in std::mem::take(&mut *self.0.turns.lock().unwrap()) {
            let mut reply = TurnReply::default();
            reply.text = text.into();
            poll.events.push(AmbientFeedEvent::Ended {
                key: key.into(),
                ended,
                reply: Some(reply),
            });
        }
        assert!(budget.deadline > Instant::now() - Duration::from_secs(10));
        poll
    }
}

#[test]
fn a_feed_budget_carries_the_attachment_s_limits() {
    let limits = TurnOptions {
        reply_bytes: 9,
        ..TurnOptions::default()
    };
    let deadline = Instant::now();
    let budget = AmbientFeedBudget::new(deadline, &limits);
    assert_eq!(
        (budget.deadline, budget.reply_bytes, budget.last),
        (deadline, 9, false)
    );
}
