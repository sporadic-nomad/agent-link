//! Real model turn: run explicitly on the user's Codex installation/account.
use agent_link::agent::{ProfileRegistry, RpcLaunchOptions};
use agent_link::codex::{CodexAppServer, CodexLimits, RequestOutcome, RequestTicket};
use agent_link::portable::{EventKind, TextUpdate};
use agent_link::rpc::RpcSpawnSpec;
use serde_json::json;
use std::time::{Duration, Instant};

fn result(ticket: RequestTicket) -> Result<(), Box<dyn std::error::Error>> {
    match ticket
        .wait_until(Instant::now() + Duration::from_secs(60))
        .as_deref()
    {
        Some(RequestOutcome::Response(_)) => Ok(()),
        _ => Err(
            "native request did not succeed; inspect the ticket, do not automatically resend"
                .into(),
        ),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let profile = ProfileRegistry::builtin()
        .get("codex")
        .ok_or("missing profile")?;
    let launch = profile.rpc_launch_with_options(&RpcLaunchOptions::default())?;
    let spec = RpcSpawnSpec::new(launch, std::env::current_dir()?);
    let (app, events) = CodexAppServer::spawn(spec, CodexLimits::default())?;
    let deadline = || Instant::now() + Duration::from_secs(60);
    result(app.initialize("agent-link-example", env!("CARGO_PKG_VERSION"), deadline())?)?;
    result(app.thread_start(json!({}), deadline())?)?;
    let root = app.snapshot().thread_id.ok_or("missing root thread")?;
    let turn = app.turn_start("Reply with one short greeting.", deadline())?;
    // Drain while waiting for native completion. Even unattended launches can receive
    // unsupported requests; essential approvals here are explicitly declined.
    let end = deadline();
    while let Some(event) = events.recv_until(end) {
        if let Some(approval) = event.approval {
            let _reply = approval.respond(json!("decline"))?;
        }
        // Subagent output shares this connection; only the root ends this example.
        if event.portable.context.session_id.as_deref() != Some(root.as_str()) {
            continue;
        }
        // This example streams deltas. A transcript UI should also reconcile Replace
        // text by item ID; a completed item's snapshot can differ from earlier deltas.
        match &event.portable.kind {
            EventKind::Text {
                text,
                update: TextUpdate::Delta,
                ..
            } => print!("{text}"),
            EventKind::TurnCompleted { .. } => break,
            _ => {}
        }
    }
    result(turn)?;
    if let Some(terminal) = app.snapshot().last_turn {
        println!("\nTurn status: {:?}", terminal.status);
    } else {
        return Err("no terminal turn was observed; outcome remains unknown".into());
    }
    Ok(()) // Drop ends the owned process; it does not make the turn durable.
}
