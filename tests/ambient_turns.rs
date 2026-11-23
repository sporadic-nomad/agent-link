//! Best-effort ambient turn observation on real PTYs: the fake agent runs in the terminal
//! (direct, or launched from the test shell) while the test plays the CLI's transcript
//! writer in a temporary directory, through a synthetic `AmbientSource`. Covers from-now
//! reading, explicit and inferred ends, truncation, queue overflow, stop and drop, child
//! exit, launches and departures in a shell, typed unavailability, transcript switches,
//! blocking and awaited waits, and the process-wide worker budget. No real agent CLI or
//! user configuration is used.
#![cfg(feature = "turns")]

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use agent_link::agent::turns::AttachError;
use agent_link::agent::turns::TurnReply;
use agent_link::agent::turns::ambient::{
    AmbientClosed, AmbientContext, AmbientEvidence, AmbientOptions, AmbientParser, AmbientReceiver,
    AmbientRecord, AmbientRecvError, AmbientSource, AmbientStatus, AmbientTurn, AmbientUnavailable,
    AmbientWaiting, ambient_worker_limit, ambient_workers, set_ambient_worker_limit,
};
use agent_link::agent::{AgentSpec, AgentState, AgentTerminal};
use agent_link::{PtySize, SpawnSpec};
use common::agent::spawn_agent;
use common::fake::fake_profile;
use common::{fake_agent_path, wait_until};
use crossbeam_channel::{Receiver, Sender};
use serde_json::Value;

/// The tests share the process-wide worker budget: one at a time, each after the
/// previous test's workers retired.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    workers_back_to(0);
    guard
}

/// A synthetic CLI: `reply` holds the candidate final text, `turn_end` ends a turn,
/// `user` ends a turn whose candidate had no end record, and — with `quiet` — a quiet
/// transcript ends a pending candidate.
struct Source {
    path: Mutex<Result<PathBuf, AmbientUnavailable>>,
    quiet: bool,
    contexts: Mutex<Vec<AmbientContext>>,
    locates: AtomicUsize,
    hold: Mutex<Option<Pause>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Callback {
    Locate,
    Parser,
}

struct Pause {
    at: Callback,
    entered: Sender<()>,
    release: Receiver<()>,
}

impl Source {
    fn new(path: &Path, quiet: bool) -> Arc<Self> {
        Arc::new(Self {
            path: Mutex::new(Ok(path.to_path_buf())),
            quiet,
            contexts: Mutex::new(Vec::new()),
            locates: AtomicUsize::new(0),
            hold: Mutex::new(None),
        })
    }

    fn set(&self, located: Result<PathBuf, AmbientUnavailable>) {
        *self.path.lock().unwrap() = located;
    }

    fn last_context(&self) -> Option<AmbientContext> {
        self.contexts.lock().unwrap().last().cloned()
    }

    fn pause_next(&self, at: Callback) -> (Receiver<()>, Sender<()>) {
        let (entered, waiting) = crossbeam_channel::bounded(1);
        let (resume, release) = crossbeam_channel::bounded(1);
        *self.hold.lock().unwrap() = Some(Pause {
            at,
            entered,
            release,
        });
        (waiting, resume)
    }

    fn reached(&self, at: Callback) {
        let pause = self.hold.lock().unwrap().take_if(|pause| pause.at == at);
        if let Some(pause) = pause {
            let _ = pause.entered.send(());
            let _ = pause.release.recv_timeout(common::SHELL_START_WAIT);
        }
    }

    /// Waits for a lookup that starts after this call: the worker has taken its baseline
    /// for the launch current now.
    #[cfg(unix)]
    fn located_again(&self) {
        let seen = self.locates.load(Ordering::SeqCst);
        wait_until("another lookup", || {
            self.locates.load(Ordering::SeqCst) > seen + 1
        });
    }
}

impl AmbientSource for Source {
    fn locate(&self, ctx: &AmbientContext) -> Result<PathBuf, AmbientUnavailable> {
        let mut contexts = self.contexts.lock().unwrap();
        if contexts.len() > 64 {
            contexts.remove(0);
        }
        contexts.push(ctx.clone());
        drop(contexts);
        self.locates.fetch_add(1, Ordering::SeqCst);
        self.reached(Callback::Locate);
        self.path.lock().unwrap().clone()
    }

    fn parser(&self) -> Box<dyn AmbientParser> {
        self.reached(Callback::Parser);
        Box::new(Parser {
            quiet: self.quiet,
            candidate: None,
        })
    }
}

struct Parser {
    quiet: bool,
    candidate: Option<TurnReply>,
}

impl AmbientParser for Parser {
    fn record(&mut self, line: &[u8], reply_bytes: usize) -> Option<AmbientRecord> {
        let value: Value = serde_json::from_slice(line).ok()?;
        match value["type"].as_str()? {
            "reply" => {
                let text = value["text"].as_str()?;
                // Keep at most the limit, flagging the cut, as built-in parsers do.
                let mut keep = text.len().min(reply_bytes);
                while !text.is_char_boundary(keep) {
                    keep -= 1;
                }
                let mut reply = TurnReply::default();
                reply.text = text[..keep].to_string();
                reply.truncated = keep < text.len();
                self.candidate = Some(reply);
                None
            }
            "turn_end" => Some(AmbientRecord::Ended(self.candidate.take())),
            "user" => self
                .candidate
                .take()
                .map(|reply| AmbientRecord::EndedBefore(Some(reply))),
            _ => None,
        }
    }

    fn quiet(&mut self, _: usize) -> Option<AmbientRecord> {
        if !self.quiet {
            return None;
        }
        self.candidate
            .take()
            .map(|reply| AmbientRecord::Ended(Some(reply)))
    }
}

fn reply_line(text: &str) -> String {
    format!("{{\"type\":\"reply\",\"text\":\"{text}\"}}\n")
}

fn turn(text: &str) -> String {
    format!("{}{{\"type\":\"turn_end\"}}\n", reply_line(text))
}

fn append(path: &Path, text: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

/// Fast polling, no quiet inference unless a test asks for it.
fn options() -> AmbientOptions {
    let mut options = AmbientOptions::default();
    options.poll = Duration::from_millis(20);
    options.quiet = Duration::from_secs(600);
    options
}

/// The fake agent as the terminal's direct child.
fn direct(dir: &Path) -> AgentTerminal {
    let mut spec = SpawnSpec::direct(
        fake_agent_path(),
        ["ready"],
        dir,
        PtySize { rows: 24, cols: 80 },
    );
    spec.env = common::test_env(dir);
    // Agent events are not read here: the receiver is dropped.
    spawn_agent(spec, AgentSpec::new(fake_profile("ready", &[]))).0
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

fn text(turn: &AmbientTurn) -> &str {
    turn.reply.as_ref().map_or("", |reply| reply.text.as_str())
}

/// Waits until the workers retired down to `count`.
fn workers_back_to(count: usize) {
    wait_until("ambient workers to retire", || ambient_workers() <= count);
}

#[test]
fn reading_starts_from_now_and_explicit_ends_are_delivered() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    // A complete old turn, and a reply line still being written at attach.
    append(&path, &turn("old"));
    append(&path, "{\"type\":\"reply\",\"text\":\"ha");
    let term = direct(dir.path());
    let source = Source::new(&path, false);
    let attached = SystemTime::now();
    let rx = term
        .attach_ambient_turns_with(source.clone(), options())
        .unwrap();
    active(&rx);
    // The line incomplete at attach is new once it ends.
    append(&path, "lf\"}\n{\"type\":\"turn_end\"}\n");
    let first = next_turn(&rx);
    assert_eq!(text(&first), "half");
    assert_eq!(first.evidence, AmbientEvidence::ExplicitEnd);
    assert!(!first.reply.as_ref().unwrap().truncated);
    append(&path, &turn("new"));
    assert_eq!(text(&next_turn(&rx)), "new");
    assert!(rx.try_recv().is_none(), "the old turn is never reported");

    let ctx = source.last_context().expect("located");
    assert_eq!(ctx.program, term.terminal().pid(), "the direct child");
    assert!(ctx.launched_at.is_some_and(|at| at <= attached));
    assert!(ctx.cwd.is_absolute());
    assert_eq!(ctx.home(), Some(dir.path().to_path_buf()));
}

#[test]
fn a_direct_launch_fence_precedes_post_spawn_setup() {
    struct SpawnLog(Mutex<Option<SystemTime>>);
    impl log::Log for SpawnLog {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            if record.target() == "agent_link::pty::session"
                && record.args().to_string().contains("] spawned ")
            {
                *self.0.lock().unwrap() = Some(SystemTime::now());
            }
        }
        fn flush(&self) {}
    }
    static LOG: SpawnLog = SpawnLog(Mutex::new(None));
    let _serial = serial();
    log::set_logger(&LOG).unwrap();
    log::set_max_level(log::LevelFilter::Debug);
    let dir = tempfile::tempdir().unwrap();
    let term = direct(dir.path());
    let after_spawn = LOG.0.lock().unwrap().expect("child was created");
    let source = Source::new(&dir.path().join("session"), false);
    let rx = term
        .attach_ambient_turns_with(source.clone(), options())
        .unwrap();
    active(&rx);
    assert!(source.last_context().unwrap().launched_at.unwrap() <= after_spawn);
}

#[test]
fn a_quiet_transcript_infers_the_end_after_the_window() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.jsonl");
    let term = direct(dir.path());
    let mut options = options();
    options.quiet = Duration::from_millis(300);
    let rx = term
        .attach_ambient_turns_with(Source::new(&path, true), options)
        .unwrap();
    active(&rx);
    let written = Instant::now();
    append(
        &path,
        &format!("{{\"type\":\"user\"}}\n{}", reply_line("quiet answer")),
    );
    let turn = next_turn(&rx);
    assert!(
        written.elapsed() >= options.quiet,
        "inferred after {:?}",
        written.elapsed()
    );
    assert_eq!(text(&turn), "quiet answer");
    assert_eq!(turn.evidence, AmbientEvidence::InferredAfterQuiet);
}

#[test]
fn a_reply_over_the_limit_is_cut_and_flagged() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let term = direct(dir.path());
    let mut options = options();
    options.limits.reply_bytes = 8;
    let rx = term
        .attach_ambient_turns_with(Source::new(&path, false), options)
        .unwrap();
    active(&rx);
    append(&path, &turn("0123456789abcdef"));
    let turn = next_turn(&rx);
    let reply = turn.reply.unwrap();
    assert_eq!(reply.text, "01234567");
    assert!(reply.truncated);
    assert!(!format!("{reply:?}").contains("0123"), "Debug is redacted");
}

#[test]
fn a_full_queue_closes_with_an_overflow_and_keeps_what_fit() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let term = direct(dir.path());
    let mut options = options();
    options.limits.queue_items = 2;
    let rx = term
        .attach_ambient_turns_with(Source::new(&path, false), options)
        .unwrap();
    active(&rx);
    append(&path, &(turn("a") + &turn("b") + &turn("c")));
    status_until(&rx, "an overflow", |s| {
        s == AmbientStatus::Closed(AmbientClosed::Overflow)
    });
    assert_eq!(text(&rx.try_recv().unwrap()), "a");
    assert_eq!(text(&rx.try_recv().unwrap()), "b");
    assert!(rx.try_recv().is_none());
    drop(term);
}

#[test]
fn stop_and_drop_end_observation_without_waiting_for_the_worker() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let term = direct(dir.path());
    let before = ambient_workers();

    let rx = term
        .attach_ambient_turns_with(Source::new(&path, false), options())
        .unwrap();
    active(&rx);
    rx.stop();
    assert_eq!(rx.status(), AmbientStatus::Closed(AmbientClosed::Stopped));
    workers_back_to(before);
    append(&path, &turn("after stop"));
    assert!(rx.try_recv().is_none(), "nothing is published after stop");

    let rx = term
        .attach_ambient_turns_with(Source::new(&path, false), options())
        .unwrap();
    active(&rx);
    drop(rx);
    workers_back_to(before);

    // A lookup that hangs: stop still returns at once, and nothing is published once the
    // lookup returns.
    let source = Source::new(&path, false);
    let (entered, release) = source.pause_next(Callback::Locate);
    let rx = term
        .attach_ambient_turns_with(source.clone(), options())
        .unwrap();
    entered.recv_timeout(common::SHELL_START_WAIT).unwrap();
    let stopping = Instant::now();
    rx.stop();
    assert!(stopping.elapsed() < Duration::from_secs(1));
    assert_eq!(rx.status(), AmbientStatus::Closed(AmbientClosed::Stopped));
    append(&path, &turn("while hung"));
    release.send(()).unwrap();
    workers_back_to(before);
    assert!(rx.try_recv().is_none());
    assert_eq!(rx.status(), AmbientStatus::Closed(AmbientClosed::Stopped));
}

#[test]
fn the_childs_exit_closes_the_receiver_after_a_last_read() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let term = direct(dir.path());
    let rx = term
        .attach_ambient_turns_with(Source::new(&path, false), options())
        .unwrap();
    active(&rx);
    append(&path, &turn("last words"));
    // Ctrl-C ends the fake agent, and with it the direct session.
    term.interrupt().unwrap();
    status_until(&rx, "the terminal's end", |s| {
        s == AmbientStatus::Closed(AmbientClosed::TerminalEnded)
    });
    assert_eq!(
        text(&rx.try_recv().expect("read a last time")),
        "last words"
    );
    wait_until("the session's end", || term.state() == AgentState::Exited);
    assert_eq!(
        term.attach_ambient_turns_with(Source::new(&path, false), options())
            .unwrap_err(),
        AttachError::NoLaunch
    );
}

#[test]
fn dropping_the_terminal_drains_more_than_64_passes_before_close() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let term = direct(dir.path());
    let before = ambient_workers();
    let mut final_options = options();
    final_options.limits.records = 1;
    final_options.poll = Duration::from_secs(60);
    let rx = term
        .attach_ambient_turns_with(Source::new(&path, false), final_options)
        .unwrap();
    active(&rx);
    // Teardown wakes the worker before its next poll. The reply is beyond 64 passes.
    append(&path, &("{}\n".repeat(80) + &turn("last words")));
    drop(term);
    status_until(&rx, "the terminal's end", |s| {
        s == AmbientStatus::Closed(AmbientClosed::TerminalEnded)
    });
    assert_eq!(
        text(&rx.try_recv().expect("read through EOF")),
        "last words"
    );
    workers_back_to(before);
}

#[test]
fn a_failed_final_transcript_read_reports_incomplete_drain() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let term = direct(dir.path());
    let rx = term
        .attach_ambient_turns_with(Source::new(&path, false), options())
        .unwrap();
    active(&rx);
    // The path was followed while absent. It cannot be read as a transcript on exit.
    std::fs::create_dir(&path).unwrap();
    drop(term);
    status_until(&rx, "a failed final read", |s| {
        s == AmbientStatus::Closed(AmbientClosed::IncompleteDrain)
    });
}

#[test]
fn rewriting_the_departing_transcript_closes_without_a_replacement_turn() {
    struct SwitchAfterFirstLookup {
        departing: PathBuf,
        lookups: AtomicUsize,
    }

    impl AmbientSource for SwitchAfterFirstLookup {
        fn locate(&self, _: &AmbientContext) -> Result<PathBuf, AmbientUnavailable> {
            if self.lookups.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(self.departing.clone());
            }
            // The first live read found no file. On the switch, the old path
            // contains one fresh turn for the departure drain.
            std::fs::write(&self.departing, turn("old")).unwrap();
            Ok(self.departing.with_extension("next"))
        }

        fn accepts_file(&self, _: &AmbientContext, _: &std::fs::File) -> std::io::Result<bool> {
            // The drain captured its EOF before calling the source.
            std::fs::write(&self.departing, turn("new"))?;
            Ok(true)
        }

        fn parser(&self) -> Box<dyn AmbientParser> {
            Box::new(Parser {
                quiet: false,
                candidate: None,
            })
        }
    }

    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let term = direct(dir.path());
    let source = Arc::new(SwitchAfterFirstLookup {
        departing: dir.path().join("old.jsonl"),
        lookups: AtomicUsize::new(0),
    });
    let rx = term.attach_ambient_turns_with(source, options()).unwrap();
    assert_eq!(
        rx.recv_timeout(common::SHELL_START_WAIT).unwrap_err(),
        AmbientRecvError::Closed(AmbientClosed::IncompleteDrain)
    );
}

#[test]
fn a_final_lookup_that_runs_out_of_bytes_reports_incomplete_drain() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let term = direct(dir.path());
    let source = Source::new(&dir.path().join("t.jsonl"), false);
    source.set(Err(AmbientUnavailable::ReadBudgetExceeded));
    let rx = term.attach_ambient_turns_with(source, options()).unwrap();
    status_until(&rx, "an incomplete lookup", |s| {
        s == AmbientStatus::Waiting(AmbientWaiting::Unavailable(
            AmbientUnavailable::ReadBudgetExceeded,
        ))
    });
    drop(term);
    status_until(&rx, "an incomplete final lookup", |s| {
        s == AmbientStatus::Closed(AmbientClosed::IncompleteDrain)
    });
}

#[test]
fn losing_an_observed_transcript_before_departure_reports_incomplete_drain() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let term = direct(dir.path());
    let rx = term
        .attach_ambient_turns_with(Source::new(&path, true), options())
        .unwrap();
    active(&rx);
    // The explicit turn proves the worker read this file. Its next reply is
    // waiting for quiet confirmation when the transcript disappears.
    append(&path, &(turn("complete") + &reply_line("pending")));
    assert_eq!(
        text(&rx.recv_timeout(common::SHELL_START_WAIT).unwrap()),
        "complete"
    );
    std::fs::remove_file(&path).unwrap();
    drop(term);
    assert_eq!(
        rx.recv_timeout(common::SHELL_START_WAIT).unwrap_err(),
        AmbientRecvError::Closed(AmbientClosed::IncompleteDrain)
    );
}

/// Waits on status changes, without polling, until the reader is active.
fn wait_active(rx: &AmbientReceiver) {
    let deadline = Instant::now() + common::SHELL_START_WAIT;
    let mut seen = rx.status();
    while !matches!(seen, AmbientStatus::Active { .. }) {
        assert!(!seen.is_closed(), "closed before reading: {seen:?}");
        let left = deadline.saturating_duration_since(Instant::now());
        seen = rx
            .wait_status_change(seen, left)
            .unwrap_or_else(|| panic!("no active reader; status {seen:?}"));
    }
}

/// The blocking waits on a real terminal: a status wait sees the reader become active, a
/// blocked receive wakes for a turn, `stop` on another thread ends a wait without a
/// deadline, and the child's exit ends a receive after the turn written just before it.
#[test]
fn blocking_waits_follow_the_reader_turns_stop_and_the_childs_exit() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let term = direct(dir.path());
    let rx = term
        .attach_ambient_turns_with(Source::new(&path, false), options())
        .unwrap();
    wait_active(&rx);
    std::thread::scope(|s| {
        let reply = s.spawn(|| rx.recv_timeout(common::SHELL_START_WAIT));
        append(&path, &turn("woken"));
        assert_eq!(text(&reply.join().unwrap().unwrap()), "woken");
    });
    let idle = Duration::from_millis(100);
    let start = Instant::now();
    assert_eq!(
        rx.recv_timeout(idle).unwrap_err(),
        AmbientRecvError::Timeout
    );
    assert!(start.elapsed() >= idle);

    let other = Arc::new(
        term.attach_ambient_turns_with(Source::new(&path, false), options())
            .unwrap(),
    );
    let (done, waited) = crossbeam_channel::bounded(1);
    let waiter = Arc::clone(&other);
    std::thread::spawn(move || {
        let _ = done.send(waiter.recv_timeout(Duration::MAX));
    });
    std::thread::spawn(move || other.stop()).join().unwrap();
    let waited = waited.recv_timeout(common::WAIT).expect("woken by stop");
    assert_eq!(
        waited.unwrap_err(),
        AmbientRecvError::Closed(AmbientClosed::Stopped)
    );

    append(&path, &turn("last words"));
    // Ctrl-C ends the fake agent, and with it the direct session.
    term.interrupt().unwrap();
    let last = rx.recv_timeout(common::SHELL_START_WAIT);
    assert_eq!(text(&last.expect("read a last time")), "last words");
    assert_eq!(
        rx.recv_timeout(common::SHELL_START_WAIT).unwrap_err(),
        AmbientRecvError::Closed(AmbientClosed::TerminalEnded)
    );
}

/// The awaited waits (feature `tokio`) on a real terminal, on a current-thread runtime:
/// activation, a turn, a receive dropped by its timeout that loses nothing, and the
/// terminal's teardown after the turn written just before it.
#[cfg(feature = "tokio")]
#[test]
fn awaited_waits_follow_the_reader_turns_and_the_terminals_teardown() {
    use tokio::time::timeout;

    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let term = direct(dir.path());
    let rx = Arc::new(
        term.attach_ambient_turns_with(Source::new(&path, false), options())
            .unwrap(),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let long = common::SHELL_START_WAIT;
        let mut seen = rx.status();
        while !matches!(seen, AmbientStatus::Active { .. }) {
            assert!(!seen.is_closed(), "closed before reading: {seen:?}");
            seen = timeout(long, rx.status_change(seen))
                .await
                .expect("an active reader");
        }
        let reply = {
            let rx = Arc::clone(&rx);
            tokio::spawn(async move { rx.recv().await })
        };
        tokio::task::yield_now().await;
        append(&path, &turn("awaited"));
        let turn_received = timeout(long, reply).await.expect("woken").unwrap();
        assert_eq!(text(&turn_received.unwrap()), "awaited");

        // A receive dropped by its timeout removed nothing.
        assert!(timeout(Duration::from_millis(50), rx.recv()).await.is_err());
        append(&path, &turn("kept"));
        let kept = timeout(long, rx.recv()).await.expect("woken");
        assert_eq!(text(&kept.unwrap()), "kept");

        append(&path, &turn("last words"));
        // Teardown off the runtime thread; it ends the worker's wait at once.
        term.kill_async().await;
        let last = timeout(long, rx.recv()).await.expect("woken");
        assert_eq!(text(&last.expect("read a last time")), "last words");
        assert_eq!(
            timeout(long, rx.recv()).await.expect("woken").unwrap_err(),
            AmbientRecvError::Closed(AmbientClosed::TerminalEnded)
        );
    });
}

#[test]
fn unavailable_transcripts_report_a_typed_reason_and_switches_read_the_old_one_last() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.jsonl"), dir.path().join("b.jsonl"));
    let term = direct(dir.path());
    let source = Source::new(&a, true);
    source.set(Err(AmbientUnavailable::NotFound));
    let rx = term
        .attach_ambient_turns_with(source.clone(), options())
        .unwrap();
    status_until(&rx, "not found", |s| {
        s == AmbientStatus::Waiting(AmbientWaiting::Unavailable(AmbientUnavailable::NotFound))
    });
    source.set(Ok(dir.path().join("missing").join("t.jsonl")));
    status_until(&rx, "a missing directory", |s| {
        s == AmbientStatus::Waiting(AmbientWaiting::Unavailable(
            AmbientUnavailable::MissingDirectory,
        ))
    });
    source.set(Ok(a.clone()));
    active(&rx);
    // A final message without an end record, then the agent moves to another transcript
    // long before the quiet window: the one it left ends its turn.
    append(&a, &reply_line("left behind"));
    source.set(Ok(b));
    let turn = next_turn(&rx);
    assert_eq!(text(&turn), "left behind");
    assert_eq!(turn.evidence, AmbientEvidence::InferredAtTranscriptEnd);
    active(&rx);
}

/// Source code runs on the worker only: a panic there closes the attachment.
#[test]
fn a_panicking_source_closes_the_attachment() {
    struct Panics;
    impl AmbientSource for Panics {
        fn locate(&self, _: &AmbientContext) -> Result<PathBuf, AmbientUnavailable> {
            panic!("synthetic source failure");
        }
        fn parser(&self) -> Box<dyn AmbientParser> {
            unreachable!("never located")
        }
    }

    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let term = direct(dir.path());
    let rx = term
        .attach_ambient_turns_with(Arc::new(Panics), options())
        .unwrap();
    status_until(&rx, "the worker's panic", |s| {
        s == AmbientStatus::Closed(AmbientClosed::WorkerPanic)
    });
    workers_back_to(0);
    assert!(
        term.terminal().try_wait().is_none(),
        "the terminal keeps running"
    );
}

#[test]
fn attach_errors_are_typed() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let term = direct(dir.path());
    assert_eq!(
        term.attach_ambient_turns(options()).unwrap_err(),
        AttachError::NoAmbientSource,
        "the fake profile has no ambient source"
    );
    let mut invalid = options();
    invalid.poll = Duration::ZERO;
    assert_eq!(
        term.attach_ambient_turns_with(Source::new(&dir.path().join("t"), false), invalid)
            .unwrap_err(),
        AttachError::InvalidLimits
    );
}

#[test]
fn the_worker_budget_is_process_wide() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let term = direct(dir.path());
    let limit = ambient_worker_limit();
    let before = ambient_workers();
    assert_eq!(before, 0);
    set_ambient_worker_limit(1).unwrap();
    let first = term.attach_ambient_turns_with(Source::new(&path, false), options());
    let second = term.attach_ambient_turns_with(Source::new(&path, false), options());
    let refused = matches!(second, Err(AttachError::Busy));
    drop(second);
    let first = first.unwrap();
    first.stop();
    workers_back_to(before);
    let third = term.attach_ambient_turns_with(Source::new(&path, false), options());
    set_ambient_worker_limit(limit).unwrap();
    assert!(refused, "a worker beyond the budget is refused");
    assert!(third.is_ok(), "a retired worker frees its slot");
    assert_eq!(
        set_ambient_worker_limit(0).unwrap_err(),
        AttachError::InvalidLimits
    );
}

/// A shell terminal: nothing is read before the agent's launch or after it returned to
/// the shell; each launch reads from its own start.
#[cfg(unix)]
#[test]
fn a_shell_terminal_follows_launches_and_departures() {
    use agent_link::ForegroundOwner;
    use common::agent::{start_agent_shell, states_until};

    let _serial = serial();
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("t.jsonl");
    append(&path, &turn("before attach"));
    let (term, events) = start_agent_shell(home.path(), AgentSpec::new(fake_profile("ready", &[])));
    // A launch arms the agent's startup again: wait for that launch's own `Ready`.
    let ready_again = |term: &AgentTerminal, events: &crossbeam_channel::Receiver<_>| {
        let mut starting = false;
        states_until(term, events, common::LAUNCH_WAIT, |s, _| {
            starting |= s == AgentState::Starting;
            starting && s == AgentState::Ready
        });
    };
    let source = Source::new(&path, false);
    let rx = term
        .attach_ambient_turns_with(source.clone(), options())
        .unwrap();
    status_until(&rx, "no agent yet", |s| {
        s == AmbientStatus::Waiting(AmbientWaiting::NoAgent)
    });
    append(&path, &turn("typed in the shell"));

    let launched = SystemTime::now();
    term.launch().unwrap();
    ready_again(&term, &events);
    source.located_again();
    active(&rx);
    append(&path, &turn("agent one"));
    let first = next_turn(&rx);
    assert_eq!(text(&first), "agent one");
    wait_until("the agent's program in the context", || {
        source
            .last_context()
            .is_some_and(|ctx| ctx.program.is_some())
    });
    let ctx = source.last_context().unwrap();
    assert!(ctx.launched_at.is_some_and(|at| at >= launched));
    assert_ne!(ctx.program, term.terminal().pid(), "not the shell");

    // The agent exits to the shell: noticed without a delivery.
    term.interrupt().unwrap();
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    status_until(&rx, "the departure", |s| {
        s == AmbientStatus::Waiting(AmbientWaiting::NoAgent)
    });
    append(&path, &turn("while gone"));

    term.launch().unwrap();
    ready_again(&term, &events);
    source.located_again();
    active(&rx);
    append(&path, &turn("agent two"));
    assert_eq!(text(&next_turn(&rx)), "agent two");
    assert!(rx.try_recv().is_none());
    drop(events);
}

#[cfg(unix)]
#[test]
fn a_file_created_by_a_replacement_shell_launch_is_read_only_once() {
    use common::agent::{start_agent_shell, states_until};
    let _serial = serial();
    for at in [Callback::Locate, Callback::Parser] {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("new.jsonl");
        let (term, events) =
            start_agent_shell(home.path(), AgentSpec::new(fake_profile("ready", &[])));
        term.launch().unwrap();
        states_until(&term, &events, common::LAUNCH_WAIT, |s, _| {
            s == AgentState::Ready
        });
        let source = Source::new(&path, false);
        // Pause either before the first file open or in a later lookup of the missing path.
        let first_open = (at == Callback::Parser).then(|| source.pause_next(at));
        let rx = term
            .attach_ambient_turns_with(source.clone(), options())
            .unwrap();
        let (entered, release) = first_open.unwrap_or_else(|| {
            active(&rx);
            source.pause_next(at)
        });
        entered.recv_timeout(common::SHELL_START_WAIT).unwrap();
        term.interrupt().unwrap();
        wait_until("the shell in front", || {
            term.terminal().foreground().owner == agent_link::ForegroundOwner::Shell
        });
        term.launch().unwrap();
        states_until(&term, &events, common::LAUNCH_WAIT, |s, _| {
            s == AgentState::Ready
        });
        append(&path, &turn("replacement"));
        drop(release);
        source.located_again();
        append(&path, &turn("after polling resumed"));
        if std::fs::metadata(&path).unwrap().created().is_ok() {
            assert_eq!(text(&next_turn(&rx)), "replacement");
        }
        assert_eq!(text(&next_turn(&rx)), "after polling resumed");
    }
}

#[cfg(unix)]
#[test]
fn a_suspended_shell_agent_resumes_observation_when_its_group_returns() {
    use common::agent::{start_agent_shell, states_until};
    let _serial = serial();
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("turns.jsonl");
    let (term, events) = start_agent_shell(home.path(), AgentSpec::new(fake_profile("ready", &[])));
    term.launch().unwrap();
    states_until(&term, &events, common::LAUNCH_WAIT, |s, _| {
        s == AgentState::Ready
    });
    let source = Source::new(&path, false);
    let rx = term
        .attach_ambient_turns_with(source.clone(), options())
        .unwrap();
    active(&rx);
    append(&path, &turn("before suspend"));
    assert_eq!(text(&next_turn(&rx)), "before suspend");
    wait_until("identified foreground agent", || {
        source.last_context().is_some_and(|c| c.program.is_some())
    });
    let before = source.last_context().unwrap();

    term.terminal().write(b"\x1a").unwrap();
    status_until(&rx, "suspended agent", |s| {
        s == AmbientStatus::Waiting(AmbientWaiting::NoAgent)
    });
    append(&path, &turn("while suspended"));
    term.terminal().write(b"fg\n").unwrap();
    active(&rx);
    source.located_again();
    let after = source.last_context().unwrap();
    assert_eq!(after.program, before.program);
    assert_eq!(after.launched_at, before.launched_at);
    append(&path, &turn("after resume"));
    assert_eq!(text(&next_turn(&rx)), "after resume");
    assert!(rx.try_recv().is_none(), "no replay across suspension");
}

#[cfg(unix)]
#[test]
fn terminal_end_reconciles_a_relaunch_before_the_final_lookup() {
    use common::agent::{start_agent_shell, states_until};
    struct ByLaunch {
        source: Arc<Source>,
        first: SystemTime,
        replacement: PathBuf,
    }
    impl AmbientSource for ByLaunch {
        fn locate(&self, ctx: &AmbientContext) -> Result<PathBuf, AmbientUnavailable> {
            let original = self.source.locate(ctx)?;
            Ok(if ctx.launched_at.is_some_and(|at| at > self.first) {
                self.replacement.clone()
            } else {
                original
            })
        }
        fn parser(&self) -> Box<dyn AmbientParser> {
            self.source.parser()
        }
    }
    let _serial = serial();
    for drop_terminal in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let old = home.path().join("old.jsonl");
        let new = home.path().join("new.jsonl");
        let (term, events) =
            start_agent_shell(home.path(), AgentSpec::new(fake_profile("ready", &[])));
        term.launch().unwrap();
        states_until(&term, &events, common::LAUNCH_WAIT, |s, _| {
            s == AgentState::Ready
        });
        let source = Source::new(&old, false);
        // Learn the first launch context using a temporary attachment.
        let initial = term
            .attach_ambient_turns_with(source.clone(), options())
            .unwrap();
        active(&initial);
        let first = source.last_context().unwrap().launched_at.unwrap();
        initial.stop();
        workers_back_to(0);
        let rx = term
            .attach_ambient_turns_with(
                Arc::new(ByLaunch {
                    source: source.clone(),
                    first,
                    replacement: new.clone(),
                }),
                options(),
            )
            .unwrap();
        active(&rx);
        append(&old, &turn("first launch"));
        assert_eq!(text(&next_turn(&rx)), "first launch");
        let (entered, release) = source.pause_next(Callback::Locate);
        entered.recv_timeout(common::SHELL_START_WAIT).unwrap();
        term.interrupt().unwrap();
        wait_until("the shell in front", || {
            term.terminal().foreground().owner == agent_link::ForegroundOwner::Shell
        });
        term.launch().unwrap();
        states_until(&term, &events, common::LAUNCH_WAIT, |s, _| {
            s == AgentState::Ready
        });
        append(&new, &turn("replacement final reply"));
        let has_birth_time = std::fs::metadata(&new).unwrap().created().is_ok();
        let term = if drop_terminal {
            drop(term);
            None
        } else {
            term.interrupt().unwrap();
            wait_until("the shell after replacement", || {
                term.terminal().foreground().owner == agent_link::ForegroundOwner::Shell
            });
            term.terminal().write(b"exit\n").unwrap();
            wait_until("terminal exit", || term.state() == AgentState::Exited);
            Some(term)
        };
        drop(release);
        status_until(&rx, "final drain", |s| s.is_closed());
        assert_eq!(
            rx.status(),
            AmbientStatus::Closed(AmbientClosed::TerminalEnded)
        );
        assert!(
            source
                .last_context()
                .unwrap()
                .launched_at
                .is_some_and(|at| at > first)
        );
        if has_birth_time {
            assert_eq!(
                text(&rx.try_recv().expect("replacement drained")),
                "replacement final reply"
            );
        }
        assert!(rx.try_recv().is_none());
        drop(term);
        workers_back_to(0);
    }
}
