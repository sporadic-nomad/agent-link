//! A child that speaks lines over stdio, the way an agent CLI does in its JSON-lines mode,
//! with `cat` standing in for the agent: send one line, read it back, tear the child down.
//!
//! ```text
//! cargo run -p agent-link --example rpc_child --features rpc
//! ```
//!
//! unix: `cat` runs through your login shell (`<shell> -l -i -c 'exec cat'`) in a session of
//! its own, as an agent CLI would; a line your rc files print to stdout arrives first and is
//! skipped. Windows: needs a `cat` on `PATH` (Git for Windows ships one in `usr\bin`, which
//! its installer does not put on `PATH` by default). Exit status 0 once
//! the line came back; 1 when the child exited first, or nothing came back within 60 s.

use std::error::Error;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use agent_link::agent::RpcLaunch;
use agent_link::rpc::{RpcChild, RpcEvent, RpcSpawnSpec, forward_rpc, preflight};

/// A child is two execs, the shell and then `cat`; slow rc files or endpoint-security
/// vetting of each exec can take seconds.
const ECHO_WAIT: Duration = Duration::from_secs(60);
const REQUEST: &str = r#"{"id":1,"method":"ping"}"#;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rpc_child: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    // A raw command; built-in profile RPC launches add YOLO/unattended settings by default.
    let launch = RpcLaunch::new("cat", &[]);
    let mut spec = RpcSpawnSpec::new(launch, std::env::current_dir()?);
    spec.log_tag = "example".into(); // log lines read `[example:cat] …` (`log` facade)
    spec.log_key = "cat".into();
    // Validate the spec before replacing an existing child. Only Windows checks launcher
    // PATH resolution here; Unix resolves it in the spawned login shell.
    // Success does not guarantee that spawning or agent startup will succeed.
    preflight(&spec)?;
    let (child, events) = RpcChild::spawn(spec)?;
    println!("child {}: pid {}", child.id(), child.pid());

    // One call per event, on a thread of the crate's: print it, report the echo or an exit.
    let (done_tx, done_rx) = mpsc::channel();
    forward_rpc(events, move |event| match event {
        RpcEvent::Line { seq, line, .. } => {
            println!("line {seq}: {line}");
            if line == REQUEST {
                let _ = done_tx.send(Ok(()));
            }
        }
        RpcEvent::Error { error, .. } => {
            let _ = done_tx.send(Err(format!("RPC transport failed: {error}")));
        }
        RpcEvent::Exit { stderr_tail, .. } => {
            let _ = done_tx.send(Err(format!("the child exited; stderr: {stderr_tail}")));
        }
        _ => {}
    })?;

    child.send_line(REQUEST)?; // the writer thread appends `\n` and flushes
    match done_rx.recv_timeout(ECHO_WAIT) {
        Ok(Ok(())) => println!("round trip: ok"),
        Ok(Err(exited)) => return Err(exited.into()),
        Err(_) => return Err(format!("no echo within {ECHO_WAIT:?}").into()),
    }
    // Kills the child's process group (Windows: its process tree) and reaps it. `None`:
    // the kill's signal ended it.
    let code = child.kill();
    println!("torn down: exit code {code:?}");
    Ok(())
}
