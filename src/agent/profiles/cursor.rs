//! Work-in-progress Cursor Agent CLI profile: workspace-trust detection and launch.
//!
//! Authenticated startup/composer behavior with the default `--force` launch remains
//! unverified. Recorded trust and signed-out login screens do not establish that support.
//! Composer input is unreadable, so verified submission remains unavailable.
//!
//! | | cursor |
//! |---|---|
//! | `launchers()` | `cursor-agent`, `agent` (never `cursor`: that is the editor) |
//! | `launch()` | `agent`; unattended: `--force` (YOLO; explicit deny rules still apply); pre-trust: `--trust` (**saves** a trust record per cwd) |
//! | `question_dialog()` | shell approval with Run/Skip options (2026.11.13-eacade1 isolated packaged renderer) |
//! | `reset_command()` | `/new` |
//! | `login_check()` | none |
//! | `rpc_launch()` | none |
//! | `signals()` | no sync frames at cold launch; title spinner and OSC 9;4 not measured |

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::agent::detect::{dialog_at_bottom, from_last, has_all, selected_option};
use crate::agent::{
    AgentProfile, Dialog, DialogKind, LaunchPlan, Permissions, ShellKind, SignalHints, TrustKey,
    TrustPolicy, Unlock,
};
use crate::screen::{cached, normalize};

// Cursor's documented YOLO equivalent (`--yolo` aliases `--force`), checked 2026-11-14:
// https://cursor.com/docs/cli/reference/parameters . Explicit denies still apply.
// Confirmed with --help; a 2026.11.13-eacade1 --force recording reaches login without
// credentials, not a composer.
// Follow-up authenticated startup/composer validation: cursor_force_live (profile is WIP).
const UNATTENDED_ARGS: &[&str] = &["--force"];

// Workspace-trust box. verified: cursor-agent 2026.09.23: "⚠ Workspace Trust Required" /
// "▶ [a] Trust this workspace" (listed first, selected) / "[q] Quit". It also contains
// "Cursor Agent" and codex's "Do you trust the contents of this directory?". The `a`
// hotkey is not used (unmeasured); Enter only on "Trust this workspace".
const TRUST_HEADER: &str = "workspace trust required";
const TRUST_YES: &str = "trust this workspace";

// Ready composer. verified: cursor-agent 2026.09.23 (seen once trusted; no fixture): header
// "Cursor Agent" + composer placeholder "Plan, search, build anything". The trust box also
// says "Cursor Agent", never the placeholder.
const READY: &[&str] = &["cursor agent", "plan, search, build anything"];

/// Trust-box pointer "▶" (U+25B6) before the `[a]` / `[q]` hotkey label.
fn trust_pointer() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    cached(&RE, r"▶ ?(?:\[[a-z]\] ?)?(trust this workspace|quit)")
}

/// Work-in-progress Cursor profile; authenticated `--force` startup/composer behavior
/// remains unverified. Pending input uses the default rule: cursor's composer marker is `→`,
/// which that rule does not read, so it reads `None` (unreadable) — not measured with a
/// paste. Verified delivery therefore refuses submission with `SubmitUnverified`
/// when no readable composer evidence is available, unless the send opts into one
/// unverified Enter ([`UnreadableComposer::EnterOnce`](crate::agent::UnreadableComposer::EnterOnce),
/// not measured against cursor).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorProfile;

fn dialog_visible(text: &str) -> bool {
    has_all(text, &[TRUST_HEADER, TRUST_YES])
}

impl CursorProfile {
    /// cursor's best-effort ambient source that also reports its **unseparated** reply
    /// text: the text blocks after the turn's last tool call, joined by a blank line,
    /// exactly as cursor wrote them. cursor joins a message's reasoning into its text block
    /// with no marker, so this text can mix model reasoning with the displayed answer;
    /// treat it accordingly. Pass it to
    /// [`AgentTerminal::attach_ambient_turns_with`](crate::agent::AgentTerminal::attach_ambient_turns_with).
    /// The profile's own [`ambient_turns`](AgentProfile::ambient_turns) reports completion
    /// only (every `reply` is `None`), as the controlled tier's `TranscriptSource::Cursor`
    /// does; this is the ambient counterpart of `CursorWithUnseparatedText`. Best effort,
    /// see [`turns::ambient`](crate::agent::turns::ambient).
    #[cfg(feature = "turns")]
    pub fn ambient_turns_with_unseparated_text(
        &self,
    ) -> std::sync::Arc<dyn crate::agent::turns::ambient::AmbientSource> {
        std::sync::Arc::new(super::ambient::cursor::CursorAmbient { text: true })
    }
}

impl AgentProfile for CursorProfile {
    /// The project's conversation created since the launch and written last under
    /// `CURSOR_DATA_DIR` (else `~/.cursor`); a successful `turn_ended` ends a turn
    /// explicitly, the next prompt one whose end a rewrite dropped. Completion only: every
    /// `reply` is `None` (opt in with
    /// [`ambient_turns_with_unseparated_text`](CursorProfile::ambient_turns_with_unseparated_text)).
    /// Best effort, see [`turns::ambient`](crate::agent::turns::ambient).
    #[cfg(feature = "turns")]
    fn ambient_turns(
        &self,
    ) -> Option<std::sync::Arc<dyn crate::agent::turns::ambient::AmbientSource>> {
        Some(std::sync::Arc::new(super::ambient::cursor::CursorAmbient {
            text: false,
        }))
    }
    fn compatibility(&self) -> crate::agent::compatibility::Compatibility {
        use crate::agent::compatibility::{Compatibility, Evidence, EvidenceKind, Support};
        Compatibility {
            revision: Some(env!("CARGO_PKG_VERSION")),
            pty: Support::Supported,
            rpc_launch: Support::Unsupported,
            login_check: Support::Unsupported,
            evidence: &[
                Evidence {
                    operation: "packaged CLI transcript writer serialization and rewrites",
                    kind: EvidenceKind::LiveCheck,
                    cli_version: Some("2026.11.13-eacade1"),
                    platform: Some("macos-aarch64"),
                    source: "tests/fixtures/transcripts/cursor-2026.11.13-eacade1/README.md",
                },
                #[cfg(feature = "questions")]
                Evidence {
                    operation: "shell command approval relay and decision key intents",
                    kind: EvidenceKind::LiveCheck,
                    cli_version: Some("2026.11.13-eacade1"),
                    platform: Some("macos-aarch64"),
                    source: "tests/fixtures/questions/README.md",
                },
                Evidence {
                    operation: "trust screen recognition",
                    kind: EvidenceKind::CapturedTerminal,
                    cli_version: Some("2026.09.23-86fc751"),
                    platform: None,
                    source: "tests/fixtures/cursor-2026.09.23-trust.bin",
                },
                Evidence {
                    operation: "question dialog recognition",
                    kind: EvidenceKind::CapturedTerminal,
                    cli_version: Some("2026.11.13-eacade1"),
                    platform: Some("macos-aarch64"),
                    source: "tests/fixtures/cursor-2026.11.13-shell-approval-renderer.bin",
                },
                #[cfg(feature = "turns")]
                Evidence {
                    cli_version: Some("2026.10.01"),
                    source: "tests/fixtures/transcripts/ambient/cursor-2026.10.01/README.md",
                    ..super::AMBIENT_PARSING
                },
                // The packaged writer's own images (fabricated inputs), as the controlled
                // tier records them.
                #[cfg(feature = "turns")]
                Evidence {
                    kind: EvidenceKind::LiveCheck,
                    cli_version: Some("2026.11.13-eacade1"),
                    platform: Some("macos-aarch64"),
                    source: "tests/fixtures/transcripts/cursor-2026.11.13-eacade1/README.md",
                    ..super::AMBIENT_PARSING
                },
                #[cfg(feature = "turns")]
                super::AMBIENT_LOOKUP,
            ],
        }
    }

    #[cfg(feature = "questions")]
    fn question_on_screen(&self, screen: &str) -> Option<crate::agent::questions::PendingQuestion> {
        crate::agent::questions::providers::cursor(screen)
    }
    #[cfg(feature = "questions")]
    fn question_shown(
        &self,
        screen: &str,
        pending: &crate::agent::questions::PendingQuestion,
    ) -> bool {
        use crate::agent::questions::providers::{self, Provider};
        providers::shown(Provider::Cursor, screen, pending)
    }
    #[cfg(feature = "questions")]
    fn answer_keys(
        &self,
        screen: &str,
        pending: &crate::agent::questions::PendingQuestion,
        answers: &[crate::agent::questions::Answer],
    ) -> Result<Vec<crate::agent::questions::KeyStep>, crate::agent::questions::AnswerError> {
        use crate::agent::questions::providers::{self, Provider};
        providers::keys(Provider::Cursor, screen, pending, answers)
    }
    fn id(&self) -> &str {
        "cursor"
    }

    fn dialog(&self, text: &str) -> Option<Dialog> {
        let text = normalize(text);
        if text.is_empty() || !dialog_visible(&text) {
            return None;
        }
        let key = match selected_option(from_last(&text, TRUST_HEADER), trust_pointer()) {
            Some("trust this workspace") => Some(TrustKey::Enter),
            Some("quit") => Some(TrustKey::Up),
            _ => None,
        };
        Some(Dialog {
            kind: DialogKind::Trust,
            key,
        })
    }

    fn question_dialog(&self, screen: &str) -> bool {
        dialog_at_bottom(
            screen,
            24,
            "run this command?",
            &["run (once) (y)", "skip & tell the agent what to do instead"],
            "(esc or n)",
        )
    }

    fn input_ready(&self, screen: &str) -> bool {
        let text = normalize(screen);
        !text.is_empty() && !dialog_visible(&text) && has_all(&text, READY)
    }

    /// The official CLI is `agent`; the installer also links the older `cursor-agent`.
    fn launchers(&self) -> &'static [&'static str] {
        &["cursor-agent", "agent"]
    }

    /// Launches `agent`. Pre-trust: `--trust` suppresses the workspace-trust dialog
    /// (verified: cursor-agent 2026.09.23) and **saves** the decision — a trust record
    /// `~/.cursor/projects/<cwd slug>/.workspace-trusted` (`trustMethod: "cli-flag"`) per
    /// launch directory. [`PreTrust::Off`](crate::agent::PreTrust::Off) leaves it out (the
    /// dialog is steered instead — whether that answer is saved is unmeasured).
    fn launch(
        &self,
        perms: &Permissions,
        trust: &TrustPolicy,
        _cwd: &Path,
        _shell: ShellKind,
    ) -> LaunchPlan {
        let mut plan = LaunchPlan::new("agent").with_permissions(perms, UNATTENDED_ARGS, &[]);
        if trust.pre_trust.wants_flags() {
            plan.args.push("--trust".into());
        }
        plan
    }

    fn reset_command(&self) -> Option<&'static str> {
        Some("/new")
    }

    /// verified: cursor-agent 2026.09.23, cold launch only — title `Cursor Agent`, no DEC
    /// 2026 frames (a turn not measured).
    fn signals(&self) -> SignalHints {
        SignalHints {
            title_spinner: false,
            osc_progress: Unlock::Unknown,
            sync_frames: Unlock::Never,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ScreenState;

    fn trust_box(trust_row: &str, quit_row: &str) -> String {
        [
            "  ╭──────────────────────────────╮",
            "  │  ⚠ Workspace Trust Required  │",
            "  │  Cursor Agent can execute code and access files in this directory. │",
            "  │  Do you trust the contents of this directory? │",
            trust_row,
            quit_row,
            "  │  Use arrow keys to navigate, Enter to select, or press the key shown │",
            "  ╰──────────────────────────────╯",
        ]
        .join("\n")
    }

    fn key(text: &str) -> Option<TrustKey> {
        CursorProfile.dialog(text).and_then(|d| d.key)
    }

    #[test]
    fn the_trust_box_is_a_dialog_confirmed_with_enter() {
        let text = trust_box("  │  ▶ [a] Trust this workspace │", "  │    [q] Quit │");
        assert_eq!(
            CursorProfile.dialog(&text),
            Some(Dialog {
                kind: DialogKind::Trust,
                key: Some(TrustKey::Enter)
            })
        );
        assert!(!CursorProfile.input_ready(&text));
    }

    #[test]
    fn steers_up_from_quit_and_leaves_an_unreadable_selection_alone() {
        let on_quit = trust_box("  │    [a] Trust this workspace │", "  │  ▶ [q] Quit │");
        assert_eq!(key(&on_quit), Some(TrustKey::Up));
        let unselected = trust_box("  │    [a] Trust this workspace │", "  │    [q] Quit │");
        assert_eq!(key(&unselected), None);
        assert!(CursorProfile.dialog(&unselected).is_some());
    }

    #[test]
    fn the_trust_box_needs_both_markers() {
        assert_eq!(CursorProfile.dialog("⚠ Workspace Trust Required"), None);
        assert_eq!(CursorProfile.dialog("▶ [a] Trust this workspace"), None);
    }

    #[test]
    fn the_composer_is_ready() {
        let ready = " Cursor Agent\n v2026.09.23-86fc751\n\n → Plan, search, build anything";
        assert_eq!(CursorProfile.classify(ready), ScreenState::InputReady);
        assert!(!CursorProfile.input_ready(" Cursor Agent\n v2026.09.23-86fc751"));
        assert!(!CursorProfile.input_ready(" → Plan, search, build anything"));
        // A ready screen with the trust box over it is the dialog.
        let both = format!(
            "{ready}\n{}",
            trust_box("  │  ▶ [a] Trust this workspace │", "  │    [q] Quit │")
        );
        assert!(matches!(
            CursorProfile.classify(&both),
            ScreenState::Dialog(_)
        ));
    }
}
