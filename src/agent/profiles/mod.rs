//! The built-in profiles: claude, codex, pi, opencode, cursor, and [`ProfileRegistry`].
//! Each module carries its marker sets and flags with the CLI version they were verified
//! on.
//!
//! | | claude | codex | pi | opencode | cursor (WIP) |
//! |---|---|---|---|---|---|
//! | `launchers()` | `claude` | `codex` | `pi` | `opencode` | `cursor-agent`, `agent` |
//! | `launch()` program | `claude` | `codex` | `pi` | `opencode` | `agent` |
//! | unattended (default) | `--permission-mode bypassPermissions` if the profile accepts the bypass warning | `--dangerously-bypass-approvals-and-sandbox` | no flags needed | env default `OPENCODE_PERMISSION={"*":"allow"}` | `--force` (explicit denies still apply) |
//! | every launch | — | `-c check_for_update_on_startup=false` | — | — | — |
//! | explicit PTY model and optional effort | `--model`, `--effort` | `-m`, `-c model_reasoning_effort=…` | `--model`, `--thinking` | `-m` only; an effort is unsupported | unsupported |
//! | pre-trust ([`PreTrust::FlagsOnly`](crate::agent::PreTrust::FlagsOnly)) | — | `-c projects={"<cwd>"={trust_level="trusted"}}`, not under cmd.exe; saves nothing | `--approve`; saves nothing | — | `--trust`; **saves** a trust record |
//! | isolated PTY transcript observation | Explicit final | Explicit final | Inferred stop | — | Explicit or next-prompt completion, opt-in mixed text |
//! | `reset_command()` | `/clear` | `/clear` | `/new` | `/new` | `/new` |
//! | `login_check()` | `auth status` | `login status` | — | — | — |
//! | `rpc_launch()` (unattended) | [`CLAUDE_RPC_FLAGS`](claude::CLAUDE_RPC_FLAGS) + bypass mode (+ `--continue`) | [`CODEX_RPC_FLAGS`](codex::CODEX_RPC_FLAGS) + `-c approval_policy="never" -c sandbox_mode="danger-full-access"` | [`PI_RPC_FLAGS`](pi::PI_RPC_FLAGS) (+ `--continue`, or `--no-session` for a probe) | [`OPENCODE_RPC_FLAGS`](opencode::OPENCODE_RPC_FLAGS) + unattended env default | — |
//! | title spinner | no (`✳`/`◐`) | Braille | not seen | not measured | not measured |
//! | sync frames | VS Code identity only | always | always | once DECRQM 2026 is answered | not at cold launch |
//! | PTY final replies (optional `turns`) | 2.1.296 on macOS arm64, direct launch with a host-enforced exclusive writer | — | — | — | — |
//! | best-effort ambient PTY turns (optional `turns`, any PTY, every platform; `AgentProfile::ambient_turns`) | explicit `turn_duration` or interruption | explicit `task_complete` / `turn_aborted` | inferred: next prompt or quiet | TUI server feed after `spawn_with_ambient_feed` (1.18.21 shapes, declared) | explicit `turn_ended` success or next prompt; completion only, mixed text opt-in |
//! | [`AgentProfile::question_dialog`] | `AskUserQuestion` and its answer review (2.1.296) | Plan selections and notes (0.154.0) | — | `question` picker (1.18.21) | shell approval (2026.11.13-eacade1 packaged renderer) |
//! | best-effort ambient questions (optional `questions`, any PTY: `observe_ambient_transcript_questions`, `OpenCodeAmbientQuestions`) | `AskUserQuestion` from the ambient transcript, relayed while its dialog shows it | Plan `request_user_input` from the ambient rollout, relayed while its dialog shows it | — | `GET /question` for the host-specified session of its TUI server after `spawn_with_ambient_feed` (1.18.21 shapes, declared) | — (its shell approval is read off the screen by `observe_questions`) |
//! | screen evidence | 2.1.293; live smoke 2.1.295 | 0.154.0 (dialog wording since 0.142.5) | 0.85.1 | 1.18.21 | trust screen: cursor-agent 2026.09.23 |
//!
//! OSC 9;4 progress was not seen on any agent (no real turn measured).
//! RPC permissions are configurable through
//! [`AgentProfile::rpc_launch_with_options`].
//! PTY model and effort flags come from Claude Code 2.1.327, Codex CLI 0.170.0
//! and Pi 1.5.2. OpenCode's `-m` is listed by opencode 1.18.21's TUI help per prior
//! measurement; not re-recorded here; opencode 2.x not covered. Its record is a
//! declaration. The argv construction tests use synthetic choices.
//! Live permission checks separately cover Codex 0.169.0 config/read and OpenCode
//! 1.18.25 subagent external-file reads with a local model peer. Cursor support is work
//! in progress. Its 2026.11.13 `--force` launch reached login, and its shell approval
//! renderer was recorded separately. Authenticated startup/composer validation is pending.
//! These checks do not extend the screen versions above or certify native Windows.
//! Pi and custom profiles inherit the default `false` question predicate.
//!
//! The ambient sources pin no CLI version; their fixtures were recorded on Claude 2.1.296,
//! Codex 0.154.0, Pi 0.85.1 and 1.5.2, and Cursor 2026.10.01 and 2026.11.13-eacade1 (see
//! `agent::turns::ambient`).
//!
//! Profiles read screen text, so a CLI release that rewords a dialog or its composer can
//! break detection; the controller then logs a `startup markers not recognised` report.
//! Look a profile up by id in [`ProfileRegistry::builtin`]; replace a built-in, or add a
//! host's own profile, with [`ProfileRegistry::register`]:
//!
//! ```
//! use std::sync::Arc;
//!
//! use agent_link::agent::profiles::ClaudeProfile;
//! use agent_link::agent::ProfileRegistry;
//!
//! let mut registry = ProfileRegistry::builtin();
//! // claude without the bypass flag; its warning, should it show, is left to the user.
//! registry.register(Arc::new(ClaudeProfile::accepting_bypass_warning(false)));
//! assert_eq!(registry.ids(), ["claude", "opencode", "pi", "codex", "cursor"]);
//! ```

/// Best-effort transcript sources for [`AgentProfile::ambient_turns`], on every platform.
#[cfg(feature = "turns")]
pub(in crate::agent) mod ambient;
pub mod claude;
pub mod codex;
pub mod cursor;
pub mod opencode;
pub mod pi;
#[cfg(all(unix, any(test, feature = "turns")))]
pub(in crate::agent) mod turns {
    pub(in crate::agent) mod claude;
    #[cfg(feature = "turns")]
    pub(in crate::agent) mod isolated;
    pub(in crate::agent) mod json;
}
/// Elsewhere only the strict JSON helpers, for the question records of the ambient tier.
#[cfg(all(not(unix), feature = "questions"))]
pub(in crate::agent) mod turns {
    #[allow(dead_code, reason = "the controlled tier's parsers are Unix-only")]
    pub(in crate::agent) mod json;
}

use std::fmt;
use std::sync::Arc;

use super::compatibility::{Evidence, EvidenceKind};
use super::{AgentProfile, ModelChoice};
use crate::{Error, Result};
pub use claude::ClaudeProfile;
pub use codex::CodexProfile;
pub use cursor::CursorProfile;
pub use opencode::OpencodeProfile;
pub use pi::{PiProfile, PiTrust};

const MODEL_ARGV_EVIDENCE: Evidence = Evidence::new(
    "PTY model and effort argv construction",
    EvidenceKind::SyntheticFixture,
    "tests/profiles_launch.rs",
);

/// Best-effort ambient turn ends and replies parsed from a recorded transcript. Each
/// profile names its capture, version and platform; no version is admitted or pinned.
#[cfg(feature = "turns")]
const AMBIENT_PARSING: Evidence = Evidence::new(
    "best-effort ambient turn ends and replies",
    EvidenceKind::CapturedTranscript,
    "tests/fixtures/transcripts/ambient/README.md",
);

/// The built-in ambient lookups on temporary configuration roots, followed through a
/// fake-agent PTY. No real CLI and no user configuration directory.
#[cfg(feature = "turns")]
const AMBIENT_LOOKUP: Evidence = Evidence::new(
    "best-effort ambient transcript lookup on a direct fake-agent PTY",
    EvidenceKind::SyntheticFixture,
    "tests/ambient_sources.rs",
);

/// Best-effort ambient question records parsed from a recorded transcript; each profile
/// names its capture and version.
#[cfg(feature = "questions")]
const AMBIENT_QUESTION_RECORDS: Evidence = Evidence::new(
    "best-effort ambient question tool call and result",
    EvidenceKind::CapturedTranscript,
    "tests/fixtures/transcripts/ambient/README.md",
);

/// Ambient questions relayed and answered through the fake agent's dialog on real PTYs,
/// with temporary transcripts or a synthetic OpenCode-like server. No real CLI.
#[cfg(feature = "questions")]
const AMBIENT_QUESTION_RELAY: Evidence = Evidence {
    platform: Some("macos-aarch64"),
    ..Evidence::new(
        "best-effort ambient question relay and answer on a fake-agent PTY",
        EvidenceKind::SyntheticFixture,
        "tests/ambient_questions.rs",
    )
};

/// Returns a fixed spelling from the profile's measured vocabulary.
fn requested_effort(
    profile: &str,
    choice: &ModelChoice,
    levels: &'static [&str],
) -> Result<Option<&'static str>> {
    let Some(request) = choice.effort.as_deref() else {
        return Ok(None);
    };
    let level = levels
        .iter()
        .copied()
        .find(|&level| level == request)
        .ok_or_else(|| Error::Unsupported(format!("PTY reasoning effort for {profile}")))?;
    Ok(Some(level))
}

/// Claude and Pi take separate model and effort words. A required operand can itself
/// look like a model flag, e.g. `--system-prompt --model`.
fn long_model_args(
    profile: &str,
    choice: &ModelChoice,
    base_args: &[String],
    effort_flag: &str,
    levels: &'static [&str],
    value_options: &[&str],
    option_name: fn(&str) -> &str,
) -> Result<Vec<String>> {
    let effort = requested_effort(profile, choice, levels)?;
    let mut base = base_args.iter().map(String::as_str);
    while let Some(word) = base.next() {
        let name = option_name(word);
        if name == "--model" || (effort.is_some() && name == effort_flag) {
            return Err(Error::InvalidInput(format!(
                "{profile} launch already supplies {name}"
            )));
        }
        if word == effort_flag || value_options.contains(&word) {
            base.next().ok_or_else(|| {
                Error::InvalidInput(format!(
                    "{profile} launch needs a value for {word} before the model options"
                ))
            })?;
        }
    }
    let mut args = vec!["--model".into(), choice.model.clone()];
    if let Some(level) = effort {
        args.extend([effort_flag.into(), level.into()]);
    }
    Ok(args)
}

/// One instance of each built-in profile, in this order: claude, opencode, pi, codex,
/// cursor.
pub fn builtin() -> Vec<Arc<dyn AgentProfile>> {
    vec![
        Arc::new(ClaudeProfile::new()),
        Arc::new(OpencodeProfile),
        Arc::new(PiProfile::new()),
        Arc::new(CodexProfile),
        Arc::new(CursorProfile),
    ]
}

/// Profiles by id, in registration order. Look an agent id up here to validate it before
/// launching (an unknown id is `None`).
#[derive(Clone, Default)]
pub struct ProfileRegistry {
    profiles: Vec<Arc<dyn AgentProfile>>,
}

impl ProfileRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The built-ins ([`builtin`]): claude, opencode, pi, codex, cursor.
    pub fn builtin() -> Self {
        Self {
            profiles: builtin(),
        }
    }

    /// Adds `profile`; one with the same id is replaced in place (keeping its position)
    /// and returned.
    pub fn register(&mut self, profile: Arc<dyn AgentProfile>) -> Option<Arc<dyn AgentProfile>> {
        match self.profiles.iter().position(|p| p.id() == profile.id()) {
            Some(at) => Some(std::mem::replace(&mut self.profiles[at], profile)),
            None => {
                self.profiles.push(profile);
                None
            }
        }
    }

    /// The profile with this id.
    pub fn get(&self, id: &str) -> Option<Arc<dyn AgentProfile>> {
        self.profiles.iter().find(|p| p.id() == id).cloned()
    }

    /// The ids, in registration order.
    pub fn ids(&self) -> Vec<String> {
        self.profiles.iter().map(|p| p.id().to_string()).collect()
    }
}

impl fmt::Debug for ProfileRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProfileRegistry")
            .field("ids", &self.ids())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_ids_are_unique_and_in_order() {
        let ids: Vec<String> = builtin().iter().map(|p| p.id().to_string()).collect();
        assert_eq!(ids, ["claude", "opencode", "pi", "codex", "cursor"]);
        assert_eq!(ProfileRegistry::builtin().ids(), ids);
    }

    #[test]
    fn empty_text_is_neither_dialog_nor_ready() {
        for profile in builtin() {
            for text in ["", " \n \u{1b}[2J"] {
                assert_eq!(profile.dialog(text), None, "{}", profile.id());
                assert!(!profile.input_ready(text), "{}", profile.id());
                assert_eq!(profile.pending_input(text, "x"), None, "{}", profile.id());
            }
        }
    }

    #[test]
    fn register_replaces_in_place_or_appends() {
        let mut registry = ProfileRegistry::builtin();
        let old = registry.register(Arc::new(ClaudeProfile::accepting_bypass_warning(false)));
        assert_eq!(old.map(|p| p.id().to_string()).as_deref(), Some("claude"));
        assert_eq!(registry.ids(), ProfileRegistry::builtin().ids());
        assert!(registry.get("gpt").is_none());
        assert!(ProfileRegistry::new().ids().is_empty());
        assert_eq!(
            format!("{:?}", ProfileRegistry::builtin()),
            r#"ProfileRegistry { ids: ["claude", "opencode", "pi", "codex", "cursor"] }"#
        );
    }
}
