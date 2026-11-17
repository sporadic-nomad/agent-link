use super::*;
use crate::agent::{Dialog, ProfileRegistry};

fn executable() -> PathBuf {
    std::env::current_exe().unwrap()
}

fn raw() -> RpcLaunch {
    RpcLaunch::new("synthetic-agent", &["--rpc"])
}

fn server() -> McpServer {
    McpServer::stdio("my_app", executable()).arg("serve")
}

fn config_path(launch: &RpcLaunch) -> PathBuf {
    let index = launch
        .args
        .iter()
        .position(|a| a == "--mcp-config")
        .unwrap();
    launch.args[index + 1].clone().into()
}

#[test]
fn empty_tools_leave_prompt_unchanged() {
    assert_eq!(AgentTools::new().prompt("hello").unwrap(), "hello");
    let launch = AgentTools::new()
        .configure_rpc(McpBackend::Claude, raw())
        .unwrap();
    assert_eq!(launch.args, raw().args);
    assert!(launch.env.is_empty());
}

#[test]
fn bundled_cli_changes_only_child_path_and_explicit_instructions() {
    let parent_path = std::env::var_os("PATH");
    let key = "AGENT_LINK_TEST_TOOL_ENDPOINT";
    let parent_value = std::env::var_os(key);
    let tools = AgentTools::new()
        .with_cli(CliTool::new(
            executable(),
            "Use `context` to read the current document.",
        ))
        .with_env(key, "private-endpoint");
    let launch = tools.configure_rpc(None, raw()).unwrap();
    let mut env = EnvPolicy::default();
    env.path_prepend.push(executable().parent().unwrap().into());
    env.remove(key);
    launch.merge_env_into(&mut env);
    launch.merge_env_into(&mut env);
    assert_eq!(env.path_prepend, vec![executable().parent().unwrap()]);
    let mut command = std::process::Command::new("unused");
    env.apply(&mut command);
    assert!(command.get_envs().any(|(k, v)| k == key && v.is_none()));
    assert_eq!(std::env::var_os("PATH"), parent_path);
    assert_eq!(std::env::var_os(key), parent_value);
    let prompt = tools.prompt("Explain this document.").unwrap();
    assert!(prompt.contains("current document"));
    assert!(prompt.ends_with("Explain this document."));
    assert!(!prompt.contains("private-endpoint"));
    assert_eq!(launch.args, raw().args);
}

#[test]
fn cli_rejects_directories_that_need_path_escaping() {
    let directory = executable().parent().unwrap().join(if cfg!(windows) {
        "bundle;name"
    } else {
        "bundle:name"
    });
    #[cfg(windows)]
    {
        // Windows accepts semicolons by quoting them, but EnvPolicy uses the raw path.
        let encoded = std::env::join_paths([&directory]).unwrap();
        assert_ne!(encoded, directory.as_os_str());
        assert_eq!(
            std::env::split_paths(&encoded).collect::<Vec<_>>(),
            vec![directory.clone()]
        );
    }
    let tools =
        AgentTools::new().with_cli(CliTool::new(directory.join("cli.exe"), "Read context."));
    assert!(matches!(tools.instructions(), Err(Error::InvalidInput(_))));
    assert!(matches!(
        tools.configure_rpc(None, raw()),
        Err(Error::InvalidInput(_))
    ));
    let profile = ProfileRegistry::builtin().get("pi").unwrap();
    assert!(matches!(
        tools.pty_launch(
            profile.as_ref(),
            &Permissions::default(),
            &TrustPolicy::default(),
            executable().parent().unwrap(),
            ShellKind::native()
        ),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn cli_spaces_and_separators_in_the_filename_preserve_the_child_path() {
    let directory = executable().parent().unwrap().join("bundle with spaces");
    let command = directory.join(if cfg!(windows) {
        "cli;name.exe"
    } else {
        "cli:name"
    });
    let tools = AgentTools::new().with_cli(CliTool::new(command, "Read context."));
    let launch = tools.configure_rpc(None, raw()).unwrap();
    let mut env = EnvPolicy {
        inherit: false,
        ..EnvPolicy::default()
    };
    let existing = executable().parent().unwrap().to_path_buf();
    env.set("PATH", &existing);
    launch.merge_env_into(&mut env);
    assert_eq!(
        std::env::split_paths(&env.effective_path()).collect::<Vec<_>>(),
        vec![directory, existing]
    );
}

#[test]
fn private_claude_config_is_literal_and_owned_until_the_last_launch_drops() {
    let secret = "a synthetic token with \"quotes\" and \\slashes";
    let tools = AgentTools::new()
        .with_mcp(server().arg("a b'\"\\").env("APP_TOKEN", secret))
        .with_mcp(
            McpServer::http("app_http", "http://127.0.0.1:9123/mcp")
                .header("Authorization", secret),
        );
    let launch = tools.configure_rpc(McpBackend::Claude, raw()).unwrap();
    let path = config_path(&launch);
    let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["mcpServers"]["my_app"]["env"]["APP_TOKEN"], secret);
    assert_eq!(
        value["mcpServers"]["my_app"]["args"],
        json!(["serve", "a b'\"\\"])
    );
    assert_eq!(value["mcpServers"]["app_http"]["type"], "http");
    assert_eq!(
        value["mcpServers"]["app_http"]["headers"]["Authorization"],
        secret
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    assert!(!format!("{tools:?} {launch:?}").contains(secret));
    assert!(!launch.args.iter().any(|a| a.contains(secret)));
    let retained = launch.clone();
    let independent = tools.configure_rpc(McpBackend::Claude, raw()).unwrap();
    assert_ne!(config_path(&independent), path);
    drop(launch);
    drop(tools);
    assert!(path.exists());
    drop(retained);
    assert!(!path.parent().unwrap().exists());
    assert!(config_path(&independent).exists());
}

#[test]
fn codex_carries_credentials_in_child_environment_and_refuses_conflicting_names() {
    let secret = "synthetic-token";
    let tools = AgentTools::new()
        .with_mcp(server().env("APP_TOKEN", secret))
        .with_mcp(
            McpServer::http("app_http", "http://127.0.0.1:9123/mcp")
                .header("Authorization", secret),
        );
    let launch = tools.configure_rpc(McpBackend::Codex, raw()).unwrap();
    let args = launch.args.join(" ");
    assert!(args.contains("env_vars=[\"APP_TOKEN\"]"));
    assert!(args.contains("env_http_headers={\"Authorization\"=\"AGENT_LINK_MCP_"));
    assert!(!args.contains(secret));
    assert!(!format!("{tools:?} {launch:?}").contains(secret));
    assert_eq!(launch.env.iter().filter(|(_, v)| v == secret).count(), 2);
    let conflict =
        tools.with_mcp(McpServer::stdio("another", executable()).env("APP_TOKEN", "different"));
    assert!(matches!(
        conflict.configure_rpc(McpBackend::Codex, raw()),
        Err(Error::InvalidInput(_))
    ));
    // Independent per-server environments are supported by the file-based mapping.
    assert!(conflict.configure_rpc(McpBackend::Claude, raw()).is_ok());
}

#[test]
fn exclusive_configuration_is_explicit_and_preserves_permission_arguments() {
    let registry = ProfileRegistry::builtin();
    let claude = registry.get("claude").unwrap();
    let opts = RpcLaunchOptions::default();
    let original = claude.rpc_launch_with_options(&opts).unwrap();
    let tools = AgentTools::new().with_mcp_policy(McpPolicy::Exclusive);
    let launch = tools.rpc_launch(claude.as_ref(), &opts).unwrap();
    assert!(launch.args.starts_with(&original.args));
    assert!(launch.args.iter().any(|a| a == "--strict-mcp-config"));
    let value: Value =
        serde_json::from_slice(&std::fs::read(config_path(&launch)).unwrap()).unwrap();
    assert_eq!(value, json!({"mcpServers":{}}));
    for backend in [McpBackend::Codex, McpBackend::Pi, McpBackend::Acp] {
        assert!(matches!(
            tools.configure_rpc(backend, raw()),
            Err(Error::Unsupported(_))
        ));
    }
}

#[test]
fn unsupported_profiles_do_not_guess_a_mapping_from_their_id() {
    struct Custom(Option<McpBackend>);
    impl AgentProfile for Custom {
        fn mcp_backend(&self, _: ToolTransport) -> Option<McpBackend> {
            self.0
        }
        fn id(&self) -> &str {
            "codex"
        }
        fn dialog(&self, _: &str) -> Option<Dialog> {
            None
        }
        fn input_ready(&self, _: &str) -> bool {
            false
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let tools = AgentTools::new().with_mcp(server());
    assert!(matches!(
        tools.pty_launch(
            &Custom(None),
            &Permissions::default(),
            &TrustPolicy::default(),
            dir.path(),
            ShellKind::native()
        ),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        tools.pty_launch(
            &Custom(Some(McpBackend::Acp)),
            &Permissions::default(),
            &TrustPolicy::default(),
            dir.path(),
            ShellKind::native()
        ),
        Err(Error::Unsupported(_))
    ));
    let registry = ProfileRegistry::builtin();
    for name in ["cursor", "opencode"] {
        let profile = registry.get(name).unwrap();
        assert!(matches!(
            tools.pty_launch(
                profile.as_ref(),
                &Permissions::default(),
                &TrustPolicy::default(),
                dir.path(),
                ShellKind::native()
            ),
            Err(Error::Unsupported(_))
        ));
        assert!(
            AgentTools::new()
                .with_cli(CliTool::new(executable(), "help"))
                .pty_launch(
                    profile.as_ref(),
                    &Permissions::default(),
                    &TrustPolicy::default(),
                    dir.path(),
                    ShellKind::native()
                )
                .is_ok()
        );
    }
    let opencode = registry.get("opencode").unwrap();
    let launch = tools
        .rpc_launch(opencode.as_ref(), &RpcLaunchOptions::default())
        .unwrap();
    assert_eq!(
        launch.tools.unwrap().acp_servers().unwrap()[0]["name"],
        "my_app"
    );
}

#[test]
fn malformed_descriptors_are_rejected_before_creating_configuration() {
    let cases = [
        AgentTools::new().with_cli(CliTool::new("relative", "help")),
        AgentTools::new().with_cli(CliTool::new(
            executable()
                .parent()
                .unwrap()
                .join(if cfg!(windows) {
                    "bad;path"
                } else {
                    "bad:path"
                })
                .join("app-cli"),
            "help",
        )),
        AgentTools::new().with_cli(CliTool::new(executable(), " ")),
        AgentTools::new().with_env("BAD=KEY", "value"),
        AgentTools::new().with_env("APP_TOKEN", "\0"),
        AgentTools::new().with_env("AGENT_LINK_MCP_1", "reserved"),
        AgentTools::new().with_mcp(server()).with_mcp(server()),
        AgentTools::new().with_mcp(server().header("Header", "value")),
        AgentTools::new().with_mcp(McpServer::http("app", "http:///mcp")),
        AgentTools::new().with_mcp(McpServer::http("app", "http://token@localhost/mcp")),
        AgentTools::new().with_mcp(McpServer::http("app", "http://localhost/mcp").arg("wrong")),
        AgentTools::new().with_mcp(
            McpServer::http("app", "http://localhost/mcp").header("X-Token", "one\r\ntwo"),
        ),
        AgentTools::new().with_mcp(server().env("DUP", "1").env("dup", "2")),
        AgentTools::new().with_cli(CliTool::new(
            executable(),
            "x".repeat(MAX_CONFIGURATION_BYTES),
        )),
        AgentTools {
            servers: (0..=MAX_TOOLS)
                .map(|i| McpServer::stdio(format!("s{i}"), executable()))
                .collect(),
            ..AgentTools::default()
        },
    ];
    for tools in cases {
        assert!(matches!(
            tools.configure_rpc(McpBackend::Claude, raw()),
            Err(Error::InvalidInput(_))
        ));
    }
}

#[test]
fn malformed_mcp_urls_are_refused_by_every_backend() {
    for url in [
        "http://:",
        "http://localhost:not-a-port/mcp",
        "http://localhost:65536/mcp",
        "http://localhost:-1/mcp",
        "http://localhost:123:456/mcp",
        "http://[::1/mcp",
        "http://::1/mcp",
        "http://[gg::1]/mcp",
        "http://[1:2:3:4:5:6:7:8:9]/mcp",
        "http://[::1]suffix/mcp",
        "http://[localhost]/mcp",
        "http://[]/mcp",
        "http://[::1]:not-a-port/mcp",
        "http://[::1]:65536/mcp",
        "http://127.0.0.999/mcp",
        "http://bad%zzhost/mcp",
        "http://%20/mcp",
        r"http://\localhost/mcp",
        r"http://localhost\other/mcp",
        "http:///mcp",
        "https://",
        "http://@localhost/mcp",
        "http://user:password@localhost/mcp",
        "http://localhost/mcp?query=with space",
        "http://localhost/mcp\n",
        "http://localhost/mcp\u{a0}",
        "http://localhost/mcp#fragment",
        "http://localhost/mcp#",
        "ftp://localhost/mcp",
        "//localhost/mcp",
    ] {
        let tools = AgentTools::new().with_mcp(McpServer::http("app", url));
        for backend in [
            McpBackend::Codex,
            McpBackend::Claude,
            McpBackend::Pi,
            McpBackend::Acp,
        ] {
            assert!(
                matches!(
                    tools.configure_rpc(backend, raw()),
                    Err(Error::InvalidInput(_))
                ),
                "accepted {url:?} for {backend:?}"
            );
        }
    }
    let secret = "synthetic-private-query";
    let tools = AgentTools::new().with_mcp(McpServer::http(
        "app",
        format!("http://localhost:bad/mcp?token={secret}"),
    ));
    let error = tools.configure_rpc(McpBackend::Claude, raw()).unwrap_err();
    assert!(!error.to_string().contains(secret));
}

#[test]
fn valid_mcp_urls_preserve_host_port_path_and_query_text() {
    for url in [
        "http://localhost",
        "http://localhost:65535/mcp",
        "http://127.0.0.1:9123/mcp",
        "http://[::1]:9123/mcp",
        "http://[2001:db8::1]/mcp",
        "http://[::ffff:192.0.2.1]:9123/mcp",
        "https://EXAMPLE.com:443/mcp?email=user@example.com&value=%23%20",
        "https://例え.テスト/mcp",
    ] {
        let tools = AgentTools::new().with_mcp(McpServer::http("app", url));
        let launch = tools.configure_rpc(McpBackend::Claude, raw()).unwrap();
        let value: Value =
            serde_json::from_slice(&std::fs::read(config_path(&launch)).unwrap()).unwrap();
        assert_eq!(value["mcpServers"]["app"]["url"], url);
        let launch = tools.configure_rpc(McpBackend::Acp, raw()).unwrap();
        assert_eq!(
            launch.tools.as_ref().unwrap().acp_servers().unwrap()[0]["url"],
            url
        );
    }
}

#[test]
fn literal_descriptors_cannot_invoke_native_expansion() {
    for backend in [McpBackend::Claude, McpBackend::Pi] {
        assert!(matches!(
            AgentTools::new()
                .with_mcp(server().env("TOKEN", "${SECRET}"))
                .configure_rpc(backend, raw()),
            Err(Error::InvalidInput(_))
        ));
    }
    assert!(matches!(
        AgentTools::new()
            .with_mcp(server().env("TOKEN", "!a-command"))
            .configure_rpc(McpBackend::Pi, raw()),
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        AgentTools::new()
            .with_mcp(McpServer::stdio("a-b", executable()))
            .with_mcp(McpServer::stdio("a_b", executable()))
            .configure_rpc(McpBackend::Pi, raw()),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn pi_mcp_rejects_templates_in_both_environment_and_headers() {
    for value in [
        "$HOME",
        "prefix$HOME",
        "prefix$_PRIVATE_TOKEN",
        "prefix$TOKEN_1/suffix",
        "prefix$$suffix",
        "prefix$!suffix",
        r"prefix\$HOME",
        "${HOME}",
        "!a-command",
        "  !a-command",
    ] {
        for server in [
            server().env("TOKEN", value),
            McpServer::http("my_app", "http://127.0.0.1:9123/mcp").header("Authorization", value),
        ] {
            let tools = AgentTools::new().with_mcp(server);
            assert!(
                matches!(
                    tools.configure_rpc(McpBackend::Pi, raw()),
                    Err(Error::InvalidInput(_))
                ),
                "Pi must refuse template {value:?}"
            );
        }
    }
}

#[test]
fn pi_mcp_preserves_dollars_that_do_not_start_templates() {
    for value in [
        "",
        "trailing$",
        "price$9",
        "$-literal",
        "$é",
        "inside!literal",
    ] {
        let launch = AgentTools::new()
            .with_mcp(server().env("TOKEN", value))
            .with_mcp(
                McpServer::http("my_http", "http://127.0.0.1:9123/mcp")
                    .header("Authorization", value),
            )
            .configure_rpc(McpBackend::Pi, raw())
            .unwrap();
        let index = launch.args.iter().position(|a| a == "--extension").unwrap();
        let config = Path::new(&launch.args[index + 1])
            .parent()
            .unwrap()
            .join("mcp.json");
        let value_on_disk: Value = serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
        assert_eq!(value_on_disk["mcpServers"]["my_app"]["env"]["TOKEN"], value);
        assert_eq!(
            value_on_disk["mcpServers"]["my_http"]["headers"]["Authorization"],
            value
        );
    }
}

#[test]
fn pi_specific_templates_remain_literal_in_claude_and_agent_environment() {
    let value = "prefix$HOME $$ $!";
    let tools = AgentTools::new()
        .with_mcp(server().env("TOKEN", value))
        .with_mcp(
            McpServer::http("my_http", "http://127.0.0.1:9123/mcp").header("Authorization", value),
        );
    let launch = tools.configure_rpc(McpBackend::Claude, raw()).unwrap();
    let value_on_disk: Value =
        serde_json::from_slice(&std::fs::read(config_path(&launch)).unwrap()).unwrap();
    assert_eq!(value_on_disk["mcpServers"]["my_app"]["env"]["TOKEN"], value);
    assert_eq!(
        value_on_disk["mcpServers"]["my_http"]["headers"]["Authorization"],
        value
    );

    let launch = AgentTools::new()
        .with_env("TOKEN", value)
        .with_mcp(server())
        .configure_rpc(McpBackend::Pi, raw())
        .unwrap();
    assert!(
        launch
            .env
            .iter()
            .any(|(key, v)| key == "TOKEN" && v == value)
    );
}

#[test]
fn double_attachment_and_existing_native_mcp_flags_are_rejected() {
    let tools = AgentTools::new().with_mcp(server());
    let launch = tools.configure_rpc(McpBackend::Claude, raw()).unwrap();
    let path = config_path(&launch);
    assert!(matches!(
        tools.configure_rpc(McpBackend::Claude, launch),
        Err(Error::InvalidInput(_))
    ));
    assert!(!path.exists());
    for args in [
        vec!["-c", "mcp_servers.app.command=\"old\""],
        vec!["--config=mcp_servers={}"],
        vec!["-cmcp_servers={}"],
    ] {
        assert!(matches!(
            tools.configure_rpc(McpBackend::Codex, RpcLaunch::new("agent", &args)),
            Err(Error::InvalidInput(_))
        ));
    }
    let ordinary = RpcLaunch::new(
        "agent",
        &["--prompt", "explain mcp_servers={}", "-c", "model=example"],
    );
    assert!(tools.configure_rpc(McpBackend::Codex, ordinary).is_ok());
    assert!(matches!(
        tools.configure_rpc(
            McpBackend::Claude,
            RpcLaunch::new("agent", &["--mcp-config=old.json"])
        ),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn pi_extension_config_is_private_and_dropped_with_its_launch() {
    let launch = AgentTools::new()
        .with_mcp(server())
        .configure_rpc(McpBackend::Pi, raw())
        .unwrap();
    let i = launch.args.iter().position(|a| a == "--extension").unwrap();
    let script = PathBuf::from(&launch.args[i + 1]);
    let config = script.parent().unwrap().join("mcp.json");
    let value: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    assert_eq!(value["mcpServers"]["my_app"]["args"], json!(["serve"]));
    drop(launch);
    assert!(!script.exists() && !config.exists());
}
