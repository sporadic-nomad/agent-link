//! Consumer-owned tools attached to real offline children, without vendor configuration.
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_link::agent::{
    AgentProfile, AgentSpec, AgentTerminal, Dialog, LaunchPlan, ModelChoice, Permissions,
    RpcLaunch, ShellKind, TrustPolicy,
};
use agent_link::env::EnvPolicy;
use agent_link::rpc::{RpcChild, RpcEvent, RpcSpawnSpec};
use agent_link::tools::{AgentTools, CliTool, McpBackend, McpServer, ToolTransport};
use agent_link::{Error, PtySize, SpawnSpec};
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

#[derive(Default)]
struct ToolProfile {
    launches: AtomicUsize,
    mappings: AtomicUsize,
    backend_calls: AtomicUsize,
    extra_model_arg: Option<&'static str>,
}

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
        self.backend_calls.fetch_add(1, Ordering::Relaxed);
        Some(McpBackend::Claude)
    }
    fn launch(
        &self,
        permissions: &Permissions,
        _: &TrustPolicy,
        _: &Path,
        _: ShellKind,
    ) -> LaunchPlan {
        self.launches.fetch_add(1, Ordering::Relaxed);
        let mut plan = LaunchPlan::new(common::fake_agent_path().to_str().unwrap());
        plan.args.push("--tool-launch-peer".into());
        plan.with_permissions(permissions, &["--synthetic-permission-flag"], &[])
    }
    fn model_args(
        &self,
        choice: &ModelChoice,
        base_args: &[String],
    ) -> agent_link::Result<Vec<String>> {
        self.mappings.fetch_add(1, Ordering::Relaxed);
        assert_eq!(
            base_args,
            ["--tool-launch-peer", "--synthetic-permission-flag"]
        );
        let mut args = vec!["--model".into(), choice.model.clone()];
        if let Some(effort) = &choice.effort {
            args.extend(["--effort".into(), effort.clone()]);
        }
        if let Some(extra) = self.extra_model_arg {
            args.push(extra.into());
        }
        Ok(args)
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
        let mut agent = AgentSpec::new(Arc::new(ToolProfile::default()));
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

fn read_pty_report(cwd: &Path) -> Value {
    let path = cwd.join("tool-report.json");
    common::wait_until("PTY tool peer report", || path.exists());
    let report = std::fs::read(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    serde_json::from_slice(&report).unwrap()
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
    spec.env.set("AGENT_LINK_TEST_TOOL_REPORT", "1");
    let mut agent = AgentSpec::new(Arc::new(ToolProfile::default()));
    agent.trace = Some(false);
    let (terminal, _events) = {
        let _guard = common::spawn_lock();
        AgentTerminal::spawn_with_tools(spec, agent, &tools()).unwrap()
    };
    let report = read_pty_report(home.path());
    check_report(&report);
    assert!(
        !report["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "--synthetic-permission-flag")
    );
    let path = PathBuf::from(report["config_path"].as_str().unwrap());
    let inspect = if cfg!(windows) {
        b"inspect\r"
    } else {
        b"inspect\n"
    };
    terminal.terminal().write(inspect).unwrap();
    check_report(&read_pty_report(home.path()));
    assert!(path.exists());
    drop(terminal);
    assert!(!path.parent().unwrap().exists());
}

#[cfg(unix)]
#[test]
fn shell_pty_applies_tools_to_the_normal_profile_launch() {
    let home = tempfile::tempdir().unwrap();
    for model in [
        None,
        Some(ModelChoice::new("tool peer [1m]").with_effort("high")),
    ] {
        let mut spec = common::sh_spec(home.path());
        spec.env.set("AGENT_LINK_TEST_TOOL_REPORT", "1");
        let selected = model.is_some();
        let profile = Arc::new(ToolProfile::default());
        let agent = AgentSpec {
            model,
            trace: Some(false),
            ..AgentSpec::new(profile.clone())
        };
        let (terminal, _events) = {
            let _guard = common::spawn_lock();
            AgentTerminal::spawn_with_tools(spec, agent, &tools()).unwrap()
        };
        terminal.launch().unwrap();
        let report = read_pty_report(home.path());
        check_report(&report);
        let program = common::fake_agent_path();
        let mut expected = vec![
            program.to_str().unwrap(),
            "--tool-launch-peer",
            "--synthetic-permission-flag",
        ];
        if selected {
            expected.extend(["--model", "tool peer [1m]", "--effort", "high"]);
        }
        let path = Path::new(report["config_path"].as_str().unwrap());
        expected.extend(["--mcp-config", path.to_str().unwrap()]);
        assert_eq!(report["args"], serde_json::json!(expected));
        assert_eq!(profile.launches.load(Ordering::Relaxed), 1);
        assert_eq!(
            profile.mappings.load(Ordering::Relaxed),
            usize::from(selected)
        );
        terminal.terminal().write(b"inspect\n").unwrap();
        check_report(&read_pty_report(home.path()));
        assert!(path.exists());
        drop(terminal);
        assert!(!path.parent().unwrap().exists());
    }
}

fn tool_model_error(spec: SpawnSpec, profile: &Arc<ToolProfile>, model: &str) -> Error {
    let agent = AgentSpec {
        model: Some(ModelChoice::new(model)),
        ..AgentSpec::new(profile.clone())
    };
    let _guard = common::spawn_lock();
    AgentTerminal::spawn_with_tools(spec, agent, &tools()).unwrap_err()
}

#[test]
fn invalid_model_requests_and_input_limits_precede_tool_preparation() {
    let home = tempfile::tempdir().unwrap();
    let spec = SpawnSpec::shell(home.path(), PtySize { rows: 24, cols: 80 });
    let profile = Arc::new(ToolProfile::default());
    assert!(matches!(
        tool_model_error(spec.clone(), &profile, "bad\nmodel"),
        Error::InvalidInput(_)
    ));
    let direct = SpawnSpec::direct(
        common::fake_agent_path(),
        ["--tool-launch-peer"],
        home.path(),
        spec.size,
    );
    assert!(matches!(
        tool_model_error(direct, &profile, "chosen"),
        Error::InvalidInput(_)
    ));
    let mut invalid = spec.clone();
    invalid.input_limits.max_pending_chunks = 0;
    assert!(matches!(
        tool_model_error(invalid, &profile, "chosen"),
        Error::InvalidInput(_)
    ));
    assert_eq!(profile.backend_calls.load(Ordering::Relaxed), 0);

    let unquotable = Arc::new(ToolProfile {
        extra_model_arg: Some("bad\nword"),
        ..ToolProfile::default()
    });
    assert!(matches!(
        tool_model_error(spec, &unquotable, "chosen"),
        Error::InvalidInput(_)
    ));
    assert_eq!(unquotable.backend_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn selected_launch_checks_write_size_before_and_after_tools_extend_the_argv() {
    let home = tempfile::tempdir().unwrap();
    let mut spec = SpawnSpec::shell(home.path(), PtySize { rows: 24, cols: 80 });
    let profile = Arc::new(ToolProfile::default());
    let shell = ShellKind::native();
    let program = shell
        .quote(common::fake_agent_path().to_str().unwrap())
        .unwrap();
    let model = shell.quote("two words").unwrap();
    let before_tools = format!(
        "{}{program} --tool-launch-peer --synthetic-permission-flag --model {model}{}",
        shell.clear_line().unwrap_or_default(),
        shell.line_terminator(),
    )
    .len();
    spec.input_limits.max_write_bytes = before_tools - 1;
    let error = tool_model_error(spec.clone(), &profile, "two words");
    assert!(
        matches!(
            error,
            Error::InputTooLarge { size, limit } if size == before_tools && limit == before_tools - 1
        ),
        "{error}"
    );
    assert_eq!(profile.backend_calls.load(Ordering::Relaxed), 0);
    spec.input_limits.max_write_bytes = before_tools;
    let error = tool_model_error(spec, &profile, "two words");
    assert!(
        matches!(
            error,
            Error::InputTooLarge { size, limit } if size > before_tools && limit == before_tools
        ),
        "{error}"
    );
    assert_eq!(profile.backend_calls.load(Ordering::Relaxed), 1);
}

#[test]
fn tool_conflict_checks_see_the_model_hooks_arguments() {
    let home = tempfile::tempdir().unwrap();
    let spec = SpawnSpec::shell(home.path(), PtySize { rows: 24, cols: 80 });
    let profile = Arc::new(ToolProfile {
        extra_model_arg: Some("--mcp-config=from-profile"),
        ..ToolProfile::default()
    });
    let error = tool_model_error(spec, &profile, "chosen");
    assert!(
        matches!(
            &error,
            Error::InvalidInput(message) if message.contains("already configures Claude MCP")
        ),
        "{error}"
    );
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
