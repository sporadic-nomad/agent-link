//! The startup gate on a fake clock with a scripted profile. Each test names the embedded
//! terminal panel's function it mirrors; the post-ready watch, the cooldown retry of a
//! skipped Enter, the redraw wait between arrow keys, parking on a dialog left to the
//! user and the profile panic guard are this crate's additions.

use std::time::{Duration, Instant};

use super::{GateEffect, StartupGate};
use crate::agent::{AgentProfile, AgentState, Dialog, DialogKind, Timing, TrustKey};
use crate::screen::normalize;

/// Screens: `trust dialog` with a `>` (or `❯`) pointer on `no` (Down) or `yes` (Enter), or
/// none (unreadable); `bypass warning` (a dialog the profile does not answer: no key);
/// `update prompt` (another kind, answered with Enter); `composer ready` is the ready screen.
struct Scripted;

impl AgentProfile for Scripted {
    fn id(&self) -> &str {
        "scripted"
    }

    fn dialog(&self, text: &str) -> Option<Dialog> {
        let text = normalize(text).replace('❯', ">");
        if text.contains("bypass warning") {
            return Some(Dialog {
                kind: DialogKind::BypassWarning,
                key: None,
            });
        }
        if text.contains("update prompt") {
            return Some(Dialog {
                kind: DialogKind::UpdatePrompt,
                key: Some(TrustKey::Enter),
            });
        }
        let at = text.rfind("trust dialog")?;
        let tail = &text[at..];
        let key = match (tail.rfind("> no"), tail.rfind("> yes")) {
            (Some(no), Some(yes)) if no > yes => Some(TrustKey::Down),
            (_, Some(_)) => Some(TrustKey::Enter),
            (Some(_), None) => Some(TrustKey::Down),
            (None, None) => None,
        };
        Some(Dialog {
            kind: DialogKind::Trust,
            key,
        })
    }

    fn input_ready(&self, screen: &str) -> bool {
        normalize(screen).contains("composer ready") && self.dialog(screen).is_none()
    }
}

const ON_NO: &str = "trust dialog\n> no\n  yes";
const ON_YES: &str = "trust dialog\n  no\n> yes";
const UNREADABLE: &str = "trust dialog\n  no\n  yes";
const BYPASS: &str = "bypass warning\n> no, exit\n  yes, i accept";
const UPDATE: &str = "update prompt\n> skip";
const READY: &str = "banner\n> \ncomposer ready";
const LOADING: &str = "banner\nloading";

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The default timing without the dialog settle: the panel's gate keys a dialog at once,
/// and most tests here check its rules. The settle has its own tests.
fn panel_timing() -> Timing {
    Timing {
        dialog_settle: Duration::ZERO,
        ..Timing::default()
    }
}

/// A gate launched at `t0` with [`panel_timing`].
fn launched() -> (StartupGate, Instant) {
    launched_with(panel_timing(), true)
}

fn launched_with(timing: Timing, steer: bool) -> (StartupGate, Instant) {
    let mut gate = StartupGate::new(timing, steer);
    let t0 = Instant::now();
    gate.launch(t0);
    (gate, t0)
}

fn eval(gate: &mut StartupGate, screen: &str, at: Instant) -> Vec<GateEffect> {
    gate.evaluate(screen, &Scripted, at)
}

fn keys(effects: &[GateEffect]) -> Vec<TrustKey> {
    effects
        .iter()
        .filter_map(|e| match e {
            GateEffect::Key { key, .. } => Some(*key),
            _ => None,
        })
        .collect()
}

fn notes(effects: &[GateEffect]) -> Vec<&str> {
    effects
        .iter()
        .filter_map(|e| match e {
            GateEffect::Note(n) => Some(n.as_str()),
            _ => None,
        })
        .collect()
}

/// Fires deadlines up to `at` against `screen`.
fn tick(gate: &mut StartupGate, screen: &str, at: Instant) -> Vec<GateEffect> {
    gate.on_deadline(at, screen, &Scripted)
}

/// `armAgentStartup`: blocking, not ready, the trust window open, the cap running.
#[test]
fn launch_arms_the_gate() {
    let mut gate = StartupGate::new(Timing::default(), true);
    assert_eq!(gate.state(), AgentState::ShellOnly);
    assert_eq!(gate.next_deadline(), None);
    let t0 = Instant::now();
    gate.launch(t0);
    assert!(gate.running() && gate.blocking() && !gate.ready());
    assert_eq!(gate.state(), AgentState::Starting);
    assert_eq!(gate.next_deadline(), Some(t0 + ms(3000)));
    assert!(gate.wants_screen(t0));
}

/// `noteOutput`: every chunk restarts the quiet settle; before a launch output is ignored.
#[test]
fn output_restarts_the_quiet_settle() {
    let mut idle = StartupGate::new(Timing::default(), true);
    let t0 = Instant::now();
    idle.on_output(b"$ ", t0);
    assert_eq!(idle.next_deadline(), None);

    let (mut gate, t0) = launched();
    gate.on_output(b"x", t0 + ms(10));
    assert_eq!(gate.next_deadline(), Some(t0 + ms(460)));
    gate.on_output(b"y", t0 + ms(400));
    assert_eq!(gate.next_deadline(), Some(t0 + ms(850)));
}

/// `markAgentReady` from the quiet settle: the overlay lifts, but without a ready screen
/// or an answered dialog input waits for the trust window.
#[test]
fn a_premature_quiet_settle_drops_blocking_but_not_readiness() {
    let (mut gate, t0) = launched();
    gate.on_output(b"$ agent\r\n", t0 + ms(5));
    assert!(eval(&mut gate, LOADING, t0 + ms(5)).is_empty());
    tick(&mut gate, LOADING, t0 + ms(455));
    assert!(!gate.blocking());
    assert!(!gate.ready());
    assert_eq!(gate.state(), AgentState::Starting);
    // Only the window remains.
    assert_eq!(gate.next_deadline(), Some(t0 + ms(12_000)));
}

/// `restartPasteMaxTimer`: with no output at all the cap settles the gate.
#[test]
fn the_cap_settles_a_silent_start() {
    let (mut gate, t0) = launched();
    tick(&mut gate, "", t0 + ms(2999));
    assert!(gate.blocking());
    tick(&mut gate, "", t0 + ms(3000));
    assert!(!gate.blocking() && !gate.ready());
}

/// The trust-window timer of `armAgentStartup`: its end readies a settled gate and reports
/// the unrecognised screen once.
#[test]
fn the_window_end_readies_a_settled_gate_and_reports_markers_once() {
    let (mut gate, t0) = launched();
    gate.on_output(b"hello", t0);
    tick(&mut gate, "hello", t0 + ms(450));
    let effects = tick(&mut gate, "hello", t0 + ms(12_000));
    assert!(gate.ready());
    assert_eq!(gate.state(), AgentState::Ready);
    let reports: Vec<_> = effects
        .iter()
        .filter(|e| matches!(e, GateEffect::MarkersUnrecognised { .. }))
        .collect();
    assert_eq!(
        reports,
        [&GateEffect::MarkersUnrecognised {
            elapsed: ms(12_000)
        }]
    );
    assert_eq!(gate.next_deadline(), None);
    // Nothing fires again.
    assert!(tick(&mut gate, "hello", t0 + ms(60_000)).is_empty());
}

/// Not in the panel: no marker report when this launch showed a dialog or the ready screen.
/// And the window end of `armAgentStartup` readies only a settled gate.
#[test]
fn the_marker_report_needs_an_unrecognised_launch() {
    let (mut gate, t0) = launched();
    eval(&mut gate, UNREADABLE, t0 + ms(100));
    let effects = tick(&mut gate, UNREADABLE, t0 + ms(12_000));
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, GateEffect::MarkersUnrecognised { .. }))
    );
    // (The dialog left to the user keeps the gate parked: see the parking tests.)
    assert_eq!(gate.state(), AgentState::TrustPrompt);

    // Not settled at the window end (output kept coming): reported, not ready.
    let timing = Timing {
        startup_max: ms(60_000),
        ..panel_timing()
    };
    let (mut gate, t0) = launched_with(timing, true);
    gate.on_output(b"x", t0 + ms(11_900));
    let effects = tick(&mut gate, "x", t0 + ms(12_000));
    assert!(matches!(
        effects[..],
        [GateEffect::MarkersUnrecognised { .. }]
    ));
    assert!(!gate.ready() && gate.blocking());
    // The next quiet settle readies it (the window is closed).
    tick(&mut gate, "x", t0 + ms(12_350));
    assert!(gate.ready());
}

/// `evaluateStartupScreen` → `markAgentReady`: the ready screen readies the gate at once.
#[test]
fn the_ready_screen_readies_at_once() {
    let (mut gate, t0) = launched();
    let effects = eval(&mut gate, READY, t0 + ms(200));
    assert!(gate.ready() && !gate.blocking());
    assert_eq!(gate.state(), AgentState::Ready);
    assert_eq!(notes(&effects), ["ready (ready composer on screen)"]);
    // Timers and the window are gone; only the post-ready watch remains (not a deadline).
    assert_eq!(gate.next_deadline(), None);
    assert!(gate.wants_screen(t0 + ms(1199)));
    assert!(!gate.wants_screen(t0 + ms(1200)));
}

/// `approveTrustPrompt`: at most 2 Enters per dialog, 350 ms apart. Not in the panel's
/// gate: a second Enter on the unchanged screen waits `nav_redraw_wait` (the first may still
/// be in flight), a changed screen gets it after the cooldown.
#[test]
fn enter_is_bounded_by_its_budget_and_cooldown() {
    let (mut gate, t0) = launched();
    assert_eq!(keys(&eval(&mut gate, ON_YES, t0)), [TrustKey::Enter]);
    assert_eq!(gate.state(), AgentState::TrustPrompt);
    assert!(gate.blocking());
    assert!(keys(&eval(&mut gate, ON_YES, t0 + ms(349))).is_empty());
    assert!(keys(&eval(&mut gate, ON_YES, t0 + ms(350))).is_empty());
    assert_eq!(
        keys(&eval(&mut gate, ON_YES, t0 + ms(1000))),
        [TrustKey::Enter],
        "lost: retried"
    );
    assert!(keys(&eval(&mut gate, ON_YES, t0 + ms(5000))).is_empty());

    let (mut gate, t0) = launched();
    eval(&mut gate, ON_YES, t0);
    let redrawn = "trust dialog\n  no\n> yes\nredrawn";
    assert_eq!(
        keys(&eval(&mut gate, redrawn, t0 + ms(350))),
        [TrustKey::Enter]
    );
}

/// A lost Enter on a dialog that never redraws is retried once the redraw wait is over,
/// with no further output.
#[test]
fn a_lost_enter_is_retried_without_output() {
    let (mut gate, t0) = launched();
    assert_eq!(keys(&eval(&mut gate, ON_YES, t0)), [TrustKey::Enter]);
    assert_eq!(gate.next_deadline(), Some(t0 + ms(1000)));
    assert_eq!(
        keys(&gate.on_deadline(t0 + ms(1000), ON_YES, &Scripted)),
        [TrustKey::Enter]
    );
}

/// A different dialog drawn straight over the first gets its own budgets.
#[test]
fn a_new_kind_of_dialog_gets_fresh_budgets() {
    let (mut gate, t0) = launched();
    assert_eq!(keys(&eval(&mut gate, ON_YES, t0)), [TrustKey::Enter]);
    assert_eq!(
        keys(&eval(&mut gate, ON_YES, t0 + ms(1000))),
        [TrustKey::Enter]
    );
    assert!(
        keys(&eval(&mut gate, ON_YES, t0 + ms(2000))).is_empty(),
        "budget used"
    );
    assert_eq!(
        keys(&eval(&mut gate, UPDATE, t0 + ms(2100))),
        [TrustKey::Enter]
    );
}

/// While an arrow's redraw is awaited the overlay stays up: no quiet settle in between.
#[test]
fn an_arrow_keeps_the_overlay_until_the_redraw() {
    let (mut gate, t0) = launched();
    gate.on_output(b"x", t0);
    assert_eq!(keys(&eval(&mut gate, ON_NO, t0)), [TrustKey::Down]);
    let effects = gate.on_deadline(t0 + ms(460), ON_NO, &Scripted);
    assert!(keys(&effects).is_empty());
    assert!(gate.blocking(), "{effects:?}");
}

/// Not in the panel: an Enter skipped by the cooldown is retried by a re-evaluation when
/// no more output comes (a second dialog drawn right after the first was answered).
#[test]
fn an_enter_skipped_by_the_cooldown_is_retried() {
    let (mut gate, t0) = launched();
    assert_eq!(keys(&eval(&mut gate, ON_YES, t0)), [TrustKey::Enter]);
    let second = "trust dialog (second)\n  no\n> yes";
    assert!(keys(&eval(&mut gate, second, t0 + ms(100))).is_empty());
    assert_eq!(gate.next_deadline(), Some(t0 + ms(400)));
    assert_eq!(
        keys(&tick(&mut gate, second, t0 + ms(400))),
        [TrustKey::Enter]
    );
}

/// The arrow path of `approveTrustPrompt`: at most 4 presses, 350 ms apart, a
/// re-evaluation 400 ms after each.
#[test]
fn arrows_are_bounded_by_their_budget_and_cooldown() {
    let (mut gate, t0) = launched();
    // Each screen differs, as a redraw after a key would.
    let screen = |n: u32| format!("{ON_NO}\nframe {n}");
    assert_eq!(keys(&eval(&mut gate, &screen(0), t0)), [TrustKey::Down]);
    assert!(gate.blocking());
    assert_eq!(gate.next_deadline(), Some(t0 + ms(400)));
    assert!(keys(&eval(&mut gate, &screen(1), t0 + ms(100))).is_empty());
    let mut pressed = 1;
    for n in 2..10 {
        let at = t0 + ms(400 * u64::from(n - 1));
        pressed += keys(&tick(&mut gate, &screen(n), at)).len();
    }
    assert_eq!(pressed, 4);
}

/// Not in the panel: the next arrow waits until the screen changed — a key sent but not
/// drawn yet is not sent again (overshoot) — and counts as lost after `nav_redraw_wait`.
#[test]
fn an_arrow_waits_for_the_selection_to_move() {
    let (mut gate, t0) = launched();
    assert_eq!(keys(&eval(&mut gate, ON_NO, t0)), [TrustKey::Down]);
    // The re-evaluation after the cooldown sees the same screen: wait for the redraw.
    assert!(keys(&tick(&mut gate, ON_NO, t0 + ms(400))).is_empty());
    assert_eq!(gate.next_deadline(), Some(t0 + ms(1000)));
    // Still not drawn at 1 s: the key counts as lost and is sent again.
    assert_eq!(
        keys(&tick(&mut gate, ON_NO, t0 + ms(1000))),
        [TrustKey::Down]
    );

    // A redraw (here: the selection moved, but not far enough) allows the next arrow at
    // once after the cooldown.
    let (mut gate, t0) = launched();
    assert_eq!(keys(&eval(&mut gate, ON_NO, t0)), [TrustKey::Down]);
    let moved = "trust dialog\n> no (redrawn)\n  yes";
    assert_eq!(
        keys(&eval(&mut gate, moved, t0 + ms(450))),
        [TrustKey::Down]
    );
}

/// `evaluateStartupScreen`: a dialog that left the screen gives the next one both budgets
/// back.
#[test]
fn a_dialog_leaving_the_screen_restores_both_budgets() {
    let (mut gate, t0) = launched();
    eval(&mut gate, ON_YES, t0);
    eval(&mut gate, ON_YES, t0 + ms(400));
    assert!(keys(&eval(&mut gate, ON_YES, t0 + ms(800))).is_empty());
    eval(&mut gate, LOADING, t0 + ms(900));
    assert_eq!(gate.state(), AgentState::Starting);
    assert_eq!(
        keys(&eval(&mut gate, ON_YES, t0 + ms(1000))),
        [TrustKey::Enter]
    );
    assert_eq!(
        keys(&eval(&mut gate, ON_NO, t0 + ms(1400))),
        [TrustKey::Down]
    );
}

/// Drives a gate whose screen shows `dialog` from `from` to 60 s: output (a redraw of the
/// same dialog) every 700 ms, deadlines in between. Returns every (state, blocking) seen
/// after each step and all effects.
fn hold_dialog(
    gate: &mut StartupGate,
    dialog: &str,
    t0: Instant,
    from: u64,
) -> (Vec<(AgentState, bool)>, Vec<GateEffect>) {
    let mut seen = Vec::new();
    let mut all = Vec::new();
    let mut at = from;
    while at < 60_000 {
        let now = t0 + ms(at);
        all.extend(tick(gate, dialog, now));
        if at % 700 == 0 {
            gate.on_output(b"redraw", now);
            all.extend(eval(gate, dialog, now));
        }
        seen.push((gate.state(), gate.blocking()));
        at += 50;
    }
    (seen, all)
}

/// Not in the panel: a dialog left to the user parks the gate in `TrustPrompt` — past the
/// cap, the quiet settles and the trust window's end, through redraws of the dialog —
/// with the overlay lifted so the user can answer it. Never `Ready`, never a key.
fn assert_parked(seen: &[(AgentState, bool)], effects: &[GateEffect]) {
    assert!(keys(effects).is_empty(), "{effects:?}");
    assert!(
        seen.iter()
            .all(|(state, _)| *state == AgentState::TrustPrompt),
        "{seen:?}"
    );
    // Lifted at the first settle and never raised again (no flicker).
    let lifted = seen.iter().position(|(_, b)| !b).expect("overlay lifted");
    assert!(seen[lifted..].iter().all(|(_, b)| !b), "{seen:?}");
}

/// `approveTrustPrompt` with steering off (not in the panel): the gate never presses a
/// key; the dialog holds input back past the window's end, until it leaves the screen.
#[test]
fn steering_off_writes_nothing_and_parks() {
    let (mut gate, t0) = launched_with(panel_timing(), false);
    for (n, screen) in [ON_NO, ON_YES, ON_NO].into_iter().enumerate() {
        let effects = eval(&mut gate, screen, t0 + ms(500 * n as u64));
        assert!(keys(&effects).is_empty());
    }
    assert_eq!(gate.state(), AgentState::TrustPrompt);
    tick(&mut gate, ON_NO, t0 + ms(4000));
    assert!(!gate.ready() && !gate.blocking());
    let (seen, effects) = hold_dialog(&mut gate, ON_YES, t0, 4050);
    assert_parked(&seen, &effects);
    assert_eq!(gate.next_deadline(), None, "the window is over");
}

/// `approveTrustPrompt` with an unreadable selection: nothing pressed, one note; parked
/// past the window's end (with one note when it ends).
#[test]
fn an_unreadable_selection_is_left_to_the_user() {
    let (mut gate, t0) = launched();
    let mut all = Vec::new();
    for n in 0..5 {
        gate.on_output(b"x", t0 + ms(100 * n));
        all.extend(eval(&mut gate, UNREADABLE, t0 + ms(100 * n)));
    }
    assert!(keys(&all).is_empty());
    assert_eq!(notes(&all).len(), 1, "{all:?}");
    assert_eq!(gate.state(), AgentState::TrustPrompt);
    let (seen, effects) = hold_dialog(&mut gate, UNREADABLE, t0, 450);
    assert_parked(&seen, &effects);
    assert_eq!(
        notes(&effects),
        ["trust window over with a Trust dialog on screen: not ready until it leaves the screen"]
    );
}

/// Not in the panel: a dialog the profile does not answer (no key — claude's bypass
/// warning without acceptance) parks the gate; the user's answer readies it: the ready
/// composer at once, an unknown screen at the next settle (the window is over).
#[test]
fn a_dialog_without_a_key_parks_until_the_user_answers() {
    for (after, settle) in [(READY, false), (LOADING, true)] {
        let (mut gate, t0) = launched();
        gate.on_output(b"x", t0 + ms(100));
        assert!(keys(&eval(&mut gate, BYPASS, t0 + ms(100))).is_empty());
        let (seen, effects) = hold_dialog(&mut gate, BYPASS, t0, 150);
        assert_parked(&seen, &effects);
        // The user answers at 60 s.
        let answered = t0 + ms(60_000);
        gate.on_output(b"answered", answered);
        let mut effects = eval(&mut gate, after, answered);
        if settle {
            assert_eq!(gate.state(), AgentState::Starting);
            assert!(tick(&mut gate, after, answered + ms(449)).is_empty());
            effects = tick(&mut gate, after, answered + ms(450));
        }
        assert!(gate.ready() && !gate.blocking(), "after {after:?}");
        assert_eq!(gate.state(), AgentState::Ready);
        let why = if settle {
            "ready (trust window over)"
        } else {
            "ready (ready composer on screen)"
        };
        assert_eq!(notes(&effects), [why]);
    }
}

/// Not in the panel: a dialog without a key inside the post-ready watch un-readies the gate
/// and parks it — no Ready → TrustPrompt cycle at each window end, however often the
/// dialog redraws.
#[test]
fn a_dialog_without_a_key_after_ready_does_not_flicker() {
    let (mut gate, t0) = launched();
    eval(&mut gate, READY, t0 + ms(100));
    assert!(gate.ready());
    gate.on_output(b"dialog", t0 + ms(400));
    assert!(keys(&eval(&mut gate, BYPASS, t0 + ms(400))).is_empty());
    assert_eq!(gate.state(), AgentState::TrustPrompt);
    let (seen, effects) = hold_dialog(&mut gate, BYPASS, t0, 450);
    assert_parked(&seen, &effects);
    gate.on_output(b"answered", t0 + ms(60_000));
    eval(&mut gate, READY, t0 + ms(60_000));
    assert!(gate.ready());
}

/// `approveTrustPrompt`'s Enter path after a premature settle: blocking again, not ready,
/// the cap restarted; the post-confirm ready screen readies the gate.
#[test]
fn an_enter_blocks_again_until_the_ready_screen() {
    let (mut gate, t0) = launched();
    gate.on_output(b"x", t0);
    tick(&mut gate, LOADING, t0 + ms(450));
    assert!(!gate.blocking());
    assert_eq!(
        keys(&eval(&mut gate, ON_YES, t0 + ms(1000))),
        [TrustKey::Enter]
    );
    assert!(gate.blocking() && !gate.ready());
    // The lost-Enter re-check first; the restarted cap after it.
    assert_eq!(gate.next_deadline(), Some(t0 + ms(2000)));
    assert_eq!(gate.max_deadline, Some(t0 + ms(4000)));
    eval(&mut gate, READY, t0 + ms(1100));
    assert!(gate.ready() && !gate.blocking());
}

/// `markAgentReady` after an answered dialog: a settle readies the gate even without a
/// recognised ready screen.
#[test]
fn an_answered_dialog_readies_on_the_next_settle() {
    let (mut gate, t0) = launched();
    eval(&mut gate, ON_YES, t0);
    gate.on_output(b"redraw", t0 + ms(50));
    eval(&mut gate, LOADING, t0 + ms(50));
    assert!(!gate.ready());
    let effects = tick(&mut gate, LOADING, t0 + ms(500));
    assert!(gate.ready());
    assert_eq!(
        notes(&effects),
        ["ready (dialog answered, startup settled)"]
    );
}

/// `evaluateStartupScreen`'s raw-buffer fallback: a dialog in the output that the screen
/// read does not show is still answered — but the buffer never readies the gate.
#[test]
fn the_raw_buffer_finds_a_dialog_the_screen_missed() {
    let (mut gate, t0) = launched();
    // Split inside the multi-byte pointer: the decoder joins it.
    let raw = "\u{1b}[2Jtrust dialog\r\n  no\r\n\u{1b}[1m❯ yes".as_bytes();
    let cut = raw.len() - 6;
    gate.on_output(&raw[..cut], t0);
    gate.on_output(&raw[cut..], t0 + ms(1));
    assert_eq!(
        keys(&eval(&mut gate, LOADING, t0 + ms(2))),
        [TrustKey::Enter]
    );
    assert_eq!(gate.state(), AgentState::Starting);

    let (mut gate, t0) = launched();
    gate.on_output(b"composer ready", t0);
    eval(&mut gate, LOADING, t0);
    assert!(!gate.ready());
}

/// Not in the panel: the post-ready watch. A dialog on the current screen shortly after
/// readiness un-readies the gate and is steered; the gate is ready again once answered.
#[test]
fn a_dialog_right_after_ready_is_steered() {
    let (mut gate, t0) = launched();
    eval(&mut gate, READY, t0 + ms(100));
    assert!(gate.ready());
    let effects = eval(&mut gate, ON_NO, t0 + ms(400));
    assert_eq!(keys(&effects), [TrustKey::Down]);
    assert!(!gate.ready() && gate.blocking());
    assert_eq!(gate.state(), AgentState::TrustPrompt);
    // The window reopened and the cap restarted.
    assert_eq!(gate.next_deadline(), Some(t0 + ms(800)));
    assert_eq!(
        keys(&eval(&mut gate, ON_YES, t0 + ms(450))),
        [TrustKey::Enter]
    );
    eval(&mut gate, READY, t0 + ms(500));
    assert!(gate.ready() && !gate.blocking());
    // The watch is re-armed by the new readiness.
    assert!(gate.wants_screen(t0 + ms(1400)));
}

/// The same dialog after the watch (1 s) is left alone — as the panel's
/// `evaluateStartupScreen` does once ready: ready stays, nothing written.
#[test]
fn a_dialog_after_the_watch_is_not_touched() {
    let (mut gate, t0) = launched();
    eval(&mut gate, READY, t0 + ms(100));
    assert!(eval(&mut gate, ON_NO, t0 + ms(1100)).is_empty());
    assert!(eval(&mut gate, ON_NO, t0 + ms(1300)).is_empty());
    assert!(gate.ready());
    assert_eq!(gate.state(), AgentState::Ready);
}

/// `noteOutput` / `evaluateStartupScreen` after readiness: the rolling buffer is never
/// read, dialog text in it changes nothing.
#[test]
fn dialog_text_only_in_the_buffer_after_ready_is_ignored() {
    let (mut gate, t0) = launched();
    eval(&mut gate, READY, t0 + ms(100));
    gate.on_output(b"trust dialog\r\n  no\r\n> yes", t0 + ms(200));
    assert!(eval(&mut gate, READY, t0 + ms(200)).is_empty());
    assert!(gate.ready());
}

/// `resetSessionGates`: the session ended — `Exited`, nothing pending, nothing pressed.
#[test]
fn the_session_end_clears_the_gate() {
    let (mut gate, t0) = launched();
    eval(&mut gate, ON_NO, t0);
    gate.end_session();
    assert_eq!(gate.state(), AgentState::Exited);
    assert!(!gate.running() && !gate.ready() && !gate.blocking() && gate.ended());
    assert_eq!(gate.next_deadline(), None);
    assert!(eval(&mut gate, ON_YES, t0 + ms(1000)).is_empty());
}

/// The panel's timers (`setTimeout`) fire in time order; so do the gate's timers due
/// together on a late wake-up, each at its own time.
#[test]
fn late_timers_fire_in_order_at_their_own_time() {
    let (mut gate, t0) = launched();
    gate.on_output(b"hello", t0);
    // Quiet (450), cap (3000), window (12 000) at once: the quiet settle, then the window
    // end — reported at 12 s, ready.
    let effects = tick(&mut gate, "hello", t0 + ms(20_000));
    assert!(gate.ready());
    assert_eq!(
        effects
            .iter()
            .filter(|e| matches!(e, GateEffect::MarkersUnrecognised { .. }))
            .collect::<Vec<_>>(),
        [&GateEffect::MarkersUnrecognised {
            elapsed: ms(12_000)
        }]
    );
}

/// Not in the panel: a late wake-up (a stalled monitor) re-evaluates the screen once, now —
/// never several arrow keys against one screen. An unchanged screen restarts the redraw
/// wait (the key's redraw may still be unread); a changed one gets one key.
#[test]
fn a_late_wake_sends_at_most_one_arrow() {
    // Unchanged: no key at the late wake; lost only after a full wait from then.
    let (mut gate, t0) = launched();
    gate.on_output(b"x", t0);
    assert_eq!(keys(&eval(&mut gate, ON_NO, t0)), [TrustKey::Down]);
    let effects = tick(&mut gate, ON_NO, t0 + ms(5000));
    assert!(keys(&effects).is_empty(), "{effects:?}");
    assert_eq!(gate.state(), AgentState::TrustPrompt);
    assert_eq!(gate.next_deadline(), Some(t0 + ms(6000)));
    assert!(keys(&tick(&mut gate, ON_NO, t0 + ms(5999))).is_empty());
    assert_eq!(
        keys(&tick(&mut gate, ON_NO, t0 + ms(6000))),
        [TrustKey::Down]
    );

    // Changed: exactly one key, the overlay up again.
    let (mut gate, t0) = launched();
    gate.on_output(b"x", t0);
    assert_eq!(keys(&eval(&mut gate, ON_NO, t0)), [TrustKey::Down]);
    let moved = "trust dialog\n> no (redrawn)\n  yes";
    assert_eq!(
        keys(&tick(&mut gate, moved, t0 + ms(5000))),
        [TrustKey::Down]
    );
    assert!(gate.blocking());
    assert_eq!(gate.next_deadline(), Some(t0 + ms(5400)));
}

/// Raw bookkeeping of `noteOutput`: output is decoded only for the buffer, i.e. while the
/// trust window is open; a reopened window starts with a fresh decoder (no stale partial
/// sequence from a chunk it skipped).
#[test]
fn output_is_decoded_only_while_the_window_is_open() {
    let (mut gate, t0) = launched();
    eval(&mut gate, READY, t0 + ms(100));
    // Window closed: half a `❯` is not decoded (nor held back).
    gate.on_output(&"❯".as_bytes()[..1], t0 + ms(200));
    assert_eq!(gate.output.text(), "");
    // The post-ready watch reopens the window.
    eval(&mut gate, ON_NO, t0 + ms(300));
    gate.on_output(b"ab", t0 + ms(310));
    assert_eq!(gate.output.text(), "ab");
}

/// A profile whose dialog check panics on screens containing `boom`.
struct Panicky;

impl AgentProfile for Panicky {
    fn id(&self) -> &str {
        "panicky"
    }

    fn dialog(&self, text: &str) -> Option<Dialog> {
        assert!(!text.contains("boom"), "profile bug");
        Scripted.dialog(text)
    }

    fn input_ready(&self, screen: &str) -> bool {
        Scripted.input_ready(screen)
    }
}

fn panics(effects: &[GateEffect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, GateEffect::ProfilePanicked(_)))
        .count()
}

/// Not in the panel: a panicking profile is reported once and not called again — screens
/// read as unknown, no key is pressed, and readiness falls back to the timers.
#[test]
fn a_profile_panic_is_contained() {
    let (mut gate, t0) = launched();
    gate.on_output(b"boom", t0 + ms(100));
    let effects = gate.evaluate("boom", &Panicky, t0 + ms(100));
    assert_eq!(panics(&effects), 1, "{effects:?}");
    assert_eq!(gate.state(), AgentState::Starting);
    assert!(!gate.wants_screen(t0 + ms(100)));
    // Not called again: a dialog it would steer is left alone, a new panic not reported.
    let effects = gate.evaluate("boom\ntrust dialog\n> no\n  yes", &Panicky, t0 + ms(200));
    assert!(effects.is_empty(), "{effects:?}");
    gate.on_output(b"x", t0 + ms(300));
    tick(&mut gate, "boom", t0 + ms(750));
    assert!(!gate.blocking() && !gate.ready());
    tick(&mut gate, "boom", t0 + ms(12_000));
    assert!(gate.ready());

    // During the post-ready watch: still ready, nothing pressed.
    let (mut gate, t0) = launched();
    gate.evaluate(READY, &Panicky, t0 + ms(100));
    let effects = gate.evaluate("boom", &Panicky, t0 + ms(300));
    assert_eq!(panics(&effects), 1);
    assert!(gate.evaluate(ON_NO, &Panicky, t0 + ms(400)).is_empty());
    assert!(gate.ready());
}

/// Not in the panel: the first key for a dialog waits `dialog_settle` after the gate first
/// saw it (claude 2.1.295 resets its trust dialog's selection when a key comes right after
/// its first draw); later keys follow the usual rules.
#[test]
fn the_first_key_waits_until_the_dialog_settled() {
    let (mut gate, t0) = launched_with(Timing::default(), true);
    gate.on_output(b"x", t0);
    assert!(keys(&eval(&mut gate, ON_NO, t0)).is_empty());
    assert_eq!(gate.state(), AgentState::TrustPrompt);
    assert!(gate.blocking());
    assert_eq!(gate.next_deadline(), Some(t0 + ms(300)));
    assert!(
        keys(&eval(&mut gate, ON_NO, t0 + ms(299))).is_empty(),
        "more output"
    );
    assert_eq!(
        keys(&tick(&mut gate, ON_NO, t0 + ms(300))),
        [TrustKey::Down]
    );
    assert_eq!(
        keys(&eval(&mut gate, ON_YES, t0 + ms(310))),
        [TrustKey::Enter],
        "the Enter after the arrow does not wait again"
    );

    // An Enter as the first key waits too.
    let (mut gate, t0) = launched_with(Timing::default(), true);
    assert!(keys(&eval(&mut gate, ON_YES, t0)).is_empty());
    assert_eq!(
        keys(&tick(&mut gate, ON_YES, t0 + ms(300))),
        [TrustKey::Enter]
    );

    // A dialog after the overlay lifted masks it again at once; no quiet settle lifts it
    // before the key.
    let (mut gate, t0) = launched_with(Timing::default(), true);
    gate.on_output(b"x", t0);
    tick(&mut gate, "", t0 + ms(450));
    assert!(!gate.blocking());
    gate.on_output(b"dialog", t0 + ms(1000));
    assert!(keys(&eval(&mut gate, ON_NO, t0 + ms(1000))).is_empty());
    assert!(gate.blocking());
    assert_eq!(gate.next_deadline(), Some(t0 + ms(1300)));
    assert_eq!(
        keys(&tick(&mut gate, ON_NO, t0 + ms(1300))),
        [TrustKey::Down]
    );
    assert!(gate.blocking());
}

/// A dialog first seen just before the trust window ends still gets its first key once it
/// settled: the window stays open until then.
#[test]
fn a_dialog_seen_at_the_window_end_is_still_answered() {
    let timing = Timing {
        trust_watch: ms(1000),
        startup_quiet: ms(5000),
        startup_max: ms(10_000),
        ..Timing::default()
    };
    let (mut gate, t0) = launched_with(timing, true);
    gate.on_output(b"x", t0 + ms(900));
    assert!(keys(&eval(&mut gate, ON_NO, t0 + ms(900))).is_empty());
    assert_eq!(
        keys(&tick(&mut gate, ON_NO, t0 + ms(1200))),
        [TrustKey::Down]
    );
}

/// A dialog that left the screen, a new kind drawn over the first, and a dialog inside
/// the post-ready watch each settle again.
#[test]
fn every_new_dialog_settles_again() {
    let (mut gate, t0) = launched_with(Timing::default(), true);
    eval(&mut gate, ON_YES, t0);
    assert_eq!(
        keys(&tick(&mut gate, ON_YES, t0 + ms(300))),
        [TrustKey::Enter]
    );
    assert!(keys(&eval(&mut gate, UPDATE, t0 + ms(400))).is_empty());
    assert_eq!(
        keys(&tick(&mut gate, UPDATE, t0 + ms(700))),
        [TrustKey::Enter]
    );
    eval(&mut gate, LOADING, t0 + ms(800));
    assert!(keys(&eval(&mut gate, ON_YES, t0 + ms(900))).is_empty());
    assert_eq!(
        keys(&tick(&mut gate, ON_YES, t0 + ms(1200))),
        [TrustKey::Enter]
    );

    let (mut gate, t0) = launched_with(Timing::default(), true);
    eval(&mut gate, READY, t0 + ms(100));
    assert!(gate.ready());
    assert!(keys(&eval(&mut gate, ON_NO, t0 + ms(400))).is_empty());
    assert!(!gate.ready() && gate.blocking());
    assert_eq!(
        keys(&tick(&mut gate, ON_NO, t0 + ms(700))),
        [TrustKey::Down]
    );
}
