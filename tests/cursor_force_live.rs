//! Follow-up validation for the WIP Cursor profile's --force launch, trust and composer.
//! Explicitly ignored: requires an installed CLI and CURSOR_API_KEY, which is passed
//! only to the child. It may validate authentication online, but submits no model turn.
#![cfg(all(unix, feature = "agent"))]

mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_link::agent::{AgentSpec, AgentState, ProfileRegistry, ShellKind, TrustPolicy};
use agent_link::{PtySize, SpawnSpec};

fn guarded_command(launcher: &Path, args: &[String]) -> (PathBuf, Vec<OsString>) {
    let args: Vec<OsString> = args.iter().map(Into::into).collect();
    #[cfg(target_os = "macos")]
    {
        let real_home = std::env::var("HOME").expect("real home for the write guard");
        let home = serde_json::to_string(&real_home).unwrap();
        let policy = format!("(version 1)(allow default)(deny file-write* (subpath {home}))");
        let mut guarded = vec!["-p".into(), policy.into(), launcher.as_os_str().to_owned()];
        guarded.extend(args);
        ("/usr/bin/sandbox-exec".into(), guarded)
    }
    #[cfg(not(target_os = "macos"))]
    {
        (launcher.into(), args)
    }
}

#[test]
#[ignore = "live WIP validation: authenticated Cursor CLI, --force composer; no submitted turn"]
fn cursor_force_reaches_a_composer_and_displays_an_unsubmitted_paste() {
    let key = std::env::var_os("CURSOR_API_KEY")
        .filter(|key| !key.is_empty())
        .expect(
            "set CURSOR_API_KEY outside the test; signed-out startup cannot validate a composer",
        );
    let launcher = std::env::var_os("AGENT_LINK_LIVE_CURSOR")
        .map(Into::into)
        .or_else(|| agent_link::resolve::find_on_path(&["cursor-agent", "agent"]))
        .expect("install Cursor CLI or set AGENT_LINK_LIVE_CURSOR to its executable");
    let scratch = tempfile::tempdir().unwrap();
    let workspace = scratch.path().join("workspace");
    let home = scratch.path().join("home");
    let config = home.join("cursor");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    let profile = ProfileRegistry::builtin().get("cursor").unwrap();
    let mut agent = AgentSpec::new(Arc::clone(&profile));
    // Recognized folder-trust dialogs may be answered in this disposable workspace.
    // Keep pre-trust off so the argv being checked contains exactly --force.
    agent.trust = TrustPolicy {
        steer_dialogs: true,
        ..TrustPolicy::default()
    };
    agent.timing.trust_watch = Duration::from_secs(60);
    agent.trace = Some(false);
    let plan = profile.launch(
        &agent.permissions,
        &agent.trust,
        &workspace,
        ShellKind::Posix,
    );
    assert_eq!(plan.args, ["--force"]);
    let (program, args) = guarded_command(&launcher, &plan.args);
    let mut spec = SpawnSpec::direct(
        program,
        args,
        &workspace,
        PtySize {
            rows: 40,
            cols: 120,
        },
    );
    spec.env.inherit = false;
    let path = std::env::var_os("PATH").unwrap_or_default();
    spec.env
        .set("PATH", &path)
        .set("HOME", &home)
        .set("CURSOR_CONFIG_DIR", &config)
        .set("CURSOR_API_KEY", key);
    for name in [
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "TMPDIR",
    ] {
        let directory = scratch.path().join(name);
        std::fs::create_dir(&directory).unwrap();
        spec.env.set(name, directory);
    }
    let (program, args) = guarded_command(&launcher, &["--version".into()]);
    let mut version = Command::new(program);
    version.args(args).current_dir(&workspace);
    let mut version_env = spec.env.clone();
    version_env.remove("CURSOR_API_KEY");
    version_env.apply(&mut version);
    let output = agent_link::proc::output_with_timeout(version, Duration::from_secs(30))
        .expect("Cursor version command")
        .expect("Cursor version deadline");
    assert!(output.status.success());
    println!(
        "Cursor {} --force",
        String::from_utf8_lossy(&output.stdout).trim()
    );

    let (term, events) = common::agent::spawn_agent(spec, agent);
    let ready_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        events
            .recv_deadline(ready_deadline)
            .expect("Cursor --force ready composer before deadline");
        let text = term.terminal().screen().expect("screen model").text();
        if term.state() == AgentState::Ready && profile.input_ready(&text) {
            break;
        }
        assert_ne!(
            term.state(),
            AgentState::Exited,
            "Cursor exited before its ready composer"
        );
    }
    const PASTE: &str = "AGENT_LINK_FORCE_PASTE_PROBE";
    term.terminal()
        .write(format!("\x1b[200~{PASTE}\x1b[201~").as_bytes())
        .unwrap();
    let paste_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        events
            .recv_deadline(paste_deadline)
            .expect("unsubmitted paste displayed before deadline");
        let text = term.terminal().screen().expect("screen model").text();
        if text.contains(PASTE) {
            break;
        }
    }
    println!("Ready composer and unsubmitted paste observed; no Enter sent.");
    drop(events);
    drop(term);
}
