//! How an agent is launched with respect to approvals and folder trust: [`Permissions`]
//! picks the approval posture, [`TrustPolicy`] the pre-trust flags and dialog steering;
//! [`ForegroundPolicy`] says which foreground answers a delivery trusts.

use std::fmt;

/// The approval posture an agent is launched with, for PTY and RPC profiles.
///
/// Defaults to [`Unattended`](Self::Unattended) (YOLO or the profile's equivalent).
/// Select [`AgentDefault`](Self::AgentDefault) or [`Custom`](Self::Custom) explicitly
/// to use CLI configuration or supply restrictions. These are launch settings, not
/// an OS sandbox or a host approval callback.
#[derive(Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Permissions {
    /// Use the profile's flags/env that let the agent run tools without asking.
    /// Some profiles also disable the agent's sandbox. This is the default;
    /// explicit host/CLI rules may still restrict operations.
    #[default]
    Unattended,
    /// No permission flags or env: explicitly use the agent's own configured posture.
    /// That configuration may itself allow unattended tools.
    AgentDefault,
    /// These args and env instead of the profile's unattended ones. The env is always set
    /// for the agent ([`LaunchPlan::env`](super::LaunchPlan::env) or
    /// [`RpcLaunch::env`](super::RpcLaunch::env)); host env removals still win.
    Custom {
        /// Arguments placed where the unattended flags would go.
        args: Vec<String>,
        /// Variables set for the agent.
        env: Vec<(String, String)>,
    },
}

impl fmt::Debug for Permissions {
    // Env values can hold tokens: print keys only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Permissions::Unattended => f.write_str("Unattended"),
            Permissions::AgentDefault => f.write_str("AgentDefault"),
            Permissions::Custom { args, env } => f
                .debug_struct("Custom")
                .field("args", args)
                .field("env_keys", &super::launch::env_keys(env))
                .finish(),
        }
    }
}

/// Whether the launch line carries the agent's "trust this folder" flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum PreTrust {
    /// Add the profile's pre-trust flag where it has one (its folder-trust dialog is then not
    /// shown). Some of these flags make the agent save the trust decision; each profile says
    /// which. Config files are never edited by the crate.
    FlagsOnly,
    /// No pre-trust flag: the trust dialog shows and is steered (or left to the user).
    #[default]
    Off,
}

impl PreTrust {
    /// Whether the launch line carries the profile's pre-trust flag (every setting but
    /// [`Off`](Self::Off)). Profiles test this, never one variant, so a later variant that
    /// adds more than flags still gets them.
    pub fn wants_flags(self) -> bool {
        self != PreTrust::Off
    }
}

/// Folder-trust handling for one launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TrustPolicy {
    /// Pre-trust flags on the launch line.
    pub pre_trust: PreTrust,
    /// Whether the startup gate answers recognised dialogs with the profile's keys (`false`:
    /// it only waits for the user to answer them).
    pub steer_dialogs: bool,
}

impl Default for TrustPolicy {
    /// No pre-trust flags and no automatic dialog answers.
    fn default() -> Self {
        Self {
            pre_trust: PreTrust::Off,
            steer_dialogs: false,
        }
    }
}

impl TrustPolicy {
    /// Explicitly trust the working folder through supported flags and answer known
    /// startup dialogs. Some CLIs persist the trust decision. Use only for a folder
    /// the host intends to trust; this is independent of tool approval permissions.
    pub fn unattended() -> Self {
        Self {
            pre_trust: PreTrust::FlagsOnly,
            steer_dialogs: true,
        }
    }
}

/// Which foreground answers a delivery trusts. Every delivery checks ownership before
/// pasting and before Enter. Only a trusted shell answer permits automatic launch or
/// shell fallback; only the agent's trusted program identity permits agent input.
/// Unknown or untrusted ownership ends `ForegroundUnproven`, even for non-strict sends.
/// Strict sends additionally recheck ownership after submission evidence settles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ForegroundPolicy {
    /// Only an exact answer ([`Confidence::Exact`](crate::Confidence::Exact), unix) counts.
    /// Windows answers are heuristic, so automatic delivery and launch are refused
    /// there unless the host explicitly selects `AcceptHeuristic`.
    #[default]
    ExactOnly,
    /// A heuristic answer counts too (Windows: the shell's process tree).
    AcceptHeuristic,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_use_unattended_permissions_and_leave_trust_to_the_user() {
        assert_eq!(ForegroundPolicy::default(), ForegroundPolicy::ExactOnly);
        assert_eq!(Permissions::default(), Permissions::Unattended);
        assert_eq!(PreTrust::default(), PreTrust::Off);
        assert_eq!(
            TrustPolicy::default(),
            TrustPolicy {
                pre_trust: PreTrust::Off,
                steer_dialogs: false
            }
        );
        assert_eq!(
            TrustPolicy::unattended(),
            TrustPolicy {
                pre_trust: PreTrust::FlagsOnly,
                steer_dialogs: true,
            }
        );
    }

    #[test]
    fn debug_never_prints_custom_env_values() {
        let perms = Permissions::Custom {
            args: vec!["--yolo".into()],
            env: vec![("API_TOKEN".into(), "secret-value".into())],
        };
        let shown = format!("{perms:?}");
        assert!(
            shown.contains("API_TOKEN") && shown.contains("--yolo"),
            "{shown}"
        );
        assert!(!shown.contains("secret-value"), "{shown}");
    }
}
