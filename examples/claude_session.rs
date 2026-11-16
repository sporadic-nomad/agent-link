//! Real persistent Claude stream-json session (`claude` feature): run explicitly against an
//! installed and authenticated `claude` CLI.
//!
//! ```text
//! cargo run -p agent-link --example claude_session --features claude
//! ```

use std::error::Error;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use agent_link::agent::Permissions;
use agent_link::claude::{self, ClaudeSession, Events, Limits, Outcome, Ticket};
use agent_link::portable::{CompletionStatus, EventKind, TextUpdate};
use agent_link::rpc::RpcSpawnSpec;

fn wait_ticket(ticket: Ticket) -> Result<Outcome, Box<dyn Error>> {
    match ticket.wait_until(Instant::now() + Duration::from_secs(60)) {
        Some(outcome) => Ok((*outcome).clone()),
        None => Err("Claude request timed out; do not automatically resend".into()),
    }
}

pub(crate) fn write_turn(
    session: &ClaudeSession,
    events: &Events,
    end: Instant,
    output: &mut impl Write,
) -> Result<(), Box<dyn Error>> {
    while let Some(event) = events.recv_until(end) {
        if let Some(approval) = event.approval {
            // Explicitly deny any unexpected tool approval request in this example.
            let _reply = approval.respond(None, Some("Denied by example"))?;
        }
        match &event.portable.kind {
            EventKind::Text {
                text,
                update: TextUpdate::Delta,
                ..
            } => write!(output, "{text}")?,
            EventKind::Text {
                text,
                update: TextUpdate::Replace,
                ..
            } => writeln!(output, "\n{text}")?,
            EventKind::TurnCompleted { status } => {
                // Completed requires a native success result with is_error=false.
                // A flushed user write, silence or a future status is not success.
                if *status != CompletionStatus::Completed {
                    return Err(format!("Claude turn did not succeed: {status:?}").into());
                }
                writeln!(output, "\nTurn completed: {status:?}")?;
                return Ok(());
            }
            _ => {}
        }
    }
    if let Some(failure) = session.failure() {
        return Err(format!("Claude session ended before a successful result: {failure}; do not automatically resend").into());
    }
    Err("Claude turn timed out before a successful result; do not automatically resend".into())
}

fn main() -> Result<(), Box<dyn Error>> {
    let launch = claude::launch(&Permissions::Unattended, false);
    let spec = RpcSpawnSpec::new(launch, std::env::current_dir()?);
    let (session, events) = ClaudeSession::spawn(spec, Limits::default(), None)?;

    let deadline = || Instant::now() + Duration::from_secs(60);
    match wait_ticket(session.initialize(deadline())?)? {
        Outcome::Response(_) => {}
        Outcome::Failed { failure, .. } => {
            return Err(format!("initialize failed: {failure}").into());
        }
        Outcome::Written => return Err("unexpected initialize outcome".into()),
    }

    let send_ticket = session.send("Reply with one short greeting.", None, deadline())?;
    match wait_ticket(send_ticket)? {
        Outcome::Written => {}
        Outcome::Failed { failure, .. } => return Err(format!("send failed: {failure}").into()),
        Outcome::Response(_) => return Err("unexpected send outcome".into()),
    }

    write_turn(&session, &events, deadline(), &mut io::stdout().lock())?;
    println!("Session {:?}", session.snapshot().session_id);
    Ok(())
}
