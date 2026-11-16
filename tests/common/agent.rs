//! `AgentTerminal` helpers: spawn under the shared spawn lock, the test shell with an
//! agent spec, and waits on agent events.

use std::path::Path;
use std::time::{Duration, Instant};

use agent_link::agent::{Activity, AgentSpec, AgentState, AgentTerminal};
use agent_link::{Event, SpawnSpec};
use crossbeam_channel::Receiver;

use super::{SHELL_START_WAIT, sh_spec, spawn_lock, wait_event};

/// `AgentTerminal::spawn`, one spawn at a time (see `common::spawn`); panics on an error.
pub fn spawn_agent(spec: SpawnSpec, agent: AgentSpec) -> (AgentTerminal, Receiver<Event>) {
    let _guard = spawn_lock();
    AgentTerminal::spawn(spec, agent).expect("spawn an agent terminal")
}

/// The test shell (`sh_spec`, job control on) as an agent terminal, after its next `$ `
/// prompt: the receiver holds only what came later.
pub fn start_agent_shell(home: &Path, agent: AgentSpec) -> (AgentTerminal, Receiver<Event>) {
    start_agent_shell_spec(sh_spec(home), agent)
}

/// [`start_agent_shell`] with another shell `spec` (its prompt must be `$ `).
pub fn start_agent_shell_spec(
    spec: SpawnSpec,
    agent: AgentSpec,
) -> (AgentTerminal, Receiver<Event>) {
    let (term, events) = spawn_agent(spec, agent);
    term.terminal().write(b"set -m\n").expect("write set -m");
    term.terminal()
        .write(b"echo shell-$((20+1))\n")
        .expect("write echo");
    let deadline = Instant::now() + SHELL_START_WAIT;
    let mut text = String::new();
    loop {
        match events.recv_deadline(deadline) {
            Ok(Event::Output { bytes, .. }) => {
                text.push_str(&String::from_utf8_lossy(&bytes));
                // The echo of the typed line shows `$((20+1))`, never `shell-21`.
                if let Some(at) = text.find("shell-21") {
                    if text[at..].contains("$ ") {
                        return (term, events);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => panic!("no prompt ({e}); output so far: {text:?}"),
        }
    }
}

/// The `AgentState` events (state, blocking) received until one matches `until` (included;
/// it may keep state across calls). Panics after `timeout` with the screen.
pub fn states_until(
    term: &AgentTerminal,
    events: &Receiver<Event>,
    timeout: Duration,
    mut until: impl FnMut(AgentState, bool) -> bool,
) -> Vec<(AgentState, bool)> {
    let mut seen = Vec::new();
    wait_event(term.terminal(), events, timeout, |e| match e {
        Event::AgentState {
            state, blocking, ..
        } => {
            seen.push((*state, *blocking));
            until(*state, *blocking)
        }
        _ => false,
    });
    seen
}

/// Waits for an `Activity` event with `want` and returns the activities seen until then.
pub fn activity_until(
    term: &AgentTerminal,
    events: &Receiver<Event>,
    timeout: Duration,
    want: Activity,
) -> Vec<Activity> {
    let mut seen = Vec::new();
    wait_event(term.terminal(), events, timeout, |e| match e {
        Event::Activity { state, .. } => {
            seen.push(*state);
            *state == want
        }
        _ => false,
    });
    seen
}
