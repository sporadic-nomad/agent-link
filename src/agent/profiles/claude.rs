//! claude (Claude Code): the folder-trust dialog, the bypass-permissions warning, the ready
//! composer, its slash-menu row in the pending-input check, and how to start it.
//!
//! | | claude |
//! |---|---|
//! | `launchers()` | `claude` |
//! | `launch()` | `claude`; unattended: `--permission-mode bypassPermissions` only when the profile accepts the bypass warning ([`BYPASS_WARNING_VERIFIED`] for the built-in); no pre-trust flag |
//! | `reset_command()` | `/clear` |
//! | `login_check()` | `claude auth status`; signed out: `"loggedIn": false` on stdout |
//! | `rpc_launch()` | `claude` + [`CLAUDE_RPC_FLAGS`] + unattended permission flags, `--continue` to resume; explicit policy through `rpc_launch_with_options()` |
//! | `signals()` | no Braille title spinner; OSC 9;4 not measured; sync frames only under a VS Code identity |

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::KnownTerminal;
use crate::agent::detect::{from_last, has_all, has_any, pending_input_where, steer_two};
use crate::agent::{
    AgentProfile, Dialog, DialogKind, LaunchPlan, LoginCheck, ModelChoice, Permissions, RpcLaunch,
    RpcLaunchOptions, ShellKind, SignalHints, TrustPolicy, Unlock,
};
use crate::screen::{cached, normalize};

/// Whether the built-in claude profile accepts the bypass-permissions warning (steers it to
/// "Yes, I accept") and so launches with `--permission-mode bypassPermissions` under
/// [`Permissions::Unattended`]. Markers and keys verified: claude 2.1.293.
///
/// Accepting is persisted by claude: it writes `"skipDangerousModePermissionPrompt": true`
/// into the user's own `~/.claude/settings.json` (its config dir's `settings.json`), and
/// later launches skip the warning. A host that must not cause that write uses
/// [`ClaudeProfile::accepting_bypass_warning`]`(false)`: no flag, and a warning shown anyway
/// (claude's own settings default to bypass mode) is recognised with no key — left to the
/// user.
pub const BYPASS_WARNING_VERIFIED: bool = true;

/// The unattended flag. With it claude opens its bypass warning with "No, exit" selected:
/// a gate that cannot recognise and steer that screen would let the next programmatic Enter
/// quit claude, so the flag goes only with a profile that accepts the warning.
const BYPASS_ARGS: &[&str] = &["--permission-mode", "bypassPermissions"];

// Claude Code 2.1.327 --help. These options consume at least one operand, even
// if the operand begins with a dash. Further variadic operands stop at an option.
const VALUE_OPTIONS: &[&str] = &[
    "--add-dir",
    "--agent",
    "--agents",
    "--allowedTools",
    "--allowed-tools",
    "--append-system-prompt",
    "--append-system-prompt-file",
    "--autocompact",
    "--betas",
    "--debug-file",
    "--disallowedTools",
    "--disallowed-tools",
    "--environment",
    "--fallback-model",
    "--file",
    "--input-format",
    "--json-schema",
    "--max-budget-usd",
    "--mcp-config",
    "--name",
    "-n",
    "--output-format",
    "--permission-mode",
    "--permission-prompts",
    "--plugin-dir",
    "--plugin-url",
    "--remote-control-session-name-prefix",
    "--session-id",
    "--setting-sources",
    "--settings",
    "--system-prompt",
    "--system-prompt-file",
    "--system-prompt-snapshot",
    "--tools",
];

/// Claude's transport-only stdio flags: stream-json in and out, partial messages and
/// echoed user messages. The profile adds `--permission-mode bypassPermissions` under
/// [`Permissions::Unattended`] (the default); [`Permissions::AgentDefault`] omits it and
/// [`Permissions::Custom`] replaces it. Headless mode has no bypass warning screen,
/// so [`ClaudeProfile::accepting_bypass_warning`] only affects PTY launches.
/// Claude exits non-zero on an unknown flag.
pub const CLAUDE_RPC_FLAGS: &[&str] = &[
    "--print",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--replay-user-messages",
];

// Trust dialog. verified: claude 2.1.293, unchanged in 2.1.295 (live smoke; older builds use
// the same labels). Option order changed across versions: older builds list "Yes, I trust
// this folder" first and selected, 2.1.28x+ lists "No, exit" first and selected — a bare
// Enter exits claude. Older builds number the options ("1."); the pointer is "❯" (">" and
// "›" as fallbacks). 2.1.295 redraws the dialog with "No, exit" selected again when a key
// comes within ~140 ms of its first draw (the gate's `Timing::dialog_settle`).
const TRUST_HEADER: &str = "quick safety check";
const TRUST_YES: &str = "yes, i trust this folder";
const NO_EXIT: &str = "no, exit";

// Bypass-permissions warning (`--permission-mode bypassPermissions`). verified: claude
// 2.1.293, unchanged in 2.1.295 (live smoke): "WARNING: Claude Code running in Bypass
// Permissions mode", options "❯ No, exit" (first, selected) / "Yes, I accept", no
// numbering. In an untrusted folder it follows the trust dialog, pointer back on "No, exit".
const BYPASS_HEADER: &str = "claude code running in bypass permissions mode";
const BYPASS_YES: &str = "yes, i accept";

// Ready composer: a brand marker and a composer-chrome marker, both required.
// "(shift+tab to cycle)" and "/effort" are claude 2.1.293's composer footer ("⏵⏵ auto mode on
// (shift+tab to cycle)", "◐ medium · /effort"), which shows none of the older markers.
// verified: claude 2.1.293 in auto and bypass mode only, 2.1.295 in bypass mode (live smoke)
// — the default-mode footer was not measured (a startup gate's timers cover a footer with
// neither).
const BRAND: &[&str] = &["claude code", "bypass permissions mode", "cwd:"];
const COMPOSER_CHROME: &[&str] = &[
    "esc to interrupt",
    "ctrl-c to quit",
    "type your message",
    "(shift+tab to cycle)", // verified: claude 2.1.293
    "/effort",              // verified: claude 2.1.293
];

fn trust_pointer() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    cached(
        &RE,
        r"[❯›>] ?(?:[0-9]+\. ?)?(yes, i trust this folder|no, exit)",
    )
}

fn bypass_pointer() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    cached(&RE, r"[❯›>] ?(?:[0-9]+\. ?)?(yes, i accept|no, exit)")
}

/// claude's screens. `ClaudeProfile::new()` follows [`BYPASS_WARNING_VERIFIED`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaudeProfile {
    accept_bypass_warning: bool,
}

impl ClaudeProfile {
    /// The built-in profile: accepts the bypass warning iff [`BYPASS_WARNING_VERIFIED`].
    pub const fn new() -> Self {
        Self::accepting_bypass_warning(BYPASS_WARNING_VERIFIED)
    }

    /// A profile that steers the bypass warning to "Yes, I accept" (`true`) or recognises
    /// it without a key (`false`).
    pub const fn accepting_bypass_warning(accept: bool) -> Self {
        Self {
            accept_bypass_warning: accept,
        }
    }

    /// Whether this profile steers the bypass warning to "Yes, I accept".
    pub const fn accepts_bypass_warning(&self) -> bool {
        self.accept_bypass_warning
    }

    fn dialog_in(&self, text: &str) -> Option<Dialog> {
        let trust = has_all(text, &[TRUST_HEADER, TRUST_YES]);
        let bypass = has_all(text, &[BYPASS_HEADER, BYPASS_YES]);
        // The latest dialog counts (a rolling buffer holds the trust dialog before the
        // warning); both offer "No, exit", so each is read from its own header on.
        if bypass && (!trust || text.rfind(BYPASS_HEADER) > text.rfind(TRUST_HEADER)) {
            let key = if self.accept_bypass_warning {
                let warning = from_last(text, BYPASS_HEADER);
                steer_two(warning, bypass_pointer(), BYPASS_YES, NO_EXIT)
            } else {
                None
            };
            return Some(Dialog {
                kind: DialogKind::BypassWarning,
                key,
            });
        }
        trust.then(|| Dialog {
            kind: DialogKind::Trust,
            key: steer_two(
                from_last(text, TRUST_HEADER),
                trust_pointer(),
                TRUST_YES,
                NO_EXIT,
            ),
        })
    }
}

impl Default for ClaudeProfile {
    fn default() -> Self {
        Self::new()
    }
}

/// The selected row of claude's slash-command menu (`  ❯ /clear   Start a new session …`)
/// opens below the composer and carries its marker indented by two spaces; the composer's
/// marker sits in the first column (or inside a `│` border in older builds).
fn is_menu_row(chrome: &str) -> bool {
    chrome.chars().count() >= 2 && !chrome.contains(&['│', '┃', '|'][..])
}

impl AgentProfile for ClaudeProfile {
    #[cfg(feature = "tools")]
    fn mcp_backend(&self, _: crate::tools::ToolTransport) -> Option<crate::tools::McpBackend> {
        Some(crate::tools::McpBackend::Claude)
    }
    fn compatibility(&self) -> crate::agent::compatibility::Compatibility {
        use crate::agent::compatibility::{Compatibility, Evidence, EvidenceKind, Support};
        Compatibility {
            revision: Some(env!("CARGO_PKG_VERSION")),
            pty: Support::Supported,
            rpc_launch: Support::Supported,
            login_check: Support::Supported,
            evidence: &[
                Evidence {
                    operation: "ready screen recognition",
                    kind: EvidenceKind::CapturedTerminal,
                    cli_version: Some("2.1.293"),
                    platform: None,
                    source: "tests/fixtures/claude-2.1.293-ready.bin",
                },
                Evidence {
                    cli_version: Some("2.1.327"),
                    ..super::MODEL_ARGV_EVIDENCE
                },
            ],
        }
    }

    fn id(&self) -> &str {
        "claude"
    }

    fn dialog(&self, text: &str) -> Option<Dialog> {
        let text = normalize(text);
        if text.is_empty() {
            return None;
        }
        self.dialog_in(&text)
    }

    fn input_ready(&self, screen: &str) -> bool {
        let text = normalize(screen);
        if text.is_empty() || self.dialog_in(&text).is_some() {
            return false;
        }
        has_any(&text, BRAND) && has_any(&text, COMPOSER_CHROME)
    }

    fn pending_input(&self, screen: &str, sent: &str) -> Option<bool> {
        pending_input_where(screen, sent, is_menu_row)
    }

    fn launchers(&self) -> &'static [&'static str] {
        &["claude"]
    }

    /// claude has no pre-trust flag: folder trust lives in its config (`.claude.json`),
    /// which the crate never edits — the trust dialog is steered instead.
    fn launch(
        &self,
        perms: &Permissions,
        _trust: &TrustPolicy,
        _cwd: &Path,
        _shell: ShellKind,
    ) -> LaunchPlan {
        let unattended = if self.accept_bypass_warning {
            BYPASS_ARGS
        } else {
            &[]
        };
        LaunchPlan::new("claude").with_permissions(perms, unattended, &[])
    }

    fn model_args(&self, choice: &ModelChoice, base_args: &[String]) -> crate::Result<Vec<String>> {
        super::long_model_args(
            self.id(),
            choice,
            base_args,
            "--effort",
            &["low", "medium", "high", "xhigh", "max"],
            VALUE_OPTIONS,
            |word| word.split_once('=').map_or(word, |(name, _)| name),
        )
    }

    fn reset_command(&self) -> Option<&'static str> {
        Some("/clear")
    }

    /// verified: claude 2.1.293 — title `✳ Claude Code` idle, `◐ Claude Code` working (not
    /// Braille; only one working frame seen); DEC 2026 frames only under
    /// `TERM_PROGRAM=vscode`; OSC 9;4 not seen (signed out, no real turn).
    fn signals(&self) -> SignalHints {
        SignalHints {
            title_spinner: false,
            osc_progress: Unlock::Unknown,
            sync_frames: Unlock::WithIdentity(KnownTerminal::VsCode),
        }
    }

    /// See [`LoginCheck`] for the three-way verification table.
    fn login_check(&self) -> Option<LoginCheck> {
        Some(LoginCheck {
            args: &["auth", "status"],
            signed_out_marker: "\"loggedIn\": false",
        })
    }

    /// `probe` changes nothing: claude has no "no session" flag here.
    fn rpc_launch_with_options(&self, options: &RpcLaunchOptions) -> crate::Result<RpcLaunch> {
        let mut launch = RpcLaunch::new("claude", CLAUDE_RPC_FLAGS).with_permissions(
            &options.permissions,
            BYPASS_ARGS,
            &[],
        );
        if options.resume {
            launch.args.push("--continue".into());
        }
        Ok(launch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ScreenState, TrustKey};

    // Visible-screen renders of claude's trust dialog.
    const HEADER: &[&str] = &[
        " Accessing workspace:",
        "",
        " /Users/me/project",
        "",
        " Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source",
        " project, or work from your team). If not, take a moment to review what's in this folder first.",
        "",
        " Claude Code'll be able to read, edit, and execute files here.",
        "",
        " Security guide",
        "",
    ];
    const FOOTER: &[&str] = &["", " Enter to confirm · Esc to cancel"];

    fn screen(options: &[&str]) -> String {
        [HEADER, options, FOOTER].concat().join("\n")
    }

    // Raw PTY stream captured from claude 2.1.282: the initial frame ("No, exit" first and
    // selected), then the partial redraw after one Down arrow.
    const RAW_INITIAL: &str = concat!(
        "\u{1b}[2GQuick\u{1b}[8Gsafety\u{1b}[15Gcheck:\u{1b}[22GIs\u{1b}[25Gthis\u{1b}[30Ga\u{1b}[32Gproject\r\r\n",
        "\u{1b}[2GClaude\u{1b}[9GCode'll\u{1b}[17Gbe\u{1b}[20Gable\u{1b}[25Gto\u{1b}[28Gread,\r\r\n",
        "\u{1b}[2G\u{1b}[38;2;177;185;249m❯\u{1b}[4GNo,\u{1b}[8Gexit\u{1b}[39m\r\r\n",
        "\u{1b}[4GYes,\u{1b}[9GI\u{1b}[11Gtrust\u{1b}[17Gthis\u{1b}[22Gfolder\r\r\n\r\r\n",
        "\u{1b}[2G\u{1b}[38;2;153;153;153mEnter\u{1b}[8Gto\u{1b}[11Gconfirm\u{1b}[19G·\u{1b}[21GEsc\u{1b}[25Gto\u{1b}[28Gcancel\u{1b}[39m\r\r\n",
    );
    const RAW_AFTER_DOWN: &str = concat!(
        "\u{1b}[?2026h\u{1b}[1D\u{1b}[4B\r\u{1b}[1C\u{1b}[4A \u{1b}[4GNo, exit\r\u{1b}[1C\u{1b}[1B",
        "\u{1b}[38;2;177;185;249m❯\u{1b}[4GYes, I trust this folder\u{1b}[39m\r\r\n\u{1b}[?2026l",
    );

    // The bypass warning as claude 2.1.293 draws it.
    fn warning(options: &[&str]) -> String {
        let head: &[&str] = &[
            "  WARNING: Claude Code running in Bypass Permissions mode",
            "",
            "  In Bypass Permissions mode, Claude Code will not ask for your approval before running potentially dangerous",
            "  commands.",
            "",
            "  By proceeding, you accept all responsibility for actions taken while running in Bypass Permissions mode.",
            "",
        ];
        [head, options, &["", "  Enter to confirm · Esc to cancel"]]
            .concat()
            .join("\n")
    }

    fn key(text: &str) -> Option<TrustKey> {
        ClaudeProfile::new().dialog(text).and_then(|d| d.key)
    }

    fn trust(key: Option<TrustKey>) -> Option<Dialog> {
        Some(Dialog {
            kind: DialogKind::Trust,
            key,
        })
    }

    #[test]
    fn steers_down_when_no_exit_is_listed_first_and_selected() {
        let text = screen(&[" ❯ No, exit", "   Yes, I trust this folder"]);
        assert_eq!(
            ClaudeProfile::new().dialog(&text),
            trust(Some(TrustKey::Down))
        );
    }

    #[test]
    fn confirms_once_yes_is_selected() {
        let text = screen(&["   No, exit", " ❯ Yes, I trust this folder"]);
        assert_eq!(key(&text), Some(TrustKey::Enter));
    }

    #[test]
    fn confirms_the_older_layout_where_yes_is_first_and_selected() {
        let text = screen(&[" ❯ 1. Yes, I trust this folder", "   2. No, exit"]);
        assert_eq!(key(&text), Some(TrustKey::Enter));
    }

    #[test]
    fn steers_up_in_the_older_layout_when_no_is_selected() {
        let text = screen(&["   1. Yes, I trust this folder", " ❯ 2. No, exit"]);
        assert_eq!(key(&text), Some(TrustKey::Up));
    }

    #[test]
    fn accepts_the_ascii_pointer_fallback() {
        let text = screen(&[" > No, exit", "   Yes, I trust this folder"]);
        assert_eq!(key(&text), Some(TrustKey::Down));
    }

    #[test]
    fn has_no_key_when_no_pointer_is_readable() {
        let text = screen(&["   No, exit", "   Yes, I trust this folder"]);
        assert_eq!(ClaudeProfile::new().dialog(&text), trust(None));
    }

    #[test]
    fn ignores_a_shell_prompt_pointer_above_the_dialog() {
        let text = ["❯ claude"]
            .iter()
            .chain(HEADER)
            .chain(&[" ❯ No, exit", "   Yes, I trust this folder"])
            .chain(FOOTER)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(key(&text), Some(TrustKey::Down));
    }

    #[test]
    fn reads_the_selection_from_the_captured_raw_stream() {
        assert_eq!(
            ClaudeProfile::new().dialog(RAW_INITIAL),
            trust(Some(TrustKey::Down))
        );
        let after = format!("{RAW_INITIAL}{RAW_AFTER_DOWN}");
        assert_eq!(key(&after), Some(TrustKey::Enter));
    }

    #[test]
    fn scopes_a_rolling_buffer_to_the_latest_full_render() {
        let old_yes_frame = screen(&[" ❯ Yes, I trust this folder", "   No, exit"]);
        assert_eq!(
            key(&format!("{old_yes_frame}{RAW_INITIAL}")),
            Some(TrustKey::Down)
        );
    }

    #[test]
    fn the_trust_dialog_needs_both_markers() {
        let p = ClaudeProfile::new();
        assert_eq!(p.dialog("Quick safety check"), None);
        assert_eq!(p.dialog("❯ Yes, I trust this folder"), None);
        assert_eq!(p.dialog(""), None);
    }

    #[test]
    fn the_built_in_profile_follows_the_constant() {
        assert_eq!(
            ClaudeProfile::new().accepts_bypass_warning(),
            BYPASS_WARNING_VERIFIED
        );
        assert_eq!(ClaudeProfile::default(), ClaudeProfile::new());
    }

    #[test]
    fn a_non_accepting_profile_leaves_the_bypass_warning_to_the_user() {
        let p = ClaudeProfile::accepting_bypass_warning(false);
        assert!(!p.accepts_bypass_warning());
        let text = warning(&["  ❯ No, exit", "    Yes, I accept"]);
        let dialog = Some(Dialog {
            kind: DialogKind::BypassWarning,
            key: None,
        });
        assert_eq!(p.dialog(&text), dialog);
        assert!(!p.input_ready(&text));
    }

    #[test]
    fn an_accepting_profile_steers_the_bypass_warning_to_accept() {
        let p = ClaudeProfile::accepting_bypass_warning(true);
        let keyed = |text: &str| p.dialog(text).map(|d| (d.kind, d.key));
        let on_no = warning(&["  ❯ No, exit", "    Yes, I accept"]);
        assert_eq!(
            keyed(&on_no),
            Some((DialogKind::BypassWarning, Some(TrustKey::Down)))
        );
        let on_yes = warning(&["    No, exit", "  ❯ Yes, I accept"]);
        assert_eq!(
            keyed(&on_yes),
            Some((DialogKind::BypassWarning, Some(TrustKey::Enter)))
        );
        // Never Enter blind: no readable pointer, no key.
        let unreadable = warning(&["    No, exit", "    Yes, I accept"]);
        assert_eq!(keyed(&unreadable), Some((DialogKind::BypassWarning, None)));
        // A layout with "Yes" first steers up from "No".
        let yes_first = warning(&["    Yes, I accept", "  ❯ No, exit"]);
        assert_eq!(
            keyed(&yes_first),
            Some((DialogKind::BypassWarning, Some(TrustKey::Up)))
        );
    }

    #[test]
    fn the_latest_of_trust_dialog_and_bypass_warning_counts() {
        let p = ClaudeProfile::accepting_bypass_warning(true);
        let trust_yes = screen(&["   No, exit", " ❯ Yes, I trust this folder"]);
        let warning_no = warning(&["  ❯ No, exit", "    Yes, I accept"]);
        let then_warning = format!("{trust_yes}\n{warning_no}");
        assert_eq!(
            p.dialog(&then_warning).map(|d| (d.kind, d.key)),
            Some((DialogKind::BypassWarning, Some(TrustKey::Down)))
        );
        let then_trust = format!("{warning_no}\n{trust_yes}");
        assert_eq!(p.dialog(&then_trust), trust(Some(TrustKey::Enter)));
    }

    #[test]
    fn ready_needs_a_brand_marker_and_a_composer_marker() {
        let p = ClaudeProfile::new();
        assert!(p.input_ready("Claude Code\n> \n esc to interrupt"));
        assert!(p.input_ready("cwd: /x\n ctrl-c to quit"));
        assert!(p.input_ready(" bypass permissions mode\n type your message"));
        assert!(p.input_ready(
            " ▐▛███▛█   Claude Code v2.1.293\n❯ \n  ⏵⏵ auto mode on (shift+tab to cycle)"
        ));
        assert!(p.input_ready(" Claude Code v2.1.293\n◐ medium · /effort"));
        assert!(!p.input_ready(" ▐▛███▛█   Claude Code v2.1.293"));
        assert!(!p.input_ready("❯ \n  ⏵⏵ auto mode on (shift+tab to cycle) · /effort"));
        assert_eq!(
            p.classify("Claude Code\n esc to interrupt"),
            ScreenState::InputReady
        );
        assert_eq!(p.classify("Claude Code"), ScreenState::Unknown);
    }

    #[test]
    fn a_dialog_is_never_ready() {
        let p = ClaudeProfile::new();
        let text = format!(
            "{}\n Claude Code\n (shift+tab to cycle)",
            screen(&[" ❯ No, exit", "   Yes, I trust this folder"])
        );
        assert!(!p.input_ready(&text));
        assert!(matches!(p.classify(&text), ScreenState::Dialog(_)));
    }

    #[test]
    fn the_slash_menu_row_below_the_composer_is_not_the_composer() {
        let p = ClaudeProfile::new();
        let menu = [
            "❯ /clear",
            "──────────",
            "  ❯ /clear                      Start a new session with empty context",
            "                                /resume)",
        ]
        .join("\n");
        assert_eq!(p.pending_input(&menu, "/clear"), Some(true));
        // The default rule reads the menu row instead.
        assert_eq!(
            crate::agent::detect::pending_input_default(&menu, "/clear"),
            Some(false)
        );
        // A bordered composer (older builds) is still read.
        let sent = "first line of the pasted text\nsecond";
        assert_eq!(
            p.pending_input("│ > first line of the pasted text │", sent),
            Some(true)
        );
        assert_eq!(
            p.pending_input("❯ \n  ❯ /clear  menu", "/clear"),
            Some(false)
        );
    }
}
