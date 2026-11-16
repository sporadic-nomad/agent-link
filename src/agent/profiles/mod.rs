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
//! | pre-trust ([`PreTrust::FlagsOnly`](crate::agent::PreTrust::FlagsOnly)) | — | `-c projects={"<cwd>"={trust_level="trusted"}}`, not under cmd.exe; saves nothing | `--approve`; saves nothing | — | `--trust`; **saves** a trust record |
//! | `reset_command()` | `/clear` | `/clear` | `/new` | `/new` | `/new` |
//! | `login_check()` | `auth status` | `login status` | — | — | — |
//! | `rpc_launch()` (unattended) | [`CLAUDE_RPC_FLAGS`](claude::CLAUDE_RPC_FLAGS) + bypass mode (+ `--continue`) | [`CODEX_RPC_FLAGS`](codex::CODEX_RPC_FLAGS) + `-c approval_policy="never" -c sandbox_mode="danger-full-access"` | [`PI_RPC_FLAGS`](pi::PI_RPC_FLAGS) (+ `--continue`, or `--no-session` for a probe) | [`OPENCODE_RPC_FLAGS`](opencode::OPENCODE_RPC_FLAGS) + unattended env default | — |
//! | title spinner | no (`✳`/`◐`) | Braille | not seen | not measured | not measured |
//! | sync frames | VS Code identity only | always | always | once DECRQM 2026 is answered | not at cold launch |
//! | verified on | 2.1.293; live smoke 2.1.295 | 0.154.0 (dialog wording since 0.142.5) | 0.85.1 | 1.18.21 | trust screen: cursor-agent 2026.09.23 |
//!
//! OSC 9;4 progress was not seen on any agent (no real turn measured).
//! RPC permissions are configurable through
//! [`AgentProfile::rpc_launch_with_options`].
//! Live permission checks separately cover Codex 0.169.0 config/read and OpenCode
//! 1.18.25 subagent external-file reads with a local model peer. Cursor support is work
//! in progress: 2026.11.13 has a `--force` login recording only, and authenticated
//! startup/composer validation is pending.
//! These checks do not extend the screen versions above or certify native Windows.
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

pub mod claude;
pub mod codex;
pub mod cursor;
pub mod opencode;
pub mod pi;

use std::fmt;
use std::sync::Arc;

use super::AgentProfile;
pub use claude::ClaudeProfile;
pub use codex::CodexProfile;
pub use cursor::CursorProfile;
pub use opencode::OpencodeProfile;
pub use pi::{PiProfile, PiTrust};

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
