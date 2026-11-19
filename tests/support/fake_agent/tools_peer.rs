//! Offline launch observer. Reads configuration again on request to catch early cleanup.
use std::io::{BufRead, Write};

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    let index = args
        .iter()
        .position(|a| a == "--mcp-config")
        .expect("MCP file argument");
    let path = std::path::Path::new(&args[index + 1]);
    let report = || {
        let data = std::fs::read(path).expect("configuration must survive while child runs");
        let config: serde_json::Value = serde_json::from_slice(&data).unwrap();
        let first_path = std::env::split_paths(&std::env::var_os("PATH").unwrap()).next();
        let report = serde_json::json!({
            "tool_peer": true,
            "args": args,
            "config_path": path,
            "server": config["mcpServers"]["app"],
            "endpoint": std::env::var("AGENT_LINK_TEST_TOOL_ENDPOINT").ok(),
            "first_path": first_path,
        });
        if std::env::var_os("AGENT_LINK_TEST_TOOL_REPORT").is_some() {
            // A console can redraw or wrap long JSON lines. Publish one complete
            // report in the test's private working directory for each inspection.
            std::fs::write("tool-report.tmp", report.to_string()).unwrap();
            std::fs::rename("tool-report.tmp", "tool-report.json").unwrap();
        } else {
            println!("{report}");
            std::io::stdout().flush().unwrap();
        }
    };
    report();
    for line in std::io::stdin().lock().lines() {
        if line.unwrap() == "inspect" {
            report();
        } else {
            break;
        }
    }
}
