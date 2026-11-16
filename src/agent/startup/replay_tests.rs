//! The startup gate against recorded codex startups, frame by frame on a fake clock: the
//! stream is cut after each DEC 2026 frame end, the n-th frame arrives at the time of the
//! n-th recorded frame end, and the gate (armed at 0, default timing) evaluates the screen
//! model after each frame — what the controller does with a live session.
//!
//! codex draws a ready-looking "loading" composer before its trust dialog and its update
//! prompt. The built-in profile never reads it as ready; a profile without that veto does,
//! and then the post-ready watch still catches and steers the dialog.

use std::time::{Duration, Instant};

#[allow(dead_code)] // shared with the integration tests, which use more of it
#[path = "../../../tests/common/fixtures.rs"]
mod fixtures;

use super::StartupGate;
use crate::agent::detect::{has_all, has_any};
use crate::agent::profiles::{ClaudeProfile, CodexProfile};
use crate::agent::{AgentProfile, AgentState, Dialog, ScreenState, Timing, TrustKey};
use crate::screen::{ScreenModel, Vt100Model, normalize};
use fixtures::Fixture;

/// The gate after one frame.
struct Step {
    at_ms: u64,
    state: AgentState,
    blocking: bool,
    keys: Vec<TrustKey>,
    dialog_on_screen: bool,
}

fn replay(f: &Fixture, profile: &dyn AgentProfile) -> Vec<Step> {
    let times = f.frame_end_times();
    let frames = f.frames();
    assert!(!times.is_empty(), "{}: no frame ends", f.label());
    let mut model = Vt100Model::new(f.size(), 0, None);
    let mut gate = StartupGate::new(Timing::default(), true);
    let t0 = Instant::now();
    gate.launch(t0);
    let mut steps = Vec::new();
    let mut last_ms = 0;
    for (n, bytes) in frames.iter().enumerate() {
        // A tail after the last frame end arrives with it.
        let at_ms = times.get(n).or(times.last()).copied().unwrap_or_default();
        let at = t0 + Duration::from_millis(at_ms);
        let before = model.snapshot().text();
        let mut effects = gate.on_deadline(at, &before, profile);
        gate.on_output(bytes, at);
        model.process(bytes);
        let screen = model.snapshot().text();
        effects.extend(gate.evaluate(&screen, profile, at));
        let keys = effects
            .iter()
            .filter_map(|e| match e {
                super::GateEffect::Key { key, .. } => Some(*key),
                _ => None,
            })
            .collect();
        steps.push(Step {
            at_ms,
            state: gate.state(),
            blocking: gate.blocking(),
            keys,
            dialog_on_screen: matches!(profile.classify(&screen), ScreenState::Dialog(_)),
        });
        last_ms = at_ms;
    }
    // The stream ends on the dialog: its first key comes once the dialog settled.
    let at_ms = last_ms + Timing::default().dialog_settle.as_millis() as u64;
    let screen = model.snapshot().text();
    let keys = gate
        .on_deadline(t0 + Duration::from_millis(at_ms), &screen, profile)
        .iter()
        .filter_map(|e| match e {
            super::GateEffect::Key { key, .. } => Some(*key),
            _ => None,
        })
        .collect();
    steps.push(Step {
        at_ms,
        state: gate.state(),
        blocking: gate.blocking(),
        keys,
        dialog_on_screen: matches!(profile.classify(&screen), ScreenState::Dialog(_)),
    });
    steps
}

/// The first frame showing the dialog; asserts the gate steers it — the first key once it
/// settled — and never read ready before unless `ready_before`.
fn check_dialog_frame(f: &Fixture, steps: &[Step], key: TrustKey, ready_before: bool) {
    let at = steps
        .iter()
        .position(|s| s.dialog_on_screen)
        .unwrap_or_else(|| panic!("{}: no dialog frame", f.label()));
    let was_ready = steps[..at].iter().any(|s| s.state == AgentState::Ready);
    assert_eq!(
        was_ready,
        ready_before,
        "{}: ready before the dialog",
        f.label()
    );
    let step = &steps[at];
    assert_eq!(step.state, AgentState::TrustPrompt, "{}", f.label());
    assert!(step.blocking, "{}", f.label());
    assert!(
        steps[..at].iter().all(|s| s.keys.is_empty()),
        "{}: a key before the dialog",
        f.label()
    );
    let first = steps[at..]
        .iter()
        .find(|s| !s.keys.is_empty())
        .unwrap_or_else(|| panic!("{}: no key for the dialog", f.label()));
    assert_eq!(first.keys, [key], "{} at {} ms", f.label(), first.at_ms);
    let settle = Timing::default().dialog_settle.as_millis() as u64;
    assert!(
        first.at_ms >= step.at_ms + settle,
        "{}: key at {} ms, dialog at {} ms",
        f.label(),
        first.at_ms,
        step.at_ms
    );
}

#[test]
fn the_codex_dialogs_are_steered_and_never_preceded_by_ready() {
    // Trust: the pointer is on "Yes, continue" — Enter. Update prompt: the pointer is on
    // "Update now" (an installer) — Down, toward "Skip".
    for (scene, key) in [
        ("trust", TrustKey::Enter),
        ("update-prompt", TrustKey::Down),
    ] {
        let f = fixtures::find("codex", scene);
        let steps = replay(&f, &CodexProfile);
        check_dialog_frame(&f, &steps, key, false);
    }
    assert_eq!(TrustKey::Enter.bytes(), b"\r");
    assert_eq!(TrustKey::Down.bytes(), b"\x1b[B");
}

/// codex's ready rule as it was before the loading veto: the loading composer passes it.
struct NoLoadingVeto;

impl AgentProfile for NoLoadingVeto {
    fn id(&self) -> &str {
        "codex"
    }

    fn dialog(&self, text: &str) -> Option<Dialog> {
        CodexProfile.dialog(text)
    }

    fn input_ready(&self, screen: &str) -> bool {
        let text = normalize(screen);
        let hints = [
            "for shortcuts",
            "to submit message",
            "to queue message",
            "to edit previous message",
        ];
        let chrome = has_any(&text, &hints)
            || has_all(&text, &["directory:", "permissions:", "model:"])
            || has_any(&text, &["type a message", "ask codex"]);
        CodexProfile.dialog(screen).is_none() && text.contains("codex") && chrome
    }
}

#[test]
fn without_the_loading_veto_the_post_ready_watch_steers_the_dialog() {
    for (scene, key) in [
        ("trust", TrustKey::Enter),
        ("update-prompt", TrustKey::Down),
    ] {
        let f = fixtures::find("codex", scene);
        let steps = replay(&f, &NoLoadingVeto);
        check_dialog_frame(&f, &steps, key, true);
    }
}

/// A claude that must not accept its bypass-permissions warning (the profile has no key for
/// it): the gate presses nothing and stays in `TrustPrompt` past the trust window, with the
/// overlay lifted for the user — the first send never types into the warning. claude draws
/// no DEC 2026 frames: the stream arrives in 256-byte cuts, 10 ms apart.
#[test]
fn an_unaccepted_bypass_warning_parks_the_gate() {
    let f = fixtures::find("claude", "bypass-warning");
    let profile = ClaudeProfile::accepting_bypass_warning(false);
    let mut model = Vt100Model::new(f.size(), 0, None);
    let mut gate = StartupGate::new(Timing::default(), true);
    let t0 = Instant::now();
    gate.launch(t0);
    let mut effects = Vec::new();
    let mut saw_dialog = false;
    for (n, bytes) in f.frames().iter().enumerate() {
        let at = t0 + Duration::from_millis(10 * n as u64);
        effects.extend(gate.on_deadline(at, &model.snapshot().text(), &profile));
        gate.on_output(bytes, at);
        model.process(bytes);
        effects.extend(gate.evaluate(&model.snapshot().text(), &profile, at));
        assert_ne!(gate.state(), AgentState::Ready, "{} frame {n}", f.label());
        saw_dialog |= gate.state() == AgentState::TrustPrompt;
    }
    assert!(saw_dialog, "{}: no dialog", f.label());
    let screen = model.snapshot().text();
    for secs in 1..=60 {
        effects.extend(gate.on_deadline(t0 + Duration::from_secs(secs), &screen, &profile));
        assert_eq!(gate.state(), AgentState::TrustPrompt, "at {secs} s");
    }
    assert!(!gate.blocking());
    assert_eq!(gate.next_deadline(), None);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, super::GateEffect::Key { .. })),
        "{effects:?}"
    );
}
