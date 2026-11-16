//! `AgentTerminal::send` against the fake agent (`agent-link-fake-agent`): paste and
//! verified submit, a chip, a swallowed Enter, FIFO order, paste without submit, control
//! characters in the text, an empty text, the agent-vs-shell gate (relaunch, an agent that
//! left during the lull or after the paste, inert one-line paste — tabs as spaces, never
//! run by a later launch line — multi-line refused, for paste-only sends too, in the test
//! shell with a stale bracketed-paste mode and in zsh / bash 5.1+, `Fail`), a launch line
//! after a Ctrl-C never running what sits on the shell's line, a bare shell, dialogs before
//! and after readiness and before the Enter, a gate re-armed at the commit point, the
//! commit-point check, refused texts, and that every `Event::Delivery` matches its ticket.
//! Which program gets the text (nested shells, programs and agents started by hand, a
//! relaunch over a leftover frame): `agent_delivery_owner.rs`. Strict delivery, the
//! commit-point handshake, session end and drop: `agent_strict.rs`; reset and
//! reset-and-send, interrupt: `agent_reset.rs`.
#![cfg(all(unix, feature = "agent"))]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use agent_link::agent::{
    AbortReason, AgentSpec, AgentState, AgentTerminal, DeliveryOutcome, Revalidate, SendOptions,
    ShellFallback, Ticket, TicketId, Timing, TrustPolicy,
};
use agent_link::screen::ScreenSnapshot;
use agent_link::shell::ShellSpec;
use agent_link::{Event, ForegroundOwner, SpawnMode};
use common::agent::{start_agent_shell_spec, states_until};
use common::delivery::{
    DELIVERY_WAIT, assert_launched_on_a_clear_line, assert_nothing_ran, collect_output, fake,
    long_window, outcome, outcome_where, outcome_with_output, outcome_with_states, shell,
    short_window, submit,
};
use common::fake::{READY_MARKER, fake_profile, unlaunchable_profile};
use common::{
    LAUNCH_WAIT, WAIT, bottom_rows, logger, screen_contains, sh_spec, wait_screen, wait_until,
};
use crossbeam_channel::Receiver;

use DeliveryOutcome::{Aborted, Delivered, PastedUnsubmitted};

/// The screen's rows (trimmed), non-blank only.
fn rows(term: &AgentTerminal) -> Vec<String> {
    let screen = term.terminal().screen().unwrap();
    screen
        .rows
        .into_iter()
        .filter(|row| !row.trim().is_empty())
        .collect()
}

fn count_rows(term: &AgentTerminal, row: &str) -> usize {
    rows(term).iter().filter(|r| r.as_str() == row).count()
}

#[test]
fn tickets_and_options_cross_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}
    assert_send_sync::<Ticket>();
    assert_send_sync::<DeliveryOutcome>();
    assert_send::<SendOptions>();
}

#[test]
fn a_message_is_pasted_and_submitted() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: long_window(),
        trace: Some(true),
        ..AgentSpec::new(fake_profile("composer", &[]))
    });
    // `send` launches the stopped agent first and waits for it.
    let ticket = term.send("hello", submit());
    assert_eq!(ticket.id(), TicketId(1));
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
    screen_contains(term.terminal(), "you: hello", WAIT);
    screen_contains(term.terminal(), "agent: ok #1", WAIT);
    // The composer is empty again.
    let screen = screen_contains(term.terminal(), READY_MARKER, WAIT);
    assert!(
        screen.rows.iter().any(|r| r == ">"),
        "{}",
        bottom_rows(&screen)
    );
    // Traced step by step, never with the text.
    let id = term.terminal().id();
    logger::wait_line(id, "delivery #1: Delivered", WAIT);
    let lines = logger::lines_for(id);
    for step in [
        "delivery #1: start",
        "foreground Agent",
        "input lull",
        "pasted",
        "submit verified",
    ] {
        assert!(lines.iter().any(|l| l.contains(step)), "{step}: {lines:#?}");
    }
    assert!(!lines.iter().any(|l| l.contains("hello")), "{lines:#?}");
}

#[test]
fn a_multi_line_message_is_one_chip_and_one_submit() {
    let (term, events, _home) = fake("composer", &[]);
    let text = "first line of the message\nsecond line\nthird line";
    let ticket = term.send(text, submit());
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
    screen_contains(term.terminal(), "agent: ok #1", WAIT);
    assert_eq!(count_rows(&term, "you: [Pasted text #1 +3 lines]"), 1);
    assert!(!rows(&term).iter().any(|r| r.contains("second line")));
}

/// The first Enter lands inside the fake's swallow window (it comes once the paste
/// settled, ~250 ms after it) and is absorbed; the retry after the verify window submits.
#[test]
fn a_swallowed_enter_is_sent_again() {
    let (term, events, _home) = shell(AgentSpec {
        timing: Timing {
            submit_retry_max: 2, // This test explicitly accepts the duplicate-submit risk.
            ..long_window()
        },
        ..AgentSpec::new(fake_profile("swallow", &["--swallow-ms", "600"]))
    });
    let ticket = term.send("please run the whole test suite", submit());
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
    screen_contains(term.terminal(), "agent: ok #1", WAIT);
    assert_eq!(count_rows(&term, "enter-absorbed"), 1, "{:?}", rows(&term));
    assert_eq!(count_rows(&term, "you: please run the whole test suite"), 1);
}

#[test]
fn sends_are_delivered_one_at_a_time_in_order() {
    let (term, events, _home) = fake("composer", &[]);
    let first = term.send("the first message", submit());
    let second = term.send("the second message", submit());
    assert_eq!((first.id(), second.id()), (TicketId(1), TicketId(2)));
    assert_eq!(outcome(&term, &events, &first), Delivered);
    assert_eq!(outcome(&term, &events, &second), Delivered);
    screen_contains(term.terminal(), "agent: ok #2", WAIT);
    let rows = rows(&term);
    let at = |row: &str| {
        rows.iter()
            .position(|r| r == row)
            .unwrap_or_else(|| panic!("no {row:?} in {rows:?}"))
    };
    assert!(at("you: the first message") < at("agent: ok #1"));
    assert!(at("agent: ok #1") < at("you: the second message"));
    // Nothing else followed.
    while let Ok(event) = events.recv_timeout(Duration::from_millis(300)) {
        assert!(!matches!(event, Event::Delivery { .. }), "{event:?}");
    }
}

#[test]
fn a_paste_without_submit_stays_in_the_composer() {
    let (term, events, _home) = fake("composer", &[]);
    let ticket = term.send("an editable draft", SendOptions::default());
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
    screen_contains(term.terminal(), "> an editable draft", WAIT);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !rows(&term).iter().any(|r| r.starts_with("you:")),
        "{:?}",
        rows(&term)
    );
}

/// The fake exits at its first Enter without leaving a readable composer: no receipt
/// is claimed. The next send still finds the shell, relaunches the agent, and reaches it;
/// its receipt is equally unverified.
#[test]
fn an_agent_that_left_is_relaunched_for_the_next_send() {
    let (term, events, _home) = fake("exit-to-shell", &[]);
    let mut states = Vec::new();
    let first = term.send("the first message", submit());
    assert_eq!(
        outcome_with_states(&term, &events, &first, &mut states),
        Aborted(AbortReason::SubmitUnverified)
    );
    screen_contains(term.terminal(), "you: the first message", WAIT);
    // Wait for the shell in front (its prompt can lag the fake's exit under load).
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    let launch = (AgentState::Starting, true);
    let launches_before = states.iter().filter(|s| **s == launch).count();
    let second = term.send("the second message", submit());
    assert_eq!(
        outcome_with_states(&term, &events, &second, &mut states),
        Aborted(AbortReason::SubmitUnverified)
    );
    screen_contains(term.terminal(), "you: the second message", WAIT);
    let launches = states.iter().filter(|s| **s == launch).count();
    assert_eq!((launches_before, launches), (1, 2), "{states:?}");
}

/// The "agent" is `false`: the shell stays in front after the one relaunch. `Fail` types
/// nothing; the default pastes into the shell's line without Enter, and nothing runs —
/// neither then nor with the next launch line, which clears the line first.
#[test]
fn an_unlaunchable_agent_gets_an_inert_paste_or_nothing() {
    let (term, events, _home) = shell(AgentSpec {
        timing: short_window(),
        ..AgentSpec::new(unlaunchable_profile())
    });
    let fail = SendOptions {
        on_shell: ShellFallback::Fail,
        ..submit()
    };
    let ticket = term.send("echo failed-$((6*7))", fail);
    assert_eq!(
        outcome(&term, &events, &ticket),
        Aborted(AbortReason::ShellOnly)
    );
    let screen = term.terminal().screen().unwrap();
    assert!(
        !screen.text().contains("failed-"),
        "{}",
        bottom_rows(&screen)
    );

    let mut output = String::new();
    let ticket = term.send("echo inert-$((6*7))", submit());
    assert_eq!(
        outcome_with_output(&term, &events, &ticket, &mut output),
        PastedUnsubmitted
    );
    assert_inert(&term, "echo inert-$((6*7))", "inert-42");
    // Each next send relaunches first, after the text left on the line. Bash 3.2 puts
    // `00~` in front of a paste: the `;` makes the rest a command of its own if it ran.
    for (text, ran) in [
        ("; echo PWNED-$((1+1)) ;", "PWNED-2"),
        ("; echo PWNED-$((2+1)) ;", "PWNED-3"),
    ] {
        let ticket = term.send(text, submit());
        assert_eq!(
            outcome_with_output(&term, &events, &ticket, &mut output),
            PastedUnsubmitted
        );
        assert_inert(&term, text, ran);
    }
    // The host's own launch clears the line too.
    term.launch().unwrap();
    assert_launched_on_a_clear_line(&term, &events, &mut output);
    for ran in ["inert-42", "PWNED-2", "PWNED-3"] {
        assert!(!output.contains(ran), "{ran} in {output:?}");
    }
}

/// The pasted line sits on the shell's last line and did not run: once the screen holds it
/// and stayed the same for 500 ms (the shell can echo the rest of a paste — bash 3.2's
/// `01~` tail — in a later chunk), its last row is that line at a prompt.
fn assert_inert(term: &AgentTerminal, line: &str, output: &str) {
    let mut screen = screen_contains(term.terminal(), line, WAIT);
    let deadline = Instant::now() + WAIT;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let after = term.terminal().screen().unwrap();
        if after.rows == screen.rows {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the screen keeps changing: {}",
            bottom_rows(&after)
        );
        screen = after;
    }
    let rows: Vec<&String> = screen
        .rows
        .iter()
        .filter(|r| !r.trim().is_empty())
        .collect();
    let last = rows.last().expect("rows");
    assert!(last.contains(line), "{}", bottom_rows(&screen));
    assert!(last.starts_with("$ "), "{}", bottom_rows(&screen));
    assert!(!screen.text().contains(output), "{}", bottom_rows(&screen));
}

/// No profile: the same pipeline, with the shell in front — a paste-only send too. One
/// line is typed inert (macOS `/bin/sh` is bash 3.2: no bracketed paste, so no markers), a
/// multi-line text is refused: each newline would run a line. The next one replaces it: a
/// Ctrl-C empties the line first, so the texts never pile up into one.
#[test]
fn a_bare_shell_gets_an_inert_paste() {
    let (term, events, _home) = shell(AgentSpec::default());
    let ticket = term.send("echo bare-$((6*7))", submit());
    assert_eq!(ticket.id(), TicketId(1));
    // The outcome is published as an event without a profile too.
    assert_eq!(outcome(&term, &events, &ticket), PastedUnsubmitted);
    assert_inert(&term, "echo bare-$((6*7))", "bare-42");
    assert_eq!(term.state(), AgentState::ShellOnly);
    // Not a plain `Delivered` paste: the same fallback, on a fresh line, nothing runs.
    let ticket = term.send("echo also-$((6*7))", SendOptions::default());
    assert_eq!(outcome(&term, &events, &ticket), PastedUnsubmitted);
    assert_inert(&term, "echo also-$((6*7))", "also-42");
    let screen = term.terminal().screen().unwrap();
    let last = screen.rows.iter().rfind(|r| !r.trim().is_empty()).unwrap();
    assert_eq!(
        last.trim_end(),
        "$ echo also-$((6*7))",
        "{}",
        bottom_rows(&screen)
    );

    for opts in [submit(), SendOptions::default()] {
        let ticket = term.send("echo one-$((6*7))\necho two", opts);
        assert_eq!(
            outcome(&term, &events, &ticket),
            Aborted(AbortReason::ShellOnly)
        );
    }
    let screen = term.terminal().screen().unwrap();
    assert!(!screen.text().contains("one-"), "{}", bottom_rows(&screen));
}

/// Shells that turn bracketed paste on at their prompt: zsh, and bash 5.1+ when one is
/// installed. (fish is not covered: its markers and arithmetic differ — its Ctrl-U,
/// `backward-kill-line`, clears the single line an inert paste leaves.)
fn bracketed_paste_shells() -> Vec<PathBuf> {
    let zsh = ["/bin/zsh", "/usr/bin/zsh"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists());
    let bash = [
        "/opt/homebrew/bin/bash",
        "/usr/local/bin/bash",
        "/bin/bash",
        "/usr/bin/bash",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| {
        std::process::Command::new(p)
            .args(["-c", "echo $((BASH_VERSINFO[0] * 100 + BASH_VERSINFO[1]))"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|v| v.trim().parse::<u32>().ok())
            .is_some_and(|v| v >= 501)
    });
    zsh.into_iter().chain(bash).collect()
}

/// The test shell's spec with `program` instead of `/bin/sh`: prompt `$ `, no user config,
/// bracketed paste on (zsh: emacs keys, no beep, no partial-line marker).
fn bracketed_paste_spec(program: &Path, home: &Path) -> agent_link::SpawnSpec {
    let mut spec = sh_spec(home);
    spec.mode = SpawnMode::Shell(ShellSpec {
        program: Some(program.to_path_buf()),
        login: false,
    });
    let zshrc = "PS1='$ '\nbindkey -e\nunsetopt beep prompt_sp\n";
    std::fs::write(home.join(".zshrc"), zshrc).unwrap();
    let inputrc = home.join(".inputrc-bracketed");
    std::fs::write(&inputrc, "set enable-bracketed-paste on\n").unwrap();
    spec.env.set("ZDOTDIR", home).set("INPUTRC", &inputrc);
    spec
}

/// The agent left and will not come back (`false`), while the startup gate reads it as
/// running and ready: every send — a paste-only one too — finds the shell in front after
/// its relaunch. One line is pasted inert; more than one is refused whatever the terminal's
/// bracketed-paste mode says (the test shell, bash 3.2 on macOS, gets a stale mode a gone
/// program left on; zsh and bash 5.1+, when installed, turn it on themselves). Nothing ever
/// runs — not at the paste, not with the relaunches or the host's launch that follow.
#[test]
fn text_never_runs_in_the_shell() {
    let mut shells: Vec<Option<PathBuf>> = vec![None];
    let others = bracketed_paste_shells();
    if others.is_empty() {
        eprintln!("zsh and bash 5.1+ skipped: neither is installed");
    }
    shells.extend(others.into_iter().map(Some));
    for program in shells {
        let home = tempfile::tempdir().unwrap();
        let spec = match &program {
            Some(program) => bracketed_paste_spec(program, home.path()),
            None => sh_spec(home.path()),
        };
        let name = program
            .as_deref()
            .map_or_else(|| "/bin/sh".into(), |p| p.display().to_string());
        let (term, events) = start_agent_shell_spec(
            spec,
            AgentSpec {
                timing: short_window(),
                ..AgentSpec::new(unlaunchable_profile())
            },
        );
        if program.is_none() {
            // Left on by a program that is gone (an agent that crashed); this shell never
            // turned it on and takes each pasted newline as Enter.
            term.terminal().write(b"printf '\\033[?2004h'\n").unwrap();
        }
        wait_screen(term.terminal(), WAIT, |s| s.bracketed_paste);
        // Ready by the trust window's end, with the shell in front.
        term.launch().unwrap();
        states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);

        // Bash 3.2 puts `00~` in front of a paste: the `;` makes the rest a command of its
        // own if a line ran.
        let mut output = String::new();
        for (text, opts, ran) in [
            ("; echo PWNED-$((1+1)) ;", submit(), "PWNED-2"),
            ("; echo PWNED-$((2+1)) ;", SendOptions::default(), "PWNED-3"),
        ] {
            let ticket = term.send(text, opts);
            assert_eq!(
                outcome_with_output(&term, &events, &ticket, &mut output),
                PastedUnsubmitted,
                "{name}"
            );
            assert_inert(&term, text, ran);
        }
        for (text, opts) in [
            (
                "; echo PWNED-$((3+1)) ;\n; echo PWNED-$((4+1)) ;\n",
                SendOptions::default(),
            ),
            (
                "; echo PWNED-$((5+1)) ;\u{1b}[201~\r; echo PWNED-$((6+1)) ;",
                submit(),
            ),
        ] {
            let ticket = term.send(text, opts);
            assert_eq!(
                outcome_with_output(&term, &events, &ticket, &mut output),
                Aborted(AbortReason::ShellOnly),
                "{name}"
            );
        }
        term.launch().unwrap();
        assert_launched_on_a_clear_line(&term, &events, &mut output);
        for ran in [
            "PWNED-2", "PWNED-3", "PWNED-4", "PWNED-5", "PWNED-6", "PWNED-7",
        ] {
            assert!(!output.contains(ran), "{name}: {ran} in {output:?}");
        }
        for typed in ["PWNED-$((3+1))", "PWNED-$((5+1))"] {
            assert!(!output.contains(typed), "{name}: {typed} in {output:?}");
        }
    }
}

/// The agent leaves while a multi-line send waits at its commit point (the host's check):
/// the foreground is read again right before the paste, so the text is not pasted onto
/// the shell's line (where bash 3.2 would run each line) — the agent is relaunched and gets
/// it.
#[test]
fn an_agent_that_leaves_before_the_paste_is_relaunched_first() {
    let (term, events, _home) = fake("exit-to-shell", &[]);
    let mut states = Vec::new();
    term.launch().unwrap();
    states.extend(states_until(&term, &events, LAUNCH_WAIT, |s, _| {
        s == AgentState::Ready
    }));
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
    let (go_tx, go_rx) = crossbeam_channel::bounded::<()>(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let opts = SendOptions {
        revalidate: Revalidate::Sync(Box::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = entered_tx.send(());
                let _ = go_rx.recv_timeout(DELIVERY_WAIT);
            }
            true
        })),
        ..SendOptions::default()
    };
    // `;`: a command of its own after bash 3.2's `00~` if a line ran.
    let ticket = term.send("; echo PWNED-$((1+1)) ;\n; echo PWNED-$((2+1)) ;", opts);
    entered_rx
        .recv_timeout(DELIVERY_WAIT)
        .expect("the job reaches its commit point");
    // The user submits something in the agent, which exits at it.
    term.terminal().write(b"bye\r").unwrap();
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    go_tx.send(()).unwrap();
    let mut output = String::new();
    let delivered = outcome_where(&term, &events, &ticket, |event| match event {
        Event::AgentState {
            state, blocking, ..
        } => states.push((state, blocking)),
        event => collect_output(event, &mut output),
    });
    assert_eq!(delivered, Delivered);
    screen_contains(term.terminal(), "> [Pasted text #1 +2 lines]", WAIT);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "checked again");
    let launch = (AgentState::Starting, true);
    assert_eq!(
        states.iter().filter(|s| **s == launch).count(),
        2,
        "{states:?}"
    );
    for ran in ["PWNED-2", "PWNED-3"] {
        assert!(!output.contains(ran), "{ran} in {output:?}");
    }
}

/// A paste end marker and a CR in the text do not end the paste early: the agent gets one
/// two-line block and one submit.
#[test]
fn control_characters_in_the_text_do_not_act_as_keys() {
    let (term, events, _home) = fake("composer", &[]);
    let ticket = term.send("hello there\u{1b}[201~\rgeneral kenobi\u{3}", submit());
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
    screen_contains(term.terminal(), "agent: ok #1", WAIT);
    std::thread::sleep(Duration::from_millis(500));
    let rows = rows(&term);
    let sent: Vec<&String> = rows.iter().filter(|r| r.starts_with("you:")).collect();
    assert_eq!(sent, ["you: [Pasted text #1 +2 lines]"], "{rows:?}");
}

/// The startup gate is re-armed while the job sits at its commit point (the host launches
/// again during the revalidation check): the job waits for readiness again instead of
/// pasting into a starting agent.
#[test]
fn a_gate_no_longer_ready_at_the_commit_point_is_waited_for() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: Timing {
            trust_watch: Duration::from_millis(2500),
            ..Timing::default()
        },
        trace: Some(true),
        ..AgentSpec::new(fake_profile("silent", &[]))
    });
    // Silent: ready only at the trust window's end.
    term.launch().unwrap();
    screen_contains(term.terminal(), "fake-agent v1 (silent)", LAUNCH_WAIT);
    if term.state() != AgentState::Ready {
        states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
    }
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
    let (go_tx, go_rx) = crossbeam_channel::bounded::<()>(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let opts = SendOptions {
        revalidate: Revalidate::Sync(Box::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = entered_tx.send(());
                let _ = go_rx.recv_timeout(WAIT);
            }
            true
        })),
        // This test concerns gate re-arming, not a receipt from the silent scene.
        ..SendOptions::default()
    };
    let ticket = term.send("hello there agent", opts);
    entered_rx
        .recv_timeout(DELIVERY_WAIT)
        .expect("the job reaches its commit point");
    term.launch().unwrap();
    assert_eq!(term.state(), AgentState::Starting);
    go_tx.send(()).unwrap();
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "checked again");
    let id = term.terminal().id();
    let lines = logger::lines_for(id);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("delivery #1: the agent is no longer ready")),
        "{lines:#?}"
    );
    let ready = lines
        .iter()
        .filter(|l| l.contains("delivery #1: agent ready"))
        .count();
    assert_eq!(ready, 2, "{lines:#?}");
}

/// A dialog drawn after readiness (after the post-ready watch) is not answered by the
/// startup gate; a send then fails at the commit point and types nothing into it.
#[test]
fn a_dialog_after_readiness_blocks_the_send() {
    // 2.5 s: well past the 1 s watch even when readiness is read late under load.
    let (term, events, _home) = fake("late-dialog", &["--dialog-after-ms", "2500"]);
    term.launch().unwrap();
    states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
    screen_contains(term.terminal(), "fake trust dialog", LAUNCH_WAIT);
    let ticket = term.send("hello there agent", submit());
    assert_eq!(
        outcome(&term, &events, &ticket),
        Aborted(AbortReason::TrustPromptBlocking)
    );
    std::thread::sleep(Duration::from_millis(300));
    let screen = screen_contains(term.terminal(), "❯ 1. No, exit", WAIT);
    assert!(!screen.text().contains("hello"), "{}", bottom_rows(&screen));
    assert!(!screen.text().contains("you:"), "{}", bottom_rows(&screen));
}

/// A dialog drawn after the paste, while it settles, gets no Enter: the delivery fails and
/// the dialog keeps its selection ("No, exit" — an Enter would have quit the agent).
#[test]
fn a_dialog_drawn_before_the_enter_gets_no_enter() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: Timing {
            // The Enter waits for 2 s of quiet after the paste's echo: a dialog drawn
            // within that wait holds it off until the dialog is on screen.
            paste_echo_quiet: Duration::from_millis(2000),
            paste_echo_max: Duration::from_millis(5000),
            ..long_window()
        },
        trace: Some(true),
        ..AgentSpec::new(fake_profile("late-dialog", &["--dialog-after-ms", "3000"]))
    });
    term.launch().unwrap();
    // The fake draws its header at its start; the dialog follows 3 s later. The paste
    // comes 1.5 s after the test saw the header: before the dialog unless the header was
    // seen 1.5 s late, and the Enter (≥ 2 s later) after it.
    screen_contains(term.terminal(), "fake-agent v1 (late-dialog)", LAUNCH_WAIT);
    let paste_at = Instant::now() + Duration::from_millis(1500);
    let calls = AtomicUsize::new(0);
    let opts = SendOptions {
        // Holds the job at its commit point until just before the dialog.
        revalidate: Revalidate::Sync(Box::new(move || {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                std::thread::sleep(paste_at.saturating_duration_since(Instant::now()));
            }
            true
        })),
        ..submit()
    };
    let ticket = term.send("hello there agent", opts);
    assert_eq!(
        outcome(&term, &events, &ticket),
        Aborted(AbortReason::TrustPromptBlocking)
    );
    // Caught before the Enter, not at the commit point (the paste came first).
    let id = term.terminal().id();
    logger::wait_line(id, "delivery #1: Aborted(TrustPromptBlocking)", WAIT);
    let lines = logger::lines_for(id);
    let at = |step: &str| {
        lines
            .iter()
            .position(|l| l.contains(step))
            .unwrap_or_else(|| panic!("no {step:?}: {lines:#?}"))
    };
    assert!(
        at("delivery #1: pasted") < at("dialog on screen before the Enter"),
        "{lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("at the commit point")),
        "{lines:#?}"
    );
    std::thread::sleep(Duration::from_millis(300));
    let screen = screen_contains(term.terminal(), "❯ 1. No, exit", WAIT);
    assert!(!screen.text().contains("bye"), "{}", bottom_rows(&screen));
    assert!(!screen.text().contains("you:"), "{}", bottom_rows(&screen));
}

/// A send queued before the agent is ready waits for the startup gate, which answers the
/// dialog; then it is delivered.
#[test]
fn a_send_before_readiness_waits_for_the_dialog_to_be_answered() {
    let (term, events, _home) = fake("slow-dialog", &["--dialog-after-ms", "800"]);
    let mut states = Vec::new();
    let ticket = term.send("hello there agent", submit());
    assert_eq!(
        outcome_with_states(&term, &events, &ticket, &mut states),
        Delivered
    );
    assert!(
        states.iter().any(|s| s.0 == AgentState::TrustPrompt),
        "{states:?}"
    );
    screen_contains(term.terminal(), "you: hello there agent", WAIT);
    let screen = term.terminal().screen().unwrap();
    assert!(!screen.text().contains("bye"), "{}", bottom_rows(&screen));
}

/// A dialog the gate leaves to the user (steering off) keeps a queued send waiting —
/// nothing typed into it — until the user answered it.
#[test]
fn a_send_waits_while_a_dialog_is_left_to_the_user() {
    let (term, events, _home) = shell(AgentSpec {
        timing: long_window(),
        trust: TrustPolicy {
            steer_dialogs: false,
            ..TrustPolicy::default()
        },
        ..AgentSpec::new(fake_profile("trust", &[]))
    });
    let ticket = term.send("hello there agent", submit());
    states_until(&term, &events, LAUNCH_WAIT, |s, b| {
        s == AgentState::TrustPrompt && !b
    });
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(ticket.try_outcome(), None);
    let screen = screen_contains(term.terminal(), "❯ 1. No, exit", WAIT);
    assert!(!screen.text().contains("hello"), "{}", bottom_rows(&screen));
    // The user answers: Down to "Yes", Enter.
    term.terminal().write(b"\x1b[B").unwrap();
    screen_contains(term.terminal(), "❯ 2. Yes, trust this folder", WAIT);
    term.terminal().write(b"\r").unwrap();
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
    screen_contains(term.terminal(), "you: hello there agent", WAIT);
}

#[test]
fn a_failed_commit_point_check_pastes_nothing() {
    let (term, events, _home) = fake("composer", &[]);
    let checked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = std::sync::Arc::clone(&checked);
    let opts = SendOptions {
        revalidate: Revalidate::Sync(Box::new(move || {
            seen.store(true, std::sync::atomic::Ordering::SeqCst);
            false
        })),
        ..submit()
    };
    let ticket = term.send("a stale instruction", opts);
    assert_eq!(
        outcome(&term, &events, &ticket),
        Aborted(AbortReason::Revalidate)
    );
    assert!(checked.load(std::sync::atomic::Ordering::SeqCst));
    // The agent was ready (the check runs after the lull), and its composer is empty.
    let screen = wait_screen(term.terminal(), WAIT, |s| {
        s.text().contains(READY_MARKER) && s.rows.iter().any(|r| r == ">")
    });
    assert!(!screen.text().contains("stale"), "{}", bottom_rows(&screen));
}

/// A text with nothing to paste is refused before anything happens — a send, and a reset
/// and send (no reset either): nothing typed, and the stopped agent is not launched.
#[test]
fn refused_deliveries_launch_nothing() {
    let (term, events, _home) = fake("composer", &[]);
    let a = term.send("\u{3}\u{1b}[201~ \n", submit());
    let b = term.reset_and_send(" \t\u{7f}", submit());
    let mut states = Vec::new();
    for ticket in [&a, &b] {
        assert_eq!(
            outcome_with_states(&term, &events, ticket, &mut states),
            Aborted(AbortReason::EmptyText)
        );
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(term.state(), AgentState::ShellOnly);
    assert!(states.is_empty(), "{states:?}");
    let screen = term.terminal().screen().unwrap();
    for typed in ["/new", "agent-link-fake-agent"] {
        assert!(
            !screen.text().contains(typed),
            "{typed}: {}",
            bottom_rows(&screen)
        );
    }
}

/// A text with nothing to paste once sanitised gets no Enter: it would submit the draft the
/// user left in the composer.
#[test]
fn an_empty_text_never_submits_the_draft() {
    let (term, events, _home) = fake("composer", &[]);
    term.launch().unwrap();
    states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
    term.terminal().write(b"my draft").unwrap();
    screen_contains(term.terminal(), "> my draft", WAIT);
    for text in ["", "\u{3}\u{1b}[201~", " \n\t "] {
        let ticket = term.send(text, submit());
        assert_eq!(
            outcome(&term, &events, &ticket),
            Aborted(AbortReason::EmptyText),
            "{text:?}"
        );
    }
    std::thread::sleep(Duration::from_millis(500));
    let rows = rows(&term);
    assert!(rows.iter().any(|r| r == "> my draft"), "{rows:?}");
    assert!(!rows.iter().any(|r| r.starts_with("you:")), "{rows:?}");
}

/// The agent leaves after the paste without reading it (`--exit-on-input`: it exits as
/// soon as input is pending), so the paste lands on the shell's line. The foreground read
/// right before the Enter finds the shell: no Enter, the line interrupted, `ShellOnly` —
/// and nothing runs, then or with the next launch line. One line in the test shell (bash
/// 3.2 has no bracketed paste: each line of a multi-line paste would run as it is read),
/// two in zsh and bash 5.1+ (a multi-line buffer one line kill does not empty in zsh).
#[test]
fn an_agent_that_leaves_after_the_paste_gets_no_enter() {
    logger::install();
    let mut shells: Vec<Option<PathBuf>> = vec![None];
    shells.extend(bracketed_paste_shells().into_iter().map(Some));
    for program in shells {
        let home = tempfile::tempdir().unwrap();
        let (spec, text) = match &program {
            Some(program) => (
                bracketed_paste_spec(program, home.path()),
                "; echo PWNED-$((1+1)) ;\n; echo PWNED-$((2+1)) ;",
            ),
            None => (sh_spec(home.path()), "; echo PWNED-$((1+1)) ;"),
        };
        let name = program
            .as_deref()
            .map_or_else(|| "/bin/sh".into(), |p| p.display().to_string());
        let (term, events) = start_agent_shell_spec(
            spec,
            AgentSpec {
                timing: Timing {
                    // The Enter waits for a second of quiet after the shell's redraw: by
                    // then the shell has the terminal back (until it takes it, the dead
                    // job's group still reads as the agent).
                    paste_echo_quiet: Duration::from_millis(1000),
                    paste_echo_max: Duration::from_millis(5000),
                    ..long_window()
                },
                trace: Some(true),
                ..AgentSpec::new(fake_profile("composer", &["--exit-on-input"]))
            },
        );
        term.launch().unwrap();
        states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
        let mut output = String::new();
        let ticket = term.send(text, submit());
        assert_eq!(
            outcome_with_output(&term, &events, &ticket, &mut output),
            Aborted(AbortReason::ShellOnly),
            "{name}"
        );
        assert_eq!(term.state(), AgentState::ShellOnly, "{name}");
        // The next launch line runs on its own.
        term.launch().unwrap();
        let deadline = Instant::now() + LAUNCH_WAIT;
        while !term
            .terminal()
            .screen()
            .unwrap()
            .text()
            .contains(READY_MARKER)
        {
            match events.recv_deadline(deadline) {
                Ok(event) => collect_output(event, &mut output),
                Err(e) => panic!(
                    "{name}: not relaunched ({e}); {}",
                    bottom_rows(&term.terminal().screen().unwrap())
                ),
            }
        }
        while let Ok(event) = events.recv_timeout(Duration::from_millis(300)) {
            collect_output(event, &mut output);
        }
        for ran in ["PWNED-2", "PWNED-3"] {
            assert!(!output.contains(ran), "{name}: {ran} in {output:?}");
        }
        let lines = logger::lines_for(term.terminal().id());
        assert!(
            lines.iter().any(
                |l| l.contains("delivery #1: the shell is in front before the Enter: no Enter")
            ),
            "{name}: {lines:#?}"
        );
    }
}

/// The test shell (`None`: bash 3.2 on macOS, no bracketed paste — only a single line can
/// sit on its input line), then zsh and bash 5.1+ when installed.
fn test_shells() -> Vec<Option<PathBuf>> {
    let mut shells = vec![None];
    shells.extend(bracketed_paste_shells().into_iter().map(Some));
    shells
}

/// The spec and a name for one of [`test_shells`].
fn shell_under_test(program: &Option<PathBuf>, home: &Path) -> (agent_link::SpawnSpec, String) {
    match program {
        Some(program) => (
            bracketed_paste_spec(program, home),
            program.display().to_string(),
        ),
        None => (sh_spec(home), "/bin/sh".into()),
    }
}

/// Leaves `; echo PWNED-$((n+0)) ;` on the shell's input line without running it — a
/// draft, or a paste the shell took after the agent left — and a second line
/// (`PWNED-$((n+1))`) where the shell takes a bracketed paste: a multi-line buffer.
fn leave_on_the_line(term: &AgentTerminal, multi: bool, n: u32) {
    let first = format!("; echo PWNED-$(({n}+0)) ;");
    let keys = if multi {
        format!(
            "\u{1b}[200~{first}\n; echo PWNED-$(({}+0)) ;\u{1b}[201~",
            n + 1
        )
    } else {
        first.clone()
    };
    term.terminal().write(keys.as_bytes()).unwrap();
    screen_contains(term.terminal(), &first, WAIT);
}

/// Collects output until the screen matches `done`.
fn output_until_screen(
    term: &AgentTerminal,
    events: &Receiver<Event>,
    output: &mut String,
    done: impl Fn(&ScreenSnapshot) -> bool,
) {
    let deadline = Instant::now() + LAUNCH_WAIT;
    while !done(&term.terminal().screen().unwrap()) {
        match events.recv_deadline(deadline) {
            Ok(event) => collect_output(event, output),
            Err(e) => panic!(
                "no such screen ({e}); {}",
                bottom_rows(&term.terminal().screen().unwrap())
            ),
        }
    }
    while let Ok(event) = events.recv_timeout(Duration::from_millis(300)) {
        collect_output(event, output);
    }
}

/// What sits on the shell's line when the crate types a launch line there — a multi-line
/// buffer (zsh keeps its earlier lines through a line kill), a draft, an inert paste —
/// never runs with it: once the crate typed something into the session, a Ctrl-C goes
/// first, and the launch line follows once the shell took it, intact (bash drops input
/// that arrives while it handles the interrupt). The relaunch of a send that finds the
/// shell in front, then the host's `launch()`; the "agent" is `false`.
#[test]
fn a_launch_line_never_runs_what_sits_on_the_shell_line() {
    logger::install();
    for program in test_shells() {
        let home = tempfile::tempdir().unwrap();
        let (spec, name) = shell_under_test(&program, home.path());
        let multi = program.is_some();
        let (term, events) = start_agent_shell_spec(
            spec,
            AgentSpec {
                timing: short_window(),
                trace: Some(true),
                ..AgentSpec::new(unlaunchable_profile())
            },
        );
        let mut output = String::new();
        // The session's first launch: nothing of the crate's on the line, no Ctrl-C.
        term.launch().unwrap();
        assert_launched_on_a_clear_line(&term, &events, &mut output);

        leave_on_the_line(&term, multi, 11);
        let text = "; echo PWNED-$((20+1)) ;";
        let ticket = term.send(text, submit());
        assert_eq!(
            outcome_with_output(&term, &events, &ticket, &mut output),
            PastedUnsubmitted,
            "{name}"
        );
        assert_inert(&term, text, "PWNED-21");
        leave_on_the_line(&term, multi, 13);
        term.launch().unwrap();
        assert_launched_on_a_clear_line(&term, &events, &mut output);
        assert_nothing_ran(&name, &output);

        let lines = logger::lines_for(term.terminal().id());
        let interrupted = lines
            .iter()
            .filter(|l| l.contains("launch: typing the launch line after the Ctrl-C"))
            .count();
        assert_eq!(interrupted, 2, "{name}: {lines:#?}");
    }
}

/// The agent leaves on input without reading it (`--exit-on-input`), so each paste ends up
/// with the shell. A paste-only send first — several lines where the shell takes a
/// bracketed paste: read once it settled, the shell in front holds a multi-line buffer ⇒
/// interrupted, `shell_only`; one line in the test shell (more would run line by line
/// there), reported `delivered`. Then submits, each launching the agent again (a relaunch,
/// or `send` to the stopped agent — with a multi-line buffer left on the line) and ending
/// `shell_only` without an Enter, and the host's `launch()` over another buffer. Nothing
/// ever runs.
#[test]
fn a_paste_the_shell_took_never_runs_with_a_launch_line() {
    logger::install();
    for program in test_shells() {
        let home = tempfile::tempdir().unwrap();
        let (spec, name) = shell_under_test(&program, home.path());
        let multi = program.is_some();
        let (term, events) = start_agent_shell_spec(
            spec,
            AgentSpec {
                timing: Timing {
                    // A second of quiet after the shell's redraw: by then the shell has the
                    // terminal back (until it takes it, the dead job's group still reads as
                    // the agent).
                    paste_echo_quiet: Duration::from_millis(1000),
                    paste_echo_max: Duration::from_millis(5000),
                    ..long_window()
                },
                trace: Some(true),
                ..AgentSpec::new(fake_profile("composer", &["--exit-on-input"]))
            },
        );
        term.launch().unwrap();
        states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
        let shell_in_front = || {
            wait_until("the shell in front", || {
                term.terminal().foreground().owner == ForegroundOwner::Shell
            })
        };
        let mut output = String::new();

        let (text, ended) = if multi {
            (
                "; echo PWNED-$((10+1)) ;\n; echo PWNED-$((10+2)) ;",
                Aborted(AbortReason::ShellOnly),
            )
        } else {
            ("; echo PWNED-$((10+1)) ;", Delivered)
        };
        let ticket = term.send(text, SendOptions::default());
        assert_eq!(
            outcome_with_output(&term, &events, &ticket, &mut output),
            ended,
            "{name}"
        );
        shell_in_front();
        for buffer in [None, Some(31)] {
            if let Some(n) = buffer {
                leave_on_the_line(&term, multi, n);
            }
            let ticket = term.send("; echo PWNED-$((40+1)) ;", submit());
            assert_eq!(
                outcome_with_output(&term, &events, &ticket, &mut output),
                Aborted(AbortReason::ShellOnly),
                "{name}"
            );
            shell_in_front();
        }
        leave_on_the_line(&term, multi, 51);
        term.launch().unwrap();
        output_until_screen(&term, &events, &mut output, |s| {
            s.text().contains(READY_MARKER)
        });
        assert_nothing_ran(&name, &output);
        if multi {
            let lines = logger::lines_for(term.terminal().id());
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("delivery #1: the shell is in front after the paste")),
                "{name}: {lines:#?}"
            );
        }
    }
}

/// bash with vi keys: the installed ones — bash 3.2 (`/bin/bash` on macOS) and bash 5.1+.
fn vi_bashes() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = ["/bin/bash", "/usr/bin/bash"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .take(1)
        .collect();
    for bash in bracketed_paste_shells() {
        if bash.ends_with("bash") && !found.contains(&bash) {
            found.push(bash);
        }
    }
    found
}

/// A bare shell, bash with vi keys (`set editing-mode vi`), takes the fallback's line
/// literally. The first send finds it in insert mode at a fresh prompt: typed without
/// paste markers (bash 3.2 has no bracketed paste — the start marker's ESC would switch to
/// command mode and the text's letters would be vi commands: `#` accepts the line, `v`
/// runs `$EDITOR` and then the line). The user then leaves the line in command mode (ESC),
/// where no bash takes a bracketed paste: the next send's Ctrl-C starts a fresh line in
/// insert mode (and empties the first text off it); bash 5.1+ turns bracketed paste on
/// there, so it gets a bracketed paste, bash 3.2 the plain line. Nothing runs.
#[test]
fn a_shell_in_vi_mode_gets_the_text_as_typed() {
    for program in vi_bashes() {
        let name = program.display().to_string();
        let home = tempfile::tempdir().unwrap();
        let mut spec = sh_spec(home.path());
        spec.mode = SpawnMode::Shell(ShellSpec {
            program: Some(program.clone()),
            login: false,
        });
        let inputrc = home.path().join(".inputrc-vi");
        std::fs::write(
            &inputrc,
            "set editing-mode vi\nset enable-bracketed-paste on\n",
        )
        .unwrap();
        // `v` would run the line once this "editor" exits.
        spec.env
            .set("INPUTRC", &inputrc)
            .set("VISUAL", "true")
            .set("EDITOR", "true");
        let (term, events) = start_agent_shell_spec(spec, AgentSpec::default());
        let mut output = String::new();
        for (n, text) in ["#1 v: echo PWNED-$((6*7))", "#2 v: echo PWNED-$((6*7))"]
            .into_iter()
            .enumerate()
        {
            if n > 0 {
                // Command mode. (Readline waits `keyseq-timeout`, 500 ms, for the rest of a
                // sequence after a lone ESC: a Ctrl-C inside that wait leaves bash 5.3 on
                // the same line.)
                term.terminal().write(b"\x1b").unwrap();
                std::thread::sleep(Duration::from_millis(800));
            }
            let ticket = term.send(text, SendOptions::default());
            assert_eq!(
                outcome_with_output(&term, &events, &ticket, &mut output),
                PastedUnsubmitted,
                "{name}"
            );
            assert_inert(&term, text, "PWNED-42");
            let screen = term.terminal().screen().unwrap();
            let last = screen.rows.iter().rfind(|r| !r.trim().is_empty()).unwrap();
            assert_eq!(
                last.trim_end(),
                format!("$ {text}"),
                "{name}: {}",
                bottom_rows(&screen)
            );
        }
        while let Ok(event) = events.recv_timeout(Duration::from_millis(300)) {
            collect_output(event, &mut output);
        }
        assert_nothing_ran(&name, &output);
    }
}

/// The shell fallback pastes a tab as a space: bash 3.2 (the test shell, no bracketed
/// paste) would read it as the Tab key and complete instead.
#[test]
fn a_tab_reaches_the_shell_line_as_a_space() {
    let (term, events, _home) = shell(AgentSpec::default());
    let ticket = term.send("echo tab-$((6*7))\tdone", submit());
    assert_eq!(outcome(&term, &events, &ticket), PastedUnsubmitted);
    assert_inert(&term, "echo tab-$((6*7)) done", "tab-42");
}

/// Shell exit: new work is refused immediately without reserving a queue slot or event.
#[test]
fn a_send_after_the_session_ended_is_not_connected() {
    let (term, events, _home) = shell(AgentSpec::default());
    term.terminal().write(b"exit\n").unwrap();
    wait_until("the session ended", || term.state() == AgentState::Exited);
    let ticket = term.send("anything", submit());
    assert_eq!(
        ticket.try_outcome(),
        Some(Aborted(AbortReason::NotConnected))
    );
    assert!(
        !events
            .try_iter()
            .any(|event| matches!(event, Event::Delivery { .. }))
    );
}

/// Background terminal updates must never substitute for a receipt. These profiles read
/// the same fake PTY; only the uncertain cases deliberately lose composer visibility.
mod verified_receipts {
    use super::*;
    use agent_link::agent::{AgentProfile, Dialog, detect};
    use agent_link::{PtySize, SpawnSpec};

    #[derive(Clone, Copy)]
    enum Visibility {
        Readable,
        NeverPending,
        Unknown,
        UnknownAfterEnter,
        PanicAfterEnter,
    }

    struct Profile(Visibility);

    impl AgentProfile for Profile {
        fn id(&self) -> &str {
            "receipt-test"
        }

        fn dialog(&self, _: &str) -> Option<Dialog> {
            None
        }

        fn input_ready(&self, screen: &str) -> bool {
            screen.contains(READY_MARKER)
        }

        fn pending_input(&self, screen: &str, sent: &str) -> Option<bool> {
            match self.0 {
                Visibility::NeverPending => Some(false),
                Visibility::Unknown => None,
                Visibility::UnknownAfterEnter if screen.contains("enter-received") => None,
                Visibility::PanicAfterEnter if screen.contains("enter-received") => {
                    panic!("composer detector failed")
                }
                _ => detect::pending_input_default(screen, sent),
            }
        }
    }

    fn session(
        visibility: Visibility,
        ignore_enter: bool,
    ) -> (AgentTerminal, Receiver<Event>, tempfile::TempDir) {
        session_options(
            visibility,
            ignore_enter,
            Timing::default().submit_retry_max,
            &[],
        )
    }

    fn session_options(
        visibility: Visibility,
        ignore_enter: bool,
        retries: u32,
        extra_args: &[&str],
    ) -> (AgentTerminal, Receiver<Event>, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let mut args = vec!["composer", "--background-title", "--record-enter"];
        args.extend_from_slice(extra_args);
        if ignore_enter {
            args.push("--ignore-enter");
        }
        let spec = SpawnSpec::direct(
            common::fake_agent_path(),
            args,
            home.path(),
            PtySize { rows: 24, cols: 80 },
        );
        let agent = AgentSpec {
            timing: Timing {
                paste_echo_quiet: Duration::from_millis(150),
                paste_echo_max: Duration::from_millis(200),
                submit_verify_quiet: Duration::from_millis(150),
                submit_verify_window: Duration::from_millis(350),
                submit_verify_poll: Duration::from_millis(20),
                submit_retry_max: retries,
                ..Timing::default()
            },
            ..AgentSpec::new(Arc::new(Profile(visibility)))
        };
        let (term, events) = common::agent::spawn_agent(spec, agent);
        states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
        (term, events, home)
    }

    #[test]
    fn background_titles_do_not_deliver_a_retained_message() {
        for strict in [false, true] {
            let (term, events, _home) = session_options(Visibility::Readable, true, 1, &[]);
            let ticket = term.send("still pending", SendOptions { strict, ..submit() });
            assert_eq!(
                outcome(&term, &events, &ticket),
                Aborted(AbortReason::SubmitUnverified)
            );
            assert_eq!(count_rows(&term, "> still pending"), 1);
            assert_eq!(count_rows(&term, "enter-received"), 2, "one bounded retry");
            assert!(!rows(&term).iter().any(|r| r.starts_with("you:")));
        }
    }

    /// The agent already accepted Enter but has not redrawn its composer. Repeating
    /// Enter here would queue a second turn; defaults must leave the result uncertain.
    #[test]
    fn delayed_acceptance_sends_one_enter_by_default() {
        for strict in [false, true] {
            let (term, events, _home) = session_options(
                Visibility::Readable,
                false,
                Timing::default().submit_retry_max,
                &["--submit-display-delay-ms", "1200"],
            );
            let ticket = term.send("slow redraw", SendOptions { strict, ..submit() });
            assert_eq!(
                outcome(&term, &events, &ticket),
                Aborted(AbortReason::SubmitUnverified)
            );
            assert_eq!(
                count_rows(&term, "> slow redraw"),
                1,
                "composer is still stale"
            );
            assert_eq!(count_rows(&term, "enter-received"), 1);
            screen_contains(term.terminal(), "you: slow redraw", WAIT);
            assert_eq!(count_rows(&term, "you: slow redraw"), 1);
            assert_eq!(count_rows(&term, "enter-received"), 1);
            assert_eq!(
                ticket.try_outcome(),
                Some(Aborted(AbortReason::SubmitUnverified))
            );
        }
    }

    #[test]
    fn readable_submission_succeeds_once_during_background_titles() {
        for strict in [false, true] {
            let (term, events, _home) = session(Visibility::Readable, false);
            let ticket = term.send("submit once", SendOptions { strict, ..submit() });
            assert_eq!(outcome(&term, &events, &ticket), Delivered);
            assert_eq!(count_rows(&term, "enter-received"), 1);
            assert_eq!(count_rows(&term, "you: submit once"), 1);
        }
    }

    #[test]
    fn an_unobserved_paste_gets_no_enter_despite_background_output() {
        for visibility in [Visibility::NeverPending, Visibility::Unknown] {
            let (term, events, _home) = session(visibility, false);
            let ticket = term.send("unproven paste", submit());
            assert_eq!(
                outcome(&term, &events, &ticket),
                Aborted(AbortReason::SubmitUnverified)
            );
            assert_eq!(count_rows(&term, "enter-received"), 0);
            assert_eq!(count_rows(&term, "> unproven paste"), 1);
        }
    }

    #[test]
    fn an_unreadable_or_panicking_composer_gets_no_enter_retry() {
        for visibility in [Visibility::UnknownAfterEnter, Visibility::PanicAfterEnter] {
            let (term, events, _home) = session(visibility, true);
            let ticket = term.send("uncertain receipt", submit());
            assert_eq!(
                outcome(&term, &events, &ticket),
                Aborted(AbortReason::SubmitUnverified)
            );
            assert_eq!(count_rows(&term, "enter-received"), 1);
            assert_eq!(count_rows(&term, "> uncertain receipt"), 1);
        }
    }
}
