//! Real model turn over ACP stable-v1 (`acp` feature): run explicitly against an installed
//! ACP-capable profile such as `opencode`.
//!
//! ```text
//! cargo run -p agent-link --example acp_prompt --features acp
//! ```

use std::error::Error;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use agent_link::acp::{AcpClient, AcpEvents, AcpLimits, RequestOutcome, RequestTicket};
use agent_link::agent::{ProfileRegistry, RpcLaunchOptions};
use agent_link::portable::{EventKind, RequestId, TextUpdate};
use agent_link::rpc::RpcSpawnSpec;

fn require_response(ticket: RequestTicket) -> Result<(), Box<dyn Error>> {
    match ticket
        .wait_until(Instant::now() + Duration::from_secs(60))
        .as_deref()
    {
        Some(RequestOutcome::Response(_)) => Ok(()),
        _ => Err(
            "ACP request did not succeed; inspect the ticket, do not automatically resend".into(),
        ),
    }
}

pub(crate) fn write_prompt(
    events: &AcpEvents,
    prompt: &RequestTicket,
    end: Instant,
    output: &mut impl Write,
) -> Result<(), Box<dyn Error>> {
    while let Some(event) = events.recv_until(end) {
        if let Some(permission) = event.permission {
            // Explicitly cancel any unexpected permission request in this example.
            let _reply = permission.cancel()?;
        }
        if let EventKind::Text {
            text,
            update: TextUpdate::Delta,
            ..
        } = &event.portable.kind
        {
            write!(output, "{text}")?;
        }
        // Tickets settle independently of event consumption. Drain through this
        // prompt's response so already queued text is printed before returning.
        // Remote errors also have a response event, without TurnCompleted.
        if event.portable.native_method == "session/prompt"
            && matches!(&event.portable.context.request_id, Some(RequestId::String(id)) if id == prompt.id())
        {
            return match prompt.outcome().as_deref() {
                Some(RequestOutcome::Response(_)) => Ok(()),
                _ => {
                    Err("ACP prompt failed; inspect the ticket, do not automatically resend".into())
                }
            };
        }
    }
    Err(
        "ACP prompt stream ended or timed out before its response; do not automatically resend"
            .into(),
    )
}

fn main() -> Result<(), Box<dyn Error>> {
    let cwd = std::env::current_dir()?;
    let cwd_str = cwd
        .to_str()
        .ok_or("current directory path must be valid UTF-8")?;

    let profile = ProfileRegistry::builtin()
        .get("opencode")
        .ok_or("missing built-in opencode profile")?;
    let launch = profile.rpc_launch_with_options(&RpcLaunchOptions::default())?;
    let spec = RpcSpawnSpec::new(launch, &cwd);

    let (client, events) = AcpClient::spawn(spec, AcpLimits::default())?;
    let deadline = || Instant::now() + Duration::from_secs(60);

    require_response(client.initialize(
        "agent-link-example",
        env!("CARGO_PKG_VERSION"),
        deadline(),
    )?)?;
    require_response(client.session_new(cwd_str, deadline())?)?;

    let prompt = client.prompt("Reply with one short greeting.", deadline())?;
    write_prompt(&events, &prompt, deadline(), &mut io::stdout().lock())?;
    println!("\nSession completed: {:?}", client.snapshot().session_id);
    Ok(()) // Drop settles tickets and closes the owned child process.
}
