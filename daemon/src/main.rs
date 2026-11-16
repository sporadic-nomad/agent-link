use agent_link::{
    agent::{ProfileRegistry, RpcLaunchOptions},
    rpc::RpcSpawnSpec,
};
use agent_link_daemon::{Client, FRAME_BYTES, serve};
use agent_link_supervisor::{Binding, Limits, codex::Config};
use std::{
    io::{self, BufRead, Read},
    path::PathBuf,
};
fn main() {
    if let Err(error) = run() {
        eprintln!("agent-link-daemon: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() < 2 {
        return Err(
            "usage: agent-link-daemon serve DIR EXISTING_THREAD | client DIR (JSON lines on stdin)"
                .into(),
        );
    }
    let root = PathBuf::from(&args[1]);
    match args[0].to_str() {
        Some("serve") if args.len() == 3 => {
            let session = args[2].to_str().ok_or("invalid thread ID")?.to_owned();
            let profile = ProfileRegistry::builtin()
                .get("codex")
                .ok_or("missing profile")?;
            let launch = profile.rpc_launch_with_options(&RpcLaunchOptions::default())?;
            serve(
                &root,
                Binding {
                    provider: "codex".into(),
                    session,
                },
                RpcSpawnSpec::new(launch, std::env::current_dir()?),
                Limits::default(),
                Config::default(),
            )?;
        }
        Some("client") if args.len() == 2 => {
            let mut client = Client::connect(&root)?;
            let mut input = io::stdin().lock();
            loop {
                let mut line = Vec::new();
                let n = (&mut input)
                    .take((FRAME_BYTES + 1) as u64)
                    .read_until(b'\n', &mut line)?;
                if n == 0 {
                    break;
                }
                if n > FRAME_BYTES {
                    return Err("client input frame too large".into());
                }
                let request = serde_json::from_slice(&line)?;
                println!("{}", client.request(request)?);
            }
        }
        _ => return Err("invalid daemon command".into()),
    }
    Ok(())
}
