//! pi (pi coding agent): the project-trust dialog, the ready composer, a composer read
//! between its two rules (pi draws no prompt marker), and how to start it.
//!
//! | | pi |
//! |---|---|
//! | `launchers()` | `pi` |
//! | `launch()` | `pi`; no unattended flags (pi asks for no approvals); pre-trust: `--approve` (writes nothing) |
//! | `reset_command()` | `/new` |
//! | `login_check()` | none |
//! | `rpc_launch()` | `pi` + [`PI_RPC_FLAGS`], `--continue` to resume, else `--no-session` for a probe |
//! | `signals()` | no title spinner seen; sync frames always; OSC 9;4 not measured |

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::agent::detect::{
    PENDING_SCAN_ROWS, composer_holds, from_last, has_all, has_any, selected_option,
};
use crate::agent::{
    AgentProfile, Dialog, DialogKind, LaunchPlan, ModelChoice, Permissions, RpcLaunch,
    RpcLaunchOptions, ShellKind, SignalHints, TrustKey, TrustPolicy, Unlock,
};
use crate::screen::{ScreenText, cached, normalize};

/// pi's documented JSONL stdio mode.
pub const PI_RPC_FLAGS: &[&str] = &["--mode", "rpc"];

// Pi 1.5.2 parseArgs takes the following word even when it looks like an option.
// Other flags, such as --list-models and --mode, leave dash-leading words in place.
const VALUE_OPTIONS: &[&str] = &[
    "--provider",
    "--api-key",
    "--system-prompt",
    "--append-system-prompt",
    "--name",
    "-n",
    "--session",
    "--session-id",
    "--fork",
    "--session-dir",
    "--models",
    "--tools",
    "-t",
    "--exclude-tools",
    "-xt",
    "--export",
    "--extension",
    "-e",
    "--skill",
    "--prompt-template",
    "--theme",
];

/// Which answer the pi profile gives its project-trust dialog.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PiTrust {
    /// "Trust", the dialog's default: Enter. pi saves the decision in
    /// `~/.pi/agent/trust.json` (from pi's docs and source; not measured).
    #[default]
    Persist,
    /// "Trust (this session only)", two rows down: Down to it, then Enter. Trusts for this
    /// run and saves nothing (verified: pi 0.85.1, no `trust.json` written).
    SessionOnly,
}

// Project-trust dialog. Extracted from the pi binary, verified: pi 0.85.1: header "Trust
// project folder?" + cwd + explanation; options "Trust" (0.85.1; "Trust this folder" before)
// / "Trust parent folder (…)" / "Trust (this session only)" / "Do not trust" / "Do not trust
// (this session only)"; footer "↑↓ navigate  enter select …"; older builds also show a
// "saved decision" note. Legacy header forms are kept. "project trust" must NOT stand
// alone: pi's settings UI has a "Default project trust" item, and a loose match there would
// press Enter into a settings screen.
const HEADERS: &[&str] = &["trust project folder", "do you trust"];
const OPTIONS: &[&str] = &[
    "trust parent folder",
    "trust (this session only)",
    "do not trust",
    "saved decision",
    "trust this folder",
];
// On a short terminal the header can scroll off while the options remain — both
// session-only labels together are unmistakably the trust dialog.
const SESSION_ONLY_LABELS: &[&str] = &[
    "trust (this session only)",
    "do not trust (this session only)",
];
const TRUST_HEADER: &str = "trust project folder";

// Ready composer. verified: pi 0.85.1 (and earlier): "pi v<version>", "escape interrupt",
// "/ commands" or "ctrl+o", and a stats row ("0.0%/0 (auto)", "[context]", "$0.…").
const READY_ALL: &[&str] = &["pi v", "escape interrupt"];
const READY_HELP: &[&str] = &["/ commands", "ctrl+o", "press ctrl+o"];
const READY_STATS: &[&str] = &["[context]", "[skills]", "(auto)", "$0."];

/// Trust-dialog pointer "→" (U+2192); longer labels first.
fn trust_pointer() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    cached(
        &RE,
        r"→ ?(do not trust \(this session only\)|do not trust|trust \(this session only\)|trust parent folder|trust this folder|trust)",
    )
}

/// pi's screens. `PiProfile::new()` answers the trust dialog with [`PiTrust::Persist`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PiProfile {
    trust: PiTrust,
}

impl PiProfile {
    /// The built-in profile: "Trust" (persisting).
    pub const fn new() -> Self {
        Self::with_trust(PiTrust::Persist)
    }

    /// A profile giving the trust dialog `trust`.
    pub const fn with_trust(trust: PiTrust) -> Self {
        Self { trust }
    }

    /// The answer this profile gives the trust dialog.
    pub const fn trust(&self) -> PiTrust {
        self.trust
    }
}

fn dialog_visible(text: &str) -> bool {
    let header = has_any(text, HEADERS)
        || has_all(text, &["project trust", "current session: untrusted"])
        || has_all(text, &["trust folder", "current session: untrusted"]);
    (header && has_any(text, OPTIONS)) || has_all(text, SESSION_ONLY_LABELS)
}

/// Down while the pointer is above "Trust (this session only)", Up while below, Enter on it.
fn session_only_key(text: &str) -> Option<TrustKey> {
    match selected_option(from_last(text, TRUST_HEADER), trust_pointer())? {
        "trust (this session only)" => Some(TrustKey::Enter),
        "trust" | "trust parent folder" | "trust this folder" => Some(TrustKey::Down),
        "do not trust" | "do not trust (this session only)" => Some(TrustKey::Up),
        _ => None,
    }
}

/// A full-width rule (`────…`) around pi's composer.
fn is_rule(row: &str) -> bool {
    let row = row.trim();
    !row.is_empty() && row.chars().all(|c| c == '─')
}

impl AgentProfile for PiProfile {
    #[cfg(feature = "tools")]
    fn mcp_backend(&self, _: crate::tools::ToolTransport) -> Option<crate::tools::McpBackend> {
        Some(crate::tools::McpBackend::Pi)
    }
    fn compatibility(&self) -> crate::agent::compatibility::Compatibility {
        use crate::agent::compatibility::{Compatibility, Evidence, EvidenceKind, Support};
        Compatibility {
            revision: Some(env!("CARGO_PKG_VERSION")),
            pty: Support::Supported,
            rpc_launch: Support::Supported,
            login_check: Support::Unsupported,
            evidence: &[
                Evidence {
                    operation: "ready screen recognition",
                    kind: EvidenceKind::CapturedTerminal,
                    cli_version: Some("0.85.1"),
                    platform: None,
                    source: "tests/fixtures/pi-0.85.1-ready.bin",
                },
                Evidence {
                    cli_version: Some("1.5.2"),
                    ..super::MODEL_ARGV_EVIDENCE
                },
            ],
        }
    }

    fn id(&self) -> &str {
        "pi"
    }

    fn dialog(&self, text: &str) -> Option<Dialog> {
        let text = normalize(text);
        if text.is_empty() || !dialog_visible(&text) {
            return None;
        }
        let key = match self.trust {
            PiTrust::Persist => Some(TrustKey::Enter),
            PiTrust::SessionOnly => session_only_key(&text),
        };
        Some(Dialog {
            kind: DialogKind::Trust,
            key,
        })
    }

    fn input_ready(&self, screen: &str) -> bool {
        let text = normalize(screen);
        if text.is_empty() || dialog_visible(&text) {
            return false;
        }
        has_all(&text, READY_ALL) && has_any(&text, READY_HELP) && has_any(&text, READY_STATS)
    }

    /// pi draws no prompt marker (verified: pi 0.85.1): the composer is the row(s) between
    /// the last two full-width rules among the bottom rows, with the cwd and stats rows
    /// below. A slash-menu row (`→ new   Start a new session`) opens below the closing rule;
    /// `→` rows are skipped anyway. Fewer than two rules ⇒ `None`.
    fn pending_input(&self, screen: &str, sent: &str) -> Option<bool> {
        let text = ScreenText::from_text(screen);
        let rows = text.bottom_rows(PENDING_SCAN_ROWS);
        let rules: Vec<usize> = (0..rows.len()).filter(|&i| is_rule(&rows[i])).collect();
        let [.., open, close] = rules[..] else {
            return None;
        };
        let composer: Vec<&str> = rows[open + 1..close]
            .iter()
            .map(String::as_str)
            .filter(|row| !row.trim_start().starts_with('→'))
            .collect();
        Some(composer_holds(&normalize(&composer.join("\n")), sent))
    }

    fn launchers(&self) -> &'static [&'static str] {
        &["pi"]
    }

    /// Pre-trust: `--approve` ("trust project-local files for this run") suppresses the
    /// trust dialog (verified: pi 0.85.1) and saves nothing — read from pi 0.85.1's source
    /// (the override skips the trust prompt and its save; only `/trust` writes
    /// `~/.pi/agent/trust.json`), not measured in a run. With it, the profile's
    /// [`PiTrust`] answer is never needed.
    fn launch(
        &self,
        perms: &Permissions,
        trust: &TrustPolicy,
        _cwd: &Path,
        _shell: ShellKind,
    ) -> LaunchPlan {
        let mut plan = LaunchPlan::new("pi").with_permissions(perms, &[], &[]);
        if trust.pre_trust.wants_flags() {
            plan.args.push("--approve".into());
        }
        plan
    }

    fn model_args(&self, choice: &ModelChoice, base_args: &[String]) -> crate::Result<Vec<String>> {
        super::long_model_args(
            self.id(),
            choice,
            base_args,
            "--thinking",
            &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
            VALUE_OPTIONS,
            // Pi reserves the equals forms for extension flags.
            |word| word,
        )
    }

    fn reset_command(&self) -> Option<&'static str> {
        Some("/new")
    }

    /// verified: pi 0.85.1 — title `π - <cwd name>`, no spinner seen (only instant error
    /// turns measured); DEC 2026 frames under every identity; OSC 9;4 not seen.
    fn signals(&self) -> SignalHints {
        SignalHints {
            title_spinner: false,
            osc_progress: Unlock::Unknown,
            sync_frames: Unlock::Always,
        }
    }

    /// `resume` continues the last session; otherwise a `probe` runs with `--no-session`:
    /// it reads the user's normal config but adds no throwaway conversation to their
    /// session history. `resume` wins over `probe`.
    fn rpc_launch_with_options(&self, options: &RpcLaunchOptions) -> crate::Result<RpcLaunch> {
        let mut launch =
            RpcLaunch::new("pi", PI_RPC_FLAGS).with_permissions(&options.permissions, &[], &[]);
        if options.resume {
            launch.args.push("--continue".into());
        } else if options.probe {
            launch.args.push("--no-session".into());
        }
        Ok(launch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ScreenState;

    const RULE: &str = "────────────────────────────────────────";

    fn trust(pointer_on: usize) -> String {
        let options = [
            "Trust",
            "Trust parent folder (/private/tmp)",
            "Trust (this session only)",
            "Do not trust",
            "Do not trust (this session only)",
        ];
        let rows: Vec<String> = options
            .iter()
            .enumerate()
            .map(|(i, o)| format!(" {} {o}", if i == pointer_on { "→" } else { " " }))
            .collect();
        format!(
            "{RULE}\n\n Trust project folder?\n /private/tmp/apt.x\n\n This allows pi to load .pi settings.\n\n{}\n\n ↑↓ navigate  enter select  escape/ctrl+c cancel\n\n{RULE}",
            rows.join("\n")
        )
    }

    fn composer(rows: &[&str]) -> String {
        let mut screen = vec![" pi v0.85.1", RULE, " Update Available", RULE, "", RULE];
        screen.extend(rows);
        screen.extend([RULE, "/private/tmp/apt.x", "0.0%/0 (auto)    unknown"]);
        screen.extend([""; 20]);
        screen.join("\n")
    }

    #[test]
    fn the_built_in_profile_confirms_trust_with_enter() {
        let p = PiProfile::new();
        assert_eq!(p.trust(), PiTrust::Persist);
        let dialog = Some(Dialog {
            kind: DialogKind::Trust,
            key: Some(TrustKey::Enter),
        });
        assert_eq!(p.dialog("trust project folder? trust this folder"), dialog);
        assert_eq!(p.dialog(&trust(0)), dialog);
        assert_eq!(p.dialog(&trust(3)), dialog);
    }

    #[test]
    fn the_session_only_profile_steers_to_this_session_only() {
        let p = PiProfile::with_trust(PiTrust::SessionOnly);
        let key = |text: &str| p.dialog(text).and_then(|d| d.key);
        assert_eq!(key(&trust(0)), Some(TrustKey::Down));
        assert_eq!(key(&trust(1)), Some(TrustKey::Down));
        assert_eq!(key(&trust(2)), Some(TrustKey::Enter));
        assert_eq!(key(&trust(3)), Some(TrustKey::Up));
        assert_eq!(key(&trust(4)), Some(TrustKey::Up));
        assert_eq!(key(&trust(9)), None); // no pointer
        assert_eq!(
            key("trust project folder? → trust this folder"),
            Some(TrustKey::Down)
        );
        // A pointer above the latest header (an older render) does not vote.
        let older = format!("→ Trust (this session only)\n{}", trust(9));
        assert_eq!(key(&older), None);
    }

    #[test]
    fn header_forms_and_option_labels() {
        let p = PiProfile::new();
        assert!(
            p.dialog("Do you trust this folder?\n  Do not trust")
                .is_some()
        );
        assert!(
            p.dialog("Project trust\ncurrent session: untrusted\n do not trust")
                .is_some()
        );
        assert!(
            p.dialog("Trust folder\ncurrent session: untrusted\n saved decision")
                .is_some()
        );
        // Header scrolled off: both session-only labels are enough.
        assert!(
            p.dialog("  Trust (this session only)\n  Do not trust (this session only)")
                .is_some()
        );
        assert!(p.dialog("Trust project folder?").is_none());
        assert!(p.dialog("  Trust (this session only)").is_none());
    }

    #[test]
    fn the_settings_screen_is_not_the_trust_dialog() {
        let p = PiProfile::new();
        let settings =
            " Settings\n   Default project trust    ask\n   Theme    dark\n   Do not trust";
        assert!(p.dialog(settings).is_none());
        assert_eq!(p.classify(settings), ScreenState::Unknown);
    }

    #[test]
    fn ready_needs_all_three_marker_groups() {
        let p = PiProfile::new();
        let ready = " pi v0.85.1\n escape interrupt · / commands · ctrl+o more\n0.0%/0 (auto)";
        assert!(p.input_ready(ready));
        assert!(p.input_ready(" pi v0.9\n escape interrupt\n press ctrl+o\n [context]"));
        assert!(p.input_ready(" pi v0.9\n escape interrupt · / commands\n $0.01"));
        assert!(!p.input_ready(" pi v0.85.1\n escape interrupt · / commands"));
        assert!(!p.input_ready(" escape interrupt · / commands\n (auto)"));
        assert!(!p.input_ready(" pi v0.85.1\n escape interrupt\n (auto)"));
        let both = format!("{ready}\n{}", trust(0));
        assert!(!p.input_ready(&both));
        assert!(matches!(p.classify(&both), ScreenState::Dialog(_)));
    }

    #[test]
    fn the_composer_is_read_between_the_last_two_rules() {
        let p = PiProfile::new();
        let paste = "first line of the pasted text\nsecond line of the pasted text";
        assert_eq!(p.pending_input(&composer(&[""]), paste), Some(false));
        assert_eq!(
            p.pending_input(
                &composer(&[
                    "first line of the pasted text",
                    "second line of the pasted text"
                ]),
                paste
            ),
            Some(true)
        );
        assert_eq!(
            p.pending_input(&composer(&["[paste #1 +12 lines]"]), paste),
            Some(true)
        );
        assert_eq!(p.pending_input(&composer(&["/new"]), "/new"), Some(true));
        assert_eq!(
            p.pending_input(&composer(&["my own draft"]), paste),
            Some(false)
        );
        // The default rule finds no prompt marker on pi's screen.
        let screen = composer(&["/new"]);
        assert_eq!(
            crate::agent::detect::pending_input_default(&screen, "/new"),
            None
        );
    }

    #[test]
    fn a_slash_menu_below_the_composer_is_not_the_composer() {
        let p = PiProfile::new();
        let screen = [
            " pi v0.85.1",
            RULE,
            "/new",
            RULE,
            "→ new         Start a new session",
            "/private/tmp/apt.x",
            "0.0%/0 (auto)",
        ]
        .join("\n");
        assert_eq!(p.pending_input(&screen, "/new"), Some(true));
        let with_arrow_inside = [RULE, "→ new", "/new", RULE].join("\n");
        assert_eq!(p.pending_input(&with_arrow_inside, "/new"), Some(true));
    }

    #[test]
    fn fewer_than_two_rules_is_unreadable() {
        let p = PiProfile::new();
        assert_eq!(p.pending_input(&format!("{RULE}\n/new"), "/new"), None);
        assert_eq!(p.pending_input("", "/new"), None);
        // The rules must sit among the bottom rows.
        let mut rows = vec![RULE, "/new", RULE];
        rows.extend(["transcript"; PENDING_SCAN_ROWS]);
        assert_eq!(p.pending_input(&rows.join("\n"), "/new"), None);
    }
}
