//! Offline preparation only: no server, agent, credentials or global configuration.
//! cargo run --example application_tools --features tools -- codex /absolute/app-cli http://127.0.0.1:9123/mcp
use agent_link::agent::{ProfileRegistry, RpcLaunchOptions};
use agent_link::tools::{AgentTools, CliTool, McpServer};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        eprintln!(
            "usage: application_tools <codex|claude|pi|opencode> <absolute-app-cli> <mcp-http-url>\nPrepares a launch only; nothing is executed or contacted."
        );
        return Ok(());
    }
    let profile = ProfileRegistry::builtin()
        .get(&args[0])
        .ok_or("unknown profile")?;
    let tools = AgentTools::new()
        .with_cli(CliTool::new(
            &args[1],
            "Use `context` to read current application state; use `--help` for commands.",
        ))
        .with_mcp(McpServer::http("my_application", &args[2]));
    let launch = tools.rpc_launch(profile.as_ref(), &RpcLaunchOptions::default())?;
    let message = tools.prompt("Explain what I am currently working on.")?;
    println!(
        "Prepared {} with {} arguments. No process was started.",
        launch.launcher,
        launch.args.len()
    );
    println!("First message:\n{message}");
    // Move `launch` directly into RpcSpawnSpec and the matching native adapter (or
    // RpcChild + your protocol). Keep its `tools` metadata: it owns temporary files.
    // Supply `message` through the normal prompt API after initialization. For CLI-only
    // integration omit with_mcp; for MCP-only integration omit with_cli and prompt().
    Ok(())
}
