//! `AgentTerminal::reset`, `reset_and_send` and `interrupt` against the fake agent
//! (`agent-link-fake-agent`, whose `/new` clears its transcript: silent for
//! `--reset-silence-ms`, then `resetting conversation`, then a fresh screen
//! `--reset-delay-ms` later, dropping input meanwhile): a reset to the running agent, to an
//! agent that left (relaunched first), without a reset command; a reset and a message as
//! one delivery — the message only after the fresh screen, also through a silent reset;
//! the Ctrl-C that ends the fake. The opt-in turn gate (`Timing::reset_turn_wait`) against
//! a turn the fake shows with its spinner title (`--title`, `--reply-ms`): off, a reset
//! requested mid-turn is typed into it and refused (`--refuse-busy-reset`); on, it waits
//! for the turn, gives up with nothing typed, follows an agent that died mid-turn
//! (`--crash-mid-reply-ms`) or was interrupted, and ends with the terminal.
#![cfg(all(unix, feature = "agent"))]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use agent_link::agent::{
    AbortReason, Activity, AgentSpec, AgentState, AgentTerminal, DeliveryOutcome, Revalidate,
    SendOptions, Timing, TrustPolicy, UnreadableComposer,
};
use agent_link::{Event, ForegroundOwner};
use common::agent::states_until;
use common::delivery::{
    DELIVERY_WAIT, drained_input_log, enter_once, fake, long_window, outcome, outcome_with_states,
    shell, submit, unreadable_fake,
};
use common::fake::{QUESTION_FOOTER, READY_MARKER, RESET_REFUSAL, answer_question, fake_profile};
use common::{
    LAUNCH_WAIT, WAIT, bottom_rows, logger, screen_contains, wait_event, wait_screen, wait_until,
};
use crossbeam_channel::Receiver;
use tempfile::TempDir;

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

/// The fake in the test shell with the turn gate bounded by `wait`, tracing on.
fn gated(flags: &[&str], wait: Duration) -> (AgentTerminal, Receiver<Event>, TempDir) {
    logger::install();
    shell(AgentSpec {
        timing: Timing {
            reset_turn_wait: wait,
            ..long_window()
        },
        trust: TrustPolicy::unattended(),
        trace: Some(true),
        ..AgentSpec::new(fake_profile("composer", flags))
    })
}

/// Submits a message (ticket 1) that the fake answers after its `--reply-ms`, and waits
/// until its spinner title made the activity `Working`.
fn start_turn(term: &AgentTerminal, events: &Receiver<Event>) {
    let ticket = term.send("old conversation", submit());
    assert_eq!(outcome(term, events, &ticket), Delivered);
    wait_until("the agent working", || term.activity() == Activity::Working);
}

/// The turn gate's trace line when it holds a reset.
const WAITS: &str = "the agent is working: the reset waits for its turn to end";

/// The index of ticket 2's first trace line holding `step`.
fn step_at(term: &AgentTerminal, step: &str) -> usize {
    let lines = logger::lines_for(term.terminal().id());
    lines
        .iter()
        .position(|l| l.contains(&format!("delivery #2: {step}")))
        .unwrap_or_else(|| panic!("no {step:?}: {lines:#?}"))
}

/// Without the turn gate (the default) a reset requested mid-turn is typed into the turn
/// at once, as before: the fake refuses it (`--refuse-busy-reset`, as codex refuses
/// `/clear` during a task) and the follow-up is withheld.
#[test]
fn without_the_turn_gate_a_reset_is_typed_into_the_running_turn() {
    let (term, events, home) = fake(
        "composer",
        &[
            "--title",
            "--refuse-busy-reset",
            "--reply-ms",
            "5000",
            "--input-log",
            "input.bin",
        ],
    );
    start_turn(&term, &events);
    let both = term.reset_and_send("follow-up payload", submit());
    assert_eq!(
        outcome(&term, &events, &both),
        Aborted(AbortReason::ResetRejected)
    );
    assert_eq!(
        drained_input(&term, home.path()),
        b"\x1b[200~old conversation\x1b[201~\r\x1b[200~/new\x1b[201~\r~"
    );
}

/// With the gate on the same request waits: the reply arrives, the spinner stops, then
/// the reset is typed — not refused — and the follow-up goes to the fresh conversation.
#[test]
fn the_turn_gate_holds_a_reset_until_the_turn_ends() {
    let (term, events, home) = gated(
        &[
            "--title",
            "--refuse-busy-reset",
            "--reply-ms",
            "2500",
            "--input-log",
            "input.bin",
        ],
        Duration::from_secs(30),
    );
    start_turn(&term, &events);
    let both = term.reset_and_send("follow-up payload", submit());
    assert_eq!(outcome(&term, &events, &both), Delivered);
    let screen = screen_contains(term.terminal(), "you: follow-up payload", WAIT);
    let text = screen.text();
    assert!(
        !text.contains("old conversation") && !text.contains(RESET_REFUSAL),
        "{}",
        bottom_rows(&screen)
    );
    let waits = step_at(&term, WAITS);
    let ended = step_at(&term, "the agent's turn ended");
    assert!(waits < ended && ended < step_at(&term, "pasted"));
    assert_eq!(
        drained_input(&term, home.path()),
        concat!(
            "\x1b[200~old conversation\x1b[201~\r",
            "\x1b[200~/new\x1b[201~\r",
            "\x1b[200~follow-up payload\x1b[201~\r~"
        )
        .as_bytes()
    );
}

/// A turn longer than the gate's bound: `AgentWorking` once the bound ran out, and nothing
/// typed — no reset command, no Enter, no follow-up.
#[test]
fn a_turn_that_outlasts_the_turn_gate_ends_agent_working_with_nothing_typed() {
    let bound = Duration::from_secs(1);
    for compound in [false, true] {
        let (term, events, home) = gated(
            &["--title", "--reply-ms", "30000", "--input-log", "input.bin"],
            bound,
        );
        start_turn(&term, &events);
        let start = Instant::now();
        let ticket = if compound {
            term.reset_and_send("follow-up payload", submit())
        } else {
            term.reset()
        };
        assert_eq!(
            outcome(&term, &events, &ticket),
            Aborted(AbortReason::AgentWorking)
        );
        assert!(start.elapsed() >= bound, "{:?}", start.elapsed());
        assert_eq!(term.activity(), Activity::Working);
        assert_eq!(
            drained_input(&term, home.path()),
            b"\x1b[200~old conversation\x1b[201~\r~"
        );
    }
}

/// The fake dies while the reset waits (`--crash-mid-reply-ms`) and its spinner title
/// outlives it, so the activity stays `Working`: the gate's foreground poll finds the
/// shell, the agent is marked gone, and the reset relaunches it and goes to the new one.
#[test]
fn a_reset_held_by_the_turn_gate_relaunches_an_agent_that_died_mid_turn() {
    let (term, events, _home) = gated(
        &[
            "--title",
            "--reply-ms",
            "60000",
            "--crash-mid-reply-ms",
            "3000",
        ],
        Duration::from_secs(40),
    );
    let mut states = Vec::new();
    let ticket = term.send("old conversation", submit());
    assert_eq!(
        outcome_with_states(&term, &events, &ticket, &mut states),
        Delivered
    );
    wait_until("the agent working", || term.activity() == Activity::Working);
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
    let waits = step_at(&term, WAITS);
    assert!(
        waits
            < step_at(
                &term,
                "the shell is in front: the agent left during its turn"
            )
    );
}

/// The host interrupts the turn the reset waits for (Ctrl-C; the fake exits on it): the
/// call returns at once, and the reset and its follow-up go to a relaunched agent.
#[test]
fn interrupting_the_turn_releases_a_reset_held_by_the_turn_gate() {
    let (term, events, _home) = gated(&["--title", "--reply-ms", "60000"], Duration::from_secs(40));
    start_turn(&term, &events);
    let both = term.reset_and_send("follow-up payload", submit());
    let id = term.terminal().id();
    logger::wait_line(id, WAITS, WAIT);
    let start = Instant::now();
    term.interrupt().unwrap();
    assert!(start.elapsed() < WAIT, "{:?}", start.elapsed());
    assert_eq!(outcome(&term, &events, &both), Delivered);
    let screen = screen_contains(term.terminal(), "you: follow-up payload", WAIT);
    assert!(
        !screen.text().contains("old conversation"),
        "{}",
        bottom_rows(&screen)
    );
}

/// Dropping the terminal ends a reset held by the gate: its ticket is `Disposed`.
#[test]
fn dropping_the_terminal_ends_a_reset_held_by_the_turn_gate() {
    let (term, events, _home) = gated(&["--title", "--reply-ms", "60000"], Duration::from_secs(40));
    start_turn(&term, &events);
    let reset = term.reset();
    logger::wait_line(term.terminal().id(), WAITS, WAIT);
    drop(term);
    assert_eq!(reset.wait(WAIT), Some(Aborted(AbortReason::Disposed)));
    drop(events);
}

/// The fake's fresh screen after a reset with `--unreadable-composer`: the ready
/// composer without a prompt marker, no transcript.
fn fresh_unreadable(term: &AgentTerminal) {
    wait_screen(term.terminal(), WAIT, |s| {
        let text = s.text();
        text.contains(READY_MARKER)
            && !text.contains("you: ")
            && !text.contains("resetting")
            && s.rows.iter().any(|r| r == "┃")
    });
}

/// An unreadable composer gets no reset Enter by default: `reset()` and
/// `reset_and_send()` end `SubmitUnverified` with the command left in the composer and
/// no follow-up pasted.
#[test]
fn an_unreadable_reset_gets_no_enter_by_default() {
    for compound in [false, true] {
        let (term, events, home) = unreadable_fake("composer", &[], long_window());
        let ticket = if compound {
            term.reset_and_send("follow-up payload", submit())
        } else {
            term.reset()
        };
        assert_eq!(
            outcome(&term, &events, &ticket),
            Aborted(AbortReason::SubmitUnverified),
            "compound {compound}"
        );
        screen_contains(term.terminal(), "┃ /new", WAIT);
        assert_eq!(
            drained_input_log(&term, home.path()),
            b"\x1b[200~/new\x1b[201~~"
        );
    }
}

/// `reset_with(EnterOnce)`: exactly one unverified Enter, then the fresh conversation;
/// `reset_and_send` with the opt-in pastes its follow-up once fresh output after that
/// Enter settled and reports `UnverifiedEnter`, never `Delivered`.
#[test]
fn an_opted_in_unreadable_reset_writes_one_unverified_enter() {
    for compound in [false, true] {
        let (term, events, home) = unreadable_fake("composer", &[], long_window());
        let seed = term.send("old conversation", enter_once());
        assert_eq!(
            outcome(&term, &events, &seed),
            Aborted(AbortReason::UnverifiedEnter)
        );
        screen_contains(term.terminal(), "agent: ok #1", WAIT);
        let ticket = if compound {
            term.reset_and_send("the instruction", enter_once())
        } else {
            term.reset_with(UnreadableComposer::EnterOnce)
        };
        assert_eq!(
            outcome(&term, &events, &ticket),
            Aborted(AbortReason::UnverifiedEnter),
            "compound {compound}"
        );
        let mut expected = b"\x1b[200~old conversation\x1b[201~\r\x1b[200~/new\x1b[201~\r".to_vec();
        if compound {
            let screen = screen_contains(term.terminal(), "agent: ok #1", WAIT);
            let text = screen.text();
            assert!(
                text.contains("you: the instruction") && !text.contains("old conversation"),
                "{}",
                bottom_rows(&screen)
            );
            expected.extend_from_slice(b"\x1b[200~the instruction\x1b[201~\r");
        } else {
            fresh_unreadable(&term);
        }
        expected.push(b'~');
        assert_eq!(drained_input_log(&term, home.path()), expected);
    }
}

/// `--reset-silence-ms 2500` with `startup_max` 1 s: no fresh output settles after the
/// unverified reset Enter. The reset alone still reports its Enter (`UnverifiedEnter`);
/// the compound job withholds its follow-up (`SubmitUnverified`, nothing more typed).
#[test]
fn an_unverified_reset_without_fresh_output_withholds_the_follow_up() {
    for compound in [false, true] {
        let timing = Timing {
            startup_max: Duration::from_millis(1000),
            ..long_window()
        };
        let (term, events, home) =
            unreadable_fake("composer", &["--reset-silence-ms", "2500"], timing);
        let (ticket, expected) = if compound {
            (
                term.reset_and_send("the instruction", enter_once()),
                AbortReason::SubmitUnverified,
            )
        } else {
            (
                term.reset_with(UnreadableComposer::EnterOnce),
                AbortReason::UnverifiedEnter,
            )
        };
        assert_eq!(
            outcome(&term, &events, &ticket),
            Aborted(expected),
            "compound {compound}"
        );
        // The fake drops input until its fresh screen.
        fresh_unreadable(&term);
        assert_eq!(
            drained_input_log(&term, home.path()),
            b"\x1b[200~/new\x1b[201~\r~"
        );
    }
}

/// The unverified reset still checks the agent's response: a fresh refusal ends it
/// `ResetRejected` and withholds the follow-up.
#[test]
fn an_unverified_reset_still_reports_a_fresh_refusal() {
    for compound in [false, true] {
        let (term, events, home) =
            unreadable_fake("composer", &["--refuse-next-reset"], long_window());
        let ticket = if compound {
            term.reset_and_send("follow-up payload", enter_once())
        } else {
            term.reset_with(UnreadableComposer::EnterOnce)
        };
        assert_eq!(
            outcome(&term, &events, &ticket),
            Aborted(AbortReason::ResetRejected),
            "compound {compound}"
        );
        screen_contains(term.terminal(), RESET_REFUSAL, WAIT);
        assert_eq!(
            drained_input_log(&term, home.path()),
            b"\x1b[200~/new\x1b[201~\r~"
        );
    }
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
