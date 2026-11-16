//! Shared helpers for the PTY integration tests: spawns one at a time (`spawn`), a
//! predictable `/bin/sh` session with job control on, waits with deadlines (never a bare
//! sleep) that fail with the bottom screen rows, the fake agent (`fake_agent_path`; `fake`:
//! its test profiles), agent terminals (`agent`), the delivery suites' helpers
//! (`delivery`), a test logger (`logger`), and the recorded agent sessions (`fixtures`).
#![allow(dead_code)] // each test binary uses a different subset

#[cfg(feature = "agent")]
pub mod agent;
#[cfg(all(unix, feature = "agent"))]
pub mod delivery;
#[cfg(feature = "agent")]
pub mod fake;
pub mod fixtures;
pub mod logger;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use agent_link::env::EnvPolicy;
#[cfg(feature = "screen")]
use agent_link::screen::ScreenSnapshot;
use agent_link::shell::ShellSpec;
use agent_link::{Event, PtySize, SpawnMode, SpawnSpec, Terminal};
use crossbeam_channel::{Receiver, RecvTimeoutError};

/// How long any single wait may take before the test fails.
pub const WAIT: Duration = Duration::from_secs(5);

/// How long to wait for a freshly started program's first screen: endpoint-security
/// software may vet every new executable, and under load a start then takes seconds.
pub const LAUNCH_WAIT: Duration = Duration::from_secs(20);

/// How long the first wait after a shell start may take (the prompt waits and
/// [`deadline`]): `/bin/sh` is two execs (macOS: a stub that execs the selected shell), and
/// with a suite's tests starting shells at once, exec vetting has held a shell silent past
/// 20 s. Same bound as `tests/rpc_roundtrip.rs`; a passing wait is not slower.
pub const SHELL_START_WAIT: Duration = Duration::from_secs(60);

/// The deadline for a wait that starts now: [`SHELL_START_WAIT`], since the first wait
/// after [`start`] also covers the shell starting. Only for waits on something that must
/// happen — a passing wait is not slower.
pub fn deadline() -> Instant {
    Instant::now() + SHELL_START_WAIT
}

/// The `agent-link-fake-agent` test-support binary (Cargo builds it for every integration
/// test run, a single `--test` target included).
pub fn fake_agent_path() -> PathBuf {
    #[cfg(feature = "test-support")]
    return PathBuf::from(env!("CARGO_BIN_EXE_agent-link-fake-agent"));
    #[cfg(not(feature = "test-support"))]
    panic!("fake-agent tests require the test-support feature");
}

/// `/bin/sh`, not a login shell.
pub fn test_shell_spec() -> ShellSpec {
    ShellSpec {
        program: Some("/bin/sh".into()),
        login: false,
    }
}

/// [`test_env`]'s rc file: zsh run as `sh` without bracketed paste, beeps or the
/// partial-line marker (as the delivery suites' zsh).
const ZSH_AS_SH_RC: &str = concat!(
    "if [ -n \"${ZSH_VERSION-}\" ]; then\n",
    "  zmodload zsh/zle\n",
    "  unset zle_bracketed_paste\n",
    "  unsetopt beep prompt_sp\n",
    "fi\n",
);

/// The test shell's environment: inherited, plus prompt `$ `, `HOME=home`, and files in
/// `home` that keep the shell's own bracketed paste off — tests read the mode as the fake's
/// own: an inputrc for readline (bash 5.1+, a `/bin/sh` on Linux, turns it on at every
/// prompt) and, as the only rc file (`ENV`), one for zsh run as `sh` (macOS can select it
/// for `/bin/sh`; it reads `ENV`, and turns it on unless `zle_bracketed_paste` is unset
/// after zle loaded). Other shells find nothing to do in that file.
pub fn test_env(home: &Path) -> EnvPolicy {
    let inputrc = home.join(".inputrc-agent-link");
    let _ = std::fs::write(&inputrc, "set enable-bracketed-paste off\n");
    let rc = home.join(".shrc-agent-link");
    let _ = std::fs::write(&rc, ZSH_AS_SH_RC);
    let mut env = EnvPolicy::default();
    env.set("PS1", "$ ")
        .set("ENV", &rc)
        .set("HOME", home)
        .set("INPUTRC", &inputrc);
    env
}

/// [`test_shell_spec`] with [`test_env`] in `home`, 24x80.
pub fn sh_spec(home: &Path) -> SpawnSpec {
    let mut spec = SpawnSpec::shell(home, PtySize { rows: 24, cols: 80 });
    spec.mode = SpawnMode::Shell(test_shell_spec());
    spec.env = test_env(home);
    spec
}

/// `Terminal::spawn`, one at a time per test binary: on macOS, `openpty` called from
/// several threads at once now and then fails with "Unknown error: -6", and a test binary
/// starts most of its sessions in the same instant. The integration tests spawn through
/// it (the library's unit tests through `pty::test_spawn_lock`); the crate itself does not
/// serialise `openpty`.
pub fn spawn(spec: SpawnSpec) -> agent_link::Result<(Terminal, Receiver<Event>)> {
    let _guard = spawn_lock();
    Terminal::spawn(spec)
}

/// The lock [`spawn`] holds; hold it around any other session spawn (`AgentTerminal`).
pub fn spawn_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Spawns `spec` and turns job control on (`set -m`) first: without it `/bin/sh` runs
/// commands in its own process group and the foreground checks cannot tell them apart.
pub fn start(spec: SpawnSpec) -> (Terminal, Receiver<Event>) {
    let (term, events) = spawn(spec).expect("spawn /bin/sh");
    term.write(b"set -m\n").expect("write set -m");
    (term, events)
}

/// [`start`] with [`sh_spec`].
pub fn start_sh(home: &Path) -> (Terminal, Receiver<Event>) {
    start(sh_spec(home))
}

/// [`start_sh`], then waits until the shell has run `set -m` and prints its next `$ `
/// prompt. The returned receiver holds only what came after that prompt. Fails when the
/// shell turned bracketed paste on (see [`test_env`]).
pub fn start_shell(home: &Path) -> (Terminal, Receiver<Event>) {
    start_shell_spec(sh_spec(home))
}

/// [`start_shell`] with another shell `spec` (its prompt must be `$ `).
pub fn start_shell_spec(spec: SpawnSpec) -> (Terminal, Receiver<Event>) {
    let (term, events) = start(spec);
    term.write(b"echo shell-$((20+1))\n").expect("write echo");
    let deadline = Instant::now() + SHELL_START_WAIT;
    let mut text = String::new();
    loop {
        match events.recv_deadline(deadline) {
            Ok(Event::Output { bytes, .. }) => {
                text.push_str(&String::from_utf8_lossy(&bytes));
                // The echo of the typed line shows `$((20+1))`, never `shell-21`.
                if let Some(at) = text.find("shell-21") {
                    if text[at..].contains("$ ") {
                        assert!(
                            !text.contains("\u{1b}[?2004h"),
                            "/bin/sh turned bracketed paste on; extend test_env for it: {text:?}"
                        );
                        return (term, events);
                    }
                }
            }
            Ok(event) => panic!("{event:?} before the prompt; output so far: {text:?}"),
            Err(e) => panic!("no prompt ({e}); output so far: {text:?}"),
        }
    }
}

/// Receives until an event matches `predicate` and returns it; panics at `deadline` or
/// when the receiver disconnects first.
pub fn recv_until(
    rx: &Receiver<Event>,
    deadline: Instant,
    mut predicate: impl FnMut(&Event) -> bool,
) -> Event {
    loop {
        match rx.recv_deadline(deadline) {
            Ok(event) if predicate(&event) => return event,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for an event"),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("the receiver disconnected before the event came")
            }
        }
    }
}

/// Output text received until it contains `needle` (everything up to that point,
/// lossily decoded). Panics at the [`deadline`], on disconnect, or on an `Exit` first.
pub fn output_until(rx: &Receiver<Event>, needle: &str) -> String {
    let deadline = deadline();
    let mut text = String::new();
    loop {
        match rx.recv_deadline(deadline) {
            Ok(Event::Output { bytes, .. }) => {
                text.push_str(&String::from_utf8_lossy(&bytes));
                if text.contains(needle) {
                    return text;
                }
            }
            Ok(event) => panic!("{event:?} before {needle:?}; output so far: {text:?}"),
            Err(e) => panic!("no {needle:?} ({e}); output so far: {text:?}"),
        }
    }
}

/// Polls `ready` until it holds; panics at the deadline.
pub fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = deadline();
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Receives until an event matches `predicate` and returns it. Panics after `timeout` or
/// on disconnect, with the number of other events and `term`'s bottom screen rows.
pub fn wait_event(
    term: &Terminal,
    rx: &Receiver<Event>,
    timeout: Duration,
    mut predicate: impl FnMut(&Event) -> bool,
) -> Event {
    let deadline = Instant::now() + timeout;
    let mut others = 0usize;
    loop {
        match rx.recv_deadline(deadline) {
            Ok(event) if predicate(&event) => return event,
            Ok(_) => others += 1,
            Err(e) => panic!(
                "no matching event within {timeout:?} ({e}; {others} other events); {}",
                screen_report(term)
            ),
        }
    }
}

/// Polls `term.screen()` every 10 ms until `predicate` holds and returns that snapshot.
/// Panics after `timeout` with the bottom screen rows.
#[cfg(feature = "screen")]
pub fn wait_screen(
    term: &Terminal,
    timeout: Duration,
    predicate: impl FnMut(&ScreenSnapshot) -> bool,
) -> ScreenSnapshot {
    wait_screen_for(term, "a matching screen", timeout, predicate)
}

/// [`wait_screen`] for a screen whose text contains `needle`.
#[cfg(feature = "screen")]
pub fn screen_contains(term: &Terminal, needle: &str, timeout: Duration) -> ScreenSnapshot {
    wait_screen_for(term, &format!("{needle:?} on screen"), timeout, |s| {
        s.text().contains(needle)
    })
}

#[cfg(feature = "screen")]
fn wait_screen_for(
    term: &Terminal,
    what: &str,
    timeout: Duration,
    mut predicate: impl FnMut(&ScreenSnapshot) -> bool,
) -> ScreenSnapshot {
    let deadline = Instant::now() + timeout;
    loop {
        let screen = term.screen().expect("the session keeps a screen model");
        if predicate(&screen) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {what}; {}",
            bottom_rows(&screen)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The last 12 rows up to the lowest non-blank one, numbered from 1, for failure messages.
#[cfg(feature = "screen")]
pub fn bottom_rows(screen: &ScreenSnapshot) -> String {
    let end = screen
        .rows
        .iter()
        .rposition(|row| !row.trim().is_empty())
        .map_or(0, |at| at + 1);
    let mut text = format!(
        "bottom rows of {} (title {:?}):",
        screen.size.rows, screen.title
    );
    for (at, row) in screen.rows[..end]
        .iter()
        .enumerate()
        .skip(end.saturating_sub(12))
    {
        text.push_str(&format!("\n{:>3}| {row}", at + 1));
    }
    text
}

/// `term`'s bottom screen rows, or why there are none.
fn screen_report(term: &Terminal) -> String {
    #[cfg(feature = "screen")]
    if let Some(screen) = term.screen() {
        return bottom_rows(&screen);
    }
    let _ = term;
    "no screen model".to_string()
}
