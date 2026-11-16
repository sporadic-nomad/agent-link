#[path = "../../../tests/support/fake_agent/codex_peer.rs"]
mod codex_peer;
use agent_link::{agent::RpcLaunch, rpc::RpcSpawnSpec};
use agent_link_supervisor::{Binding, Limits, codex::Config};
use std::{path::Path, time::Duration};
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args[1] == "codex-peer" {
        codex_peer::run(&args[2]);
        return;
    }
    if args[1] == "launcher" {
        // Daemon lifetime belongs to the service, never this short-lived launcher.
        #[allow(clippy::zombie_processes)]
        let _child = std::process::Command::new(&args[0])
            .args(["serve", &args[2], &args[3]])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        return;
    }
    let root = Path::new(&args[2]);
    let launch = RpcLaunch::new(&args[0], &["codex-peer", &args[3]]);
    let mut spec = RpcSpawnSpec::new(launch, root);
    spec.env.inherit = false;
    spec.env
        .set("PATH", std::env::var_os("PATH").unwrap_or_default());
    spec.env
        .set("AGENT_LINK_TEST_START_COUNT", root.join("count"));
    spec.env
        .set("AGENT_LINK_TEST_START_MARKER", root.join("start"));
    spec.env
        .set("AGENT_LINK_TEST_HANG_MARKER", root.join("hang"));
    #[cfg(unix)]
    {
        spec.shell = Some("/bin/sh".into());
    }
    let limits = Limits {
        lease_ttl: Duration::from_millis(300),
        // Optional fixture-only override for tests unrelated to expiry. Production
        // daemon limits never read these test process arguments.
        detached_ttl: args
            .get(4)
            .map(|millis| Duration::from_millis(millis.parse().unwrap()))
            .unwrap_or(Duration::from_millis(800)),
        ..Limits::default()
    };
    let config = Config {
        request_timeout: Duration::from_secs(5),
        ..Config::default()
    };
    if let Err(error) = agent_link_daemon::serve(
        root,
        Binding {
            provider: "codex".into(),
            session: "t".into(),
        },
        spec,
        limits,
        config,
    ) {
        eprintln!("fixture host: {error}");
        std::process::exit(1);
    }
}
