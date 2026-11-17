//! opencode: no startup dialog, the ready composer, and how to start it.
//!
//! Composer submission evidence has not been measured for the supplied profile. An
//! unreadable composer returns `SubmitUnverified`; use measured profile support or
//! the structured RPC transport when verified delivery is required.
//!
//! | | opencode |
//! |---|---|
//! | `launchers()` | `opencode` |
//! | `launch()` | `opencode`; unattended: env default [`OPENCODE_PERMISSION_ENV`]`=`[`OPENCODE_PERMISSION_DEFAULT`]; no pre-trust (no trust dialog) |
//! | `reset_command()` | `/new` |
//! | `login_check()` | none (`opencode auth list` is a listing, not a check) |
//! | `rpc_launch()` | `opencode` + [`OPENCODE_RPC_FLAGS`] + the unattended env default (no resume flag); explicit policy through `rpc_launch_with_options()` |
//! | `signals()` | title spinner not measured; sync frames once its DECRQM 2026 query is answered; OSC 9;4 not measured |

use std::path::Path;

use crate::agent::detect::has_any;
use crate::agent::{
    AgentProfile, Dialog, LaunchPlan, Permissions, RpcLaunch, RpcLaunchOptions, ShellKind,
    SignalHints, TrustPolicy, Unlock,
};
use crate::screen::normalize;

/// opencode's headless stdio subcommand: Agent Client Protocol (`acp`).
pub const OPENCODE_RPC_FLAGS: &[&str] = &["acp"];

/// opencode's env override for its `permission` config section: a JSON object merged over
/// whatever the user's `opencode.json` resolved to.
pub const OPENCODE_PERMISSION_ENV: &str = "OPENCODE_PERMISSION";

/// Broad unattended default (YOLO equivalent) for PTY and RPC launches. Applied only
/// under [`Permissions::Unattended`] and only when the host env policy does not set
/// [`OPENCODE_PERMISSION_ENV`]. Explicit CLI rules can still constrain operations.
///
/// Source evidence: OpenCode v1.18.25 `packages/core/src/v1/config/permission.ts`
/// accepts wildcard keys, and `packages/opencode/src/config/config.ts` merges this env
/// object over native permission config. Live ACP check on macOS arm64, v1.18.25:
/// a real `task` subagent read an external marker file and returned it to its parent
/// with this value and with the narrow workaround, using a local scripted model peer.
/// Native policy remained pending at the deadline without forwarding an approval.
/// This evidence does not cover OpenCode v2.
pub const OPENCODE_PERMISSION_DEFAULT: &str = r#"{"*":"allow"}"#;

/// Narrow compatibility workaround, distinct from the broad unattended default.
/// Pre-allows only `external_directory`. A permission request raised inside a
/// subagent (the `task` tool) is neither forwarded to the client nor answered: opencode
/// looks the subagent's session up in its ACP registry, misses (only client-created
/// sessions are registered) and drops the request, so the tool waits forever and the turn
/// never ends — a healthy-looking session stuck at "working", nothing logged. A subagent
/// straying outside the project (a `~/.m2` glob, a `/tmp` read) hits `external_directory`,
/// so allowing it removes that trigger. Other subagent permission requests can still hang.
/// Confirmed against opencode 1.18.21; still present in 1.18.25.
///
/// Select explicitly with [`Permissions::Custom`] and an env entry
/// `(OPENCODE_PERMISSION_ENV.into(), OPENCODE_EXTERNAL_DIRECTORY_WORKAROUND.into())`,
/// or set it in the host env policy. [`Permissions::AgentDefault`] applies no workaround.
pub const OPENCODE_EXTERNAL_DIRECTORY_WORKAROUND: &str = r#"{"external_directory":"allow"}"#;

// Ready composer. verified: opencode 1.18.21 (cold launch, signed out; no fixture): the
// logo is block art, so "opencode" is not on screen; the composer shows "Ask anything…" with
// "tab agents" / "ctrl+p commands" hints. The older rule ("opencode" + a composer hint)
// stays for older builds.
const READY_PROMPT: &str = "ask anything";
const READY_HINTS: &[&str] = &["tab agents", "ctrl+p commands"];
const LEGACY_HINTS: &[&str] = &["type a message", "press enter", "ctrl+c"];

/// opencode's screens. It shows no startup dialog. Pending input uses the default rule:
/// opencode's composer (`┃  Ask anything…`) has no prompt marker, so it reads `None`
/// (unreadable) — not measured with a paste.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpencodeProfile;

impl AgentProfile for OpencodeProfile {
    #[cfg(feature = "tools")]
    fn mcp_backend(
        &self,
        transport: crate::tools::ToolTransport,
    ) -> Option<crate::tools::McpBackend> {
        match transport {
            crate::tools::ToolTransport::Rpc => Some(crate::tools::McpBackend::Acp),
            crate::tools::ToolTransport::Pty => None,
        }
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
                    kind: EvidenceKind::SyntheticFixture,
                    cli_version: Some("1.18.21"),
                    platform: None,
                    source: "src/agent/profiles/opencode.rs",
                },
                Evidence {
                    operation: "unattended ACP subagent external-file read with local model peer",
                    kind: EvidenceKind::LiveCheck,
                    cli_version: Some("1.18.25"),
                    platform: Some("macos-aarch64"),
                    source: "docs/permission-validation.md",
                },
            ],
        }
    }

    fn id(&self) -> &str {
        "opencode"
    }

    fn dialog(&self, _text: &str) -> Option<Dialog> {
        None
    }

    fn input_ready(&self, screen: &str) -> bool {
        let text = normalize(screen);
        (text.contains(READY_PROMPT) && has_any(&text, READY_HINTS))
            || (text.contains("opencode") && has_any(&text, LEGACY_HINTS))
    }

    fn launchers(&self) -> &'static [&'static str] {
        &["opencode"]
    }

    /// The permission default is env, not a flag: a host typing the launch line into a
    /// running shell must put it into the shell's spawn env
    /// ([`LaunchPlan::merge_env_into`]).
    fn launch(
        &self,
        perms: &Permissions,
        _trust: &TrustPolicy,
        _cwd: &Path,
        _shell: ShellKind,
    ) -> LaunchPlan {
        LaunchPlan::new("opencode").with_permissions(
            perms,
            &[],
            &[(OPENCODE_PERMISSION_ENV, OPENCODE_PERMISSION_DEFAULT)],
        )
    }

    fn reset_command(&self) -> Option<&'static str> {
        Some("/new")
    }

    /// verified: opencode 1.18.21, cold launch only — title `OpenCode`; it asks DECRQM 2026
    /// and draws DEC 2026 frames once answered (the crate answers by default; with
    /// `answer_queries` off it depends on the host's renderer); spinner and OSC 9;4 not
    /// measured.
    fn signals(&self) -> SignalHints {
        SignalHints {
            title_spinner: false,
            osc_progress: Unlock::Unknown,
            sync_frames: Unlock::WhenQueryAnswered,
        }
    }

    // No `login_check`: `opencode auth list` is a credential listing that exits 0 whether or
    // not the provider the project uses has credentials, so it cannot answer the question.

    /// No resume flag: ACP resumes with `session/load` (the `initialize` reply advertises
    /// `loadSession`), and `acp` takes no `--continue`. `probe` changes nothing.
    fn rpc_launch_with_options(&self, options: &RpcLaunchOptions) -> crate::Result<RpcLaunch> {
        Ok(
            RpcLaunch::new("opencode", OPENCODE_RPC_FLAGS).with_permissions(
                &options.permissions,
                &[],
                &[(OPENCODE_PERMISSION_ENV, OPENCODE_PERMISSION_DEFAULT)],
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ScreenState;

    const LOGO: &str =
        "  █▀▀█ █▀▀█ █▀▀█ █▀▀▄ █▀▀▀ █▀▀█ █▀▀▄ █▀▀█\n  █__█ █__█ █^^^ █  █ █___ █__█ █__█ █^^^";

    #[test]
    fn the_1_18_composer_is_ready_although_opencode_is_not_on_screen() {
        let ready = format!(
            "{LOGO}\n\n  ┃  Ask anything... \"Fix a TODO in the codebase\"\n  ┃  Build  GPT-5.6\n                                   tab agents  ctrl+p commands\n  /private/tmp/apt.x                                   1.18.21"
        );
        assert_eq!(OpencodeProfile.classify(&ready), ScreenState::InputReady);
        assert_eq!(OpencodeProfile.classify(LOGO), ScreenState::Unknown);
        assert!(OpencodeProfile.input_ready("Ask anything\n ctrl+p commands"));
        assert!(!OpencodeProfile.input_ready("Ask anything"));
        assert!(!OpencodeProfile.input_ready("tab agents  ctrl+p commands"));
    }

    #[test]
    fn keeps_the_older_rule() {
        assert_eq!(
            OpencodeProfile.classify("opencode\n> type a message"),
            ScreenState::InputReady
        );
        assert!(OpencodeProfile.input_ready("opencode\npress enter to send"));
        assert!(OpencodeProfile.input_ready("opencode · ctrl+c to exit"));
        assert!(!OpencodeProfile.input_ready("opencode"));
    }

    #[test]
    fn never_reports_a_dialog() {
        let text = "Do you trust the contents of this directory? yes, continue";
        assert_eq!(OpencodeProfile.dialog(text), None);
        assert_eq!(
            OpencodeProfile.pending_input("  ┃  Ask anything", "x"),
            None
        );
    }
}
