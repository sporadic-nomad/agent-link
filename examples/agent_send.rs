//! Start a coding agent in a login shell, wait until it is ready for programmatic input,
//! send one message with a verified submit, and print the outcome.
//!
//! ```text
//! cargo run -p agent-link --example agent_send -- <agent> ["<message>"]
//! ```
//!
//! `<agent>` names a built-in profile: claude, opencode, pi, codex or cursor. The CLI must be
//! installed (found by your login shell) and signed in: the message is a real turn, an API
//! call on your account. The agent starts in the current directory with the default
//! YOLO/unattended permissions. Set `agent.permissions` to `Permissions::AgentDefault`
//! or `Permissions::Custom` to configure restrictions. This example explicitly opts into
//! `TrustPolicy::unattended()`, which adds the profile's
//! pre-trust flag (cursor's `--trust` saves a trust record) and answers a folder-trust
//! dialog, which claude saves — and codex on Windows, where it gets no pre-trust flag and
//! saves its `Yes, continue` to `config.toml`. Exit status: 0 when the message was delivered,
//! 1 otherwise, 2 for a usage error.

use std::error::Error;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_link::agent::{
    AgentProfile, AgentSpec, AgentState, AgentTerminal, DeliveryOutcome, ProfileRegistry,
    SendOptions, TrustPolicy,
};
use agent_link::{Event, PtySize, SpawnSpec};

/// The size the profiles' recorded screens were captured at.
const SIZE: PtySize = PtySize {
    rows: 40,
    cols: 120,
};
/// From the launch line to `Ready`: a cold CLI can take many seconds.
const READY_WAIT: Duration = Duration::from_secs(60);
/// The paste, the Enter and its verification on screen.
const DELIVERY_WAIT: Duration = Duration::from_secs(120);
/// Time for a short reply before the screen is printed and the agent torn down.
const REPLY_WAIT: Duration = Duration::from_secs(10);
const DEFAULT_MESSAGE: &str = "Reply with the single word: pong";

fn main() -> ExitCode {
    let registry = ProfileRegistry::builtin();
    let mut args = std::env::args().skip(1);
    let Some(profile) = args.next().and_then(|id| registry.get(&id)) else {
        let ids = registry.ids().join("|");
        eprintln!("usage: agent_send <{ids}> [message]");
        return ExitCode::from(2);
    };
    let message = args.next().unwrap_or_else(|| DEFAULT_MESSAGE.to_string());
    match run(profile, &message) {
        Ok(DeliveryOutcome::Delivered) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("agent_send: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(profile: Arc<dyn AgentProfile>, message: &str) -> Result<DeliveryOutcome, Box<dyn Error>> {
    let mut agent = AgentSpec::new(profile);
    // YOLO/unattended permissions are the default. Explicitly trust this working folder
    // so startup can complete without an interactive renderer.
    agent.trust = TrustPolicy::unattended();
    let cwd = std::env::current_dir()?;
    let (term, events) = AgentTerminal::spawn(SpawnSpec::shell(cwd, SIZE), agent)?;

    term.launch()?; // types the profile's launch line into the shell
    let deadline = Instant::now() + READY_WAIT;
    loop {
        let state = match events.recv_deadline(deadline) {
            Ok(Event::AgentState { state, .. }) => state,
            Ok(_) => continue, // output, titles, activity
            Err(_) => return Err(failure(&term, &format!("not ready within {READY_WAIT:?}"))),
        };
        println!("state: {state:?}");
        match state {
            AgentState::Ready => break,
            AgentState::Exited => return Err(failure(&term, "the session ended")),
            _ => {} // starting, a startup dialog being answered
        }
    }
    drop(events); // not read from here on: an undrained receiver would stall the session

    let opts = SendOptions {
        submit: true, // paste, then an Enter verified on screen
        ..SendOptions::default()
    };
    let ticket = term.send(message, opts);
    let Some(outcome) = ticket.wait(DELIVERY_WAIT) else {
        let what = format!("no outcome within {DELIVERY_WAIT:?}");
        return Err(failure(&term, &what));
    };
    println!("outcome: {outcome:?}");
    if outcome == DeliveryOutcome::Delivered {
        std::thread::sleep(REPLY_WAIT);
    }
    print_bottom(&term);
    // Dropping `term` kills the agent (the terminal's foreground job) and the shell.
    Ok(outcome)
}

/// `what` as an error, after the bottom of the screen (a shell's "command not found", a
/// dialog left on screen).
fn failure(term: &AgentTerminal, what: &str) -> Box<dyn Error> {
    print_bottom(term);
    what.into()
}

/// The last non-blank rows of the screen the controller reads.
fn print_bottom(term: &AgentTerminal) {
    let Some(screen) = term.terminal().screen() else {
        return;
    };
    let rows = &screen.rows;
    let shown: Vec<&String> = rows.iter().filter(|r| !r.trim().is_empty()).collect();
    println!("screen (last rows):");
    for row in &shown[shown.len().saturating_sub(10)..] {
        println!("  {row}");
    }
}
