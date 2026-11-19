//! The built-in agent profiles against the recorded sessions in `tests/fixtures`:
//! - every final screen (`.txt`) classifies as recorded, and the raw streams (`.bin`, read
//!   like a rolling output buffer) show the same dialogs with the same keys;
//! - pending input on the paste, submit and slash-menu fixtures;
//! - frame-by-frame replays through the screen model — what a startup gate sees: codex's
//!   loading composer never reads ready before its dialogs.
//!
//! Each fixture's profile is the built-in named by the `agent=` of its `.meta`.
#![cfg(feature = "agent")]

mod common;

use std::sync::Arc;

use agent_link::agent::detect::pending_input_default;
use agent_link::agent::profiles::{ClaudeProfile, PiProfile, PiTrust, builtin};
use agent_link::agent::{AgentProfile, Dialog, DialogKind, ScreenState, TrustKey};
use agent_link::screen::{ScreenModel, Vt100Model};
use common::fixtures::{self, Fixture};

/// The rolling output buffer a startup gate keeps (the last 16 000 characters).
const ROLLING_CHARS: usize = 16_000;

fn profile(agent: &str) -> Arc<dyn AgentProfile> {
    builtin()
        .into_iter()
        .find(|p| p.id() == agent)
        .unwrap_or_else(|| panic!("no built-in profile {agent}"))
}

/// The raw stream decoded like a rolling output buffer.
fn raw(f: &Fixture) -> String {
    let text = String::from_utf8_lossy(&f.stream);
    let skip = text.chars().count().saturating_sub(ROLLING_CHARS);
    text.chars().skip(skip).collect()
}

/// The text the capture script pasted or typed first, bracketed-paste markers removed.
fn sent(f: &Fixture) -> String {
    let first = f.script_writes().into_iter().next();
    let first = first.unwrap_or_else(|| panic!("{}: no script write", f.label()));
    String::from_utf8_lossy(&first)
        .replace("\x1b[200~", "")
        .replace("\x1b[201~", "")
}

/// `screen` with the row whose trimmed text is `from` replaced by `to` (moves a pointer).
fn with_row(screen: &str, from: &str, to: &str) -> String {
    let mut rows: Vec<&str> = screen.split('\n').collect();
    let at = rows
        .iter()
        .position(|row| row.trim() == from.trim())
        .unwrap_or_else(|| panic!("row not found: {from}"));
    rows[at] = to;
    rows.join("\n")
}

fn dialog(kind: DialogKind, key: Option<TrustKey>) -> ScreenState {
    ScreenState::Dialog(Dialog { kind, key })
}

/// What the built-in profile must read on each fixture's final screen.
fn expected(agent: &str, scene: &str) -> ScreenState {
    match (agent, scene) {
        ("claude", "trust") => dialog(DialogKind::Trust, Some(TrustKey::Down)),
        (_, "trust") => dialog(DialogKind::Trust, Some(TrustKey::Enter)),
        ("codex", "update-prompt") => dialog(DialogKind::UpdatePrompt, Some(TrustKey::Down)),
        // The built-in claude profile accepts the bypass warning: Down to "Yes, I accept",
        // then Enter.
        ("claude", "bypass-warning") => dialog(DialogKind::BypassWarning, Some(TrustKey::Down)),
        ("claude", "bypass-warning-down") => {
            dialog(DialogKind::BypassWarning, Some(TrustKey::Enter))
        }
        // The open slash menu replaces claude's footer, which holds its ready markers.
        (
            "claude",
            "slash-menu"
            | "question-single"
            | "question-multi"
            | "question-multiselect"
            | "question-multipage-review"
            | "long-reply-select-hint",
        ) => ScreenState::Unknown,
        ("opencode", "question") => ScreenState::Unknown,
        // Captured with --force, signed out. Login is not composer evidence.
        ("cursor", "force-login" | "shell-approval-renderer") => ScreenState::Unknown,
        _ => ScreenState::InputReady,
    }
}

#[test]
fn every_final_screen_classifies_as_recorded() {
    let all = fixtures::all();
    let mut failures = Vec::new();
    for f in &all {
        let want = expected(&f.meta.agent, &f.meta.scene);
        let got = profile(&f.meta.agent).classify(&f.screen());
        if got != want {
            failures.push(format!("{}: got {got:?}, want {want:?}", f.label()));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} fixtures:\n{}",
        failures.len(),
        all.len(),
        failures.join("\n")
    );
    // Every dialog the spike measured has a fixture here.
    for (agent, scene) in [
        ("claude", "trust"),
        ("claude", "bypass-warning"),
        ("codex", "trust"),
        ("codex", "update-prompt"),
        ("pi", "trust"),
        ("cursor", "trust"),
    ] {
        fixtures::find(agent, scene);
    }
}

#[test]
fn stock_profiles_recognize_only_the_recorded_active_questions() {
    let questions = [
        ("claude", "question-single"),
        ("claude", "question-multi"),
        ("claude", "question-multiselect"),
        ("claude", "question-multipage-review"),
        ("codex", "question-plan"),
        ("codex", "question-multipage-begin"),
        ("codex", "question-multipage-review"),
        ("codex", "question-multipage-final-notes"),
        ("codex", "question-notes-empty"),
        ("opencode", "question"),
        ("cursor", "shell-approval-renderer"),
    ];
    for (agent, scene) in questions {
        fixtures::find(agent, scene);
    }
    for scene in ["prose-dialog-footer", "long-reply-select-hint"] {
        let quoted = fixtures::find("claude", scene).screen();
        assert!(quoted.contains("4. Chat about this"), "{scene}");
        assert!(
            quoted.contains("Enter to select · ↑/↓ to navigate · Esc to cancel"),
            "{scene}"
        );
    }
    for f in fixtures::all() {
        let active = questions.contains(&(f.meta.agent.as_str(), f.meta.scene.as_str()));
        let current = profile(&f.meta.agent);
        let screen = f.screen();
        assert_eq!(current.question_dialog(&screen), active, "{}", f.label());
        if active {
            assert!(current.dialog(&screen).is_none(), "{}", f.label());
            for redraw in ["❯", "> pending draft"] {
                assert!(
                    !current.question_dialog(&format!("{screen}\n{redraw}")),
                    "{}",
                    f.label()
                );
            }
            for other in builtin().iter().filter(|p| p.id() != current.id()) {
                assert!(
                    !other.question_dialog(&screen),
                    "{} matched {}",
                    other.id(),
                    f.label()
                );
            }
        }
    }
}

#[test]
fn raw_streams_show_the_recorded_dialogs() {
    for (agent, scene, kind, key) in [
        ("claude", "trust", DialogKind::Trust, Some(TrustKey::Down)),
        (
            "claude",
            "bypass-warning",
            DialogKind::BypassWarning,
            Some(TrustKey::Down),
        ),
        ("codex", "trust", DialogKind::Trust, Some(TrustKey::Enter)),
        (
            "codex",
            "update-prompt",
            DialogKind::UpdatePrompt,
            Some(TrustKey::Down),
        ),
        ("pi", "trust", DialogKind::Trust, Some(TrustKey::Enter)),
        ("cursor", "trust", DialogKind::Trust, Some(TrustKey::Enter)),
    ] {
        let f = fixtures::find(agent, scene);
        assert_eq!(
            profile(agent).dialog(&raw(&f)),
            Some(Dialog { kind, key }),
            "{}",
            f.label()
        );
    }
}

#[test]
fn the_claude_bypass_warning_is_never_ready() {
    let p = profile("claude");
    for scene in ["bypass-warning", "bypass-warning-down"] {
        let f = fixtures::find("claude", scene);
        assert!(!p.input_ready(&f.screen()), "{}", f.label());
    }
}

#[test]
fn a_declining_claude_profile_leaves_the_recorded_bypass_warning_to_the_user() {
    let p = ClaudeProfile::accepting_bypass_warning(false);
    for scene in ["bypass-warning", "bypass-warning-down"] {
        let f = fixtures::find("claude", scene);
        let left = dialog(DialogKind::BypassWarning, None);
        assert_eq!(p.classify(&f.screen()), left, "{}", f.label());
        assert_eq!(
            p.dialog(&raw(&f)).map(|d| d.key),
            Some(None),
            "{}",
            f.label()
        );
    }
    // Once accepted (no warning on screen) both profiles read the composer as ready.
    let accepted = fixtures::find("claude", "bypass-accepted");
    assert_eq!(p.classify(&accepted.screen()), ScreenState::InputReady);
}

#[test]
fn an_accepting_claude_profile_steers_the_recorded_bypass_warning() {
    // The built-in accepts the warning.
    assert!(ClaudeProfile::new().accepts_bypass_warning());
    let p = ClaudeProfile::accepting_bypass_warning(true);
    let warning = fixtures::find("claude", "bypass-warning");
    let down = fixtures::find("claude", "bypass-warning-down");
    let accepted = fixtures::find("claude", "bypass-accepted");
    let key = |text: &str| p.dialog(text).map(|d| (d.kind, d.key));
    let bypass = |k| Some((DialogKind::BypassWarning, Some(k)));
    assert_eq!(key(&warning.screen()), bypass(TrustKey::Down));
    assert_eq!(key(&raw(&warning)), bypass(TrustKey::Down));
    assert_eq!(key(&down.screen()), bypass(TrustKey::Enter));
    // The raw stream holds the first render and the redraw after Down: the last pointer counts.
    assert_eq!(key(&raw(&down)), bypass(TrustKey::Enter));
    assert_eq!(p.classify(&accepted.screen()), ScreenState::InputReady);
    // Never Enter blind on "No, exit".
    let unreadable = down.screen().replace("❯ Yes, I accept", "  Yes, I accept");
    assert_eq!(key(&unreadable), Some((DialogKind::BypassWarning, None)));
}

#[test]
fn the_codex_update_prompt_is_steered_to_skip_never_update_now() {
    let p = profile("codex");
    let screen = fixtures::find("codex", "update-prompt").screen();
    let key = |text: &str| p.dialog(text).and_then(|d| d.key);
    assert_eq!(key(&screen), Some(TrustKey::Down));
    let unselected = screen.replacen("› 1. Update now", "  1. Update now", 1);
    let on_skip = with_row(&unselected, "2. Skip", "› 2. Skip");
    assert_eq!(key(&on_skip), Some(TrustKey::Enter));
    let on_skip_until = with_row(
        &unselected,
        "3. Skip until next version",
        "› 3. Skip until next version",
    );
    assert_eq!(key(&on_skip_until), Some(TrustKey::Up));
    assert_eq!(key(&unselected), None);
    assert!(p.dialog(&unselected).is_some());
}

#[test]
fn codex_matches_its_busy_turn_clear_error_including_the_wrapped_capture() {
    let p = profile("codex");
    for scene in ["clear-refused", "clear-refused-wrapped"] {
        let screen = fixtures::find("codex", scene).screen();
        assert!(p.reset_rejected(&screen), "{scene}");
        assert_eq!(p.pending_input(&screen, "/clear"), Some(false), "{scene}");
    }
    assert!(!p.reset_rejected(&fixtures::find("codex", "clear").screen()));
}

#[test]
fn codex_ignores_quotations_drafts_and_incomplete_clear_errors() {
    let p = profile("codex");
    let notice = "■ '/clear' is disabled while a task is in progress.";
    for screen in [
        format!("• {notice}"),
        format!("• Here is a quote:\n\n  {notice}\n\n  Done."),
        format!("› {notice}"),
        format!("› An example\n\n  {notice}\n  Another line"),
        format!("{notice} Continued text."),
        "'/clear' is disabled while a task is in progress.".into(),
        "■ '/clear' is disabled while a task is\n\nin progress.".into(),
        "■ '/clear' is disabled while a task is\n• Working\nin progress.".into(),
        "■ '/clear' is disabled while a task is\n  in progress.".into(),
    ] {
        assert!(!p.reset_rejected(&screen), "{screen}");
    }
}

#[test]
fn a_codex_dialog_wins_over_ready_text_on_the_same_screen() {
    let p = profile("codex");
    let ready = fixtures::find("codex", "ready").screen();
    let trust = fixtures::find("codex", "trust").screen();
    let update = fixtures::find("codex", "update-prompt").screen();
    assert_eq!(p.classify(&ready), ScreenState::InputReady);
    let trust_kind = |text: &str| match p.classify(text) {
        ScreenState::Dialog(d) => Some(d.kind),
        _ => None,
    };
    assert_eq!(
        trust_kind(&format!("{ready}\n{trust}")),
        Some(DialogKind::Trust)
    );
    assert_eq!(
        trust_kind(&format!("{ready}\n{update}")),
        Some(DialogKind::UpdatePrompt)
    );
}

#[test]
fn a_codex_rolling_buffer_answers_the_latest_dialog() {
    let p = profile("codex");
    let update = raw(&fixtures::find("codex", "update-prompt"));
    let trust = raw(&fixtures::find("codex", "trust"));
    let read = |text: String| p.dialog(&text).map(|d| (d.kind, d.key));
    assert_eq!(
        read(format!("{update}{trust}")),
        Some((DialogKind::Trust, Some(TrustKey::Enter)))
    );
    assert_eq!(
        read(format!("{trust}{update}")),
        Some((DialogKind::UpdatePrompt, Some(TrustKey::Down)))
    );
}

#[test]
fn a_codex_update_notice_without_its_options_is_not_a_dialog() {
    let ready = fixtures::find("codex", "ready").screen();
    let notice = format!("{ready}\n✨ Update available! 0.154.0 -> 0.161.0");
    assert_eq!(profile("codex").classify(&notice), ScreenState::InputReady);
}

#[test]
fn cursor_steers_up_from_quit_and_leaves_an_unreadable_selection_alone() {
    let p = profile("cursor");
    let screen = fixtures::find("cursor", "trust").screen();
    assert!(!p.input_ready(&screen));
    let key = |text: &str| p.dialog(text).and_then(|d| d.key);
    let unselected = screen.replacen(
        "▶ [a] Trust this workspace",
        "  [a] Trust this workspace",
        1,
    );
    let on_quit = unselected.replacen("  [q] Quit", "▶ [q] Quit", 1);
    assert_eq!(key(&on_quit), Some(TrustKey::Up));
    assert_eq!(key(&unselected), None);
}

#[test]
fn the_pi_session_only_answer_steers_the_recorded_dialog() {
    let session_only = PiProfile::with_trust(PiTrust::SessionOnly);
    let key = |text: &str| session_only.dialog(text).and_then(|d| d.key);
    let screen = fixtures::find("pi", "trust").screen();
    assert_eq!(key(&screen), Some(TrustKey::Down));
    let unselected = with_row(&screen, "→ Trust", "   Trust");
    assert_eq!(key(&unselected), None);
    let on = |label: &str| with_row(&unselected, label, &format!(" → {label}"));
    assert_eq!(
        key(&on("Trust parent folder (/private/tmp)")),
        Some(TrustKey::Down)
    );
    assert_eq!(key(&on("Trust (this session only)")), Some(TrustKey::Enter));
    assert_eq!(key(&on("Do not trust")), Some(TrustKey::Up));
    assert_eq!(
        key(&on("Do not trust (this session only)")),
        Some(TrustKey::Up)
    );
    // The persisting built-in answer is Enter whatever the pointer.
    assert_eq!(
        profile("pi")
            .dialog(&on("Do not trust"))
            .and_then(|d| d.key),
        Some(TrustKey::Enter)
    );
}

#[test]
fn pending_input_on_the_composer_fixtures() {
    let mut failures = Vec::new();
    let mut checked = 0;
    for f in fixtures::all() {
        let want = match f.meta.scene.as_str() {
            // Inline 3-line paste (the sent-fragment rule), the long-paste chip, a typed
            // `/clear` (`/new` for pi) under an open slash menu.
            "paste-chip" | "paste-chip-long" | "slash-menu" => Some(true),
            // Submitted: the composer is empty or shows its placeholder.
            "submit" | "swallowed-enter" | "clear" => Some(false),
            _ => continue,
        };
        checked += 1;
        let got = profile(&f.meta.agent).pending_input(&f.screen(), &sent(&f));
        if got != want {
            failures.push(format!("{}: got {got:?}, want {want:?}", f.label()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(
        checked, 18,
        "claude, codex and pi each have six composer fixtures"
    );
}

#[test]
fn the_default_pending_rule_misreads_claude_and_pi() {
    // claude's selected slash-menu row carries the prompt marker below the composer.
    let f = fixtures::find("claude", "slash-menu");
    assert_eq!(pending_input_default(&f.screen(), "/clear"), Some(false));
    // pi draws no prompt marker at all.
    for scene in ["paste-chip", "slash-menu", "submit"] {
        let f = fixtures::find("pi", scene);
        assert_eq!(
            pending_input_default(&f.screen(), &sent(&f)),
            None,
            "{scene}"
        );
    }
}

// --- frame-by-frame replay ---------------------------------------------------------------

struct Frame {
    state: ScreenState,
    screen: String,
}

/// Replays `f` through the screen model and classifies the screen after every frame: after
/// each DEC 2026 frame end, or every 256 bytes for a stream without frames, and at the end.
fn replay(f: &Fixture, profile: &dyn AgentProfile) -> Vec<Frame> {
    let mut model = Vt100Model::new(f.size(), 0, None);
    let mut frames = Vec::new();
    for bytes in f.frames() {
        model.process(bytes);
        let screen = model.snapshot().text();
        frames.push(Frame {
            state: profile.classify(&screen),
            screen,
        });
    }
    frames
}

/// The states as runs: `u` unknown, `D` dialog, `R` ready, e.g. `u12 D1`.
fn runs(frames: &[Frame]) -> String {
    let mut out: Vec<(char, usize)> = Vec::new();
    for frame in frames {
        let c = match frame.state {
            ScreenState::Dialog(_) => 'D',
            ScreenState::InputReady => 'R',
            ScreenState::Unknown => 'u',
        };
        match out.last_mut() {
            Some((last, n)) if *last == c => *n += 1,
            _ => out.push((c, 1)),
        }
    }
    let runs: Vec<String> = out.iter().map(|(c, n)| format!("{c}{n}")).collect();
    runs.join(" ")
}

/// The keys of the dialog frames, in order.
fn dialog_keys(frames: &[Frame]) -> Vec<Option<TrustKey>> {
    frames
        .iter()
        .filter_map(|f| match f.state {
            ScreenState::Dialog(d) => Some(d.key),
            _ => None,
        })
        .collect()
}

fn loading(screen: &str) -> bool {
    agent_link::screen::normalize(screen).contains("model: loading")
}

/// The header shows a model name.
fn model_named(screen: &str) -> bool {
    let text = agent_link::screen::normalize(screen);
    text.contains("model: ") && !text.contains("model: loading")
}

#[test]
fn codex_dialogs_follow_a_loading_composer_that_never_reads_ready() {
    for (scene, want, key) in [
        ("trust", "u12 D1", TrustKey::Enter),
        ("update-prompt", "u10 D1", TrustKey::Down),
    ] {
        let f = fixtures::find("codex", scene);
        let frames = replay(&f, profile("codex").as_ref());
        assert_eq!(runs(&frames), want, "{}", f.label());
        assert!(frames.iter().any(|fr| loading(&fr.screen)), "{}", f.label());
        assert_eq!(dialog_keys(&frames), [Some(key)], "{}", f.label());
    }
}

#[test]
fn codex_trust_confirmed_reads_ready_only_once_the_model_is_named() {
    let f = fixtures::find("codex", "trust-confirmed");
    let frames = replay(&f, profile("codex").as_ref());
    assert_eq!(runs(&frames), "u12 D1 u11 R3");
    for frame in &frames {
        if frame.state == ScreenState::InputReady {
            assert!(model_named(&frame.screen), "ready on:\n{}", frame.screen);
        }
    }
    assert_eq!(dialog_keys(&frames), [Some(TrustKey::Enter)]);
}

#[test]
fn codex_ready_is_unknown_while_loading_and_ready_from_the_model_name_on() {
    let f = fixtures::find("codex", "ready");
    let frames = replay(&f, profile("codex").as_ref());
    assert_eq!(runs(&frames), "u19 R3");
    let named = frames
        .iter()
        .position(|fr| model_named(&fr.screen))
        .expect("a frame names the model");
    assert!(frames[..named].iter().any(|fr| loading(&fr.screen)));
    assert!(
        frames[..named]
            .iter()
            .all(|fr| fr.state == ScreenState::Unknown)
    );
    assert!(
        frames[named..]
            .iter()
            .all(|fr| fr.state == ScreenState::InputReady)
    );
}

#[test]
fn other_startups_replay_to_their_recorded_ends() {
    let claude = profile("claude");
    let f = fixtures::find("claude", "trust-confirmed");
    let frames = replay(&f, claude.as_ref());
    assert_eq!(runs(&frames), "u4 D1 u6 R2", "{}", f.label());
    assert_eq!(dialog_keys(&frames), [Some(TrustKey::Down)]);

    for scene in ["bypass-accepted", "ready-fullscreen", "ready"] {
        let f = fixtures::find("claude", scene);
        let frames = replay(&f, claude.as_ref());
        let last = frames.last().map(|fr| fr.state);
        assert_eq!(last, Some(ScreenState::InputReady), "{}", f.label());
    }

    let f = fixtures::find("pi", "trust-confirmed");
    let frames = replay(&f, profile("pi").as_ref());
    assert_eq!(runs(&frames), "D3 u1 R4", "{}", f.label());
    assert_eq!(dialog_keys(&frames), [Some(TrustKey::Enter); 3]);

    let f = fixtures::find("cursor", "trust");
    let frames = replay(&f, profile("cursor").as_ref());
    assert_eq!(runs(&frames), "u7 D5", "{}", f.label());
    assert_eq!(dialog_keys(&frames).last(), Some(&Some(TrustKey::Enter)));
}

#[test]
fn cursor_force_login_never_proves_composer_readiness() {
    let f = fixtures::find("cursor", "force-login");
    let profile = profile("cursor");
    let frames = replay(&f, profile.as_ref());
    assert!(!frames.is_empty());
    assert!(
        frames
            .iter()
            .all(|frame| frame.state != ScreenState::InputReady)
    );
    assert!(f.screen().contains("Press any key to log in"));
    assert_eq!(profile.pending_input(&f.screen(), "probe"), None);
}

#[test]
fn the_pi_session_only_answer_follows_the_recorded_key_presses() {
    // The recording pressed Down, Down, Enter: the session-only profile asks for exactly
    // those keys on the three dialog frames.
    let f = fixtures::find("pi", "trust-confirmed");
    let profile = PiProfile::with_trust(PiTrust::SessionOnly);
    let frames = replay(&f, &profile);
    assert_eq!(
        dialog_keys(&frames),
        [
            Some(TrustKey::Down),
            Some(TrustKey::Down),
            Some(TrustKey::Enter)
        ]
    );
    let writes = f.script_writes();
    let recorded: Vec<&[u8]> = writes.iter().map(Vec::as_slice).collect();
    assert_eq!(
        recorded,
        [
            TrustKey::Down.bytes(),
            TrustKey::Down.bytes(),
            TrustKey::Enter.bytes()
        ]
    );
}

#[test]
fn the_claude_bypass_warning_replays() {
    // Declining: recognised, left to the user, never ready.
    let f = fixtures::find("claude", "bypass-warning");
    let declining = ClaudeProfile::accepting_bypass_warning(false);
    let frames = replay(&f, &declining);
    assert_eq!(runs(&frames), "u4 D2", "{}", f.label());
    assert_eq!(dialog_keys(&frames), [None, None]);
    // The built-in accepts: Down on both renders (the recording pressed nothing).
    let frames = replay(&f, profile("claude").as_ref());
    assert_eq!(runs(&frames), "u4 D2", "{}", f.label());
    assert_eq!(dialog_keys(&frames), [Some(TrustKey::Down); 2]);

    // Accepting: Down on the first render, Enter once the pointer is on "Yes, I accept" —
    // the keys the recording pressed — then ready.
    let f = fixtures::find("claude", "bypass-accepted");
    let accepting = profile("claude");
    let frames = replay(&f, accepting.as_ref());
    assert_eq!(runs(&frames), "u4 D2 u6 R2", "{}", f.label());
    let keys = dialog_keys(&frames);
    assert_eq!(keys.first(), Some(&Some(TrustKey::Down)));
    assert_eq!(keys.last(), Some(&Some(TrustKey::Enter)));
}
