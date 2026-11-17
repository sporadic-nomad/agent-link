//! Consumer-owned tools attached to real offline children, without vendor configuration.
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_link::agent::{
    AgentProfile, AgentSpec, AgentTerminal, Dialog, LaunchPlan, Permissions, RpcLaunch, ShellKind,
    TrustPolicy,
};
use agent_link::env::EnvPolicy;
use agent_link::rpc::{RpcChild, RpcEvent, RpcSpawnSpec};
use agent_link::tools::{AgentTools, CliTool, McpBackend, McpServer, ToolTransport};
use agent_link::{Error, Event, PtySize, SpawnSpec};
use serde_json::Value;

fn tools() -> AgentTools {
    AgentTools::new()
        .with_cli(CliTool::new(
            common::fake_agent_path(),
            "Use `context` for application state.",
        ))
        .with_env("AGENT_LINK_TEST_TOOL_ENDPOINT", "synthetic-endpoint")
        .with_mcp(
            McpServer::http("app", "http://127.0.0.1:9123/mcp")
                .header("Authorization", "synthetic-token"),
        )
}

fn isolated(home: &Path) -> EnvPolicy {
    let mut env = EnvPolicy {
        inherit: false,
        ..EnvPolicy::default()
    };
    env.set("HOME", home)
        .set("USERPROFILE", home)
        .set("PATH", std::env::var_os("PATH").unwrap_or_default());
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        env.set("SystemRoot", root);
    }
    env
}

fn launch() -> RpcLaunch {
    tools()
        .configure_rpc(
            McpBackend::Claude,
            RpcLaunch::new(
                common::fake_agent_path().to_str().unwrap(),
                &["--tool-launch-peer"],
            ),
        )
        .unwrap()
}

fn config_path(launch: &RpcLaunch) -> PathBuf {
    let i = launch
        .args
        .iter()
        .position(|a| a == "--mcp-config")
        .unwrap();
    launch.args[i + 1].clone().into()
}

fn read_rpc(events: &crossbeam_channel::Receiver<RpcEvent>) -> Value {
    let deadline = common::deadline();
    match events.recv_deadline(deadline).expect("peer launch report") {
        RpcEvent::Line { line, .. } => serde_json::from_str(&line).unwrap(),
        event => panic!("unexpected peer event: {event:?}"),
    }
}

fn check_report(value: &Value) {
    assert_eq!(value["tool_peer"], true);
    assert_eq!(value["endpoint"], "synthetic-endpoint");
    assert_eq!(
        value["server"]["headers"]["Authorization"],
        "synthetic-token"
    );
    assert_eq!(
        Path::new(value["first_path"].as_str().unwrap()),
        common::fake_agent_path().parent().unwrap()
    );
}

#[test]
fn rpc_child_retains_configuration_and_does_not_edit_host_environment_or_home() {
    let home = tempfile::tempdir().unwrap();
    let original_path = std::env::var_os("PATH");
    let original_endpoint = std::env::var_os("AGENT_LINK_TEST_TOOL_ENDPOINT");
    let launch = launch();
    let path = config_path(&launch);
    let mut spec = RpcSpawnSpec::new(launch, home.path());
    spec.shell = Some("/bin/sh".into());
    spec.env = isolated(home.path());
    let (child, events) = RpcChild::spawn(spec).unwrap();
    check_report(&read_rpc(&events));
    child.send_line("inspect").unwrap();
    check_report(&read_rpc(&events));
    assert!(path.exists());
    drop(child);
    assert!(!path.parent().unwrap().exists());
    assert_eq!(std::env::var_os("PATH"), original_path);
    assert_eq!(
        std::env::var_os("AGENT_LINK_TEST_TOOL_ENDPOINT"),
        original_endpoint
    );
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

#[test]
fn refused_rpc_spawn_releases_temporary_configuration() {
    let home = tempfile::tempdir().unwrap();
    let launch = launch();
    let path = config_path(&launch);
    let spec = RpcSpawnSpec::new(launch, home.path().join("missing-directory"));
    assert!(matches!(RpcChild::spawn(spec), Err(Error::InvalidInput(_))));
    assert!(!path.parent().unwrap().exists());
}

struct ToolProfile;
impl AgentProfile for ToolProfile {
    fn id(&self) -> &str {
        "tool-peer"
    }
    fn dialog(&self, _: &str) -> Option<Dialog> {
        None
    }
    fn input_ready(&self, _: &str) -> bool {
        false
    }
    fn mcp_backend(&self, _: ToolTransport) -> Option<McpBackend> {
        Some(McpBackend::Claude)
    }
    fn launch(
        &self,
        permissions: &Permissions,
        _: &TrustPolicy,
        _: &Path,
        _: ShellKind,
    ) -> LaunchPlan {
        let mut plan = LaunchPlan::new(common::fake_agent_path().to_str().unwrap());
        plan.args.push("--tool-launch-peer".into());
        plan.with_permissions(permissions, &["--synthetic-permission-flag"], &[])
    }
}

#[test]
fn malformed_mcp_urls_are_refused_before_pty_spawn() {
    let home = tempfile::tempdir().unwrap();
    for url in [
        "http://:",
        "http://localhost:not-a-port/mcp",
        "http://[gg::1]/mcp",
        "http://::1/mcp",
    ] {
        let tools = AgentTools::new().with_mcp(McpServer::http("app", url));
        let mut spec = SpawnSpec::direct(
            common::fake_agent_path(),
            ["--tool-launch-peer"],
            home.path(),
            PtySize { rows: 24, cols: 80 },
        );
        spec.env = isolated(home.path());
        let mut agent = AgentSpec::new(Arc::new(ToolProfile));
        agent.trace = Some(false);
        let result = {
            let _guard = common::spawn_lock();
            AgentTerminal::spawn_with_tools(spec, agent, &tools)
        };
        match result {
            Err(Error::InvalidInput(message)) => assert!(message.starts_with("MCP URL")),
            Err(other) => panic!("expected a URL validation error, got {other:?}"),
            Ok((terminal, _events)) => {
                drop(terminal);
                panic!("invalid MCP URL {url:?} started a child");
            }
        }
    }
}

fn read_pty(events: &crossbeam_channel::Receiver<Event>) -> Value {
    let deadline = common::deadline();
    let mut output = String::new();
    loop {
        match events
            .recv_deadline(deadline)
            .expect("PTY tool peer report")
        {
            Event::Output { bytes, .. } => {
                output.push_str(&String::from_utf8_lossy(&bytes));
                for line in output.lines() {
                    if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                        if value["tool_peer"] == true {
                            return value;
                        }
                    }
                }
            }
            Event::Exit { .. } => panic!("peer exited before reporting: {output:?}"),
            _ => {}
        }
    }
}

#[test]
fn direct_pty_retains_attachment_without_overwriting_explicit_arguments() {
    let home = tempfile::tempdir().unwrap();
    let mut spec = SpawnSpec::direct(
        common::fake_agent_path(),
        ["--tool-launch-peer"],
        home.path(),
        PtySize {
            rows: 24,
            cols: 120,
        },
    );
    spec.env = isolated(home.path());
    let mut agent = AgentSpec::new(Arc::new(ToolProfile));
    agent.trace = Some(false);
    let (terminal, events) = {
        let _guard = common::spawn_lock();
        AgentTerminal::spawn_with_tools(spec, agent, &tools()).unwrap()
    };
    let report = read_pty(&events);
    check_report(&report);
    assert!(
        !report["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "--synthetic-permission-flag")
    );
    let path = PathBuf::from(report["config_path"].as_str().unwrap());
    terminal.terminal().write(b"inspect\n").unwrap();
    check_report(&read_pty(&events));
    assert!(path.exists());
    drop(terminal);
    assert!(!path.parent().unwrap().exists());
}

#[cfg(unix)]
#[test]
fn shell_pty_applies_tools_to_the_normal_profile_launch() {
    let home = tempfile::tempdir().unwrap();
    let spec = common::sh_spec(home.path());
    let mut agent = AgentSpec::new(Arc::new(ToolProfile));
    agent.trace = Some(false);
    let (terminal, events) = {
        let _guard = common::spawn_lock();
        AgentTerminal::spawn_with_tools(spec, agent, &tools()).unwrap()
    };
    terminal.launch().unwrap();
    let report = read_pty(&events);
    check_report(&report);
    assert!(
        report["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "--synthetic-permission-flag")
    );
    let path = PathBuf::from(report["config_path"].as_str().unwrap());
    drop(terminal);
    assert!(!path.parent().unwrap().exists());
}

#[cfg(feature = "codex")]
#[test]
fn native_adapter_rejects_a_mismatched_mapping_before_spawn() {
    let home = tempfile::tempdir().unwrap();
    let launch = launch();
    let path = config_path(&launch);
    let spec = RpcSpawnSpec::new(launch, home.path());
    assert!(matches!(
        agent_link::codex::CodexAppServer::spawn(spec, agent_link::codex::CodexLimits::default()),
        Err(Error::Unsupported(_))
    ));
    assert!(!path.parent().unwrap().exists());
}
