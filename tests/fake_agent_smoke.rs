//! The fake agent (`agent-link-fake-agent`) driven through a bare `Terminal`, scene by
//! scene, and the harness around it: each scene draws what the controller tests rely on,
//! and the fake profile reads those screens. Most sessions type the profile's launch line
//! into `/bin/sh`; exit-code checks run the fake as a direct child.
#![cfg(all(unix, feature = "agent"))]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_link::agent::{AgentProfile, Dialog, DialogKind, ScreenState, ShellKind, TrustKey};
use agent_link::screen::ScreenSnapshot;
use agent_link::{Event, ForegroundOwner, ProgressState, PtySize, SpawnSpec, Terminal};
use common::fake::{FAKE_SCENES, fake_profile, launch_line, unlaunchable_profile};
use common::{
    LAUNCH_WAIT, WAIT, bottom_rows, fake_agent_path, logger, screen_contains, start_shell,
    wait_event, wait_screen, wait_until,
};
use crossbeam_channel::Receiver;
use tempfile::TempDir;

const PASTE_3: &[u8] = b"\x1b[200~line one\nline two\nline three\x1b[201~";
const CHIP_3: &str = "[Pasted text #1 +3 lines]";
const FOOTER: &str = "fake-agent ready · esc to interrupt";

/// A shell with the fake agent typed into it. Its event receiver is dropped (an undrained
/// one would stall the session); screens are read from the model.
struct Fake {
    term: Terminal,
    profile: Arc<dyn AgentProfile>,
    /// When the launch line was written: the fake cannot have started earlier.
    launched_at: Instant,
    _home: TempDir,
}

impl Fake {
    fn launch(scene: &str, flags: &[&str]) -> Self {
        let home = tempfile::tempdir().unwrap();
        let (term, _events) = start_shell(home.path());
        let profile = fake_profile(scene, flags);
        let launched_at = Instant::now();
        term.write(launch_line(&*profile, home.path()).as_bytes())
            .unwrap();
        Self {
            term,
            profile,
            launched_at,
            _home: home,
        }
    }

    /// Waits for the ready composer (the first screen may take a while to come).
    fn ready(&self) -> ScreenSnapshot {
        wait_screen(&self.term, LAUNCH_WAIT, |s| {
            self.profile.classify(&s.text()) == ScreenState::InputReady
        })
    }

    fn state(&self) -> ScreenState {
        self.profile.classify(&self.screen().text())
    }

    fn screen(&self) -> ScreenSnapshot {
        self.term.screen().unwrap()
    }

    fn contains(&self, needle: &str) -> ScreenSnapshot {
        screen_contains(&self.term, needle, WAIT)
    }

    /// Waits until the fake has started and returns when that was seen: its first output
    /// turns bracketed paste on (the test shell does not: `test_env` turns readline's off)
    /// and starts its timers.
    fn started(&self) -> Instant {
        wait_screen(&self.term, LAUNCH_WAIT, |s| s.bracketed_paste);
        Instant::now()
    }
}

/// Rows up to the lowest non-blank one.
fn content(screen: &ScreenSnapshot) -> Vec<&str> {
    let end = screen
        .rows
        .iter()
        .rposition(|r| !r.is_empty())
        .map_or(0, |at| at + 1);
    screen.rows[..end].iter().map(String::as_str).collect()
}

fn dialog(key: TrustKey) -> ScreenState {
    ScreenState::Dialog(Dialog {
        kind: DialogKind::Trust,
        key: Some(key),
    })
}

/// The fake run directly on a 40x120 PTY.
fn direct(args: &[&str]) -> (Terminal, Receiver<Event>, TempDir) {
    let home = tempfile::tempdir().unwrap();
    let size = PtySize {
        rows: 40,
        cols: 120,
    };
    let spec = SpawnSpec::direct(fake_agent_path(), args.iter().copied(), home.path(), size);
    let (term, events) = common::spawn(spec).unwrap();
    (term, events, home)
}

/// Waits for the direct child's `Exit` and returns its code.
fn exit_code(term: &Terminal, events: &Receiver<Event>) -> Option<i32> {
    wait_event(term, events, LAUNCH_WAIT, |e| {
        matches!(e, Event::Exit { .. })
    });
    // The output can end a moment before the child is waitable.
    wait_until("the exit status", || term.try_wait().is_some());
    term.try_wait().unwrap().status
}

#[test]
fn ready_draws_the_composer_with_bracketed_paste_on() {
    let fake = Fake::launch("ready", &[]);
    let screen = fake.ready();
    assert_eq!(
        content(&screen),
        ["fake-agent v1 (ready)", ">", FOOTER],
        "{}",
        bottom_rows(&screen)
    );
    assert!(screen.bracketed_paste);
    assert_eq!(screen.cursor, (1, 2), "the cursor waits after `> `");
    assert_eq!(fake.term.foreground().owner, ForegroundOwner::Agent);
    assert_eq!(
        fake.profile.pending_input(&screen.text(), "hi"),
        Some(false)
    );
}

#[test]
fn a_multi_line_paste_shows_a_chip_and_enter_submits_it() {
    let fake = Fake::launch("composer", &[]);
    fake.ready();
    fake.term.write(PASTE_3).unwrap();
    let screen = fake.contains(CHIP_3);
    assert_eq!(content(&screen)[1], format!("> {CHIP_3}"));
    let sent = "line one\nline two\nline three";
    assert_eq!(fake.profile.pending_input(&screen.text(), sent), Some(true));

    fake.term.write(b"\r").unwrap();
    let screen = fake.contains("agent: ok #1");
    let you = format!("you: {CHIP_3}");
    let expected = [
        "fake-agent v1 (composer)",
        &you,
        "agent: ok #1",
        ">",
        FOOTER,
    ];
    assert_eq!(content(&screen), expected);
    assert_eq!(
        fake.profile.pending_input(&screen.text(), sent),
        Some(false)
    );

    // A one-line paste lands inline; the next reply is #2.
    fake.term.write(b"\x1b[200~hello there\x1b[201~").unwrap();
    fake.contains("> hello there");
    fake.term.write(b"\r").unwrap();
    let screen = fake.contains("agent: ok #2");
    assert!(content(&screen).contains(&"you: hello there"));
}

#[test]
fn the_trust_dialog_is_steered_down_then_confirmed() {
    let fake = Fake::launch("trust", &[]);
    let screen = screen_contains(&fake.term, "fake trust dialog", LAUNCH_WAIT);
    assert_eq!(
        content(&screen)[1..],
        [
            "fake trust dialog",
            "❯ 1. No, exit",
            "  2. Yes, trust this folder",
            "enter to confirm"
        ]
    );
    assert_eq!(fake.state(), dialog(TrustKey::Down));
    fake.term.write(TrustKey::Down.bytes()).unwrap();
    fake.contains("❯ 2. Yes, trust this folder");
    assert_eq!(fake.state(), dialog(TrustKey::Enter));
    // Up moves the pointer back.
    fake.term.write(TrustKey::Up.bytes()).unwrap();
    fake.contains("❯ 1. No, exit");
    fake.term.write(b"\x1b[B\r").unwrap();
    let screen = fake.ready();
    assert!(!screen.text().contains("fake trust dialog"));
}

#[test]
fn no_in_the_trust_dialog_says_bye_and_exits_1() {
    let (term, events, _home) = direct(&["trust"]);
    screen_contains(&term, "❯ 1. No, exit", LAUNCH_WAIT);
    term.write(b"\r").unwrap();
    assert_eq!(exit_code(&term, &events), Some(1));
    let screen = term.screen().unwrap();
    assert_eq!(content(&screen), ["fake-agent v1 (trust)", "bye"]);
    assert!(!screen.bracketed_paste, "bracketed paste is off after exit");
}

#[test]
fn swallow_absorbs_an_enter_right_after_a_paste() {
    let fake = Fake::launch("swallow", &[]);
    fake.ready();
    // Paste and Enter in one write: the Enter arrives well within 30 ms.
    let mut bytes = PASTE_3.to_vec();
    bytes.push(b'\r');
    fake.term.write(&bytes).unwrap();
    let screen = fake.contains("enter-absorbed");
    let composer = format!("> {CHIP_3}");
    assert_eq!(
        content(&screen)[1..3],
        ["enter-absorbed", composer.as_str()]
    );
    // Measured from the moment the absorbed Enter was seen, so the fake's own delay cannot
    // shrink the gap below its 100 ms window.
    std::thread::sleep(Duration::from_millis(200));
    fake.term.write(b"\r").unwrap();
    let screen = fake.contains("agent: ok #1");
    let rows = content(&screen);
    assert_eq!(rows.iter().filter(|r| **r == "enter-absorbed").count(), 1);
    assert!(
        rows.contains(&format!("you: {CHIP_3}").as_str()),
        "{rows:?}"
    );
    assert_eq!(rows[rows.len() - 2], ">");
}

#[test]
fn swallow_ms_widens_the_window() {
    let fake = Fake::launch("swallow", &["--swallow-ms", "2000"]);
    fake.ready();
    fake.term.write(PASTE_3).unwrap();
    fake.contains(CHIP_3);
    // Past the default 100 ms, well inside 2 s.
    std::thread::sleep(Duration::from_millis(300));
    fake.term.write(b"\r").unwrap();
    let screen = fake.contains("enter-absorbed");
    assert!(!screen.text().contains("you:"), "{}", bottom_rows(&screen));
}

/// A short `--dialog-after-ms`, far below the fake's 1.5 s default.
const SHORT_DIALOG: &str = "100";

/// Splits a dialog after the [`SHORT_DIALOG`] delay from one after the default, with
/// ~700 ms of slack on either side for a loaded host (the start is seen late by the screen
/// polling plus scheduling; the dialog, by the same).
const SHORT_DIALOG_MAX: Duration = Duration::from_millis(800);

/// Launches `late-dialog` with `flags`; returns the time to the dialog from the launch
/// line (a lower bound: the fake cannot start earlier) and from the start the screen showed
/// (an upper bound), after checking the composer came first.
fn late_dialog_after(flags: &[&str]) -> (Duration, Duration) {
    let fake = Fake::launch("late-dialog", flags);
    let started = fake.started();
    let screen = fake.ready();
    assert!(!screen.text().contains("fake trust dialog"));
    let screen = screen_contains(&fake.term, "fake trust dialog", LAUNCH_WAIT);
    let elapsed = (fake.launched_at.elapsed(), started.elapsed());
    assert_eq!(
        fake.profile.classify(&screen.text()),
        dialog(TrustKey::Down)
    );
    assert!(!fake.profile.input_ready(&screen.text()));
    elapsed
}

#[test]
fn late_dialog_comes_after_its_default_delay() {
    let (since_launch, since_start) = late_dialog_after(&[]);
    assert!(
        since_launch >= Duration::from_millis(1500),
        "{since_launch:?}"
    );
    assert!(since_start > SHORT_DIALOG_MAX, "{since_start:?}");
}

#[test]
fn late_dialog_honours_dialog_after_ms() {
    let (since_launch, since_start) = late_dialog_after(&["--dialog-after-ms", SHORT_DIALOG]);
    assert!(
        since_launch >= Duration::from_millis(100),
        "{since_launch:?}"
    );
    assert!(since_start < SHORT_DIALOG_MAX, "{since_start:?}");
}

#[test]
fn slow_dialog_draws_nothing_before_its_dialog() {
    let fake = Fake::launch("slow-dialog", &["--dialog-after-ms", SHORT_DIALOG]);
    let started = fake.started();
    let screen = wait_screen(&fake.term, LAUNCH_WAIT, |s| {
        assert!(!s.text().contains(FOOTER), "a composer before the dialog");
        s.text().contains("fake-agent v1")
    });
    let since_start = started.elapsed();
    assert!(
        screen.text().contains("fake trust dialog"),
        "{}",
        bottom_rows(&screen)
    );
    assert!(fake.launched_at.elapsed() >= Duration::from_millis(100));
    assert!(since_start < SHORT_DIALOG_MAX, "{since_start:?}");
    fake.term.write(b"\x1b[B\r").unwrap();
    fake.ready();
}

/// A dialog due while `/new` resets comes with the fresh screen, not never.
#[test]
fn a_late_dialog_due_during_a_reset_follows_the_fresh_screen() {
    // `/new` must land before the dialog: 2 s leaves a loaded host room.
    let flags = ["--dialog-after-ms", "2000", "--reset-delay-ms", "3000"];
    let fake = Fake::launch("late-dialog", &flags);
    fake.ready();
    fake.term.write(b"/new\r").unwrap();
    let screen = fake.contains("resetting conversation");
    assert!(
        !screen.text().contains("fake trust dialog"),
        "the dialog came before the reset; {}",
        bottom_rows(&screen)
    );
    // The reset runs past the dialog's deadline (2 s after start, the reset ends ~3 s
    // after it began); the dialog follows the fresh screen.
    let screen = fake.contains("fake trust dialog");
    assert!(!screen.text().contains("resetting conversation"));
    assert_eq!(
        fake.state(),
        dialog(TrustKey::Down),
        "{}",
        bottom_rows(&screen)
    );
}

/// The fake exits at the submit itself: a controller resolves a verified Enter only after
/// 250 ms of quiet, so by then the shell must be in front (never a fake about to exit).
#[test]
fn exit_to_shell_exits_at_the_first_submit() {
    let fake = Fake::launch("exit-to-shell", &[]);
    fake.ready();
    assert_eq!(fake.term.foreground().owner, ForegroundOwner::Agent);
    let sent = Instant::now();
    fake.term.write(b"hello\r").unwrap();
    wait_until("the shell back in front", || {
        fake.term.foreground().owner == ForegroundOwner::Shell
    });
    let gone_after = sent.elapsed();
    let screen = wait_screen(&fake.term, WAIT, |s| content(s).last() == Some(&"$"));
    // No reply: it would have come 300 ms after the Enter.
    assert_eq!(
        content(&screen),
        ["fake-agent v1 (exit-to-shell)", "you: hello", "$"]
    );
    // Loose (a loaded host): shell reaping and polling are in it; the rows above prove no
    // reply came.
    assert!(gone_after < WAIT, "{gone_after:?}");
    assert!(!screen.bracketed_paste);
    // The terminal is back in cooked mode: the shell reads and runs a line.
    fake.term.write(b"echo after-$((2+3))\n").unwrap();
    fake.contains("after-5");
}

#[test]
fn interrupt_exits_130_and_restores_the_terminal() {
    let fake = Fake::launch("ready", &[]);
    fake.ready();
    fake.term.write(b"draft").unwrap();
    fake.contains("> draft");
    fake.term.interrupt().unwrap();
    wait_until("the shell back in front", || {
        fake.term.foreground().owner == ForegroundOwner::Shell
    });
    fake.term.write(b"echo status-$?\n").unwrap();
    let screen = fake.contains("status-130");
    assert!(!screen.text().contains(FOOTER), "the composer was erased");
    assert!(!screen.bracketed_paste);
}

#[test]
fn progress_reports_progress_frames_and_a_spinner_title() {
    let (term, events, _home) = direct(&["progress", "--sync", "--title"]);
    let profile = fake_profile("progress", &["--sync", "--title"]);
    let mut progress = Vec::new();
    let mut titles = Vec::new();
    let mut frames = 0;
    let mut submitted = false;
    wait_event(&term, &events, LAUNCH_WAIT, |event| {
        match event {
            Event::Output { .. } if !submitted => {
                if profile.input_ready(&term.screen().unwrap().text()) {
                    term.write(b"go\r").unwrap();
                    submitted = true;
                }
            }
            Event::Progress { state, .. } => progress.push(*state),
            Event::Title { title, .. } => titles.push(title.clone()),
            Event::Frame { .. } => frames += 1,
            _ => {}
        }
        progress.last() == Some(&ProgressState::Clear)
    });
    assert_eq!(
        progress,
        [ProgressState::Indeterminate, ProgressState::Clear]
    );
    assert!(frames >= 2, "{frames} frames");
    let braille = |t: &String| t.starts_with(|c| ('\u{2800}'..='\u{28ff}').contains(&c));
    let first_spin = titles.iter().position(braille).expect("a spinner title");
    assert_eq!(titles[first_spin], "⠋ fake");
    assert_eq!(titles.first().map(String::as_str), Some("fake"));
    assert_eq!(
        titles.last().map(String::as_str),
        Some("fake"),
        "{titles:?}"
    );
    let hints = profile.signals();
    assert!(hints.title_spinner);
    let screen = term.screen().unwrap();
    assert!(content(&screen).contains(&"agent: ok #1"));
}

/// A reset and ^C both end what a submit started: progress cleared, spinner title back to
/// `fake`. ^C reaches the main loop (not a signal handler), so it exits between writes.
#[test]
fn reset_and_interrupt_end_the_working_signals() {
    let (term, events, _home) = direct(&["progress", "--sync", "--title"]);
    let mut progress = Vec::new();
    let mut titles = Vec::new();
    let mut record = |event: &Event| match event {
        Event::Progress { state, .. } => progress.push(*state),
        Event::Title { title, .. } => titles.push(title.clone()),
        _ => {}
    };
    screen_contains(&term, FOOTER, LAUNCH_WAIT);
    // Read together: the reset cancels the reply before a spinner frame was drawn.
    term.write(b"go\r/new\r").unwrap();
    wait_event(&term, &events, WAIT, |e| {
        record(e);
        matches!(
            e,
            Event::Progress {
                state: ProgressState::Clear,
                ..
            }
        )
    });
    screen_contains(&term, FOOTER, WAIT);
    term.write(b"go\r").unwrap();
    wait_event(&term, &events, WAIT, |e| {
        record(e);
        matches!(e, Event::Title { title, .. } if title.starts_with('⠋'))
    });
    term.interrupt().unwrap();
    wait_event(&term, &events, WAIT, |e| {
        record(e);
        matches!(e, Event::Exit { .. })
    });
    wait_until("the exit status", || term.try_wait().is_some());
    assert_eq!(term.try_wait().unwrap().status, Some(130));
    assert_eq!(
        progress,
        [
            ProgressState::Indeterminate,
            ProgressState::Clear,
            ProgressState::Indeterminate,
            ProgressState::Clear
        ]
    );
    assert_eq!(
        titles.last().map(String::as_str),
        Some("fake"),
        "{titles:?}"
    );
    let screen = term.screen().unwrap();
    assert!(!screen.text().contains(FOOTER), "{}", bottom_rows(&screen));
}

/// A lone ESC is the Esc key (ignored); the byte after it still counts.
#[test]
fn a_lone_escape_keeps_the_next_byte() {
    let fake = Fake::launch("ready", &[]);
    fake.ready();
    fake.term.write(b"h\x1bi\x1b\r").unwrap();
    let screen = fake.contains("agent: ok #1");
    assert_eq!(content(&screen)[1], "you: hi", "{}", bottom_rows(&screen));
}

/// Backspace takes a character, then the whole chip; an empty paste adds nothing.
#[test]
fn backspace_removes_characters_then_the_chip() {
    let fake = Fake::launch("composer", &[]);
    fake.ready();
    let mut bytes = PASTE_3.to_vec();
    bytes.extend_from_slice(b"x\x1b[200~\x1b[201~");
    fake.term.write(&bytes).unwrap();
    fake.contains(&format!("> {CHIP_3}x"));
    fake.term.write(b"\x7f\x7f").unwrap();
    wait_screen(&fake.term, WAIT, |s| content(s).get(1) == Some(&">"));
    // Nothing left to submit.
    fake.term.write(b"ab\x7f\x7f\r").unwrap();
    fake.term.write(b"c").unwrap();
    let screen = fake.contains("> c");
    assert!(!screen.text().contains("you:"), "{}", bottom_rows(&screen));
}

#[test]
fn new_resets_after_the_silence_and_the_delay() {
    // The silence is long, so a loaded host's late look still falls inside it.
    let flags = ["--reset-silence-ms", "1000", "--reset-delay-ms", "200"];
    let fake = Fake::launch("ready", &flags);
    fake.ready();
    fake.term.write(b"hello\r").unwrap();
    fake.contains("agent: ok #1");
    fake.term.write(b"/new").unwrap();
    let screen = fake.contains("> /new");
    assert_eq!(
        fake.profile.pending_input(&screen.text(), "/new"),
        Some(true)
    );
    assert_eq!(fake.profile.reset_command(), Some("/new"));

    let sent = Instant::now();
    fake.term.write(b"\r").unwrap();
    // The fake read the Enter after `sent`, so it stays silent until at least sent + 1 s.
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        fake.screen().text().contains("> /new"),
        "drew during the silence"
    );
    // Input during the reset is dropped.
    fake.term.write(b"x").unwrap();
    let screen = fake.contains("resetting conversation");
    assert_eq!(fake.profile.classify(&screen.text()), ScreenState::Unknown);
    let screen = fake.ready();
    assert!(sent.elapsed() >= Duration::from_millis(1200));
    assert_eq!(content(&screen), ["fake-agent v1 (ready)", ">", FOOTER]);
    // A fresh conversation counts replies from 1 again.
    fake.term.write(b"again\r").unwrap();
    fake.contains("agent: ok #1");
}

#[test]
fn silent_never_draws_a_composer_but_still_answers() {
    let fake = Fake::launch("silent", &[]);
    let screen = screen_contains(&fake.term, "fake-agent v1 (silent)", LAUNCH_WAIT);
    assert_eq!(fake.profile.classify(&screen.text()), ScreenState::Unknown);
    fake.term.write(b"hi\r").unwrap();
    let screen = fake.contains("agent: ok #1");
    assert_eq!(
        content(&screen),
        ["fake-agent v1 (silent)", "you: hi", "agent: ok #1"]
    );
    assert_eq!(fake.state(), ScreenState::Unknown);
    assert_eq!(fake.profile.pending_input(&screen.text(), "hi"), None);
}

#[test]
fn a_bad_scene_or_flag_exits_2_with_usage() {
    let scenes = format!("scenes: {}", FAKE_SCENES.join(", "));
    for (args, error) in [
        (&["nope"][..], r#"unknown scene "nope""#),
        (
            &["ready", "--dialog-after-ms", "soon"],
            r#"--dialog-after-ms: "soon" is not a number of milliseconds"#,
        ),
        (&["ready", "--x"], r#"unknown flag "--x""#),
    ] {
        let (term, events, _home) = direct(args);
        assert_eq!(exit_code(&term, &events), Some(2), "{args:?}");
        let screen = term.screen().unwrap();
        let rows = content(&screen);
        assert_eq!(
            rows,
            [
                format!("agent-link-fake-agent: {error}").as_str(),
                "usage: agent-link-fake-agent <scene> [--sync] [--title] [--exit-on-input]",
                "       [--reset-delay-ms N] [--reset-silence-ms N] [--dialog-after-ms N]",
                "       [--swallow-ms N] [--keep-frame] [--hold-submit-display]",
                &scenes,
            ],
            "{args:?}"
        );
    }
}

#[test]
fn the_profiles_launch_the_fake_and_false() {
    let cwd = std::path::Path::new("/tmp");
    let line = launch_line(&*fake_profile("ready", &["--sync"]), cwd);
    let path = fake_agent_path();
    let quoted = ShellKind::Posix.quote(path.to_str().unwrap()).unwrap();
    assert_eq!(line, format!("{quoted} ready --sync\n"));
    assert_eq!(launch_line(&*unlaunchable_profile(), cwd), "false\n");
    let profile = fake_profile("ready", &[]);
    assert_eq!(profile.launchers(), ["agent-link-fake-agent"]);
    assert_eq!(profile.login_check(), None);
    assert_eq!(profile.rpc_launch(false, false), None);
    // The shell's echo of the launch line is not the ready footer.
    let echo = format!("$ {quoted} ready\n");
    assert!(!profile.input_ready(&echo));
    assert_eq!(profile.classify(&echo), ScreenState::Unknown);
}

#[test]
fn the_test_logger_collects_session_lines() {
    logger::install();
    let (term, events, _home) = direct(&["nope"]);
    let id = term.id();
    exit_code(&term, &events);
    drop(term);
    logger::wait_line(id, "spawned", WAIT);
    logger::wait_line(id, "torn down", WAIT);
}
