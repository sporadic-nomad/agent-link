//! Live smokes: the agent controller against the real CLIs (claude, codex, pi, opencode,
//! cursor) — launch typed into the user's login shell, readiness, a message, a reset, a
//! reset with a message, teardown — plus codex's pre-trust key from a symlinked cwd.
//!
//! All `#[ignore]`: they need the CLIs installed, take minutes and make **real API calls**
//! (at most two short turns per agent, on the user's own account where the agent is signed
//! in). Run one at a time:
//!
//! `cargo test -p agent-link --test vendor_live -- --ignored --test-threads=1 --nocapture <name>`
//!
//! They must leave no record in the user's home: every agent runs under a write-denying
//! macOS sandbox, isolated where the CLI allows it, inside a config guard that fails the
//! test on any change (`vendor_live/live_env.rs`, `vendor_live/guard.rs`). So the turns of
//! isolated agents are error turns (no credentials are copied, credential variables are
//! dropped; codex and pi get a dummy key) — except opencode's anonymous free models, which
//! answer without an account —, a
//! signed-out agent (isolated claude) gets a paste without Enter only, and an agent that
//! must write into the home to start (cursor) fails with that cause. No agent on `PATH`,
//! or no macOS sandbox: the test prints `SKIP` and passes. On a failure the output names
//! the profile, the CLI version, the normalised bottom rows, the marker report and the
//! controller's trace. `AGENT_LINK_LIVE_DUMP=<dir>` also writes each session's raw output
//! and a timeline (output chunks and trace lines, ms since the spawn) there.
#![cfg(all(unix, feature = "agent"))]

mod common;
#[path = "vendor_live/guard.rs"]
mod guard;
#[path = "vendor_live/live_env.rs"]
mod live_env;

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use agent_link::agent::{
    Activity, AgentProfile, AgentSpec, AgentState, AgentTerminal, DeliveryOutcome, Permissions,
    SendOptions, ShellKind, Timing, TrustPolicy,
};
use agent_link::screen::{ScreenText, normalize};
use agent_link::{Event, ForegroundOwner, ProgressState};
use common::logger;
use crossbeam_channel::Receiver;
use live_env::{LiveEnv, descendants, no_pre_trust, survivors};

const READY_WAIT: Duration = Duration::from_secs(60);
const SEND_WAIT: Duration = Duration::from_secs(120);
const REPLY_WAIT: Duration = Duration::from_secs(60);
const PROMPT: &str = "Reply with exactly: OK";
const SECOND: &str = "Reply with exactly: DONE";

#[test]
#[ignore = "live: real claude CLI (isolated, usually signed out); see the module doc"]
fn claude_live() {
    smoke("claude");
}

#[test]
#[ignore = "live: real codex CLI (isolated, dummy API key); see the module doc"]
fn codex_live() {
    smoke("codex");
}

#[test]
#[ignore = "live: real pi CLI (isolated); see the module doc"]
fn pi_live() {
    smoke("pi");
}

#[test]
#[ignore = "live: real opencode CLI (isolated); see the module doc"]
fn opencode_live() {
    smoke("opencode");
}

#[test]
#[ignore = "live: real cursor CLI on the read-only real home; see the module doc"]
fn cursor_live() {
    smoke("cursor");
}

/// codex looks its trusted projects up under its resolved working directory: from a cwd
/// under `/tmp` (macOS: `/private/tmp`) the pre-trust key must be the resolved path. A
/// control launch with the unresolved path as the key (pre-trust off, the dialog left
/// unanswered) shows whether codex needs it.
#[test]
#[ignore = "live: real codex CLI (isolated); see the module doc"]
fn codex_pre_trust_uses_the_resolved_cwd() {
    logger::install();
    let Some(live) = LiveEnv::new("codex", Some(Path::new("/tmp"))) else {
        return;
    };
    let resolved = std::fs::canonicalize(&live.cwd).unwrap();
    if resolved == live.cwd {
        println!(
            "SKIP codex: {} is not a symlinked path here",
            live.cwd.display()
        );
        finish(&live, Ok(()));
        return;
    }
    print_env(&live);
    let result = catch_unwind(AssertUnwindSafe(|| {
        let plan = live.builtin.launch(
            &Permissions::Unattended,
            &TrustPolicy::unattended(),
            &live.cwd,
            ShellKind::Posix,
        );
        let key = format!("projects={{\"{}\"=", resolved.display());
        assert!(
            plan.args.iter().any(|a| a.starts_with(&key)),
            "the pre-trust key is not the resolved cwd: {plan:?}"
        );
        println!(
            "codex: cwd {} → key {}",
            live.cwd.display(),
            resolved.display()
        );

        let mut session = Session::start(&live, live.profile(&[]), live.trust);
        session.launch_until(&[AgentState::Ready, AgentState::TrustPrompt]);
        session.watch_after_ready();
        if session.saw(AgentState::TrustPrompt) {
            session.fail("codex showed its trust dialog despite the pre-trust flag");
        }
        session.close();

        let control = format!(
            "projects={{\"{}\"={{trust_level=\"trusted\"}}}}",
            live.cwd.display()
        );
        let mut session =
            Session::start(&live, live.profile(&["-c".into(), control]), no_pre_trust());
        session.launch_until(&[AgentState::Ready, AgentState::TrustPrompt]);
        session.watch_after_ready();
        println!(
            "codex control (unresolved key, pre-trust off): trust dialog shown: {}",
            session.saw(AgentState::TrustPrompt)
        );
        session.close();
    }));
    finish(&live, result);
}

/// One agent's smoke, inside the guard.
fn smoke(id: &'static str) {
    logger::install();
    let Some(live) = LiveEnv::new(id, None) else {
        return;
    };
    print_env(&live);
    let signed_in = live.signed_in();
    println!(
        "{id}: sign-in check: {}",
        match signed_in {
            Some(true) => "signed in",
            Some(false) => "signed out",
            None => "no check (assumed signed in)",
        }
    );
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut session = Session::start(&live, live.profile(&[]), live.trust);
        session.launch_until(&[AgentState::Ready]);
        if signed_in == Some(false) {
            session.paste_only();
            println!("{id}: submit, reply, reset, reset and send: not run (signed out)");
        } else {
            session.turns();
        }
        session.close();
    }));
    finish(&live, result);
}

fn print_env(live: &LiveEnv) {
    println!(
        "{}: {} — {}, trust {:?}; guard: {}",
        live.id,
        live.version,
        live.home_mode,
        live.trust,
        live.guard_summary()
    );
}

/// Compares the guard (always, also after a failed step) and fails on any change.
fn finish(live: &LiveEnv, result: std::thread::Result<()>) {
    let changes = live.guard_changes();
    if changes.is_empty() {
        println!("{}: guard diff: empty", live.id);
    } else {
        println!("{}: guard diff:\n  {}", live.id, changes.join("\n  "));
    }
    if let Err(panic) = result {
        resume_unwind(panic);
    }
    assert!(
        changes.is_empty(),
        "{}: the run changed guarded config or state: {changes:#?}",
        live.id
    );
}

/// Signals seen during the run (output is dropped by the forwarder, so no receiver fills).
#[derive(Default)]
struct Signals {
    titles: BTreeSet<String>,
    progress: Vec<ProgressState>,
    frames: usize,
    activity: Vec<Activity>,
    /// Output chunks with their arrival time, kept only for `AGENT_LINK_LIVE_DUMP`.
    chunks: Option<Vec<(Instant, Vec<u8>)>>,
}

/// One agent terminal, its events and its teardown.
struct Session {
    id: String,
    version: String,
    profile: Arc<dyn AgentProfile>,
    term: Option<AgentTerminal>,
    events: Receiver<Event>,
    states: Vec<(AgentState, bool)>,
    signals: Arc<Mutex<Signals>>,
    started: Instant,
}

impl Session {
    fn start(live: &LiveEnv, profile: Arc<dyn AgentProfile>, trust: TrustPolicy) -> Session {
        let agent = AgentSpec {
            trust,
            trace: Some(true),
            ..AgentSpec::new(Arc::clone(&profile))
        };
        let started = Instant::now();
        let (term, rx) = {
            let _guard = common::spawn_lock();
            AgentTerminal::spawn(live.spawn_spec(), agent).expect("spawn the agent terminal")
        };
        let (tx, events) = crossbeam_channel::unbounded();
        let signals = Arc::new(Mutex::new(Signals {
            chunks: std::env::var_os("AGENT_LINK_LIVE_DUMP").map(|_| Vec::new()),
            ..Signals::default()
        }));
        let seen = Arc::clone(&signals);
        agent_link::forward_to(rx, move |event| {
            let mut s = seen.lock().unwrap_or_else(PoisonError::into_inner);
            match event {
                Event::Output { bytes, .. } => {
                    if let Some(chunks) = s.chunks.as_mut() {
                        chunks.push((Instant::now(), bytes));
                    }
                }
                Event::Title { title, .. } => {
                    if s.titles.len() < 24 {
                        s.titles.insert(title);
                    }
                }
                Event::Progress { state, .. } => {
                    if s.progress.last() != Some(&state) {
                        s.progress.push(state);
                    }
                }
                Event::Frame { .. } => s.frames += 1,
                Event::Activity { state, .. } => {
                    s.activity.push(state);
                    let _ = tx.send(event);
                }
                other => {
                    let _ = tx.send(other);
                }
            }
        })
        .expect("forward the events");
        let session = Session {
            id: live.id.to_string(),
            version: live.version.clone(),
            profile,
            term: Some(term),
            events,
            states: Vec::new(),
            signals,
            started,
        };
        session.wait_quiet(Duration::from_millis(1500), Duration::from_secs(30));
        session
    }

    fn term(&self) -> &AgentTerminal {
        self.term.as_ref().expect("the session is open")
    }

    /// Waits until the screen showed something and then stayed unchanged for `quiet`
    /// (the login shell's startup files, a reply being drawn), at most `max`.
    fn wait_quiet(&self, quiet: Duration, max: Duration) {
        let deadline = Instant::now() + max;
        let mut last = (u64::MAX, Instant::now());
        while Instant::now() < deadline {
            let generation = self.term().terminal().screen().map_or(0, |s| s.generation);
            if generation != last.0 {
                last = (generation, Instant::now());
            } else if generation > 0 && last.1.elapsed() >= quiet {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// After `Ready`: the `AgentState` events for as long as the gate watches for a dialog
    /// drawn after a ready-looking screen (`Timing::ready_dialog_watch`; codex draws its
    /// loading screen before its trust dialog).
    fn watch_after_ready(&mut self) {
        if !self.saw(AgentState::Ready) {
            return;
        }
        let deadline = Instant::now() + Timing::default().ready_dialog_watch;
        while let Ok(event) = self.events.recv_deadline(deadline) {
            if let Event::AgentState {
                state, blocking, ..
            } = event
            {
                self.states.push((state, blocking));
            }
        }
    }

    /// `launch()`, then the `AgentState` events until one of `until` (failing on `Exited`
    /// or after [`READY_WAIT`]); prints them and the dialogs the gate steered.
    fn launch_until(&mut self, until: &[AgentState]) {
        let started = Instant::now();
        self.term().launch().expect("launch");
        let deadline = started + READY_WAIT;
        loop {
            match self.events.recv_deadline(deadline) {
                Ok(Event::AgentState {
                    state, blocking, ..
                }) => {
                    self.states.push((state, blocking));
                    if until.contains(&state) {
                        break;
                    }
                    if state == AgentState::Exited {
                        self.fail("the session ended during the startup");
                    }
                }
                Ok(Event::Exit { .. }) => self.fail("the shell exited during the startup"),
                Ok(_) => {}
                Err(_) => self.fail(&format!("no {until:?} within {READY_WAIT:?}")),
            }
        }
        println!(
            "{}: states {:?} after {} ms",
            self.id,
            self.states,
            started.elapsed().as_millis()
        );
        for line in self.trace() {
            if line.contains("dialog") || line.contains("ready (") {
                println!("{}: gate: {line}", self.id);
            }
        }
        if self.saw(AgentState::Ready) {
            // Ready by the trust window alone: no screen of the agent was recognised (a
            // reworded CLI, or it exited).
            if self
                .trace()
                .iter()
                .any(|l| l.contains("ready (trust window over)"))
            {
                let screen = self.term().terminal().screen().map(|s| s.text());
                let text = normalize(&screen.unwrap_or_default());
                if text.contains("operation not permitted") || text.contains("readonly database") {
                    self.fail(
                        "the agent could not start: it writes into the home at startup, which \
                         the live sandbox denies — this smoke cannot run under it",
                    );
                }
                self.fail("ready only by the trust-window fallback");
            }
            let owner = self.term().terminal().foreground().owner;
            if owner != ForegroundOwner::Agent {
                self.fail(&format!("the agent is not in front once ready: {owner:?}"));
            }
        }
    }

    fn saw(&self, state: AgentState) -> bool {
        self.states.iter().any(|(s, _)| *s == state)
    }

    /// Signed out: a paste without Enter, which must sit in the composer.
    fn paste_only(&mut self) {
        let outcome = self
            .term()
            .send(PROMPT, SendOptions::default())
            .wait(SEND_WAIT);
        println!("{}: paste (no submit): {outcome:?}", self.id);
        if outcome != Some(DeliveryOutcome::Delivered) {
            self.fail(&format!("the paste ended {outcome:?}"));
        }
        self.wait_quiet(Duration::from_millis(500), Duration::from_secs(5));
        let screen = self.term().terminal().screen().expect("screen model");
        let pending = self.profile.pending_input(&screen.text(), PROMPT);
        println!("{}: pending input after the paste: {pending:?}", self.id);
        if pending != Some(true) {
            self.fail("the pasted text is not read as pending in the composer");
        }
    }

    /// Signed in (or unknown): message, reply, reset, reset and message.
    fn turns(&mut self) {
        let submit = || SendOptions {
            submit: true,
            ..SendOptions::default()
        };
        let started = Instant::now();
        let outcome = self.term().send(PROMPT, submit()).wait(SEND_WAIT);
        println!(
            "{}: send: {outcome:?} after {} ms",
            self.id,
            started.elapsed().as_millis()
        );
        if outcome != Some(DeliveryOutcome::Delivered) {
            self.fail(&format!("the message ended {outcome:?}"));
        }
        self.report_reply(PROMPT, "OK");

        let outcome = self.term().reset().wait(SEND_WAIT);
        println!("{}: reset: {outcome:?}", self.id);
        if outcome != Some(DeliveryOutcome::Delivered) {
            self.fail(&format!("the reset ended {outcome:?}"));
        }
        self.wait_quiet(Duration::from_millis(1500), Duration::from_secs(15));

        // Measured, not asserted: a reset that draws its fresh screen later than
        // `startup_quiet` after its first output loses the message.
        let outcome = self.term().reset_and_send(SECOND, submit()).wait(SEND_WAIT);
        println!("{}: reset and send: {outcome:?}", self.id);
        self.report_reply(SECOND, "DONE");
        let s = self.signals.lock().unwrap_or_else(PoisonError::into_inner);
        println!(
            "{}: signals: titles {:?}, progress {:?}, {} sync frames, activity {:?}",
            self.id, s.titles, s.progress, s.frames, s.activity
        );
    }

    /// Best effort: `reply` on a row below the last row showing `prompt`.
    fn report_reply(&self, prompt: &str, reply: &str) {
        let deadline = Instant::now() + REPLY_WAIT;
        let found = loop {
            let screen = self.term().terminal().screen().expect("screen model");
            if reply_below(&screen.rows, prompt, reply) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        println!("{}: reply {reply:?} below the prompt: {found}", self.id);
        if !found {
            println!("{}", self.bottom_rows(40));
        }
        self.wait_quiet(Duration::from_millis(1500), Duration::from_secs(20));
    }

    /// Drops the terminal; fails when the agent's process (the shell's child) outlives
    /// the teardown. Other leftovers are reported and killed.
    fn close(&mut self) {
        self.dump();
        let Some(left) = self.teardown() else { return };
        let (launchers, others): (Vec<_>, Vec<_>) =
            left.into_iter().partition(|(_, _, child)| *child);
        if !others.is_empty() {
            println!(
                "{}: left running after the teardown (killed now): {others:?}",
                self.id
            );
        }
        assert!(
            launchers.is_empty(),
            "{}: the shell or the agent outlived the teardown: {launchers:?}",
            self.id
        );
        println!("{}: teardown: the shell and the agent are gone", self.id);
    }

    /// With `AGENT_LINK_LIVE_DUMP=<dir>`: `<dir>/<id>-<session>.bin` (the raw output) and
    /// `.timeline.txt` (output chunks and the controller's trace lines in time order, ms
    /// since the spawn) — for triage; they hold whatever the agent drew, so keep them out
    /// of the repository.
    fn dump(&self) {
        let Some(dir) = std::env::var_os("AGENT_LINK_LIVE_DUMP") else {
            return;
        };
        let Some(term) = self.term.as_ref() else {
            return;
        };
        let chunks = self
            .signals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .chunks
            .clone()
            .unwrap_or_default();
        let tag = format!("[agent-link:{}]", term.terminal().id());
        let ms = |at: Instant| at.saturating_duration_since(self.started).as_millis();
        let mut timeline: Vec<(u128, String)> = logger::timed_lines()
            .into_iter()
            .filter(|(_, line)| line.contains(&tag))
            .map(|(at, line)| (ms(at), format!("log {line}")))
            .collect();
        let mut raw = Vec::new();
        for (at, bytes) in &chunks {
            raw.extend_from_slice(bytes);
            let text = String::from_utf8_lossy(bytes)
                .replace('\u{1b}', "⎋")
                .replace('\r', "\\r")
                .replace('\n', "\\n");
            timeline.push((ms(*at), format!("out {text}")));
        }
        timeline.sort_by_key(|(at, _)| *at);
        let base = Path::new(&dir).join(format!("{}-{}", self.id, term.terminal().id()));
        let lines: Vec<String> = timeline
            .iter()
            .map(|(at, what)| format!("{at:>7} {what}"))
            .collect();
        let written = std::fs::write(base.with_extension("bin"), raw).is_ok()
            && std::fs::write(base.with_extension("timeline.txt"), lines.join("\n")).is_ok();
        if written {
            println!(
                "{}: dump in {}.{{bin,timeline.txt}}",
                self.id,
                base.display()
            );
        }
    }

    /// Drops the terminal and returns the processes of its tree still alive 5 s later
    /// (`true`: the shell or its direct child), after killing them.
    fn teardown(&mut self) -> Option<Vec<(u32, String, bool)>> {
        let term = self.term.take()?;
        let Some(shell) = term.terminal().pid() else {
            return Some(Vec::new());
        };
        let direct: BTreeSet<u32> = direct_children(shell).into_iter().collect();
        let mut tree = descendants(shell);
        tree.push((shell, "shell".into()));
        drop(term);
        let left = survivors(&tree, Duration::from_secs(5));
        for (pid, _) in &left {
            // SAFETY: a process of this test's own session tree.
            unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
        }
        Some(
            left.into_iter()
                .map(|(pid, name)| (pid, name, pid == shell || direct.contains(&pid)))
                .collect(),
        )
    }

    fn trace(&self) -> Vec<String> {
        self.term
            .as_ref()
            .map(|t| logger::lines_for(t.terminal().id()))
            .unwrap_or_default()
    }

    /// The last `n` rows up to the lowest non-blank one, normalised.
    fn bottom_rows(&self, n: usize) -> String {
        let Some(screen) = self.term.as_ref().and_then(|t| t.terminal().screen()) else {
            return "no screen".into();
        };
        let text = ScreenText::from(&screen);
        let rows: Vec<String> = text
            .bottom_rows(n)
            .iter()
            .map(|r| format!("  | {}", normalize(r)))
            .collect();
        format!("bottom rows (normalised):\n{}", rows.join("\n"))
    }

    /// Panics with the profile, version, bottom rows, marker report and trace tail.
    fn fail(&self, what: &str) -> ! {
        let trace = self.trace();
        let marker = trace
            .iter()
            .find(|l| l.contains("startup markers not recognised"))
            .cloned()
            .unwrap_or_else(|| "none".into());
        let tail = trace[trace.len().saturating_sub(30)..].join("\n  ");
        panic!(
            "{}: {what}\nprofile {}, version {}\nstates {:?}\n{}\n\
             marker report: {marker}\ntrace tail:\n  {tail}",
            self.id,
            self.id,
            self.version,
            self.states,
            self.bottom_rows(14)
        );
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // A failed step (`close` took the terminal otherwise): the dump for triage, then
        // tear down and clean up, report nothing more.
        self.dump();
        let _ = self.teardown();
    }
}

/// The shell's direct children.
fn direct_children(pid: u32) -> Vec<u32> {
    std::process::Command::new("/usr/bin/pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// `reply` alone on a row (after leading marks such as `⏺`, `•`) below the last row that
/// shows `prompt`.
fn reply_below(rows: &[String], prompt: &str, reply: &str) -> bool {
    let Some(at) = rows.iter().rposition(|r| r.contains(prompt)) else {
        return false;
    };
    rows[at + 1..].iter().any(|row| {
        let text = row
            .trim()
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .trim_end_matches(['.', ' ']);
        text == reply
    })
}

#[test]
fn a_reply_is_read_below_its_prompt_only() {
    let rows = |r: &[&str]| r.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(reply_below(
        &rows(&["❯ Reply with exactly: OK", "", "⏺ OK"]),
        PROMPT,
        "OK"
    ));
    assert!(reply_below(
        &rows(&["› Reply with exactly: OK", "• OK."]),
        PROMPT,
        "OK"
    ));
    assert!(!reply_below(
        &rows(&["OK", "› Reply with exactly: OK"]),
        PROMPT,
        "OK"
    ));
    assert!(!reply_below(
        &rows(&["› Reply with exactly: OK", "• OKAY"]),
        PROMPT,
        "OK"
    ));
}
