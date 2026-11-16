//! Implement a custom `AgentProfile` using `agent::detect` helpers and register it in
//! `ProfileRegistry` (offline; no external CLI needed).
//!
//! ```text
//! cargo run -p agent-link --example custom_profile
//! ```

use std::error::Error;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};

use agent_link::agent::compatibility::{Compatibility, Evidence, EvidenceKind, Support};
use agent_link::agent::detect::{from_last, has_all, pending_input_where, steer_two};
use agent_link::agent::{
    AgentProfile, Dialog, DialogKind, LaunchPlan, Permissions, ProfileRegistry, ScreenState,
    ShellKind, TrustPolicy,
};
use agent_link::screen::normalize;
use regex::Regex;

struct CustomCliProfile;

fn trust_pointer() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[❯>] ?(yes, trust|no, exit)").expect("valid regex"))
}

impl AgentProfile for CustomCliProfile {
    fn id(&self) -> &str {
        "custom-cli"
    }

    fn launchers(&self) -> &'static [&'static str] {
        &["custom-cli"]
    }

    fn compatibility(&self) -> Compatibility {
        static EVIDENCE: &[Evidence] = &[Evidence::new(
            "trust dialog and ready composer",
            EvidenceKind::Declaration,
            "examples/custom_profile.rs",
        )];
        let mut c = Compatibility::default();
        c.pty = Support::Supported;
        c.evidence = EVIDENCE;
        c
    }

    fn launch(
        &self,
        perms: &Permissions,
        trust: &TrustPolicy,
        _cwd: &Path,
        shell: ShellKind,
    ) -> LaunchPlan {
        let mut plan = LaunchPlan::new("custom-cli")
            .for_shell(shell)
            .with_permissions(perms, &["--yolo"], &[]);
        if trust.pre_trust.wants_flags() {
            plan.args.push("--trust".into());
        }
        plan
    }

    fn reset_command(&self) -> Option<&'static str> {
        Some("/clear")
    }

    fn dialog(&self, text: &str) -> Option<Dialog> {
        let norm = normalize(text);
        if !has_all(&norm, &["trust this workspace", "yes, trust", "no, exit"]) {
            return None;
        }
        let key = steer_two(
            from_last(&norm, "trust this workspace"),
            trust_pointer(),
            "yes, trust",
            "no, exit",
        );
        Some(Dialog {
            kind: DialogKind::Trust,
            key,
        })
    }

    fn input_ready(&self, screen: &str) -> bool {
        let norm = normalize(screen);
        self.dialog(&norm).is_none() && has_all(&norm, &["custom cli", "enter to send"])
    }

    fn pending_input(&self, screen: &str, sent: &str) -> Option<bool> {
        pending_input_where(screen, sent, |row| row.trim_start().starts_with('/'))
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("custom_profile: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut registry = ProfileRegistry::builtin();
    registry.register(Arc::new(CustomCliProfile));

    let profile = registry
        .get("custom-cli")
        .ok_or("custom profile missing from registry")?;

    let plan = profile.launch(
        &Permissions::Unattended,
        &TrustPolicy::unattended(),
        Path::new("."),
        ShellKind::native(),
    );
    println!("launch line: {}", plan.command_line(ShellKind::native())?);

    let trust_screen = "Trust this workspace?\n> No, exit\n  Yes, trust";
    let ready_screen = "Custom CLI v1.0\n> hello world\nEnter to send";
    let submitted_screen = "Custom CLI v1.0\n> \nEnter to send";

    assert!(matches!(
        profile.classify(trust_screen),
        ScreenState::Dialog(Dialog {
            kind: DialogKind::Trust,
            ..
        })
    ));
    assert_eq!(profile.classify(ready_screen), ScreenState::InputReady);
    assert_eq!(
        profile.pending_input(ready_screen, "hello world"),
        Some(true)
    );
    assert_eq!(
        profile.pending_input(submitted_screen, "hello world"),
        Some(false)
    );
    println!(
        "custom profile verified; registry ids: {:?}",
        registry.ids()
    );
    Ok(())
}
