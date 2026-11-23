//! Helpers for the `AgentTerminal::send` suites (`tests/agent_delivery*.rs`): an agent
//! terminal in the test shell (`shell`, `fake`), timings, submit options, outcomes read
//! from the ticket and its `Event::Delivery` together, the output seen meanwhile, and the
//! shell checks they share.

use std::path::Path;
use std::time::{Duration, Instant};

use agent_link::Event;
use agent_link::agent::{
    AgentSpec, AgentState, AgentTerminal, DeliveryOutcome, SendOptions, Ticket, Timing,
    TrustPolicy, UnreadableComposer,
};
use agent_link::screen::ScreenSnapshot;
use crossbeam_channel::Receiver;
use tempfile::TempDir;

use super::agent::start_agent_shell;
use super::fake::fake_profile;
use super::{LAUNCH_WAIT, WAIT, bottom_rows, wait_screen};

/// A delivery that starts the fake first: its first exec can take seconds here.
pub const DELIVERY_WAIT: Duration = Duration::from_secs(60);

/// Default timing, but a trust window that outlasts a slow first exec of the fake.
pub fn long_window() -> Timing {
    Timing {
        trust_watch: LAUNCH_WAIT + Duration::from_secs(10),
        ..Timing::default()
    }
}

/// Readiness comes only from the trust window's end with the unlaunchable profile (no
/// markers): keep it short.
pub fn short_window() -> Timing {
    Timing {
        trust_watch: Duration::from_millis(1500),
        ..Timing::default()
    }
}

/// Append the previous command's status as an OSC title at the end of each shell prompt.
/// The visible prompt stays `$ `; Bash/zsh get their own nonprinting delimiters so their
/// line editors still measure its width correctly. Signal handling remains untouched.
/// Use the fake's `--title` option to replace old prompt evidence on every agent start.
pub fn install_status_prompt(term: &AgentTerminal) {
    let setup = concat!(
        r#"if [ -n "${BASH_VERSION-}" ]; then "#,
        r#"PS1='$ \[$(printf "\033]0;agent-link-shell-status:%s\007" "$?")\]'; "#,
        r#"elif [ -n "${ZSH_VERSION-}" ]; then setopt prompt_subst; "#,
        r#"PS1='$ %{$(printf "\033]0;agent-link-shell-status:%s\007" "$?")%}'; "#,
        r#"else PS1='$ $(printf "\033]0;agent-link-shell-status:%s\007" "$?")'; fi"#,
        "\n",
    );
    term.terminal().write(setup.as_bytes()).unwrap();
    wait_screen(term.terminal(), WAIT, |screen| {
        screen.title == "agent-link-shell-status:0"
    });
}

/// The unread-input fake exits successfully, whereas the shell's next Ctrl-C produces
/// status 1 (macOS Bash 3.2 as sh) or 130. Its title was replaced by `--title` at launch,
/// so this cannot be the old prompt before the agent. The OSC ends the new prompt:
/// raw host input can follow without racing the queued interrupt or interleaving with
/// the shell's prompt redraw.
pub fn wait_for_interrupted_prompt(term: &AgentTerminal) -> ScreenSnapshot {
    wait_screen(term.terminal(), WAIT, |screen| {
        matches!(
            screen.title.as_str(),
            "agent-link-shell-status:1" | "agent-link-shell-status:130"
        )
    })
}

/// The test shell as an agent terminal for `agent` (not launched yet).
pub fn shell(agent: AgentSpec) -> (AgentTerminal, Receiver<Event>, TempDir) {
    let home = tempfile::tempdir().unwrap();
    let (term, events) = start_agent_shell(home.path(), agent);
    (term, events, home)
}

/// The fake with `scene` and `flags` in the test shell.
pub fn fake(scene: &str, flags: &[&str]) -> (AgentTerminal, Receiver<Event>, TempDir) {
    shell(AgentSpec {
        timing: long_window(),
        trust: TrustPolicy::unattended(),
        ..AgentSpec::new(fake_profile(scene, flags))
    })
}

pub fn submit() -> SendOptions {
    SendOptions {
        submit: true,
        ..SendOptions::default()
    }
}

/// A submit that accepts one unverified Enter for an unreadable composer.
pub fn enter_once() -> SendOptions {
    SendOptions {
        unreadable_composer: UnreadableComposer::EnterOnce,
        ..submit()
    }
}

/// The fake with `scene`, `flags` and `timing`, drawing its composer without a prompt
/// marker (`--unreadable-composer`: the default pending-input rule reads `None`, as for
/// opencode) and logging its raw input to `input.bin` in the returned home.
pub fn unreadable_fake(
    scene: &str,
    flags: &[&str],
    timing: Timing,
) -> (AgentTerminal, Receiver<Event>, TempDir) {
    let mut all = vec!["--unreadable-composer", "--input-log", "input.bin"];
    all.extend_from_slice(flags);
    shell(AgentSpec {
        timing,
        trust: TrustPolicy::unattended(),
        ..AgentSpec::new(fake_profile(scene, &all))
    })
}

/// The fake's raw input log (`--input-log input.bin` in `home`), read once the fake drew
/// a final `~` typed into its composer (either composer marker): every byte a finished
/// delivery wrote came before it.
pub fn drained_input_log(term: &AgentTerminal, home: &Path) -> Vec<u8> {
    term.terminal().write(b"~").unwrap();
    wait_screen(term.terminal(), WAIT, |s| {
        s.rows
            .iter()
            .any(|r| (r.starts_with('>') || r.starts_with('┃')) && r.ends_with('~'))
    });
    std::fs::read(home.join("input.bin")).unwrap()
}

/// Drains `events` until the `Delivery` of `ticket` and returns its outcome, after
/// checking that the ticket holds the same one. Every other event goes to `seen`.
pub fn outcome_where(
    term: &AgentTerminal,
    events: &Receiver<Event>,
    ticket: &Ticket,
    mut seen: impl FnMut(Event),
) -> DeliveryOutcome {
    let deadline = Instant::now() + DELIVERY_WAIT;
    loop {
        match events.recv_deadline(deadline) {
            Ok(Event::Delivery {
                ticket: id,
                outcome,
                ..
            }) if id == ticket.id() => {
                assert_eq!(ticket.wait(WAIT), Some(outcome.clone()), "ticket {id}");
                return outcome;
            }
            Ok(Event::Delivery { ticket: id, .. }) => {
                panic!("delivery {id} came before {}", ticket.id())
            }
            Ok(event) => seen(event),
            Err(e) => panic!(
                "no delivery event for ticket {} ({e}); ticket {:?}; {}",
                ticket.id(),
                ticket.try_outcome(),
                bottom_rows(&term.terminal().screen().unwrap())
            ),
        }
    }
}

/// [`outcome_where`], collecting the agent states seen meanwhile into `states`.
pub fn outcome_with_states(
    term: &AgentTerminal,
    events: &Receiver<Event>,
    ticket: &Ticket,
    states: &mut Vec<(AgentState, bool)>,
) -> DeliveryOutcome {
    outcome_where(term, events, ticket, |event| {
        if let Event::AgentState {
            state, blocking, ..
        } = event
        {
            states.push((state, blocking));
        }
    })
}

/// [`outcome_where`], appending the output seen meanwhile to `output`.
pub fn outcome_with_output(
    term: &AgentTerminal,
    events: &Receiver<Event>,
    ticket: &Ticket,
    output: &mut String,
) -> DeliveryOutcome {
    outcome_where(term, events, ticket, |event| collect_output(event, output))
}

pub fn collect_output(event: Event, output: &mut String) {
    if let Event::Output { bytes, .. } = event {
        output.push_str(&String::from_utf8_lossy(&bytes));
    }
}

pub fn outcome(term: &AgentTerminal, events: &Receiver<Event>, ticket: &Ticket) -> DeliveryOutcome {
    outcome_where(term, events, ticket, drop)
}

/// The last launch line (`false`) ran on its own: the last rows are `$ false` and an
/// empty prompt. Appends the output until then (and a little after) to `output`.
pub fn assert_launched_on_a_clear_line(
    term: &AgentTerminal,
    events: &Receiver<Event>,
    output: &mut String,
) {
    let launched = |s: &ScreenSnapshot| {
        let rows: Vec<&str> = s
            .rows
            .iter()
            .map(|r| r.trim_end())
            .filter(|r| !r.is_empty())
            .collect();
        rows.ends_with(&["$ false", "$"])
    };
    let deadline = Instant::now() + WAIT;
    while !launched(&term.terminal().screen().unwrap()) {
        match events.recv_deadline(deadline) {
            Ok(event) => collect_output(event, output),
            Err(e) => panic!(
                "no clean launch ({e}); {}",
                bottom_rows(&term.terminal().screen().unwrap())
            ),
        }
    }
    while let Ok(event) = events.recv_timeout(Duration::from_millis(300)) {
        collect_output(event, output);
    }
}

/// No marker line ran (its output is `PWNED-<digits>`; the typed text shows `PWNED-$((`),
/// and no launch line lost characters (a shell's "command not found").
pub fn assert_nothing_ran(name: &str, output: &str) {
    let ran = output
        .match_indices("PWNED-")
        .any(|(at, m)| output[at + m.len()..].starts_with(|c: char| c.is_ascii_digit()));
    assert!(!ran, "{name}: a marker ran: {output:?}");
    assert!(!output.contains("not found"), "{name}: {output:?}");
}
