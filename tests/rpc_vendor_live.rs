//! Explicit live permission checks against installed CLIs, without model turns.
//! Ignored in the ordinary gate. Each child uses an isolated home/config, no inherited
//! credentials or caller-control markers, and `/bin/sh` without startup files.
#![cfg(all(unix, feature = "rpc"))]

use std::process::Command;
use std::time::{Duration, Instant};

use agent_link::agent::{Permissions, ProfileRegistry, RpcLaunchOptions};
use agent_link::rpc::{RpcChild, RpcEvent, RpcSpawnSpec};
use crossbeam_channel::Receiver;
use serde_json::{Value, json};

fn response(events: &Receiver<RpcEvent>, request_id: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let event = events
            .recv_deadline(deadline)
            .expect("RPC response before deadline");
        match event {
            RpcEvent::Line { line, .. } => {
                let message: Value = serde_json::from_str(&line).expect("valid JSON frame");
                if message["id"] == request_id {
                    assert!(
                        message.get("error").is_none(),
                        "request failed: {}",
                        message["error"]
                    );
                    return message["result"].clone();
                }
            }
            other => panic!("transport ended before response {request_id}: {other:?}"),
        }
    }
}

#[test]
#[ignore = "live: installed codex, isolated config/read only; no model calls"]
fn codex_main_command_applies_rpc_permission_overrides_after_the_subcommand() {
    let launcher = std::env::var_os("AGENT_LINK_LIVE_CODEX")
        .map(Into::into)
        .or_else(|| agent_link::resolve::find_on_path(&["codex"]))
        .expect("install codex or set AGENT_LINK_LIVE_CODEX to its executable");
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path();
    let config = home.join("codex");
    std::fs::create_dir(&config).unwrap();
    const CONFIG: &str = "approval_policy = \"on-request\"\nsandbox_mode = \"read-only\"\n[analytics]\nenabled = false\n";
    std::fs::write(config.join("config.toml"), CONFIG).unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut version = Command::new(&launcher);
    version
        .arg("--version")
        .env_clear()
        .env("PATH", &path)
        .env("HOME", home)
        .env("CODEX_HOME", &config)
        .current_dir(home);
    let output = agent_link::proc::output_with_timeout(version, Duration::from_secs(30))
        .expect("version command")
        .expect("version deadline");
    assert!(output.status.success());
    println!("{}", String::from_utf8_lossy(&output.stdout).trim());

    let profile = ProfileRegistry::builtin().get("codex").unwrap();
    for (permissions, approval, sandbox) in [
        (Permissions::AgentDefault, "on-request", "read-only"),
        (Permissions::default(), "never", "danger-full-access"),
    ] {
        let mut launch = profile
            .rpc_launch_with_options(&RpcLaunchOptions::default().with_permissions(permissions))
            .unwrap();
        launch.launcher = launcher.to_str().expect("UTF-8 launcher").into();
        let mut spec = RpcSpawnSpec::new(launch, home);
        spec.shell = Some("/bin/sh".into());
        spec.env.inherit = false;
        spec.env
            .set("PATH", &path)
            .set("HOME", home)
            .set("CODEX_HOME", &config);
        for name in [
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
        ] {
            spec.env.set(name, home.join(name));
        }
        let (child, events) = RpcChild::spawn(spec).expect("spawn real codex app-server");
        child
            .send_line(
                &json!({
                    "id": 1, "method": "initialize", "params": {
                        "clientInfo": { "name": "agent_link_permission_probe", "version": "0.1.0" },
                        "capabilities": { "experimentalApi": true }
                    }
                })
                .to_string(),
            )
            .unwrap();
        response(&events, 1);
        child
            .send_line(r#"{"method":"initialized","params":{}}"#)
            .unwrap();
        child
            .send_line(r#"{"id":2,"method":"config/read","params":{"includeLayers":false}}"#)
            .unwrap();
        let result = response(&events, 2);
        assert_eq!(result["config"]["approval_policy"], approval);
        assert_eq!(result["config"]["sandbox_mode"], sandbox);
        println!("config/read: {approval}/{sandbox}");
        child.kill();
    }
    // The host's explicit native policy remains intact on disk.
    assert_eq!(
        std::fs::read_to_string(config.join("config.toml")).unwrap(),
        CONFIG
    );
}
