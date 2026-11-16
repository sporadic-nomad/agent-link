//! Agent profiles for the fake agent (`tests/support/fake_agent.rs`): [`fake_profile`]
//! launches it with a scene and flags, [`unlaunchable_profile`] launches `false` (exits at
//! once — "the agent will not relaunch"). Screens are read from the fake's own text:
//! dialog `fake trust dialog` (pointer `❯` on `1. No, exit` / `2. Yes, trust this folder`),
//! ready footer `fake-agent ready · esc to interrupt` (in full: the echoed launch line
//! `…/agent-link-fake-agent ready` holds `fake-agent ready`); pending input is the trait's
//! default (composer `> `).

use std::path::Path;
use std::sync::{Arc, OnceLock};

use agent_link::agent::detect::{from_last, has_all, steer_two};
use agent_link::agent::{
    AgentProfile, Dialog, DialogKind, LaunchPlan, Permissions, ShellKind, SignalHints, TrustPolicy,
    Unlock,
};
use agent_link::screen::normalize;
use regex::Regex;

use super::fake_agent_path;

// The binary's own list, so the two cannot drift.
#[path = "../support/fake_agent/scenes.rs"]
mod scenes;

/// The fake agent's scenes (its binary exits 2 on any other).
pub use scenes::SCENES as FAKE_SCENES;

/// The fake's dialog header (normalised).
pub const DIALOG_HEADER: &str = "fake trust dialog";
/// The fake's ready footer (normalised). Matched in full: the shell's echo of the launch
/// line for the `ready` scene contains `fake-agent ready`.
pub const READY_MARKER: &str = "fake-agent ready · esc to interrupt";
const YES: &str = "yes, trust this folder";
const NO: &str = "no, exit";

fn pointer() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[❯›>] ?(?:[0-9]+\. ?)?(yes, trust this folder|no, exit)")
            .expect("pattern compiles")
    })
}

/// A test profile: launches `program args…`, reads the fake agent's screens.
#[derive(Clone, Debug)]
pub struct FakeProfile {
    id: &'static str,
    program: String,
    args: Vec<String>,
}

impl FakeProfile {
    /// The fake agent with `scene` and `flags` (e.g. `["--dialog-after-ms", "300"]`).
    /// Panics on an unknown scene.
    pub fn new(scene: &str, flags: &[&str]) -> Self {
        assert!(FAKE_SCENES.contains(&scene), "unknown fake scene {scene:?}");
        let program = fake_agent_path().to_str().expect("UTF-8 path").to_string();
        let args = std::iter::once(scene)
            .chain(flags.iter().copied())
            .map(String::from)
            .collect();
        Self {
            id: "fake",
            program,
            args,
        }
    }

    fn has_arg(&self, arg: &str) -> bool {
        self.args.iter().any(|a| a == arg)
    }
}

/// [`FakeProfile::new`] as a shared profile.
pub fn fake_profile(scene: &str, flags: &[&str]) -> Arc<dyn AgentProfile> {
    Arc::new(FakeProfile::new(scene, flags))
}

/// A profile like [`fake_profile`] whose launch line is `false`: the "agent" exits at once
/// and the shell stays in front.
pub fn unlaunchable_profile() -> Arc<dyn AgentProfile> {
    Arc::new(FakeProfile {
        id: "fake-unlaunchable",
        program: "false".into(),
        args: Vec::new(),
    })
}

/// The line that starts `profile` (default permissions and trust) in the POSIX test shell,
/// with its `\n`.
pub fn launch_line(profile: &dyn AgentProfile, cwd: &Path) -> String {
    let plan = profile.launch(
        &Permissions::default(),
        &TrustPolicy::default(),
        cwd,
        ShellKind::Posix,
    );
    let line = plan.command_line(ShellKind::Posix).expect("a typable line");
    format!("{line}{}", ShellKind::Posix.line_terminator())
}

impl AgentProfile for FakeProfile {
    fn id(&self) -> &str {
        self.id
    }

    fn dialog(&self, text: &str) -> Option<Dialog> {
        let text = normalize(text);
        if !has_all(&text, &[DIALOG_HEADER, YES]) {
            return None;
        }
        let dialog = from_last(&text, DIALOG_HEADER);
        Some(Dialog {
            kind: DialogKind::Trust,
            key: steer_two(dialog, pointer(), YES, NO),
        })
    }

    fn input_ready(&self, screen: &str) -> bool {
        normalize(screen).contains(READY_MARKER) && self.dialog(screen).is_none()
    }

    fn launchers(&self) -> &'static [&'static str] {
        &["agent-link-fake-agent"]
    }

    fn launch(
        &self,
        perms: &Permissions,
        _trust: &TrustPolicy,
        _cwd: &Path,
        _shell: ShellKind,
    ) -> LaunchPlan {
        let mut plan = LaunchPlan::new(self.program.clone());
        plan.args = self.args.clone();
        plan.with_permissions(perms, &[], &[])
    }

    fn reset_command(&self) -> Option<&'static str> {
        Some("/new")
    }

    fn signals(&self) -> SignalHints {
        let unlock = |on: bool| if on { Unlock::Always } else { Unlock::Never };
        SignalHints {
            title_spinner: self.has_arg("--title"),
            osc_progress: unlock(self.args.first().is_some_and(|s| s == "progress")),
            sync_frames: unlock(self.has_arg("--sync")),
        }
    }
}
