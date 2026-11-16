//! `AgentTerminal::send` and who owns the terminal: only the program this terminal's own
//! launch line started gets text. Against the fake agent, its last frame left on screen
//! where it matters (`--keep-frame`, as an inline agent leaves it): a nested shell started
//! after the agent exited — unnoticed, the gate still running, or after the crate saw it
//! leave — or after a launch that started no agent; programs and an agent started by hand
//! (nothing typed, `foreground_unproven`); a relaunch over the leftover frame (the gate
//! waits for the new agent's own screen: delivered, its dialog answered); an agent that
//! exits from a dialog left to the user, the dialog left on screen (the waiting send ends);
//! and a program started while a launch line is held back after its Ctrl-C (it gets neither).
#![cfg(all(unix, feature = "agent"))]

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_link::agent::{
    AbortReason, AgentSpec, AgentState, AgentTerminal, DeliveryOutcome, SendOptions, ShellFallback,
    Timing, TrustPolicy,
};
use agent_link::screen::ScreenSnapshot;
use agent_link::shell::ShellSpec;
use agent_link::{Event, ForegroundOwner, SpawnMode};
use common::agent::{start_agent_shell, start_agent_shell_spec, states_until};
use common::delivery::{
    assert_launched_on_a_clear_line, assert_nothing_ran, collect_output, fake, long_window,
    outcome, outcome_where, outcome_with_output, outcome_with_states, shell, short_window, submit,
};
use common::fake::{READY_MARKER, fake_profile, launch_line, unlaunchable_profile};
use common::{
    LAUNCH_WAIT, SHELL_START_WAIT, WAIT, bottom_rows, logger, screen_contains, sh_spec,
    wait_screen, wait_until,
};
use crossbeam_channel::Receiver;

use DeliveryOutcome::{Aborted, Delivered};

/// An inline agent leaves its last frame — ready footer included — when it exits. The next
/// sends relaunch it over that frame: the startup gate waits for the new agent's own
/// screen (never the leftover one, read while the shell is still in front and the launch
/// line not even typed), so each message — one line, then several — reaches the new agent
/// instead of the shell's line, which the launch line would clear.
#[test]
fn a_relaunch_over_a_leftover_frame_delivers_to_the_new_agent() {
    let (term, events, _home) = fake("exit-to-shell", &["--keep-frame"]);
    let sends = [
        ("the first message", "you: the first message"),
        ("the second message", "you: the second message"),
        ("third\nand last", "you: [Pasted text #1 +2 lines]"),
    ];
    for (n, (text, shown)) in sends.into_iter().enumerate() {
        if n > 0 {
            wait_until("the shell in front", || {
                term.terminal().foreground().owner == ForegroundOwner::Shell
            });
            screen_contains(term.terminal(), READY_MARKER, WAIT);
        }
        let ticket = term.send(text, submit());
        assert_eq!(outcome(&term, &events, &ticket), Delivered, "{text:?}");
        // Each fake redraws from the top: the transcript is this one's.
        screen_contains(term.terminal(), shown, WAIT);
    }
}

/// The relaunched agent shows its trust dialog below the frame the last one left: the gate
/// answers it (its window was not closed by a false ready on the leftover screen) and the
/// message is delivered to the new agent.
#[test]
fn a_relaunched_agent_s_dialog_is_answered_over_a_leftover_frame() {
    let (term, events, _home) = fake("trust", &["--keep-frame"]);
    let first = term.send("the first message", submit());
    assert_eq!(outcome(&term, &events, &first), Delivered);
    screen_contains(term.terminal(), "you: the first message", WAIT);
    // The user quits the agent; its frame stays.
    term.terminal().write(b"\x03").unwrap();
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    screen_contains(term.terminal(), READY_MARKER, WAIT);
    let mut states = Vec::new();
    let second = term.send("the second message", submit());
    assert_eq!(
        outcome_with_states(&term, &events, &second, &mut states),
        Delivered
    );
    screen_contains(term.terminal(), "you: the second message", WAIT);
    assert!(
        states.contains(&(AgentState::TrustPrompt, true)),
        "the new dialog was steered: {states:?}"
    );
}

/// The user started the agent by hand before any `launch()`: a send finds a program in
/// front and types nothing into it — no launch line (it would land in the agent's
/// composer), no text: it is not a program this terminal launched, and its ready composer
/// proves nothing (a leftover frame under a nested shell shows the same).
#[test]
fn an_agent_started_by_hand_gets_nothing() {
    logger::install();
    let home = tempfile::tempdir().unwrap();
    let profile = fake_profile("composer", &[]);
    let (term, events) = start_agent_shell(
        home.path(),
        AgentSpec {
            timing: long_window(),
            trace: Some(true),
            ..AgentSpec::new(Arc::clone(&profile))
        },
    );
    let line = launch_line(&*profile, home.path());
    term.terminal().write(line.as_bytes()).unwrap();
    screen_contains(term.terminal(), READY_MARKER, LAUNCH_WAIT);
    wait_until("the agent in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Agent
    });
    let mut states = Vec::new();
    let ticket = term.send("hello", submit());
    assert_eq!(
        outcome_with_states(&term, &events, &ticket, &mut states),
        Aborted(AbortReason::ForegroundUnproven)
    );
    std::thread::sleep(Duration::from_millis(500));
    // Not in the composer, not submitted.
    let screen = term.terminal().screen().unwrap();
    assert!(!screen.text().contains("hello"), "{}", bottom_rows(&screen));
    assert!(states.is_empty(), "never launched: {states:?}");
    assert_eq!(term.state(), AgentState::ShellOnly);
    let lines = logger::lines_for(term.terminal().id());
    assert!(
        lines
            .iter()
            .any(|l| l.contains("a program this terminal did not launch owns it: not pasted")),
        "{lines:#?}"
    );
}

/// No agent runs and the user started another program by hand — a nested shell, `cat`,
/// `sleep` (the terminal would queue the text for the shell): no launch of this terminal
/// started it, so nothing is typed into it — no launch line, no text — and the delivery
/// ends `foreground_unproven`, a paste-only one too.
#[test]
fn a_program_started_by_hand_gets_nothing_while_no_agent_runs() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: long_window(),
        trace: Some(true),
        ..AgentSpec::new(fake_profile("composer", &[]))
    });
    let mut output = String::new();
    let mut states = Vec::new();
    for (program, end) in [("sh", "exit\n"), ("cat", "\u{4}"), ("sleep 2", "")] {
        term.terminal()
            .write(format!("{program}\n").as_bytes())
            .unwrap();
        wait_until("the program in front", || {
            term.terminal().foreground().owner == ForegroundOwner::Agent
        });
        for (text, opts) in [
            ("; echo PWNED-$((1+1)) ;", submit()),
            (
                "; echo PWNED-$((2+1)) ;\n; echo PWNED-$((3+1)) ;",
                SendOptions::default(),
            ),
        ] {
            let ticket = term.send(text, opts);
            let ended = outcome_where(&term, &events, &ticket, |event| match event {
                Event::AgentState { state, .. } => states.push(state),
                event => collect_output(event, &mut output),
            });
            assert_eq!(ended, Aborted(AbortReason::ForegroundUnproven), "{program}");
        }
        term.terminal().write(end.as_bytes()).unwrap();
        wait_until("the shell in front", || {
            term.terminal().foreground().owner == ForegroundOwner::Shell
        });
    }
    while let Ok(event) = events.recv_timeout(Duration::from_millis(500)) {
        collect_output(event, &mut output);
    }
    // Not typed (`cat` and the tty would echo it), not run, no launch line.
    for typed in ["PWNED", "agent-link-fake-agent"] {
        assert!(!output.contains(typed), "{typed} in {output:?}");
    }
    assert!(states.is_empty(), "never launched: {states:?}");
    assert_eq!(term.state(), AgentState::ShellOnly);
    let lines = logger::lines_for(term.terminal().id());
    assert!(
        lines
            .iter()
            .any(|l| l.contains("a program this terminal did not launch owns it: not pasted")),
        "{lines:#?}"
    );
}

/// What a send must never do to a program the user started: a one-line submit and a
/// multi-line paste-only send (bash 3.2 has no bracketed paste: each line would run as it is
/// read).
fn hostile_sends() -> [(&'static str, SendOptions); 2] {
    [
        ("; echo PWNED-$((1+1)) ;", submit()),
        (
            "; echo PWNED-$((2+1)) ;\n; echo PWNED-$((3+1)) ;",
            SendOptions::default(),
        ),
    ]
}

/// Starts a nested shell by hand (a host line) and waits until it owns the terminal.
fn start_nested_shell(term: &AgentTerminal) {
    term.terminal().write(b"sh\n").unwrap();
    wait_screen(term.terminal(), SHELL_START_WAIT, |_| {
        term.terminal().foreground().owner == ForegroundOwner::Agent
    });
}

/// Every [`hostile_sends`] ends `foreground_unproven`; then the nested shell exits and
/// nothing was typed into it or ran there (it echoes what it gets). Returns the output.
fn assert_nested_shell_gets_nothing(term: &AgentTerminal, events: &Receiver<Event>) -> String {
    let mut output = String::new();
    for (text, opts) in hostile_sends() {
        let ticket = term.send(text, opts);
        assert_eq!(
            outcome_with_output(term, events, &ticket, &mut output),
            Aborted(AbortReason::ForegroundUnproven)
        );
    }
    term.terminal().write(b"exit\n").unwrap();
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    while let Ok(event) = events.recv_timeout(Duration::from_millis(500)) {
        collect_output(event, &mut output);
    }
    assert!(!output.contains("PWNED"), "typed: {output:?}");
    output
}

/// An inline agent leaves its last frame on the normal screen when it exits (codex,
/// claude) — its ready footer included. Here it exits at its first submit, on its own: the
/// crate does not see it go, so its startup gate still reads it as running and ready. The
/// user then starts a nested shell (`bash`, `ssh host`, `nix develop`): it is not the agent
/// the crate launched — nothing is typed into it, where a submit would run the text (on a
/// remote host, for `ssh`).
#[test]
fn a_nested_shell_started_after_the_agent_exited_gets_nothing() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: long_window(),
        trace: Some(true),
        ..AgentSpec::new(fake_profile("exit-to-shell", &["--keep-frame"]))
    });
    let first = term.send("the first message", submit());
    assert_eq!(outcome(&term, &events, &first), Delivered);
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    screen_contains(term.terminal(), READY_MARKER, WAIT);
    assert_eq!(term.state(), AgentState::Ready, "not seen leaving");
    start_nested_shell(&term);
    assert_nested_shell_gets_nothing(&term, &events);
    let lines = logger::lines_for(term.terminal().id());
    assert!(
        lines
            .iter()
            .any(|l| l.contains("the agent's program is not in front: not pasted")),
        "{lines:#?}"
    );
}

/// The crate saw the agent leave (it exited after the paste, `--exit-on-input`), so no
/// agent is known to run; its last frame stays on screen with the ready footer. A nested
/// shell the user starts then gets nothing: a leftover frame never shows the agent.
#[test]
fn a_leftover_frame_does_not_make_a_nested_shell_the_agent() {
    let home = tempfile::tempdir().unwrap();
    let mut spec = sh_spec(home.path());
    // This fixture needs a trap acknowledgement before the host types its next line.
    // Dash can defer a caught INT while reading input; Bash runs the trap and redraws.
    // Other ownership tests keep the platform's /bin/sh.
    spec.mode = SpawnMode::Shell(ShellSpec {
        program: Some("/bin/bash".into()),
        login: false,
    });
    let (term, events) = start_agent_shell_spec(
        spec,
        AgentSpec {
            timing: Timing {
                // A second of quiet after the shell's redraw: by then the shell has the
                // terminal back (until it takes it, the dead job's group reads as the agent).
                paste_echo_quiet: Duration::from_millis(1000),
                paste_echo_max: Duration::from_millis(5000),
                ..long_window()
            },
            ..AgentSpec::new(fake_profile(
                "composer",
                &["--exit-on-input", "--keep-frame"],
            ))
        },
    );
    // A positive acknowledgement from the shell: foreground ownership and a completed
    // delivery alone do not mean its queued Ctrl-C has finished flushing input.
    let interrupt_trap = "trap 'printf \"\\ninterrupt-%s\\n\" handled' INT";
    term.terminal()
        .write(format!("{interrupt_trap}\n").as_bytes())
        .unwrap();
    wait_rows_end(&term, &[&format!("$ {interrupt_trap}"), "$"]);
    term.launch().unwrap();
    states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
    let ticket = term.send("left unread", submit());
    assert_eq!(
        outcome(&term, &events, &ticket),
        Aborted(AbortReason::ShellOnly)
    );
    assert_eq!(term.state(), AgentState::ShellOnly);
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    // ShellOnly queues the interrupt; it does not wait for the shell to handle it.
    // Wait for this interrupt's acknowledgement and prompt before typing a host line.
    let screen = wait_rows_end(&term, &["interrupt-handled", "$"]);
    assert!(
        screen.text().contains(READY_MARKER),
        "{}",
        bottom_rows(&screen)
    );
    start_nested_shell(&term);
    assert_nested_shell_gets_nothing(&term, &events);
}

/// The launch line started no agent (`false`), and the user ran a program at the shell
/// afterwards: that program is not taken for the agent the launch line was waiting for —
/// a line the host submitted to the shell makes any later program ambiguous.
#[test]
fn a_program_run_after_a_failed_launch_is_not_the_agent() {
    let (term, events, _home) = shell(AgentSpec {
        timing: short_window(),
        ..AgentSpec::new(unlaunchable_profile())
    });
    let mut output = String::new();
    term.launch().unwrap();
    assert_launched_on_a_clear_line(&term, &events, &mut output);
    start_nested_shell(&term);
    output.push_str(&assert_nested_shell_gets_nothing(&term, &events));
    assert_nothing_ran("/bin/sh", &output);
}

/// The agent exits from a dialog left to the user (steering off; the user picks "No, exit")
/// and leaves it on screen (`--keep-frame`), where it keeps the startup gate parked. A send
/// waiting for readiness does not wait for ever: it reads the shell in front, relaunches the
/// agent once (its dialog is left to the user again) and, declined again, ends `shell_only`
/// with nothing typed.
#[test]
fn an_agent_that_exits_from_a_dialog_left_to_the_user_ends_the_wait() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: long_window(),
        trust: TrustPolicy {
            steer_dialogs: false,
            ..TrustPolicy::default()
        },
        trace: Some(true),
        ..AgentSpec::new(fake_profile("trust", &["--keep-frame"]))
    });
    let opts = SendOptions {
        on_shell: ShellFallback::Fail,
        ..submit()
    };
    let ticket = term.send("hello there agent", opts);
    for _ in 0..2 {
        states_until(&term, &events, LAUNCH_WAIT, |s, b| {
            s == AgentState::TrustPrompt && !b
        });
        assert_eq!(ticket.try_outcome(), None, "waits while the dialog is up");
        // "No, exit" is selected: the agent says `bye` and exits, its dialog stays.
        term.terminal().write(b"\r").unwrap();
        wait_until("the shell in front", || {
            term.terminal().foreground().owner == ForegroundOwner::Shell
        });
    }
    assert_eq!(
        outcome(&term, &events, &ticket),
        Aborted(AbortReason::ShellOnly)
    );
    let screen = screen_contains(term.terminal(), "❯ 1. No, exit", WAIT);
    assert!(!screen.text().contains("hello"), "{}", bottom_rows(&screen));
    let lines = logger::lines_for(term.terminal().id());
    let left = lines
        .iter()
        .filter(|l| {
            l.contains("delivery #1: the shell is in front: the agent left before it was ready")
        })
        .count();
    assert_eq!(left, 2, "{lines:#?}");
}

/// The test shell ignores SIGINT from here on (`trap '' INT`): it never answers the crate's
/// Ctrl-C, so a launch line typed after one is held back until the 1 s cap.
fn ignore_interrupts(term: &AgentTerminal) {
    term.terminal().write(b"trap '' INT\n").unwrap();
    wait_rows_end(term, &["$ trap '' INT", "$"]);
}

/// Waits until the last non-blank rows are `rows`.
fn wait_rows_end(term: &AgentTerminal, rows: &[&str]) -> ScreenSnapshot {
    wait_screen(term.terminal(), WAIT, |s| {
        let shown: Vec<&str> = s
            .rows
            .iter()
            .map(|r| r.trim_end())
            .filter(|r| !r.is_empty())
            .collect();
        shown.ends_with(rows)
    })
}

/// A launch line typed after a Ctrl-C is held back until the shell took it — up to a
/// second. A program the user starts meanwhile (`cat`) gets nothing of it: the held line
/// (or, behind a Ctrl-C still being handled, the launch's own Ctrl-C) is written only after
/// a fresh look finds the shell still in front, with no line submitted to it since; else the
/// launch is dropped. `cat` would echo the line it got.
#[test]
fn a_program_started_while_a_launch_line_is_held_gets_nothing() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: short_window(),
        trace: Some(true),
        ..AgentSpec::new(unlaunchable_profile())
    });
    let mut output = String::new();
    // The session's first launch: no Ctrl-C, and `false` runs.
    term.launch().unwrap();
    assert_launched_on_a_clear_line(&term, &events, &mut output);
    ignore_interrupts(&term);
    let id = term.terminal().id();
    // The line, held behind its own Ctrl-C; then the launch's own Ctrl-C, held behind the
    // one an earlier launch wrote a moment ago.
    for launches in [1, 2] {
        for _ in 0..launches {
            term.launch().unwrap();
        }
        term.terminal().write(b"cat\n").unwrap();
        wait_until("cat in front", || {
            term.terminal().foreground().owner == ForegroundOwner::Agent
        });
        logger::wait_line(id, "launch: not typed after the Ctrl-C", LAUNCH_WAIT);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            term.terminal().foreground().owner,
            ForegroundOwner::Agent,
            "cat still runs"
        );
        // With SIGINT ignored, Linux's tty can echo the interrupt as ^C without a
        // shell redraw. Both forms still require an otherwise exact, untouched line.
        let screen = wait_screen(term.terminal(), WAIT, |screen| {
            screen
                .rows
                .iter()
                .rfind(|row| !row.trim().is_empty())
                .is_some_and(|row| matches!(row.trim_end(), "$ cat" | "$ ^Ccat"))
        });
        let cat_line = screen
            .rows
            .iter()
            .rfind(|row| !row.trim().is_empty())
            .unwrap()
            .trim_end();
        assert!(
            !screen.text().contains("\nfalse"),
            "{}",
            bottom_rows(&screen)
        );
        assert_eq!(term.state(), AgentState::ShellOnly);
        term.terminal().write(b"\x04").unwrap();
        wait_until("the shell in front", || {
            term.terminal().foreground().owner == ForegroundOwner::Shell
        });
        wait_rows_end(&term, &[cat_line, "$"]);
    }
    let lines = logger::lines_for(id);
    let dropped = lines
        .iter()
        .filter(|l| {
            l.contains("launch: not typed after the Ctrl-C, the shell no longer owns the terminal")
        })
        .count();
    assert_eq!(dropped, 2, "{lines:#?}");
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("launch: typing the launch line after the Ctrl-C")),
        "{lines:#?}"
    );
}
