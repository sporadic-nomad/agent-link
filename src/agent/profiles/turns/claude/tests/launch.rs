use std::fs;
use std::path::Path;

use serde_json::Value;

use super::super::source::capture;
use super::CAPTURE;
use crate::agent::LaunchPlan;
use crate::agent::turns::MAX_WORKERS;
use crate::agent::turns::tests::peer::project;
use crate::{PtySize, SpawnSpec};

pub(super) const SIZE: PtySize = PtySize {
    rows: 24,
    cols: 100,
};

fn spec(project: &Path, args: &[&str]) -> SpawnSpec {
    let cli = project.join("claude");
    fs::write(&cli, "").unwrap();
    let mut spec = SpawnSpec::direct(cli, args.iter().copied(), project, SIZE);
    spec.env.inherit = false;
    spec
}

#[test]
fn capture_uses_actual_argv_merged_environment_and_exclusive_new_uuid() {
    let (_lock, scratch) = project();
    let cwd = scratch.path().canonicalize().unwrap();
    fs::create_dir(cwd.join("config")).unwrap();
    let home = cwd.join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    let data: Value = serde_json::from_str(CAPTURE).unwrap();
    let words = data["twoTerminals"]["launches"][0]["argv"]
        .as_array()
        .unwrap();
    let args: Vec<_> = words[1..].iter().map(|v| v.as_str().unwrap()).collect();
    let id = args[1];
    let mut spec = spec(&cwd, &args);
    let mut plan = LaunchPlan::new("other");
    plan.args = vec!["--continue".into()]; // ignored for host-owned direct argv
    spec.env
        .set("HOME", &home)
        .set("CLAUDE_CONFIG_DIR", "wrong");
    plan.env.push(("CLAUDE_CONFIG_DIR".into(), "config".into()));
    plan.merge_env_into(&mut spec.env);
    let claim = capture(&mut spec, &plan).unwrap();
    assert_eq!(claim.initial.as_str(), id);
    assert_eq!(claim.root.path, cwd.join("config"));
    assert!(capture(&mut spec, &plan).is_err());
    drop(claim);
    spec.env.remove("CLAUDE_CONFIG_DIR");
    let fallback = capture(&mut spec, &plan).unwrap();
    assert_eq!(fallback.root.path, home.join(".claude"));
    drop(fallback);
    spec.env.remove("HOME");
    assert!(capture(&mut spec, &plan).is_err());
}

#[test]
fn identity_and_policy_select_the_same_root_with_or_without_inheritance() {
    let (_lock, scratch) = project();
    let cwd = scratch.path().canonicalize().unwrap();
    let home_root = cwd.join(".claude");
    let from_identity = cwd.join("identity");
    fs::create_dir(&home_root).unwrap();
    fs::create_dir(&from_identity).unwrap();
    let plan = LaunchPlan::new("ignored");
    for inherit in [false, true] {
        let mut base = spec(
            &cwd,
            &["--session-id", "00000000-0000-4000-8000-000000000011"],
        );
        base.env.inherit = inherit;
        base.identity = crate::TerminalIdentity::Custom {
            vars: vec![
                ("HOME".into(), cwd.to_str().unwrap().into()),
                ("CLAUDE_CONFIG_DIR".into(), "first".into()),
                ("CLAUDE_CONFIG_DIR".into(), "identity".into()),
            ],
        };
        assert_eq!(
            capture(&mut base.clone(), &plan).unwrap().root.path,
            from_identity
        );
        let mut removed = base.clone();
        removed.env.remove("CLAUDE_CONFIG_DIR");
        assert_eq!(capture(&mut removed, &plan).unwrap().root.path, home_root);
        base.env.set("CLAUDE_CONFIG_DIR", &cwd);
        assert_eq!(capture(&mut base, &plan).unwrap().root.path, cwd);
    }
}

#[cfg(all(feature = "turns", target_os = "macos", target_arch = "aarch64"))]
#[test]
fn retargeting_the_lease_ancestor_after_capture_refuses_final_admission() {
    use std::os::unix::fs::symlink;

    use super::super::source::capture_exclusive;
    use crate::agent::turns::ExclusiveClaudeRoot;

    let (_lock, scratch) = project();
    let cwd = scratch.path();
    let home = cwd.join("home");
    let alias = cwd.join("alias");
    let other = cwd.join("other");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(other.join(".claude")).unwrap();
    symlink(&home, &alias).unwrap();
    let lease = ExclusiveClaudeRoot::new(alias.join(".claude")).unwrap();
    let mut spec = spec(
        cwd,
        &["--session-id", "00000000-0000-4000-8000-000000000001"],
    );
    spec.env.set("HOME", &alias).remove("CLAUDE_CONFIG_DIR");
    let _claim = capture_exclusive(&mut spec, &LaunchPlan::new("ignored"), &lease).unwrap();

    symlink(&other, cwd.join("new-alias")).unwrap();
    fs::rename(cwd.join("new-alias"), alias).unwrap();
    assert!(matches!(
        lease.admit_spawn(),
        Err(crate::Error::InvalidInput(_))
    ));
    // The prepared environment itself still addresses the leased object, even
    // when the original setting came from HOME and an explicit removal.
    let mut child = portable_pty::CommandBuilder::new("/bin/sh");
    spec.env.apply(&mut child);
    let config = Path::new(child.get_env("CLAUDE_CONFIG_DIR").unwrap());
    fs::write(config.join("marker"), []).unwrap();
    assert!(home.join(".claude/marker").exists());
    assert!(!other.join(".claude/marker").exists());
}

#[test]
fn conflicts_preexisting_files_ambiguous_projects_and_exhausted_claims_refuse_before_spawn() {
    let (_lock, scratch) = project();
    let cwd = scratch.path().canonicalize().unwrap();
    let plan = LaunchPlan::new("ignored");
    let claim = |args: &[&str]| {
        let mut spec = spec(&cwd, args);
        spec.env.set("CLAUDE_CONFIG_DIR", &cwd);
        capture(&mut spec, &plan)
    };
    let id = "00000000-0000-4000-8000-000000000001";
    for extra in [
        vec!["--continue"],
        vec!["-c"],
        vec!["--resume", id],
        vec!["-r"],
        vec!["--fork-session"],
        vec!["--session-id", id],
        vec!["--session-id=other"],
        vec!["--print"],
        vec!["--worktree"],
        vec!["--settings", r#"{"env":{"CLAUDE_CONFIG_DIR":"elsewhere"}}"#],
    ] {
        let mut args = vec!["--session-id", id];
        args.extend(extra);
        assert!(claim(&args).is_err());
    }
    for args in [
        vec![],
        vec!["--session-id"],
        vec!["--session-id", "../path-sentinel"],
        vec!["--model", "--session-id", id],
    ] {
        let err = claim(&args).err().unwrap();
        assert!(!format!("{err:?} {err}").contains("sentinel"));
    }
    let mut claims = Vec::new();
    for i in 0..MAX_WORKERS {
        let fresh = format!("00000000-0000-4000-8000-{:012x}", 10 + i);
        claims.push(claim(&["--session-id", &fresh]).unwrap());
    }
    assert!(claim(&["--session-id", id]).is_err());
    drop(claims);

    let slug: String = cwd
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let transcripts = cwd.join("projects").join(slug);
    fs::create_dir_all(&transcripts).unwrap();
    fs::write(transcripts.join(format!("{id}.jsonl")), "").unwrap();
    assert!(claim(&["--session-id", id]).is_err());

    let long = cwd.join("x".repeat(196));
    fs::create_dir(&long).unwrap();
    let mut spec = spec(&long, &["--session-id", id]);
    spec.env.set("CLAUDE_CONFIG_DIR", &cwd);
    assert!(capture(&mut spec, &plan).is_err());
}

#[cfg(not(target_os = "macos"))]
#[test]
fn unsupported_process_identity_reports_a_gap_on_a_live_terminal() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use super::super::source::ClaudeSource;
    use crate::agent::profiles::ClaudeProfile;
    use crate::agent::turns::tests::peer::gap;
    use crate::agent::turns::{Gap, Options};
    use crate::agent::{AgentSpec, AgentTerminal};

    let (_lock, scratch) = project();
    let cwd = scratch.path();
    let mut spec = spec(
        cwd,
        &["--session-id", "00000000-0000-4000-8000-000000000001"],
    );
    spec.env.set("CLAUDE_CONFIG_DIR", cwd);
    let program = cwd.join("claude");
    fs::write(&program, "#!/bin/sh\nprintf '> '\nread -r line\n").unwrap();
    fs::set_permissions(program, fs::Permissions::from_mode(0o700)).unwrap();
    let agent = AgentSpec {
        turn_launch: Some(Arc::new(capture)),
        ..AgentSpec::new(Arc::new(ClaudeProfile::default()))
    };
    let (term, _events) = AgentTerminal::spawn(spec, agent).unwrap();
    gap(
        &ClaudeSource::attach(&term, Options::default()).unwrap(),
        Gap::Uncorroborated,
    );
}
