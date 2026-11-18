//! The built-in profiles' launch and data rows: launch argv/env per agent, approval posture
//! and pre-trust, model selection, the typed command lines for POSIX shells and cmd.exe,
//! the stdio launches and login checks, and the registry.
#![cfg(feature = "agent")]

use std::path::Path;
use std::sync::Arc;

use agent_link::agent::profiles::{ClaudeProfile, claude, opencode};
use agent_link::agent::{
    AgentProfile, Dialog, LaunchPlan, ModelChoice, Permissions, PreTrust, ProfileRegistry,
    RpcLaunch, RpcLaunchOptions, ShellKind, SignalHints, TrustPolicy, Unlock,
};
use agent_link::{Error, KnownTerminal};

const IDS: [&str; 5] = ["claude", "opencode", "pi", "codex", "cursor"];
/// A cwd that does not exist, so it is used as given (an existing one is canonicalised).
const CWD: &str = "/agent-link-test/a b";

fn profile(id: &str) -> Arc<dyn AgentProfile> {
    ProfileRegistry::builtin()
        .get(id)
        .unwrap_or_else(|| panic!("no built-in profile {id}"))
}

fn trust(pre_trust: PreTrust) -> TrustPolicy {
    TrustPolicy {
        pre_trust,
        ..TrustPolicy::default()
    }
}

fn plan(id: &str, perms: &Permissions, pre_trust: PreTrust, shell: ShellKind) -> LaunchPlan {
    profile(id).launch(perms, &trust(pre_trust), Path::new(CWD), shell)
}

fn line(id: &str, perms: &Permissions, pre_trust: PreTrust, shell: ShellKind) -> String {
    plan(id, perms, pre_trust, shell)
        .command_line(shell)
        .unwrap_or_else(|e| panic!("{id}: {e}"))
}

fn posix(id: &str) -> String {
    line(
        id,
        &Permissions::Unattended,
        PreTrust::FlagsOnly,
        ShellKind::Posix,
    )
}

fn custom() -> Permissions {
    Permissions::Custom {
        args: vec!["--mine".into()],
        env: vec![("MINE".into(), "1".into())],
    }
}

fn kv(k: &str, v: &str) -> (String, String) {
    (k.to_string(), v.to_string())
}

#[test]
fn explicit_unattended_launch_lines() {
    assert_eq!(
        posix("claude"),
        "claude --permission-mode bypassPermissions"
    );
    assert_eq!(
        posix("codex"),
        r#"codex --dangerously-bypass-approvals-and-sandbox -c check_for_update_on_startup=false -c 'projects={"/agent-link-test/a b"={trust_level="trusted"}}'"#
    );
    assert_eq!(posix("pi"), "pi --approve");
    assert_eq!(posix("opencode"), "opencode");
    assert_eq!(posix("cursor"), "agent --force --trust");
}

#[test]
fn cmd_launch_lines_leave_out_the_codex_pre_trust() {
    let cmd = |id| {
        line(
            id,
            &Permissions::Unattended,
            PreTrust::FlagsOnly,
            ShellKind::Cmd,
        )
    };
    assert_eq!(
        cmd("codex"),
        "codex --dangerously-bypass-approvals-and-sandbox -c check_for_update_on_startup=false"
    );
    assert_eq!(cmd("claude"), "claude --permission-mode bypassPermissions");
    assert_eq!(cmd("pi"), "pi --approve");
    assert_eq!(cmd("opencode"), "opencode");
    assert_eq!(cmd("cursor"), "agent --force --trust");
}

#[test]
fn pre_trust_off_drops_only_the_trust_flags() {
    let off = |id| {
        line(
            id,
            &Permissions::Unattended,
            PreTrust::Off,
            ShellKind::Posix,
        )
    };
    assert_eq!(
        off("codex"),
        "codex --dangerously-bypass-approvals-and-sandbox -c check_for_update_on_startup=false"
    );
    assert_eq!(off("pi"), "pi");
    assert_eq!(off("cursor"), "agent --force");
    assert_eq!(off("claude"), "claude --permission-mode bypassPermissions");
    assert_eq!(off("opencode"), "opencode");
}

#[test]
fn agent_default_drops_the_unattended_args_and_env() {
    let default = |id| {
        plan(
            id,
            &Permissions::AgentDefault,
            PreTrust::Off,
            ShellKind::Posix,
        )
    };
    for id in IDS {
        let p = default(id);
        assert!(p.env.is_empty() && p.env_defaults.is_empty(), "{id}: {p:?}");
    }
    let lines: Vec<String> = IDS
        .iter()
        .map(|id| default(id).command_line(ShellKind::Posix).unwrap())
        .collect();
    // codex keeps its update-check override: it is not a permission.
    assert_eq!(
        lines,
        [
            "claude",
            "opencode",
            "pi",
            "codex -c check_for_update_on_startup=false",
            "agent"
        ]
    );
}

#[test]
fn new_agent_specs_default_to_unattended_permissions_without_folder_trust() {
    use agent_link::agent::AgentSpec;

    for id in IDS {
        let p = profile(id);
        let spec = AgentSpec::new(Arc::clone(&p));
        assert_eq!(spec.permissions, Permissions::Unattended, "{id}");
        assert_eq!(spec.model, None, "{id}");
        assert_eq!(spec.trust.pre_trust, PreTrust::Off, "{id}");
        assert!(!spec.trust.steer_dialogs, "{id}");
        for shell in [ShellKind::Posix, ShellKind::Cmd] {
            let actual = p.launch(&spec.permissions, &spec.trust, Path::new(CWD), shell);
            let expected = plan(id, &Permissions::Unattended, PreTrust::Off, shell);
            assert_eq!(
                actual.command_line(shell).unwrap(),
                expected.command_line(shell).unwrap()
            );
            assert_eq!(actual.env, expected.env, "{id}");
            assert_eq!(actual.env_defaults, expected.env_defaults, "{id}");
        }
    }
}

#[test]
fn a_spec_reports_only_the_presence_of_a_model_choice_in_debug_output() {
    let mut spec = agent_link::agent::AgentSpec::new(profile("pi"));
    spec.model = Some(ModelChoice::new("private-deployment").with_effort("private-level"));
    let shown = format!("{spec:?}");
    assert!(shown.contains("has_model_choice: true"));
    assert!(!shown.contains("private-"));
}

#[test]
fn custom_permissions_replace_the_unattended_ones() {
    for id in IDS {
        let p = plan(id, &custom(), PreTrust::Off, ShellKind::Posix);
        assert_eq!(p.args.first().map(String::as_str), Some("--mine"), "{id}");
        assert_eq!(p.env, [kv("MINE", "1")], "{id}");
        assert!(p.env_defaults.is_empty(), "{id}");
        assert!(!p.args.iter().any(|a| a.contains("bypass")), "{id}: {p:?}");
    }
    assert_eq!(
        line("codex", &custom(), PreTrust::Off, ShellKind::Posix),
        "codex --mine -c check_for_update_on_startup=false"
    );
}

/// Pre-trust flags do not depend on the approval posture; codex orders posture args, the
/// update-check override, then pre-trust.
#[test]
fn pre_trust_flags_come_with_every_posture() {
    let posix = |id, perms: &Permissions| line(id, perms, PreTrust::FlagsOnly, ShellKind::Posix);
    let default = Permissions::AgentDefault;
    assert_eq!(posix("pi", &default), "pi --approve");
    assert_eq!(posix("cursor", &default), "agent --trust");
    assert_eq!(posix("pi", &custom()), "pi --mine --approve");
    assert_eq!(posix("cursor", &custom()), "agent --mine --trust");
    assert_eq!(
        posix("codex", &custom()),
        r#"codex --mine -c check_for_update_on_startup=false -c 'projects={"/agent-link-test/a b"={trust_level="trusted"}}'"#
    );
}

/// codex's plan depends on the shell (no pre-trust under cmd.exe), so it is bound to it.
#[test]
fn a_codex_plan_is_typed_only_into_its_own_shell() {
    let p = plan(
        "codex",
        &Permissions::Unattended,
        PreTrust::FlagsOnly,
        ShellKind::Posix,
    );
    assert!(p.command_line(ShellKind::Posix).is_ok());
    let err = p.command_line(ShellKind::Cmd).unwrap_err();
    assert!(err.to_string().contains("built for Posix"), "{err}");
    // Shell-independent plans stay usable with either.
    let pi = plan(
        "pi",
        &Permissions::Unattended,
        PreTrust::FlagsOnly,
        ShellKind::Posix,
    );
    assert_eq!(pi.command_line(ShellKind::Cmd).unwrap(), "pi --approve");
}

#[test]
fn opencode_gets_its_permission_env_as_a_default() {
    let p = plan(
        "opencode",
        &Permissions::Unattended,
        PreTrust::FlagsOnly,
        ShellKind::Posix,
    );
    assert!(p.args.is_empty() && p.env.is_empty());
    assert_eq!(
        p.env_defaults,
        [kv(
            opencode::OPENCODE_PERMISSION_ENV,
            opencode::OPENCODE_PERMISSION_DEFAULT
        )]
    );
    assert_eq!(opencode::OPENCODE_PERMISSION_ENV, "OPENCODE_PERMISSION");
    assert_eq!(opencode::OPENCODE_PERMISSION_DEFAULT, r#"{"*":"allow"}"#);
}

#[test]
fn every_agent_posture_and_shell_gives_a_typable_line() {
    let postures = [Permissions::Unattended, Permissions::AgentDefault, custom()];
    for id in IDS {
        let program = if id == "cursor" { "agent" } else { id };
        for perms in &postures {
            for pre_trust in [PreTrust::FlagsOnly, PreTrust::Off] {
                for shell in [ShellKind::Posix, ShellKind::Cmd] {
                    let text = line(id, perms, pre_trust, shell);
                    assert!(
                        text == program || text.starts_with(&format!("{program} ")),
                        "{id} {perms:?} {pre_trust:?} {shell:?}: {text}"
                    );
                }
            }
        }
    }
}

fn check_model_launch(id: &str, choice: &ModelChoice, expected_words: &[&str]) {
    let profile = profile(id);
    for (shell, quoted_model) in [
        (ShellKind::Posix, "'provider/model:tag [1m]'"),
        (ShellKind::Cmd, r#""provider/model:tag [1m]""#),
    ] {
        let mut rendered = expected_words.to_vec();
        rendered[1] = quoted_model;
        let suffix = rendered.join(" ");
        for perms in [Permissions::Unattended, Permissions::AgentDefault, custom()] {
            for pre_trust in [PreTrust::Off, PreTrust::FlagsOnly] {
                let mut plan = profile.launch(&perms, &trust(pre_trust), Path::new(CWD), shell);
                let expected_line = format!("{} {suffix}", plan.command_line(shell).unwrap());
                let args = profile.model_args(choice, &plan.args).unwrap();
                assert_eq!(
                    args, expected_words,
                    "{id} {perms:?} {pre_trust:?} {shell:?}"
                );
                plan.args.extend(args);
                assert_eq!(plan.command_line(shell).unwrap(), expected_line);
            }
        }
    }
}

#[test]
fn model_and_effort_words_follow_every_posture_and_pre_trust_line() {
    const MODEL: &str = "provider/model:tag [1m]";
    let model = ModelChoice::new(MODEL);
    for (id, model_flag, effort_flag, levels) in [
        ("claude", "--model", "--effort", "low medium high xhigh max"),
        (
            "codex",
            "-m",
            "-c",
            "none minimal low medium high xhigh max ultra persistent",
        ),
        (
            "pi",
            "--model",
            "--thinking",
            "off minimal low medium high xhigh max",
        ),
    ] {
        check_model_launch(id, &model, &[model_flag, MODEL]);
        for level in levels.split_whitespace() {
            let value = if id == "codex" {
                format!("model_reasoning_effort={level}")
            } else {
                level.to_owned()
            };
            let choice = model.clone().with_effort(level);
            check_model_launch(id, &choice, &[model_flag, MODEL, effort_flag, &value]);
        }
    }
}

#[test]
fn profiles_refuse_efforts_outside_their_vocabulary_and_typed_toml_values() {
    for (id, level) in [
        ("claude", "off"),
        ("pi", "none"),
        ("codex", "off"),
        ("codex", "true"),
        ("codex", "[1]"),
    ] {
        let choice = ModelChoice::new("example").with_effort(level);
        let error = profile(id).model_args(&choice, &[]).unwrap_err();
        assert!(
            matches!(error, Error::Unsupported(_)),
            "{id} {level} {error}"
        );
    }
}

#[test]
fn deferred_and_custom_profiles_refuse_an_explicit_choice() {
    let custom: Arc<dyn AgentProfile> = Arc::new(Scripted);
    for profile in [profile("opencode"), profile("cursor"), custom] {
        for choice in [
            ModelChoice::new("id"),
            ModelChoice::new("id").with_effort("high"),
        ] {
            let error = profile.model_args(&choice, &[]).unwrap_err();
            assert!(
                matches!(error, Error::Unsupported(_)),
                "{} {error}",
                profile.id()
            );
        }
    }
}

fn mapped_args(id: &str, choice: &ModelChoice, base: &[&str]) -> agent_link::Result<Vec<String>> {
    let base: Vec<String> = base.iter().map(|arg| (*arg).into()).collect();
    profile(id).model_args(choice, &base)
}

#[test]
fn profiles_refuse_conflicts_and_missing_operands_before_the_model_options() {
    let model = ModelChoice::new("selected");
    let both = model.clone().with_effort("high");
    for (id, choice, base) in [
        ("claude", &model, &["--model", "old"][..]),
        ("claude", &model, &["--model=old"]),
        ("claude", &both, &["--effort=low"]),
        ("claude", &model, &["--system-prompt"]),
        ("pi", &model, &["--model", "old"]),
        ("pi", &both, &["--thinking", "low"]),
        ("pi", &model, &["--system-prompt"]),
        ("codex", &model, &["-m", "old"]),
        ("codex", &model, &["-mold"]),
        ("codex", &model, &["-m=old"]),
        ("codex", &model, &["--model", "old"]),
        ("codex", &model, &["--model=old"]),
        ("codex", &model, &["-c", "model=old"]),
        ("codex", &model, &["--config", " model = 'old'"]),
        ("codex", &model, &["--config=model=old"]),
        ("codex", &model, &["-cmodel=old"]),
        ("codex", &both, &["-c=model_reasoning_effort=low"]),
        ("codex", &both, &["--config", "model_reasoning_effort=low"]),
        ("codex", &model, &["--config"]),
    ] {
        let error = mapped_args(id, choice, base).unwrap_err();
        assert!(
            matches!(error, Error::InvalidInput(_)),
            "{id} {base:?} {error}"
        );
    }
}

#[test]
fn pi_equals_form_extension_flags_allow_explicit_model_and_effort() {
    let choice = ModelChoice::new("selected").with_effort("high");
    for flag in ["--model=extension-value", "--thinking=extension-value"] {
        assert_eq!(
            mapped_args("pi", &choice, &[flag]).unwrap(),
            ["--model", "selected", "--thinking", "high"]
        );
    }
}

#[test]
fn model_without_separate_effort_allows_base_effort_flags_and_option_names_as_data() {
    let model = ModelChoice::new("selected");
    for (id, model_flag, base) in [
        (
            "claude",
            "--model",
            &["--effort", "low", "--system-prompt", "--model"][..],
        ),
        (
            "pi",
            "--model",
            &["--thinking", "low", "--system-prompt", "--model"],
        ),
        ("pi", "--model", &["--models", "provider/*"]),
        (
            "codex",
            "-m",
            &["-c", "model_reasoning_effort=low", "model=old"],
        ),
        ("codex", "-m", &["--config=other=model=old"]),
    ] {
        assert_eq!(
            mapped_args(id, &model, base).unwrap(),
            [model_flag, "selected"],
            "{id} {base:?}"
        );
    }
    assert_eq!(
        mapped_args("pi", &ModelChoice::new("provider/model:high"), &[]).unwrap(),
        ["--model", "provider/model:high"]
    );
}

#[test]
fn a_model_with_shell_expansion_characters_is_posix_only() {
    let profile = profile("claude");
    let choice = ModelChoice::new("%NAME%!");
    let mut plan = plan(
        "claude",
        &Permissions::AgentDefault,
        PreTrust::Off,
        ShellKind::Posix,
    );
    plan.args
        .extend(profile.model_args(&choice, &plan.args).unwrap());
    assert_eq!(
        plan.command_line(ShellKind::Posix).unwrap(),
        "claude --model '%NAME%!'"
    );
    assert!(matches!(
        plan.command_line(ShellKind::Cmd),
        Err(Error::InvalidInput(_))
    ));
}

fn codex_trust_arg(cwd: &str) -> String {
    let p = profile("codex").launch(
        &Permissions::AgentDefault,
        &TrustPolicy::unattended(),
        Path::new(cwd),
        ShellKind::Posix,
    );
    assert_eq!(p.args[..2], ["-c", "check_for_update_on_startup=false"]);
    assert_eq!(p.args.len(), 4, "{p:?}");
    assert_eq!(p.args[2], "-c");
    p.args[3].clone()
}

#[test]
fn the_codex_pre_trust_escapes_the_cwd_for_toml_and_the_shell() {
    let quoted = |cwd: &str| ShellKind::Posix.quote(&codex_trust_arg(cwd)).unwrap();
    assert_eq!(
        quoted("/agent-link-test/it's"),
        r#"'projects={"/agent-link-test/it'\''s"={trust_level="trusted"}}'"#
    );
    assert_eq!(
        codex_trust_arg(r#"/agent-link-test/a"b\c"#),
        r#"projects={"/agent-link-test/a\"b\\c"={trust_level="trusted"}}"#
    );
    assert_eq!(
        quoted(r#"/agent-link-test/a"b\c"#),
        r#"'projects={"/agent-link-test/a\"b\\c"={trust_level="trusted"}}'"#
    );
    // Dots stay inside the TOML key (a dotted `-c` path would split on them).
    assert_eq!(
        codex_trust_arg("/agent-link-test/apt.x"),
        r#"projects={"/agent-link-test/apt.x"={trust_level="trusted"}}"#
    );
    // A control character is escaped for TOML, so the line stays typable.
    assert_eq!(
        codex_trust_arg("/agent-link-test/a\nb"),
        r#"projects={"/agent-link-test/a\u000Ab"={trust_level="trusted"}}"#
    );
}

#[cfg(unix)]
#[test]
fn the_codex_pre_trust_names_the_resolved_directory() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real dir");
    std::fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let resolved = std::fs::canonicalize(&real).unwrap();
    let want = format!(
        r#"projects={{"{}"={{trust_level="trusted"}}}}"#,
        resolved.to_str().unwrap()
    );
    assert_eq!(codex_trust_arg(link.to_str().unwrap()), want);
}

#[test]
fn the_claude_bypass_flag_goes_with_accepting_its_warning() {
    let launch = |p: &ClaudeProfile, perms: &Permissions| {
        p.launch(
            perms,
            &TrustPolicy::default(),
            Path::new(CWD),
            ShellKind::Posix,
        )
        .command_line(ShellKind::Posix)
        .unwrap()
    };
    for accept in [true, false] {
        let p = ClaudeProfile::accepting_bypass_warning(accept);
        let unattended = launch(&p, &Permissions::Unattended);
        assert_eq!(
            unattended.contains("--permission-mode"),
            p.accepts_bypass_warning(),
            "{unattended}"
        );
        assert_eq!(launch(&p, &Permissions::AgentDefault), "claude");
    }
    assert_eq!(
        launch(
            &ClaudeProfile::accepting_bypass_warning(false),
            &Permissions::Unattended
        ),
        "claude"
    );
    let built_in = launch(&ClaudeProfile::new(), &Permissions::Unattended);
    assert_eq!(
        built_in.contains("--permission-mode"),
        claude::BYPASS_WARNING_VERIFIED
    );
    assert_eq!(
        ClaudeProfile::new().accepts_bypass_warning(),
        claude::BYPASS_WARNING_VERIFIED
    );
}

/// The stdio launch as one POSIX line (the `rpc` module runs it as `exec <line>`).
fn rpc(id: &str, resume: bool, probe: bool) -> Option<String> {
    let launch = profile(id).rpc_launch(resume, probe)?;
    let mut line = ShellKind::Posix.quote(&launch.launcher).unwrap();
    for arg in &launch.args {
        line.push(' ');
        line.push_str(&ShellKind::Posix.quote(arg).unwrap());
    }
    Some(line)
}

#[test]
fn rpc_launches_default_to_unattended_and_preserve_resume_and_probe_flags() {
    const CLAUDE: &str = "claude --print --input-format stream-json --output-format stream-json \
         --verbose --include-partial-messages --replay-user-messages --permission-mode \
         bypassPermissions";
    assert_eq!(rpc("pi", false, false).as_deref(), Some("pi --mode rpc"));
    assert_eq!(
        rpc("pi", true, false).as_deref(),
        Some("pi --mode rpc --continue")
    );
    assert_eq!(
        rpc("pi", false, true).as_deref(),
        Some("pi --mode rpc --no-session")
    );
    assert_eq!(
        rpc("pi", true, true).as_deref(),
        Some("pi --mode rpc --continue")
    );
    assert_eq!(rpc("claude", false, false).as_deref(), Some(CLAUDE));
    assert_eq!(rpc("claude", false, true).as_deref(), Some(CLAUDE));
    assert_eq!(
        rpc("claude", true, false),
        Some(format!("{CLAUDE} --continue"))
    );
    for (resume, probe) in [(false, false), (true, false), (false, true)] {
        assert_eq!(
            rpc("codex", resume, probe).as_deref(),
            Some(
                r#"codex app-server -c 'approval_policy="never"' -c 'sandbox_mode="danger-full-access"'"#
            )
        );
        assert_eq!(
            rpc("opencode", resume, probe).as_deref(),
            Some("opencode acp")
        );
        assert_eq!(rpc("cursor", resume, probe), None);
    }
    // Only opencode carries an env default.
    for id in ["claude", "pi", "codex"] {
        let launch = profile(id).rpc_launch(false, false).unwrap();
        assert!(
            launch.env.is_empty() && launch.env_defaults.is_empty(),
            "{id}"
        );
    }
    let opencode = profile("opencode").rpc_launch(false, false).unwrap();
    assert_eq!(
        opencode.env_defaults,
        [kv("OPENCODE_PERMISSION", r#"{"*":"allow"}"#)]
    );
    assert_eq!(claude::CLAUDE_RPC_FLAGS.len(), 8);
}

#[test]
fn rpc_native_and_custom_policies_replace_all_unattended_overrides() {
    use agent_link::agent::profiles::{codex, pi};

    for (id, transport) in [
        ("claude", claude::CLAUDE_RPC_FLAGS),
        ("codex", codex::CODEX_RPC_FLAGS),
        ("opencode", opencode::OPENCODE_RPC_FLAGS),
        ("pi", pi::PI_RPC_FLAGS),
    ] {
        let p = profile(id);
        for resume in [false, true] {
            for probe in [false, true] {
                let native = p
                    .rpc_launch_with_options(
                        &RpcLaunchOptions::default()
                            .with_resume(resume)
                            .with_probe(probe)
                            .with_permissions(Permissions::AgentDefault),
                    )
                    .unwrap();
                let mut expected: Vec<String> = transport.iter().map(|arg| (*arg).into()).collect();
                let suffix = match (id, resume, probe) {
                    ("claude" | "pi", true, _) => Some("--continue"),
                    ("pi", false, true) => Some("--no-session"),
                    _ => None,
                };
                expected.extend(suffix.map(String::from));
                assert_eq!(
                    native.args, expected,
                    "{id}, resume={resume}, probe={probe}"
                );
                assert!(
                    native.env.is_empty() && native.env_defaults.is_empty(),
                    "{id}"
                );

                let custom = p
                    .rpc_launch_with_options(
                        &RpcLaunchOptions::default()
                            .with_resume(resume)
                            .with_probe(probe)
                            .with_permissions(custom()),
                    )
                    .unwrap();
                expected.insert(transport.len(), "--mine".into());
                assert_eq!(custom.args, expected, "{id}");
                assert_eq!(custom.env, [kv("MINE", "1")], "{id}");
                assert!(custom.env_defaults.is_empty(), "{id}");
            }
        }
        assert_eq!(
            p.rpc_launch_with_options(&RpcLaunchOptions::default())
                .unwrap(),
            p.rpc_launch(false, false).unwrap(),
            "{id}: convenience method must use default permissions"
        );
    }
}

#[test]
fn opencode_narrow_compatibility_workaround_requires_explicit_config() {
    let permissions = Permissions::Custom {
        args: Vec::new(),
        env: vec![kv(
            opencode::OPENCODE_PERMISSION_ENV,
            opencode::OPENCODE_EXTERNAL_DIRECTORY_WORKAROUND,
        )],
    };
    let pty = plan("opencode", &permissions, PreTrust::Off, ShellKind::Posix);
    let rpc = profile("opencode")
        .rpc_launch_with_options(&RpcLaunchOptions::default().with_permissions(permissions))
        .unwrap();
    let expected = [kv(
        "OPENCODE_PERMISSION",
        r#"{"external_directory":"allow"}"#,
    )];
    assert_eq!(pty.env, expected);
    assert_eq!(rpc.env, expected);
    assert!(pty.env_defaults.is_empty() && rpc.env_defaults.is_empty());
    assert!(pty.args.is_empty());
    assert_eq!(rpc.args, ["acp"]);
}

#[test]
fn disabling_claude_pty_warning_acceptance_does_not_change_headless_policy() {
    let p = ClaudeProfile::accepting_bypass_warning(false);
    let default = p.rpc_launch(false, false).unwrap();
    assert_eq!(
        &default.args[claude::CLAUDE_RPC_FLAGS.len()..],
        ["--permission-mode", "bypassPermissions"]
    );
    let native = p
        .rpc_launch_with_options(
            &RpcLaunchOptions::default().with_permissions(Permissions::AgentDefault),
        )
        .unwrap();
    assert_eq!(native.args, claude::CLAUDE_RPC_FLAGS);
}

#[test]
fn unsupported_rpc_modes_and_legacy_policy_selection_refuse_without_fallback() {
    struct LegacyRpc;
    impl AgentProfile for LegacyRpc {
        fn id(&self) -> &str {
            "legacy"
        }
        fn dialog(&self, _: &str) -> Option<Dialog> {
            None
        }
        fn input_ready(&self, _: &str) -> bool {
            false
        }
        fn rpc_launch(&self, _: bool, _: bool) -> Option<RpcLaunch> {
            Some(RpcLaunch::new("legacy", &["--bypass"]))
        }
    }
    // Existing host overrides remain callable, but explicit policy must never silently
    // reuse their hardcoded permission settings.
    assert_eq!(
        LegacyRpc.rpc_launch(false, false).unwrap().args,
        ["--bypass"]
    );
    for permissions in [Permissions::default(), Permissions::AgentDefault, custom()] {
        let options = RpcLaunchOptions::default().with_permissions(permissions);
        for p in [
            profile("cursor"),
            Arc::new(LegacyRpc) as Arc<dyn AgentProfile>,
        ] {
            let error = p.rpc_launch_with_options(&options).unwrap_err();
            assert!(
                matches!(error, agent_link::Error::Unsupported(_)),
                "{error}"
            );
            assert!(error.to_string().contains(p.id()), "{error}");
        }
    }
}

#[test]
fn login_checks_exist_only_for_verified_agents() {
    // Every row was verified signed in, signed out and against an unsupported subcommand;
    // opencode has none (`auth list` is a listing, not a check), pi and cursor need none.
    let check = |id: &str| profile(id).login_check();
    let codex = check("codex").expect("codex has a login check");
    assert_eq!(codex.args, ["login", "status"]);
    assert_eq!(codex.signed_out_marker, "Not logged in");
    let claude = check("claude").expect("claude has a login check");
    assert_eq!(claude.args, ["auth", "status"]);
    assert_eq!(claude.signed_out_marker, "\"loggedIn\": false");
    for id in ["opencode", "pi", "cursor"] {
        assert!(check(id).is_none(), "{id}");
    }
    for id in IDS {
        if let Some(c) = check(id) {
            assert!(!c.signed_out_marker.trim().is_empty(), "{id}");
        }
    }
}

#[test]
fn launchers_reset_commands_and_signals() {
    let launchers: Vec<&[&str]> = IDS.iter().map(|id| profile(id).launchers()).collect();
    assert_eq!(
        launchers,
        [
            &["claude"][..],
            &["opencode"],
            &["pi"],
            &["codex"],
            &["cursor-agent", "agent"]
        ]
    );
    let resets: Vec<Option<&str>> = IDS.iter().map(|id| profile(id).reset_command()).collect();
    assert_eq!(
        resets,
        [
            Some("/clear"),
            Some("/new"),
            Some("/new"),
            Some("/clear"),
            Some("/new")
        ]
    );
    let hints = |title_spinner, sync_frames| SignalHints {
        title_spinner,
        osc_progress: Unlock::Unknown,
        sync_frames,
    };
    let signals: Vec<SignalHints> = IDS.iter().map(|id| profile(id).signals()).collect();
    assert_eq!(
        signals,
        [
            hints(false, Unlock::WithIdentity(KnownTerminal::VsCode)),
            hints(false, Unlock::WhenQueryAnswered),
            hints(false, Unlock::Always),
            hints(true, Unlock::Always),
            hints(false, Unlock::Never),
        ]
    );
}

/// A host profile overriding only the screen checks.
struct Scripted;

impl AgentProfile for Scripted {
    fn id(&self) -> &str {
        "claude"
    }
    fn dialog(&self, _: &str) -> Option<Dialog> {
        None
    }
    fn input_ready(&self, screen: &str) -> bool {
        screen.contains("scripted ready")
    }
}

#[test]
fn the_registry_holds_the_builtins_and_takes_overrides() {
    let mut registry = ProfileRegistry::builtin();
    assert_eq!(registry.ids(), IDS);
    assert!(registry.get("gpt").is_none());
    assert!(registry.get("").is_none());

    let replaced = registry.register(Arc::new(Scripted));
    assert!(replaced.is_some_and(|p| p.launchers() == ["claude"]));
    assert_eq!(registry.ids(), IDS, "an override keeps its position");
    let claude = registry.get("claude").unwrap();
    assert!(claude.input_ready("scripted ready"));
    // Defaults: launches its id, no data.
    let p = claude.launch(
        &Permissions::Unattended,
        &TrustPolicy::default(),
        Path::new(CWD),
        ShellKind::Posix,
    );
    assert_eq!(p.command_line(ShellKind::Posix).unwrap(), "claude");
    assert!(claude.rpc_launch(true, false).is_none() && claude.login_check().is_none());

    let mut empty = ProfileRegistry::new();
    assert!(empty.register(Arc::new(Scripted)).is_none());
    assert_eq!(empty.ids(), ["claude"]);
}
