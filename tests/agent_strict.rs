//! Strict delivery, the commit-point handshake, and how deliveries end with the session and
//! the terminal — `AgentTerminal::send` against the fake agent (`agent-link-fake-agent`):
//! a strict submit and paste to the running agent (shell and direct session), the shell in
//! front after the relaunch (no inert paste, the agent marked gone), the agent leaving
//! after the paste or with its submit (no receipt claimed) or at the commit point (no
//! relaunch), a program started by hand (the proof fails first), a failed sync check, the
//! handshake (`Event::CommitPoint`, `confirm` yes / no / never / for another ticket; a
//! program started or a shell line submitted while an inert paste waits for it), a shell exit with
//! deliveries queued, and the drop (every ticket `Disposed`, published before the teardown;
//! the process tree gone).
//!
//! unix answers are exact and name the program, so at the paste, the initial submit and a
//! retried Enter the only failing proof is the shell — which the owner read before them
//! already ends `ShellOnly`. The heuristic and unnamed verdicts there are covered by the
//! unit tests in `src/agent/proof.rs` and the Windows-host checklist.
#![cfg(all(unix, feature = "agent"))]

mod common;

use std::time::{Duration, Instant};

use agent_link::agent::{
    AbortReason, AgentSpec, AgentState, AgentTerminal, DeliveryOutcome, Revalidate, SendOptions,
    TicketId, Timing,
};
use agent_link::{Event, ForegroundOwner, PtySize, SpawnSpec};
use common::agent::{spawn_agent, states_until};
use common::delivery::{
    DELIVERY_WAIT, collect_output, fake, long_window, outcome, outcome_where, outcome_with_output,
    shell, short_window, submit,
};
use common::fake::{READY_MARKER, fake_profile, unlaunchable_profile};
use common::{
    LAUNCH_WAIT, WAIT, bottom_rows, fake_agent_path, logger, screen_contains, wait_event,
    wait_screen, wait_until,
};
use crossbeam_channel::Receiver;

use DeliveryOutcome::{Aborted, Delivered};

/// A strict submit.
fn strict() -> SendOptions {
    SendOptions {
        strict: true,
        ..submit()
    }
}

/// A submit asking the host at its commit point, waiting up to `timeout`.
fn handshake(timeout: Duration) -> SendOptions {
    SendOptions {
        revalidate: Revalidate::Handshake { timeout },
        ..submit()
    }
}

/// Waits for the `CommitPoint` of `ticket`; every other event goes to `output` (its bytes).
fn commit_point(
    term: &AgentTerminal,
    events: &Receiver<Event>,
    ticket: TicketId,
    output: &mut String,
) {
    wait_event(
        term.terminal(),
        events,
        DELIVERY_WAIT,
        |event| match event {
            Event::CommitPoint { ticket: t, .. } => *t == ticket,
            Event::Delivery {
                ticket: t, outcome, ..
            } => {
                panic!("delivery {t} ended before the commit point: {outcome:?}")
            }
            other => {
                collect_output(other.clone(), output);
                false
            }
        },
    );
}

/// The composer is the fake's empty `> ` row, on its ready screen.
fn composer_empty(term: &AgentTerminal) {
    wait_screen(term.terminal(), WAIT, |s| {
        s.text().contains(READY_MARKER) && s.rows.iter().any(|r| r == ">")
    });
}

#[test]
fn a_strict_send_to_the_running_agent_is_delivered() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: long_window(),
        trace: Some(true),
        ..AgentSpec::new(fake_profile("composer", &[]))
    });
    // `send` launches the stopped agent first; the proofs find its program.
    let ticket = term.send("a strict message", strict());
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
    screen_contains(term.terminal(), "you: a strict message", WAIT);
    // Paste only: strict too.
    let paste = SendOptions {
        strict: true,
        ..SendOptions::default()
    };
    let ticket = term.send("a strict draft", paste);
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
    screen_contains(term.terminal(), "> a strict draft", WAIT);
    let lines = logger::lines_for(term.terminal().id());
    assert!(
        lines
            .iter()
            .any(|l| l.contains("delivery #1: start (submit true, strict true")),
        "{lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("strict proof")),
        "{lines:#?}"
    );
}

/// A direct session's child is the agent: any program in front is its own.
#[test]
fn a_strict_send_in_a_direct_session_is_delivered() {
    let home = tempfile::tempdir().unwrap();
    let size = PtySize { rows: 24, cols: 80 };
    let spec = SpawnSpec::direct(fake_agent_path(), ["composer"], home.path(), size);
    let (term, events) = spawn_agent(
        spec,
        AgentSpec {
            timing: long_window(),
            ..AgentSpec::new(fake_profile("composer", &[]))
        },
    );
    let ticket = term.send("a direct message", strict());
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
    screen_contains(term.terminal(), "you: a direct message", WAIT);
}

/// The "agent" is `false`: after the one relaunch the shell is still in front. A strict
/// send never takes the shell fallback, `PasteInert` or not: `ShellOnly`, nothing typed
/// onto the shell's line, and the agent is marked gone.
#[test]
fn a_strict_send_with_the_shell_in_front_types_nothing() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: short_window(),
        trace: Some(true),
        ..AgentSpec::new(unlaunchable_profile())
    });
    term.launch().unwrap();
    let mut output = String::new();
    for (text, submit) in [
        ("echo strict-$((6*7))", true),
        ("echo draft-$((6*7))", false),
    ] {
        let opts = SendOptions {
            submit,
            strict: true,
            ..SendOptions::default()
        };
        let ticket = term.send(text, opts);
        assert_eq!(
            outcome_with_output(&term, &events, &ticket, &mut output),
            Aborted(AbortReason::ShellOnly),
            "{text}"
        );
        assert_eq!(term.state(), AgentState::ShellOnly, "{text}");
    }
    while let Ok(event) = events.recv_timeout(Duration::from_millis(300)) {
        collect_output(event, &mut output);
    }
    for typed in ["strict-", "draft-"] {
        assert!(!output.contains(typed), "{typed} typed: {output:?}");
    }
    // Found at the shell fallback after the relaunch (or, when a read caught `false` still
    // running, by the proof at the commit point).
    let lines = logger::lines_for(term.terminal().id());
    assert!(
        lines.iter().any(|l| l.contains("strict proof")
            && (l.contains("still the shell") || l.contains("shell_only"))),
        "{lines:#?}"
    );
}

/// The fake exits right after drawing the submitted text: the screen shows the submit took,
/// but the proof after it finds the shell — a strict send claims no receipt (`ShellOnly`,
/// the agent marked gone); a non-strict one is delivered.
#[test]
fn a_strict_submit_the_agent_exits_with_is_not_claimed() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: long_window(),
        trace: Some(true),
        ..AgentSpec::new(fake_profile("exit-to-shell", &[]))
    });
    let ticket = term.send("the last message", strict());
    assert_eq!(
        outcome(&term, &events, &ticket),
        Aborted(AbortReason::ShellOnly)
    );
    screen_contains(term.terminal(), "you: the last message", WAIT);
    assert_eq!(term.state(), AgentState::ShellOnly);
    let lines = logger::lines_for(term.terminal().id());
    assert!(
        lines
            .iter()
            .any(|l| l.contains("strict proof (submit verification) failed: shell_only")),
        "{lines:#?}"
    );
}

/// The agent leaves after the paste without reading it (`--exit-on-input`): the read before
/// the Enter finds the shell — no Enter, its line interrupted, nothing runs.
#[test]
fn a_strict_send_gets_no_enter_after_the_agent_left() {
    let (term, events, _home) = shell(AgentSpec {
        timing: Timing {
            // By the Enter the shell has the terminal back (see agent_delivery.rs).
            paste_echo_quiet: Duration::from_millis(1000),
            paste_echo_max: Duration::from_millis(5000),
            ..long_window()
        },
        ..AgentSpec::new(fake_profile("composer", &["--exit-on-input"]))
    });
    term.launch().unwrap();
    states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
    let mut output = String::new();
    let ticket = term.send("; echo PWNED-$((1+1)) ;", strict());
    assert_eq!(
        outcome_with_output(&term, &events, &ticket, &mut output),
        Aborted(AbortReason::ShellOnly)
    );
    assert_eq!(term.state(), AgentState::ShellOnly);
    while let Ok(event) = events.recv_timeout(Duration::from_millis(500)) {
        collect_output(event, &mut output);
    }
    assert!(!output.contains("PWNED-2"), "{output:?}");
}

/// No agent runs and the user started `cat` by hand: no launch line is typed, and the
/// strict proof at the foreground fails first (no launch of this terminal started that
/// program) — `ForegroundUnproven`, nothing typed (`cat` and the tty would echo it).
#[test]
fn a_strict_send_to_a_program_started_by_hand_is_unproven() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: long_window(),
        trace: Some(true),
        ..AgentSpec::new(fake_profile("composer", &[]))
    });
    term.terminal().write(b"cat\n").unwrap();
    wait_until("cat in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Agent
    });
    let mut output = String::new();
    let ticket = term.send("; echo PWNED-$((1+1)) ;", strict());
    assert_eq!(
        outcome_with_output(&term, &events, &ticket, &mut output),
        Aborted(AbortReason::ForegroundUnproven)
    );
    term.terminal().write(b"\x04").unwrap();
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    while let Ok(event) = events.recv_timeout(Duration::from_millis(300)) {
        collect_output(event, &mut output);
    }
    for typed in ["PWNED", "agent-link-fake-agent"] {
        assert!(!output.contains(typed), "{typed} in {output:?}");
    }
    let lines = logger::lines_for(term.terminal().id());
    assert!(
        lines
            .iter()
            .any(|l| l.contains("strict proof (the foreground) failed: foreground_unproven")),
        "{lines:#?}"
    );
}

/// The sync check comes before the strict proof: `Revalidate`, nothing pasted.
#[test]
fn a_failed_sync_check_pastes_nothing() {
    let (term, events, _home) = fake("composer", &[]);
    let opts = SendOptions {
        revalidate: Revalidate::Sync(Box::new(|| false)),
        ..strict()
    };
    let ticket = term.send("a stale instruction", opts);
    assert_eq!(
        outcome(&term, &events, &ticket),
        Aborted(AbortReason::Revalidate)
    );
    composer_empty(&term);
    let screen = term.terminal().screen().unwrap();
    assert!(!screen.text().contains("stale"), "{}", bottom_rows(&screen));
}

/// The agent exits while a strict send waits at its commit point for the host's answer:
/// the proof right after it (*pre-paste readiness*) finds the shell — `ShellOnly`, nothing
/// pasted, the agent marked gone and **not** relaunched (a non-strict send relaunches and
/// pastes, see `agent_delivery.rs`).
#[test]
fn a_strict_send_finds_the_shell_at_its_commit_point() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: long_window(),
        trace: Some(true),
        ..AgentSpec::new(fake_profile("exit-to-shell", &[]))
    });
    term.launch().unwrap();
    let mut states = states_until(&term, &events, LAUNCH_WAIT, |s, _| s == AgentState::Ready);
    let opts = SendOptions {
        strict: true,
        ..handshake(LAUNCH_WAIT)
    };
    let ticket = term.send("; echo PWNED-$((1+1)) ;", opts);
    let mut output = String::new();
    commit_point(&term, &events, ticket.id(), &mut output);
    // The user submits something in the agent, which exits at it.
    term.terminal().write(b"bye\r").unwrap();
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    term.confirm(ticket.id(), true);
    let ended = outcome_where(&term, &events, &ticket, |event| match event {
        Event::AgentState {
            state, blocking, ..
        } => states.push((state, blocking)),
        event => collect_output(event, &mut output),
    });
    assert_eq!(ended, Aborted(AbortReason::ShellOnly));
    assert_eq!(term.state(), AgentState::ShellOnly);
    let launches = states
        .iter()
        .filter(|s| **s == (AgentState::Starting, true))
        .count();
    assert_eq!(launches, 1, "not relaunched: {states:?}");
    while let Ok(event) = events.recv_timeout(Duration::from_millis(300)) {
        collect_output(event, &mut output);
    }
    assert!(!output.contains("PWNED-$((1+1))"), "typed: {output:?}");
    let lines = logger::lines_for(term.terminal().id());
    assert!(
        lines
            .iter()
            .any(|l| l.contains("strict proof (pre-paste readiness) failed: shell_only")),
        "{lines:#?}"
    );
}

/// The handshake: nothing is pasted until the host answers; yes pastes and submits.
#[test]
fn a_confirmed_commit_point_is_delivered() {
    logger::install();
    let (term, events, _home) = shell(AgentSpec {
        timing: long_window(),
        trace: Some(true),
        ..AgentSpec::new(fake_profile("composer", &[]))
    });
    // An answer for a ticket nobody waits on is ignored.
    term.confirm(TicketId(7), true);
    let ticket = term.send("a confirmed message", handshake(LAUNCH_WAIT));
    let mut output = String::new();
    commit_point(&term, &events, ticket.id(), &mut output);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(ticket.try_outcome(), None, "waits for the answer");
    composer_empty(&term);
    term.confirm(ticket.id(), true);
    // A second answer changes nothing.
    term.confirm(ticket.id(), false);
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
    screen_contains(term.terminal(), "you: a confirmed message", WAIT);
    let id = term.terminal().id();
    logger::wait_line(id, "the host answered the commit point: paste", WAIT);
    // After it ended, an answer for it is ignored too.
    term.confirm(ticket.id(), true);
}

/// A bare shell's inert paste waits for the host's answer; the user starts `cat` meanwhile.
/// The owner is read again after the answer: `cat` gets nothing (`ForegroundUnproven`; it
/// and the tty would echo the text).
#[test]
fn a_program_started_during_the_handshake_gets_nothing() {
    let (term, events, _home) = shell(AgentSpec::default());
    let mut output = String::new();
    let ticket = term.send("echo inert-$((6*7))", handshake(LAUNCH_WAIT));
    commit_point(&term, &events, ticket.id(), &mut output);
    term.terminal().write(b"cat\n").unwrap();
    wait_until("cat in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Agent
    });
    term.confirm(ticket.id(), true);
    assert_eq!(
        outcome_with_output(&term, &events, &ticket, &mut output),
        Aborted(AbortReason::ForegroundUnproven)
    );
    term.terminal().write(b"\x04").unwrap();
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    while let Ok(event) = events.recv_timeout(Duration::from_millis(300)) {
        collect_output(event, &mut output);
    }
    assert!(!output.contains("inert-"), "typed: {output:?}");
}

/// The agent cannot start (`false`), so a send ends at the shell fallback's inert paste,
/// which waits for the host's answer. A line the host submits to the shell meanwhile (the
/// terminal counts them once a profile can launch) ends it `ShellOnly`, nothing typed.
#[test]
fn a_shell_line_submitted_during_the_handshake_ends_it() {
    let (term, events, _home) = shell(AgentSpec {
        timing: short_window(),
        ..AgentSpec::new(unlaunchable_profile())
    });
    let mut output = String::new();
    let ticket = term.send("echo inert-$((6*8))", handshake(LAUNCH_WAIT));
    commit_point(&term, &events, ticket.id(), &mut output);
    // Ran and done (the shell in front again) before the answer.
    term.terminal().write(b"echo done-$((2*3))\n").unwrap();
    screen_contains(term.terminal(), "done-6", WAIT);
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    term.confirm(ticket.id(), true);
    assert_eq!(
        outcome_with_output(&term, &events, &ticket, &mut output),
        Aborted(AbortReason::ShellOnly)
    );
    while let Ok(event) = events.recv_timeout(Duration::from_millis(300)) {
        collect_output(event, &mut output);
    }
    assert!(!output.contains("inert-"), "typed: {output:?}");
}

/// No, or no answer in time: `Revalidate`, nothing pasted; the next send is unaffected.
#[test]
fn a_refused_or_unanswered_commit_point_pastes_nothing() {
    let (term, events, _home) = fake("composer", &[]);
    let mut output = String::new();
    let refused = term.send("a refused message", handshake(LAUNCH_WAIT));
    commit_point(&term, &events, refused.id(), &mut output);
    // An answer for another ticket does not count.
    term.confirm(TicketId(refused.id().0 + 1), true);
    term.confirm(refused.id(), false);
    assert_eq!(
        outcome(&term, &events, &refused),
        Aborted(AbortReason::Revalidate)
    );

    let timeout = Duration::from_millis(400);
    let unanswered = term.send("an unanswered message", handshake(timeout));
    commit_point(&term, &events, unanswered.id(), &mut output);
    let asked = Instant::now();
    assert_eq!(
        outcome(&term, &events, &unanswered),
        Aborted(AbortReason::Revalidate)
    );
    assert!(asked.elapsed() >= timeout / 2, "{:?}", asked.elapsed());
    // Too late.
    term.confirm(unanswered.id(), true);
    composer_empty(&term);
    let screen = term.terminal().screen().unwrap();
    for text in ["refused", "unanswered"] {
        assert!(!screen.text().contains(text), "{}", bottom_rows(&screen));
    }
    let ticket = term.send("the next message", submit());
    assert_eq!(outcome(&term, &events, &ticket), Delivered);
}

/// The shell exits while a delivery waits at its commit point and two more are queued: all
/// three end `NotConnected` (the waiting handshake too).
#[test]
fn a_shell_exit_ends_the_waiting_and_the_queued_deliveries() {
    let (term, events, _home) = shell(AgentSpec::default());
    let waiting = term.send("echo waiting", handshake(LAUNCH_WAIT));
    let mut output = String::new();
    commit_point(&term, &events, waiting.id(), &mut output);
    let queued = [
        term.send("echo first", submit()),
        term.send("echo second", submit()),
    ];
    term.terminal().write(b"exit\n").unwrap();
    for ticket in std::iter::once(&waiting).chain(&queued) {
        assert_eq!(
            outcome(&term, &events, ticket),
            Aborted(AbortReason::NotConnected),
            "ticket {}",
            ticket.id()
        );
    }
    assert_eq!(term.state(), AgentState::Exited);
}

/// Dropping the terminal ends the delivery waiting for readiness and the queued one
/// `Disposed` — on their tickets at once, and as events published before the session is
/// torn down — and kills the shell and the agent.
#[test]
fn dropping_the_terminal_disposes_and_publishes_its_deliveries() {
    // Silent: never ready before the (long) trust window ends.
    let (term, events, _home) = fake("silent", &[]);
    let waiting = term.send("the first message", submit());
    let queued = term.send("the second message", submit());
    // The agent runs (a job of the shell) and the first delivery waits for it.
    wait_until("the agent in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Agent
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(waiting.try_outcome(), None);
    let shell_pid = term.terminal().pid().expect("the shell's pid") as libc::pid_t;
    let agent_pid = child_of(shell_pid);

    drop(term);
    let disposed = Some(Aborted(AbortReason::Disposed));
    assert_eq!(waiting.wait(Duration::from_secs(1)), disposed);
    assert_eq!(queued.wait(Duration::from_secs(1)), disposed);
    // Published before the bus closed, in ticket order.
    let mut delivered = Vec::new();
    let deadline = Instant::now() + WAIT;
    while let Ok(event) = events.recv_deadline(deadline) {
        if let Event::Delivery {
            ticket, outcome, ..
        } = event
        {
            delivered.push((ticket, outcome));
        }
    }
    let disposed = Aborted(AbortReason::Disposed);
    assert_eq!(
        delivered,
        [(waiting.id(), disposed.clone()), (queued.id(), disposed)]
    );
    for (what, pid) in [("the shell", shell_pid), ("the agent", agent_pid)] {
        wait_until(what, || {
            // SAFETY: signal 0 only checks that the process exists.
            let gone = unsafe { libc::kill(pid, 0) } != 0;
            gone && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        });
    }
}

/// A full receiver and host callback cannot delay cancellation or process teardown.
/// The callback is deliberately released only after drop has returned.
#[test]
fn dropping_with_a_full_receiver_resolves_even_a_job_blocked_in_host_code() {
    let (term, events, _home) = shell(AgentSpec::default());
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
    let (resume_tx, resume_rx) = crossbeam_channel::bounded(1);
    let waiting = term.send(
        "never pasted",
        SendOptions {
            revalidate: Revalidate::Sync(Box::new(move || {
                entered_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
                true
            })),
            ..SendOptions::default()
        },
    );
    entered_rx
        .recv_timeout(LAUNCH_WAIT)
        .expect("host callback started");
    let queued = term.send("also never pasted", submit());
    let shell_pid = term.terminal().pid().unwrap() as libc::pid_t;
    term.terminal().write(b"yes lifecycle-output\n").unwrap();
    wait_until("full retained event receiver", || events.is_full());
    let foreground_pid = child_of(shell_pid);
    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    let dropper = std::thread::spawn(move || {
        drop(term);
        done_tx.send(()).unwrap();
    });
    let completed = done_rx.recv_timeout(Duration::from_secs(5)).is_ok();
    if !completed {
        // Rescue a regressed implementation before failing so it leaves no child behind.
        drop(events);
        let _ = resume_tx.send(());
        let _ = done_rx.recv_timeout(WAIT);
        panic!("teardown waited for a receiver or host callback");
    }
    let disposed = Some(Aborted(AbortReason::Disposed));
    assert_eq!(waiting.try_outcome(), disposed);
    assert_eq!(queued.try_outcome(), disposed);
    assert!(
        events.is_full(),
        "the receiver stayed undrained throughout drop"
    );
    for pid in [shell_pid, foreground_pid] {
        wait_until("the killed process to disappear", || {
            // SAFETY: signal zero only queries existence.
            (unsafe { libc::kill(pid, 0) }) == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        });
    }
    resume_tx.send(()).unwrap();
    dropper.join().unwrap();
}

/// The monitor can hold its publication lock while fanout waits on a full receiver.
/// Drop must cancel that publisher rather than trying to acquire its lock first.
#[test]
fn dropping_with_a_blocked_controller_publisher_never_waits_for_the_receiver() {
    let home = tempfile::tempdir().unwrap();
    let (term, events) = spawn_agent(
        SpawnSpec::direct(
            "/bin/cat",
            std::iter::empty::<&str>(),
            home.path(),
            PtySize { rows: 24, cols: 80 },
        ),
        AgentSpec::default(),
    );
    let capacity = events.capacity().unwrap();
    for _ in 0..capacity + 2 {
        // One job at a time avoids depending on the size of the pending-job queue.
        let ticket = term.send("", SendOptions::default());
        assert_eq!(ticket.wait(WAIT), Some(Aborted(AbortReason::EmptyText)));
    }
    wait_until("full controller event receiver", || events.is_full());
    let pid = term.terminal().pid().unwrap() as libc::pid_t;
    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    let dropper = std::thread::spawn(move || {
        drop(term);
        done_tx.send(()).unwrap();
    });
    let completed = done_rx.recv_timeout(Duration::from_secs(5)).is_ok();
    if !completed {
        drop(events);
        let _ = done_rx.recv_timeout(WAIT);
        panic!("teardown waited for the blocked controller publisher");
    }
    assert!(events.is_full());
    // SAFETY: signal zero only queries existence.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    dropper.join().unwrap();
}

/// The one child process of `parent` (`pgrep -P`).
fn child_of(parent: libc::pid_t) -> libc::pid_t {
    let out = std::process::Command::new("pgrep")
        .arg("-P")
        .arg(parent.to_string())
        .output()
        .expect("run pgrep");
    let text = String::from_utf8_lossy(&out.stdout);
    let pids: Vec<libc::pid_t> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
    assert_eq!(pids.len(), 1, "children of {parent}: {text:?}");
    pids[0]
}
