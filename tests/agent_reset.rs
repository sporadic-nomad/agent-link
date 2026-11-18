//! `AgentTerminal::reset`, `reset_and_send` and `interrupt` against the fake agent
//! (`agent-link-fake-agent`, whose `/new` clears its transcript: silent for
//! `--reset-silence-ms`, then `resetting conversation`, then a fresh screen
//! `--reset-delay-ms` later, dropping input meanwhile): a reset to the running agent, to an
//! agent that left (relaunched first), without a reset command; a reset and a message as
//! one delivery — the message only after the fresh screen, also through a silent reset;
//! the Ctrl-C that ends the fake.
#![cfg(all(unix, feature = "agent"))]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use agent_link::agent::{
    AbortReason, AgentSpec, AgentState, AgentTerminal, DeliveryOutcome, Revalidate, SendOptions,
    Timing,
};
use agent_link::{Event, ForegroundOwner};
use common::agent::states_until;
use common::delivery::{
    DELIVERY_WAIT, fake, long_window, outcome, outcome_with_states, shell, submit,
};
use common::fake::{QUESTION_FOOTER, READY_MARKER, RESET_REFUSAL, answer_question, fake_profile};
use common::{
    LAUNCH_WAIT, WAIT, bottom_rows, logger, screen_contains, wait_event, wait_screen, wait_until,
};

use DeliveryOutcome::{Aborted, Delivered};

/// The fake's fresh screen after a reset: the ready composer, no transcript.
fn fresh(term: &AgentTerminal) {
    wait_screen(term.terminal(), WAIT, |s| {
        let text = s.text();
        text.contains(READY_MARKER)
            && !text.contains("you: ")
            && !text.contains("resetting")
            && s.rows.iter().any(|r| r == ">")
    });
}

#[test]
fn a_reset_clears_the_conversation() {
    let (term, events, _home) = fake("composer", &[]);
    let ticket = term.send("the old conversation", submit());
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
    screen_contains(term.terminal(), "agent: ok #1", WAIT);
    let reset = term.reset();
    assert_eq!(outcome(&term, &events, &reset), Delivered);
    fresh(&term);
}

/// Have the peer acknowledge a final byte before inspecting its raw input log. This
/// includes any paste or extra Enter queued before the ticket was resolved.
fn drained_input(term: &AgentTerminal, home: &Path) -> Vec<u8> {
    term.terminal().write(b"~").unwrap();
    screen_contains(term.terminal(), "> ~", WAIT);
    std::fs::read(home.join("input.bin")).unwrap()
}

#[test]
fn a_refused_reset_reports_the_reason_and_withholds_the_follow_up() {
    for compound in [false, true] {
        let (term, events, home) = fake(
            "composer",
            &["--refuse-next-reset", "--input-log", "input.bin"],
        );
        let seed = term.send("old conversation", submit());
        assert_eq!(outcome(&term, &events, &seed), Delivered);
        screen_contains(term.terminal(), "agent: ok #1", WAIT);
        let ticket = if compound {
            term.reset_and_send("follow-up payload", submit())
        } else {
            term.reset()
        };
        assert_eq!(
            outcome(&term, &events, &ticket),
            Aborted(AbortReason::ResetRejected)
        );
        let screen = screen_contains(term.terminal(), RESET_REFUSAL, WAIT);
        assert!(screen.text().contains("you: old conversation"));
        assert_eq!(
            drained_input(&term, home.path()),
            b"\x1b[200~old conversation\x1b[201~\r\x1b[200~/new\x1b[201~\r~"
        );
    }
}

#[test]
fn an_ordinary_reset_command_is_delivered_and_its_old_notice_allows_the_next_reset() {
    let (term, events, _home) = fake("composer", &["--refuse-next-reset"]);
    let sent = term.send("/new", submit());
    assert_eq!(outcome(&term, &events, &sent), Delivered);
    screen_contains(term.terminal(), RESET_REFUSAL, WAIT);
    let both = term.reset_and_send("new conversation", submit());
    assert_eq!(outcome(&term, &events, &both), Delivered);
    let screen = screen_contains(term.terminal(), "you: new conversation", WAIT);
    assert!(!screen.text().contains(RESET_REFUSAL));
}

#[test]
fn a_refusal_drawn_during_the_follow_up_handshake_stops_the_paste() {
    let (term, events, home) = fake(
        "composer",
        &[
            "--refuse-next-reset",
            "--hold-submit-display",
            "--input-log",
            "input.bin",
        ],
    );
    let both = term.reset_and_send(
        "follow-up payload",
        SendOptions {
            revalidate: Revalidate::Handshake {
                timeout: DELIVERY_WAIT,
            },
            ..submit()
        },
    );
    wait_event(
        term.terminal(),
        &events,
        DELIVERY_WAIT,
        |event| match event {
            Event::CommitPoint { ticket, .. } => *ticket == both.id(),
            Event::Delivery { outcome, .. } => {
                panic!("delivery ended before the handshake: {outcome:?}")
            }
            _ => false,
        },
    );
    // The reset cleared the composer. Release its diagnostic while the follow-up waits.
    term.terminal().write(b"\x1b[B").unwrap();
    screen_contains(term.terminal(), RESET_REFUSAL, WAIT);
    term.confirm(both.id(), true);
    assert_eq!(
        outcome(&term, &events, &both),
        Aborted(AbortReason::ResetRejected)
    );
    assert_eq!(
        drained_input(&term, home.path()),
        b"\x1b[200~/new\x1b[201~\r\x1b[B~"
    );
}

/// A bare shell (no profile) has no reset command: `Unsupported`, in its turn, with its
/// event; nothing typed.
#[test]
fn a_reset_without_a_reset_command_is_unsupported() {
    let (term, events, _home) = shell(AgentSpec::default());
    let reset = term.reset();
    let both = term.reset_and_send("echo never-$((6*7))", submit());
    for ticket in [&reset, &both] {
        assert_eq!(
            outcome(&term, &events, ticket),
            Aborted(AbortReason::Unsupported)
        );
    }
    std::thread::sleep(Duration::from_millis(300));
    let screen = term.terminal().screen().unwrap();
    assert!(
        !screen.text().contains("never-"),
        "{}",
        bottom_rows(&screen)
    );
}

#[test]
fn a_question_opened_by_the_reset_paste_blocks_its_enter_and_follow_up() {
    let (term, events, home) = fake("question-on-paste", &["--input-log", "input.bin"]);
    let both = term.reset_and_send("follow-up payload", submit());
    assert_eq!(
        outcome(&term, &events, &both),
        Aborted(AbortReason::DialogBlocking)
    );
    screen_contains(term.terminal(), QUESTION_FOOTER, WAIT);
    answer_question(term.terminal());
    assert_eq!(
        std::fs::read(home.path().join("input.bin")).unwrap(),
        b"\x1b[200~/new\x1b[201~1"
    );
}

/// The fake exits at its first submit: the reset goes through the same entry point as a
/// send, so it relaunches the agent first and is delivered to the new one.
#[test]
fn a_reset_relaunches_an_agent_that_left() {
    let (term, events, _home) = fake("exit-to-shell", &[]);
    let mut states = Vec::new();
    let first = term.send("the last message", submit());
    assert_eq!(
        outcome_with_states(&term, &events, &first, &mut states),
        Aborted(AbortReason::SubmitUnverified)
    );
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    let reset = term.reset();
    assert_eq!(
        outcome_with_states(&term, &events, &reset, &mut states),
        Delivered
    );
    let launches = states
        .iter()
        .filter(|s| **s == (AgentState::Starting, true))
        .count();
    assert_eq!(launches, 2, "{states:?}");
    fresh(&term);
}

/// One delivery: the reset, then — after the fresh screen — the message. The fresh screen
/// comes 400 ms after `resetting conversation`. The reset is verified only once its
/// readable empty composer arrives, and the wait for fresh output keeps the message from being
/// pasted into the reset, where the fake drops it. That wait ends on `startup_quiet` of
/// silence too: 1.5 s here, so a loaded host's late fresh screen still comes first.
#[test]
fn reset_and_send_delivers_the_message_to_the_fresh_conversation() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: Timing {
            startup_quiet: Duration::from_millis(1500),
            ..long_window()
        },
        trace: Some(true),
        ..AgentSpec::new(fake_profile("composer", &["--reset-delay-ms", "400"]))
    });
    let ticket = term.send("the old conversation", submit());
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
    screen_contains(term.terminal(), "agent: ok #1", WAIT);
    let both = term.reset_and_send("the instruction", submit());
    assert_eq!(outcome(&term, &events, &both), Delivered);
    let screen = screen_contains(term.terminal(), "agent: ok #1", WAIT);
    let text = screen.text();
    assert!(
        text.contains("you: the instruction") && !text.contains("old conversation"),
        "{}",
        bottom_rows(&screen)
    );
    let id = term.terminal().id();
    let lines = logger::lines_for(id);
    let at = |step: &str| {
        lines
            .iter()
            .position(|l| l.contains(&format!("delivery #2: {step}")))
            .unwrap_or_else(|| panic!("no {step:?}: {lines:#?}"))
    };
    assert!(at("reset first") < at("fresh output after the reset"));
    assert!(at("fresh output after the reset") < at("then the message"));
    assert!(at("then the message") < at("Delivered"));
}

/// `--reset-silence-ms 1000`: after Enter on `/new` the fake draws nothing (its composer
/// still shows `> /new`) and drops input until its fresh screen. Verification uses the
/// longer startup window and sends no repeated Enter while reset may be executing.
/// Silence and background title updates never count as reset completion, so the message
/// is pasted only once the fresh readable composer is up; earlier input would be lost.
#[test]
fn reset_and_send_waits_through_a_silent_reset() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: long_window(),
        trace: Some(true),
        ..AgentSpec::new(fake_profile(
            "composer",
            &["--reset-silence-ms", "1000", "--background-title"],
        ))
    });
    term.launch().unwrap();
    states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
    let id = term.terminal().id();
    let both = term.reset_and_send("the instruction", submit());
    let pasted = || {
        logger::lines_for(id)
            .iter()
            .filter(|l| l.ends_with("delivery #1: pasted"))
            .count()
    };
    // Sample until the delivery ends: the message's paste (the second) only once the fresh
    // screen is up, after `resetting conversation`.
    let (mut silent, mut resetting, mut fresh_seen) = (false, false, false);
    let deadline = Instant::now() + DELIVERY_WAIT;
    while both.try_outcome().is_none() {
        assert!(Instant::now() < deadline, "the delivery did not end");
        // The lines first: a paste traced before this screen read had the fresh screen.
        let pastes = pasted();
        let text = term.terminal().screen().unwrap().text();
        silent |= text.contains("> /new");
        resetting |= text.contains("resetting conversation");
        fresh_seen |= resetting && !text.contains("resetting") && text.contains(READY_MARKER);
        assert!(
            pastes < 2 || fresh_seen,
            "the message was pasted before the fresh screen: {text}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        silent && resetting,
        "silent {silent}, resetting {resetting}"
    );
    assert_eq!(outcome(&term, &events, &both), Delivered);
    screen_contains(term.terminal(), "you: the instruction", WAIT);
    let lines = logger::lines_for(id);
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("delivery #1: the text is still in the composer: Enter again")),
        "{lines:#?}"
    );
}

/// Ctrl-C raises SIGINT in the fake: it exits 130 and the shell is back.
#[test]
fn interrupt_ends_the_agent() {
    let (term, events, _home) = fake("composer", &[]);
    term.launch().unwrap();
    states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
    term.interrupt().unwrap();
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    term.terminal().write(b"echo status-$?\n").unwrap();
    screen_contains(term.terminal(), "status-130", WAIT);
}
