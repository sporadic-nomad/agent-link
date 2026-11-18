//! codex (OpenAI Codex CLI): the directory-trust, full-access and update-prompt dialogs,
//! the ready composer with its "model: loading" veto, and how to start it.
//!
//! | | codex |
//! |---|---|
//! | `launchers()` | `codex` |
//! | `launch()` | `codex`; unattended: `--dangerously-bypass-approvals-and-sandbox`; always `-c check_for_update_on_startup=false`; pre-trust: `-c projects={"<cwd>"={trust_level="trusted"}}` (writes nothing; not under cmd.exe) |
//! | `reset_command()` | `/clear` |
//! | `login_check()` | `codex login status`; signed out: `Not logged in` |
//! | `rpc_launch()` | `codex` + [`CODEX_RPC_FLAGS`] + unattended config overrides (no resume flag); explicit policy through `rpc_launch_with_options()` |
//! | `signals()` | Braille title spinner; sync frames always; OSC 9;4 not measured |

use std::borrow::Cow;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::agent::detect::{has_all, has_any, selected_option};
use crate::agent::launch::toml_basic_string;
use crate::agent::{
    AgentProfile, Dialog, DialogKind, LaunchPlan, LoginCheck, ModelChoice, Permissions, RpcLaunch,
    RpcLaunchOptions, ShellKind, SignalHints, TrustKey, TrustPolicy, Unlock,
};
use crate::screen::{cached, normalize};

/// Unattended: no approval prompts, no sandbox.
const UNATTENDED_ARGS: &[&str] = &["--dangerously-bypass-approvals-and-sandbox"];

/// The startup update check, off for every launch (it is not a permission). verified: codex
/// 0.154.0 — otherwise an "Update available!" prompt can open with "1. Update now" selected,
/// which runs an installer (`curl … | sh`); the update-prompt rule below is the backstop.
/// Plain enough to stay unquoted under cmd.exe, where the npm `codex.cmd` shim forwards
/// `%*` verbatim (unverified on a Windows host).
const NO_UPDATE_CHECK: &[&str] = &["-c", "check_for_update_on_startup=false"];

/// Codex's transport-only stdio subcommand: persistent JSON-RPC (`app-server`).
/// Under [`Permissions::Unattended`] the profile also adds
/// `-c approval_policy="never" -c sandbox_mode="danger-full-access"`: app-server reads
/// config overrides, not the TUI's bypass flag. Explicit native/custom policies omit or
/// replace these overrides. Native protocol requests and resumed thread settings can
/// select a different effective policy; the host owns those requests.
///
/// Source evidence at `b1573f102413e96a6481087922bf425231222dee`: the main
/// `codex-rs/cli/src/main.rs` parses config overrides and passes them to app-server;
/// `cli/src/config_override_args.rs` and `utils/cli/src/config_override.rs` collect
/// `-c` across subcommand boundaries (`global = true`). The app-server's `src/lib.rs`
/// loads those overrides. The separate `codex-app-server` executable is not used here.
/// Live `config/read` verification: `codex-cli 0.169.0`, isolated home, no model turn;
/// native policy returned `on-request`/`read-only`, unattended returned
/// `never`/`danger-full-access`. Unknown subcommands/flags exit nonzero.
pub const CODEX_RPC_FLAGS: &[&str] = &["app-server"];

const UNATTENDED_RPC_ARGS: &[&str] = &[
    "-c",
    r#"approval_policy="never""#,
    "-c",
    r#"sandbox_mode="danger-full-access""#,
];

// Dialog headers (normalised). verified: codex 0.142.5; the trust wording is unchanged in
// 0.154.0. Three interstitials:
// - directory trust: "Do you trust the contents of this directory?" / "1. Yes, continue"
//   (selected) / "2. No, quit" / "Press enter to continue";
// - full access: "Enable full access?" / "Yes, continue anyway". Confirming it is intended:
//   a host launches codex with `--dangerously-bypass-approvals-and-sandbox`, and the
//   dialog's default is the session-only choice. Not seen on 0.154.0;
// - update prompt (verified: codex 0.154.0): "Update available!" / "1. Update now"
//   (selected; runs an installer) / "2. Skip" / "3. Skip until next version" (persists the
//   dismissal). Steered to "Skip". Its options are required too: a plain "Update available!"
//   notice in the transcript is not a dialog.
const TRUST_HEADER: &str = "do you trust the contents of this directory";
const FULL_ACCESS_HEADER: &str = "enable full access?";
const UPDATE_HEADER: &str = "update available!";
const YES_CONTINUE: &str = "yes, continue";
const SKIP_UNTIL_NEXT: &str = "skip until next version";

// Ready composer. verified: codex 0.142.5 (native Rust TUI): banner ">_ OpenAI Codex",
// header labels "directory:" / "permissions:" / "model:", composer footer "Enter to submit
// … Tab to queue message … ? for shortcuts". "press enter" is NOT a ready marker (the trust
// dialog says "Press enter to continue"), and "ctrl+c" / "to interrupt" are busy or
// sub-thread chrome, not a fresh composer.
const READY_HINTS: &[&str] = &[
    "for shortcuts", // "? for shortcuts" composer hint
    "to submit message",
    "to queue message",
    "to edit previous message",
];
const READY_HEADER: &[&str] = &["directory:", "permissions:", "model:"];
const LEGACY_PROMPTS: &[&str] = &["type a message", "ask codex"];

// Not ready while the header says "model: loading" (verified: codex 0.154.0): codex draws
// this complete-looking composer for 125-373 ms before the config and model are resolved,
// and its startup dialogs (trust, update prompt) then replace it; a ready screen shows the
// model name. "for shortcuts" only appears on that loading composer in 0.154.0 and stays
// for older builds.
const LOADING: &str = "model: loading";

/// Update-prompt pointer "›" (U+203A) before a numbered option; "skip until next version"
/// is tried before its prefix "skip".
fn update_pointer() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    cached(
        &RE,
        r"› ?(?:[0-9]+\. ?)?(update now|skip until next version|skip)",
    )
}

/// codex's screens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodexProfile;

fn dialog_in(text: &str) -> Option<Dialog> {
    let visible = has_all(text, &[TRUST_HEADER, YES_CONTINUE])
        || has_all(text, &[FULL_ACCESS_HEADER, YES_CONTINUE])
        || has_all(text, &[UPDATE_HEADER, SKIP_UNTIL_NEXT]);
    visible.then(|| answer(text))
}

/// The dialog whose header occurs last (a rolling buffer answers the latest one). Trust and
/// full access default to "Yes, continue" (Enter); the update prompt is steered to "Skip":
/// Enter only there, arrows toward it otherwise, no key when unreadable.
fn answer(text: &str) -> Dialog {
    let trust_at = text.rfind(TRUST_HEADER);
    let full_access_at = text.rfind(FULL_ACCESS_HEADER);
    let other_at = trust_at.max(full_access_at);
    match text.rfind(UPDATE_HEADER) {
        Some(update_at) if other_at.is_none_or(|other| update_at > other) => {
            let key = match selected_option(&text[update_at..], update_pointer()) {
                Some("skip") => Some(TrustKey::Enter),
                Some("update now") => Some(TrustKey::Down),
                Some("skip until next version") => Some(TrustKey::Up),
                _ => None,
            };
            Dialog {
                kind: DialogKind::UpdatePrompt,
                key,
            }
        }
        _ => Dialog {
            kind: if full_access_at > trust_at {
                DialogKind::FullAccess
            } else {
                DialogKind::Trust
            },
            key: Some(TrustKey::Enter),
        },
    }
}

impl AgentProfile for CodexProfile {
    #[cfg(feature = "tools")]
    fn mcp_backend(&self, _: crate::tools::ToolTransport) -> Option<crate::tools::McpBackend> {
        Some(crate::tools::McpBackend::Codex)
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
                    cli_version: Some("0.154.0"),
                    platform: None,
                    source: "tests/fixtures/codex-0.154.0-ready.bin",
                },
                Evidence {
                    operation: "busy-turn /clear refusal",
                    kind: EvidenceKind::CapturedTerminal,
                    cli_version: Some("0.169.0"),
                    platform: Some("macos-aarch64"),
                    source: "tests/fixtures/codex-0.169.0-clear-refused.bin",
                },
                Evidence {
                    operation: "busy-turn /clear refusal (wrapped)",
                    kind: EvidenceKind::CapturedTerminal,
                    cli_version: Some("0.169.0"),
                    platform: Some("macos-aarch64"),
                    source: "tests/fixtures/codex-0.169.0-clear-refused-wrapped.bin",
                },
                Evidence {
                    operation: "unattended app-server launch config/read",
                    kind: EvidenceKind::LiveCheck,
                    cli_version: Some("0.169.0"),
                    platform: Some("macos-aarch64"),
                    source: "docs/permission-validation.md",
                },
                Evidence {
                    cli_version: Some("0.170.0"),
                    ..super::MODEL_ARGV_EVIDENCE
                },
            ],
        }
    }

    fn id(&self) -> &str {
        "codex"
    }

    fn dialog(&self, text: &str) -> Option<Dialog> {
        let text = normalize(text);
        if text.is_empty() {
            return None;
        }
        dialog_in(&text)
    }

    fn input_ready(&self, screen: &str) -> bool {
        let text = normalize(screen);
        if text.is_empty() || dialog_in(&text).is_some() || text.contains(LOADING) {
            return false;
        }
        // "openai codex" banner, older brands too.
        let branding = text.contains("codex");
        let chrome = has_any(&text, READY_HINTS)
            || has_all(&text, READY_HEADER)
            || has_any(&text, LEGACY_PROMPTS);
        branding && chrome
    }

    fn launchers(&self) -> &'static [&'static str] {
        &["codex"]
    }

    /// Pre-trust (verified: codex 0.154.0): `-c projects={"<cwd>"={trust_level="trusted"}}`
    /// suppresses the trust dialog, with dots and spaces in the cwd. It replaces the
    /// `projects` table for this run only and writes nothing (`config.toml` unchanged). The
    /// dotted `projects."<cwd>".trust_level=…` does not work: codex splits a `-c` path on
    /// every `.`, quotes or not. Left out under cmd.exe (quoting through `.cmd` shims is
    /// unverified on a Windows host) — the trust dialog is steered there. The plan is bound
    /// to `shell` ([`LaunchPlan::for_shell`]). Blocking: canonicalises `cwd` (one
    /// filesystem call; slow only on a hung network mount).
    fn launch(
        &self,
        perms: &Permissions,
        trust: &TrustPolicy,
        cwd: &Path,
        shell: ShellKind,
    ) -> LaunchPlan {
        let mut plan = LaunchPlan::new("codex")
            .with_permissions(perms, UNATTENDED_ARGS, &[])
            .for_shell(shell);
        plan.args
            .extend(NO_UPDATE_CHECK.iter().map(|a| a.to_string()));
        if trust.pre_trust.wants_flags() && shell != ShellKind::Cmd {
            if let Some(value) = trust_override(cwd) {
                plan.args.extend(["-c".to_string(), value]);
            }
        }
        plan
    }

    fn model_args(&self, choice: &ModelChoice, base_args: &[String]) -> crate::Result<Vec<String>> {
        // Built-in ReasoningEffort spellings at rust-v0.170.0. Every label here
        // is a bare word that Codex's TOML override parser reads as a string.
        let effort = super::requested_effort(
            self.id(),
            choice,
            &[
                "none",
                "minimal",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultra",
                "persistent",
            ],
        )?;
        let mut base = base_args.iter().map(String::as_str);
        while let Some(word) = base.next() {
            if word.starts_with("-m") || word == "--model" || word.starts_with("--model=") {
                return Err(crate::Error::InvalidInput(
                    "codex launch already supplies a model".into(),
                ));
            }
            // Clap accepts split, attached short, and equals forms. Codex splits
            // an override at the first equals sign and trims the key.
            let assignment = match word {
                "-c" | "--config" => Some(base.next().ok_or_else(|| {
                    crate::Error::InvalidInput("codex launch needs a config override value".into())
                })?),
                _ => word
                    .strip_prefix("--config=")
                    .or_else(|| word.strip_prefix("-c="))
                    .or_else(|| word.strip_prefix("-c")),
            };
            if let Some((key, _)) = assignment.and_then(|value| value.split_once('=')) {
                let key = key.trim();
                if key == "model" || (effort.is_some() && key == "model_reasoning_effort") {
                    return Err(crate::Error::InvalidInput(format!(
                        "codex launch already configures {key}"
                    )));
                }
            }
        }

        let mut args = vec!["-m".into(), choice.model.clone()];
        if let Some(level) = effort {
            args.extend(["-c".into(), format!("model_reasoning_effort={level}")]);
        }
        Ok(args)
    }

    fn reset_command(&self) -> Option<&'static str> {
        Some("/clear")
    }

    // Codex 0.169.0 draws the error at column zero. It word-wraps onto adjacent
    // unindented rows. Assistant and composer continuations both carry indentation.
    fn reset_rejected(&self, fresh_screen: &str) -> bool {
        static RE: OnceLock<Regex> = OnceLock::new();
        cached(
            &RE,
            r"(?m)^■ '/clear'[ \n]is[ \n]disabled[ \n]while[ \n]a[ \n]task[ \n]is[ \n]in[ \n]progress\.$",
        )
        .is_match(fresh_screen)
    }

    /// verified: codex 0.154.0 — Braille title spinner (`⠋⠙⠹…` + the cwd's name) while
    /// working, and for ~130-180 ms at startup; DEC 2026 frames under every identity; OSC
    /// 9;4 not seen (no real turn measured).
    fn signals(&self) -> SignalHints {
        SignalHints {
            title_spinner: true,
            osc_progress: Unlock::Unknown,
            sync_frames: Unlock::Always,
        }
    }

    /// See [`LoginCheck`] for the three-way verification table.
    fn login_check(&self) -> Option<LoginCheck> {
        Some(LoginCheck {
            args: &["login", "status"],
            signed_out_marker: "Not logged in",
        })
    }

    /// No resume flag: codex resumes through its protocol (`thread/resume`), and an
    /// invented flag would make it exit at once. `probe` changes nothing either.
    fn rpc_launch_with_options(&self, options: &RpcLaunchOptions) -> crate::Result<RpcLaunch> {
        Ok(RpcLaunch::new("codex", CODEX_RPC_FLAGS).with_permissions(
            &options.permissions,
            UNATTENDED_RPC_ARGS,
            &[],
        ))
    }
}

/// The `-c` value trusting `cwd` for one run. The key must be the directory codex sees: its
/// working directory with symlinks resolved (macOS `/tmp` is `/private/tmp`), so the cwd is
/// canonicalised on unix (measured only with resolved paths). `None` for a cwd that is not
/// UTF-8 (a TOML string cannot hold it): no pre-trust, the dialog is steered.
fn trust_override(cwd: &Path) -> Option<String> {
    let dir = resolved(cwd);
    let key = toml_basic_string(dir.to_str()?);
    Some(format!("projects={{{key}={{trust_level=\"trusted\"}}}}"))
}

#[cfg(unix)]
fn resolved(cwd: &Path) -> Cow<'_, Path> {
    std::fs::canonicalize(cwd).map_or(Cow::Borrowed(cwd), Cow::Owned)
}

#[cfg(not(unix))]
fn resolved(cwd: &Path) -> Cow<'_, Path> {
    Cow::Borrowed(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ScreenState;

    // codex 0.154.0 draws this composer for 125-373 ms before its trust dialog or update
    // prompt replaces it.
    const LOADING_SCREEN: &str = concat!(
        "╭─────────────────────────────────────────╮\n",
        "│ >_ OpenAI Codex (v0.154.0)              │\n",
        "│                                         │\n",
        "│ model:       loading   /model to change │\n",
        "│ directory:   /private/tmp/apt.gNiqED    │\n",
        "│ permissions: YOLO mode                  │\n",
        "╰─────────────────────────────────────────╯\n",
        "› Ask Codex to do anything\n",
        "  ? for shortcuts",
    );

    const TRUST: &str = concat!(
        "> You are in /private/tmp/apt.gNiqED\n\n",
        "  Do you trust the contents of this directory? Working with untrusted contents comes with higher risk of prompt\n",
        "  injection.\n\n",
        "› 1. Yes, continue\n",
        "  2. No, quit\n\n",
        "  Press enter to continue",
    );

    fn update(options: &[&str]) -> String {
        let head = "  ✨ Update available! 0.154.0 -> 0.161.0\n\n  Release notes: https://github.com/openai/codex/releases/latest\n\n";
        format!("{head}{}\n\n  Press enter to continue", options.join("\n"))
    }

    fn keyed(text: &str) -> Option<(DialogKind, Option<TrustKey>)> {
        CodexProfile.dialog(text).map(|d| (d.kind, d.key))
    }

    #[test]
    fn keeps_enter_for_dialogs_whose_default_is_affirmative() {
        assert_eq!(
            keyed("do you trust the contents of this directory? yes, continue"),
            Some((DialogKind::Trust, Some(TrustKey::Enter)))
        );
        assert_eq!(
            keyed(TRUST),
            Some((DialogKind::Trust, Some(TrustKey::Enter)))
        );
        assert_eq!(
            keyed("Enable full access?\n› 1. Yes, continue anyway\n  2. Go back"),
            Some((DialogKind::FullAccess, Some(TrustKey::Enter)))
        );
    }

    #[test]
    fn the_trust_screen_says_press_enter_but_is_a_dialog_not_ready() {
        assert!(!CodexProfile.input_ready(&format!("OpenAI Codex\n{TRUST}")));
        assert!(matches!(
            CodexProfile.classify(TRUST),
            ScreenState::Dialog(_)
        ));
    }

    #[test]
    fn the_update_prompt_is_steered_to_skip() {
        let on_update = update(&[
            "› 1. Update now (runs `sh -c '…'`)",
            "  2. Skip",
            "  3. Skip until next version",
        ]);
        assert_eq!(
            keyed(&on_update),
            Some((DialogKind::UpdatePrompt, Some(TrustKey::Down)))
        );
        let on_skip = update(&[
            "  1. Update now",
            "› 2. Skip",
            "  3. Skip until next version",
        ]);
        assert_eq!(keyed(&on_skip).and_then(|k| k.1), Some(TrustKey::Enter));
        let on_skip_until = update(&[
            "  1. Update now",
            "  2. Skip",
            "› 3. Skip until next version",
        ]);
        assert_eq!(keyed(&on_skip_until).and_then(|k| k.1), Some(TrustKey::Up));
        let unreadable = update(&[
            "  1. Update now",
            "  2. Skip",
            "  3. Skip until next version",
        ]);
        assert_eq!(keyed(&unreadable), Some((DialogKind::UpdatePrompt, None)));
    }

    #[test]
    fn an_update_notice_without_its_options_is_not_a_dialog() {
        assert_eq!(keyed("✨ Update available! 0.154.0 -> 0.161.0"), None);
        assert_eq!(keyed("1. update now 2. skip"), None);
    }

    #[test]
    fn the_latest_dialog_header_picks_the_answer() {
        let on_update = update(&[
            "› 1. Update now",
            "  2. Skip",
            "  3. Skip until next version",
        ]);
        assert_eq!(
            keyed(&format!("{on_update}\n{TRUST}")),
            Some((DialogKind::Trust, Some(TrustKey::Enter)))
        );
        assert_eq!(
            keyed(&format!("{TRUST}\n{on_update}")),
            Some((DialogKind::UpdatePrompt, Some(TrustKey::Down)))
        );
        let full = "Enable full access?\n› 1. Yes, continue anyway";
        assert_eq!(
            keyed(&format!("{TRUST}\n{full}")),
            Some((DialogKind::FullAccess, Some(TrustKey::Enter)))
        );
    }

    #[test]
    fn the_loading_composer_is_not_ready() {
        assert!(!CodexProfile.input_ready(LOADING_SCREEN));
        assert_eq!(CodexProfile.classify(LOADING_SCREEN), ScreenState::Unknown);
        let loaded = LOADING_SCREEN.replace("loading  ", "gpt-x high");
        assert_eq!(CodexProfile.classify(&loaded), ScreenState::InputReady);
    }

    #[test]
    fn every_ready_marker_group_needs_the_brand() {
        let p = CodexProfile;
        for chrome in [
            "? for shortcuts",
            "enter to submit message",
            "tab to queue message",
            "esc to edit previous message",
            "directory: /x\npermissions: yolo\nmodel: gpt-x",
            "type a message",
            "› Ask Codex to do anything",
        ] {
            assert!(
                p.input_ready(&format!(">_ OpenAI Codex\n{chrome}")),
                "{chrome}"
            );
            if !chrome.to_lowercase().contains("codex") {
                assert!(!p.input_ready(chrome), "{chrome} without the brand");
            }
        }
        // Busy chrome and the dialog's "press enter" are not ready markers.
        assert!(!p.input_ready("OpenAI Codex\n• Working (0s • esc to interrupt) ctrl+c"));
        assert!(!p.input_ready("OpenAI Codex\nPress enter to continue"));
        assert!(!p.input_ready("OpenAI Codex\ndirectory: /x\nmodel: gpt-x"));
    }
}
