//! Unit tests of the controller state: owners the policy trusts, the agent's program,
//! the rows drawn since a launch, events, tickets, the Ctrl-C before a launch line and
//! the launch steps held back after it, and when the agent may have left unseen.

use std::time::Duration;

use super::*;
use crate::agent::{Dialog, DialogKind, SendOptions};

/// Panics on every dialog check (so on every screen it classifies).
struct Broken;

impl AgentProfile for Broken {
    fn id(&self) -> &str {
        "broken"
    }

    fn dialog(&self, _: &str) -> Option<Dialog> {
        panic!("profile bug")
    }

    fn input_ready(&self, _: &str) -> bool {
        false
    }
}

/// Ready on a screen holding `READY`; a dialog left to the user on one holding
/// `DIALOG`.
struct Marker;

impl AgentProfile for Marker {
    fn id(&self) -> &str {
        "marker"
    }

    fn dialog(&self, screen: &str) -> Option<Dialog> {
        screen.contains("DIALOG").then_some(Dialog {
            kind: DialogKind::Trust,
            key: None,
        })
    }

    fn input_ready(&self, screen: &str) -> bool {
        screen.contains("READY")
    }
}

fn config(profile: Option<Arc<dyn AgentProfile>>, policy: ForegroundPolicy) -> Config {
    Config {
        id: SessionId(7),
        profile_id: profile.as_ref().map_or_else(String::new, |p| p.id().into()),
        profile,
        launch_line: None,
        reset_command: None,
        shell: ShellKind::Posix,
        trust: TrustPolicy::default(),
        timing: Timing::default(),
        limits: super::super::AgentLimits::default(),
        foreground_policy: policy,
        trace: false,
        identity: TerminalIdentity::Honest,
    }
}

fn state(profile: Option<Arc<dyn AgentProfile>>) -> CtlState {
    CtlState::new(Arc::new(config(profile, ForegroundPolicy::default())))
}

/// A look at the terminal: the shell in front, or a program the policy does not name.
fn front(shell: bool) -> FrontRead {
    FrontRead {
        fg: Foreground {
            owner: if shell {
                ForegroundOwner::Shell
            } else {
                ForegroundOwner::Unknown
            },
            confidence: Confidence::Exact,
        },
        shell,
        program: None,
        shell_lines: 0,
    }
}

/// Exact answers always count; heuristic ones only under `AcceptHeuristic`; an unknown
/// owner never.
#[test]
fn the_policy_decides_which_owners_count() {
    use ForegroundOwner::{Agent, Shell, Unknown};
    let fg = |owner, confidence| Foreground { owner, confidence };
    let exact = config(None, ForegroundPolicy::ExactOnly);
    let heuristic = config(None, ForegroundPolicy::AcceptHeuristic);
    for cfg in [&exact, &heuristic] {
        assert_eq!(cfg.trusted_owner(fg(Shell, Confidence::Exact)), Some(Shell));
        assert_eq!(cfg.trusted_owner(fg(Agent, Confidence::Exact)), Some(Agent));
        assert_eq!(cfg.trusted_owner(fg(Unknown, Confidence::Exact)), None);
        assert_eq!(cfg.trusted_owner(fg(Unknown, Confidence::Heuristic)), None);
    }
    assert_eq!(exact.trusted_owner(fg(Shell, Confidence::Heuristic)), None);
    assert_eq!(exact.trusted_owner(fg(Agent, Confidence::Heuristic)), None);
    assert_eq!(
        heuristic.trusted_owner(fg(Shell, Confidence::Heuristic)),
        Some(Shell)
    );
    assert_eq!(
        heuristic.trusted_owner(fg(Agent, Confidence::Heuristic)),
        Some(Agent)
    );
}

/// Unknown or untrusted ownership never permits shell fallback, with or without a
/// profile. A host must explicitly accept heuristic answers before they count.
#[test]
fn shell_fallback_requires_trusted_shell_ownership() {
    use ForegroundOwner::{Agent, Shell, Unknown};
    let fg = |owner, confidence| Foreground { owner, confidence };
    let bare = config(None, ForegroundPolicy::ExactOnly);
    let agent = config(Some(Arc::new(Broken)), ForegroundPolicy::ExactOnly);
    for cfg in [&bare, &agent] {
        assert!(cfg.shell_in_front(fg(Shell, Confidence::Exact)));
        assert!(!cfg.shell_in_front(fg(Agent, Confidence::Exact)));
    }
    for answer in [
        fg(Shell, Confidence::Heuristic),
        fg(Agent, Confidence::Heuristic),
        fg(Unknown, Confidence::Exact),
    ] {
        assert!(!bare.shell_in_front(answer), "{answer:?}");
        assert!(!agent.shell_in_front(answer), "{answer:?}");
    }
    let heuristic = config(None, ForegroundPolicy::AcceptHeuristic);
    assert!(!heuristic.shell_in_front(fg(Agent, Confidence::Heuristic)));
}

/// A look at the terminal: the shell (`program` `None`) or that program in front, after
/// `shell_lines` host lines to the shell.
fn look(program: Option<u32>, shell_lines: u64) -> FrontRead {
    let owner = match program {
        Some(_) => ForegroundOwner::Agent,
        None => ForegroundOwner::Shell,
    };
    FrontRead {
        fg: Foreground {
            owner,
            confidence: Confidence::Exact,
        },
        shell: program.is_none(),
        program: program.map(ProgramId),
        shell_lines,
    }
}

/// A launch line typed into the shell expects the agent's program: the first program
/// seen is it, once — unless the host submitted a line to the shell first (the user may
/// have started that program): then none is known until the next launch. The agent
/// leaving forgets it; a launch line typed into a program teaches nothing.
#[test]
fn the_agent_s_program_is_the_first_after_a_launch_into_the_shell() {
    let mut st = state(None);
    let now = Instant::now();
    assert_eq!(st.program, AgentProgram::Unknown);
    let _ = st.type_launch("agent\n", now, &look(None, 3), Leftover::default());
    assert_eq!(st.program, AgentProgram::Expected { shell_lines: 3 });
    st.observe(&look(None, 3));
    assert!(st.expects_program(), "the shell has not run it yet");
    st.observe(&look(Some(4300), 3));
    assert_eq!(st.program, AgentProgram::Known(ProgramId(4300)));
    assert!(!st.expects_program());
    st.observe(&look(Some(4400), 4));
    assert_eq!(
        st.program,
        AgentProgram::Known(ProgramId(4300)),
        "learned once"
    );
    let _ = st.type_launch("agent\n", now, &look(Some(4300), 4), Leftover::default());
    assert_eq!(st.program, AgentProgram::Known(ProgramId(4300)));
    st.agent_left();
    assert_eq!(st.program, AgentProgram::Unknown);

    let _ = st.type_launch("agent\n", now, &look(None, 4), Leftover::default());
    st.observe(&look(Some(4500), 5));
    assert_eq!(
        st.program,
        AgentProgram::Unknown,
        "a line went to the shell first"
    );
    st.observe(&look(Some(4500), 5));
    assert_eq!(st.program, AgentProgram::Unknown);
    let _ = st.type_launch("agent\n", now, &look(Some(4500), 5), Leftover::default());
    assert_eq!(st.program, AgentProgram::Unknown, "typed into a program");
}

/// The screen a launch line is typed over can hold the last agent's ready composer: the
/// gate reads only the rows drawn since, so the shell's `^C` and prompt do not make it
/// ready — the new agent's own composer does.
#[test]
fn a_leftover_composer_does_not_ready_the_gate() {
    let mut st = state(Some(Arc::new(Marker)));
    let now = Instant::now();
    let leftover = Leftover::new(vec!["old READY".into(), "$ ".into()]);
    let _ = st.type_launch("agent\n", now, &look(None, 0), leftover);
    st.output_for_test(b"^C", Some("old READY\n$ ^C\n$ "), now);
    assert!(!st.gate.ready(), "the leftover composer");
    assert_eq!(st.leftover().fresh("old READY\n$ ^C\n$ "), "$ ^C\n$ ");
    st.output_for_test(b"new", Some("old READY\n$ ^C\n$ agent\nnew READY"), now);
    assert!(st.gate.ready(), "the new agent's composer");
}

/// Titles, progress and the session's end, then the events queued.
fn run(st: &mut CtlState) -> Vec<Event> {
    let now = Instant::now();
    st.on_title("\u{280b} spin", now);
    st.on_progress(true, now);
    st.output_for_test(b"$ ", None, now);
    st.on_session_end();
    st.sync_events();
    st.outbox.drain(..).map(|(_, event)| event).collect()
}

#[test]
fn a_bare_shell_queues_no_agent_events() {
    let mut st = state(None);
    assert!(run(&mut st).is_empty());
    assert_eq!(st.agent_state(), AgentState::Exited);
    assert_eq!(st.activity.state(), Activity::Unknown);
}

#[test]
fn with_a_profile_activity_and_the_end_are_published() {
    let mut st = state(Some(Arc::new(Broken)));
    let id = SessionId(7);
    assert_eq!(
        run(&mut st),
        [
            Event::AgentState {
                id,
                state: AgentState::Exited,
                blocking: false
            },
            Event::Activity {
                id,
                state: Activity::Idle
            },
        ]
    );
}

#[test]
fn the_view_counts_output_frames_and_cleared_progress() {
    let mut st = state(None);
    let now = Instant::now();
    st.output_for_test(b"a", None, now);
    st.on_frame(now, 0);
    st.on_progress(true, now);
    assert_eq!(st.view.last_progress_clear, None, "working is no evidence");
    st.on_progress(false, now);
    st.output_for_test(b"b", None, now);
    assert!(st.view.unseen(3) && !st.view.unseen(2));
    assert_eq!(st.view.output_serial, 2);
    assert_eq!(st.view.last_output_at, Some(now));
    let evidence = Evidence { at: now, serial: 1 };
    assert_eq!(st.view.last_frame_end, Some(evidence));
    assert_eq!(st.view.last_progress_clear, Some(evidence));
}

/// A chunk that ends a frame and begins the next leaves the screen mid-redraw: its
/// frame end is no evidence of a settled screen.
#[test]
fn a_frame_end_with_the_next_frame_open_is_no_evidence() {
    let mut st = state(None);
    let now = Instant::now();
    st.output_for_test(b"frame", None, now);
    st.on_frame(now, 0);
    let settled = Some(Evidence { at: now, serial: 1 });
    assert_eq!(st.view.last_frame_end, settled);
    // Chunk 2 ends a frame and opens the next (the reader noted chunk 2).
    st.output_for_test(b"end, begin", None, now);
    st.on_frame(now, 2);
    assert_eq!(st.view.last_frame_end, settled, "not chunk 2");
    // Chunk 3 closes it: evidence again (the note still names chunk 2).
    st.output_for_test(b"end", None, now);
    st.on_frame(now, 2);
    assert_eq!(
        st.view.last_frame_end,
        Some(Evidence { at: now, serial: 3 })
    );
}

#[test]
fn tickets_count_from_one_and_cancellation_names_its_cause() {
    let mut st = state(None);
    assert_eq!(
        (st.next_ticket(), st.next_ticket()),
        (TicketId(1), TicketId(2))
    );
    assert_eq!(st.cancel_reason(), None);
    st.on_session_end();
    assert_eq!(st.cancel_reason(), Some(AbortReason::NotConnected));
    st.disposed = true;
    assert_eq!(st.cancel_reason(), Some(AbortReason::Disposed));
}

/// Every ended delivery is published with the outcome its ticket gets.
#[test]
fn every_ended_delivery_is_published() {
    let mut st = state(None);
    for outcome in [
        DeliveryOutcome::Delivered,
        DeliveryOutcome::PastedUnsubmitted,
        DeliveryOutcome::Aborted(AbortReason::NotConnected),
        DeliveryOutcome::Aborted(AbortReason::Disposed),
    ] {
        assert_eq!(st.delivery_ended(TicketId(2), outcome.clone()), outcome);
        let event = st.outbox.pop_front().map(|(_, event)| event);
        assert_eq!(
            event,
            Some(Event::Delivery {
                id: SessionId(7),
                ticket: TicketId(2),
                outcome,
            })
        );
    }
}

/// The drop ends the running delivery and the queued ones `Disposed`, queued in ticket
/// order; the running one, ending later whatever way, gets `Disposed` too and no second
/// event.
#[test]
fn the_drop_publishes_disposed_for_the_running_and_the_queued_deliveries() {
    use crate::agent::controller::{Job, MessageJob};
    use crate::agent::delivery::ticket;
    let mut st = state(None);
    let mut tickets = Vec::new();
    for _ in 0..3 {
        let id = st.next_ticket();
        let (t, resolver) = ticket(id);
        st.track_resolution(id, resolver.resolution());
        tickets.push(t);
        st.jobs.push_back(Job::Message {
            job: MessageJob::new(id, "text", SendOptions::default()),
            resolver,
        });
    }
    // The job thread took the first one.
    let running = st.jobs.pop_front().unwrap();
    st.running = Some(running.ticket());
    let (queued, resolutions) = st.dispose();
    for resolution in resolutions {
        resolution.resolve(DeliveryOutcome::Aborted(AbortReason::Disposed));
    }
    assert!(st.disposed && st.jobs.is_empty() && st.running.is_none());
    assert_eq!(queued.len(), 2);
    let disposed = DeliveryOutcome::Aborted(AbortReason::Disposed);
    let events: Vec<Event> = st.outbox.drain(..).map(|(_, event)| event).collect();
    let want: Vec<Event> = (1..=3)
        .map(|n| Event::Delivery {
            id: SessionId(7),
            ticket: TicketId(n),
            outcome: disposed.clone(),
        })
        .collect();
    assert_eq!(events, want);
    // Even the running job is resolved before it wakes or its Resolver is dropped.
    assert_eq!(tickets[0].try_outcome(), Some(disposed.clone()));
    assert!(st.resolutions.is_empty());
    drop(queued);
    assert_eq!(tickets[1].try_outcome(), Some(disposed.clone()));
    assert_eq!(tickets[2].try_outcome(), Some(disposed.clone()));
    // The running job finishes after the drop: its ticket agrees with the event sent.
    assert_eq!(
        st.delivery_ended(TicketId(1), DeliveryOutcome::Delivered),
        disposed
    );
    assert!(st.outbox.is_empty());
    drop(running);
}

/// A handshake keeps only the answer for the delivery that awaits it, once.
#[test]
fn a_commit_point_takes_only_its_own_answer() {
    let mut st = state(None);
    assert!(!st.confirm(TicketId(1), true), "nothing awaits an answer");
    st.await_commit(TicketId(4));
    assert_eq!(
        st.outbox.pop_front().map(|(_, event)| event),
        Some(Event::CommitPoint {
            id: SessionId(7),
            ticket: TicketId(4),
        })
    );
    assert_eq!(st.take_answer(TicketId(4)), None, "not answered yet");
    assert!(!st.confirm(TicketId(3), true), "another ticket");
    assert!(st.confirm(TicketId(4), false));
    assert!(!st.confirm(TicketId(4), true), "the first answer counts");
    assert_eq!(st.take_answer(TicketId(3)), None);
    assert_eq!(st.take_answer(TicketId(4)), Some(false));
    assert_eq!(st.take_answer(TicketId(4)), None, "taken once");
    // Timed out: a late answer is ignored.
    st.await_commit(TicketId(5));
    st.end_commit();
    assert!(!st.confirm(TicketId(5), true));
    assert_eq!(st.take_answer(TicketId(5)), None);
    // The drop ends a handshake too.
    st.await_commit(TicketId(6));
    drop(st.dispose());
    assert!(!st.confirm(TicketId(6), true));
}

/// A monitor that stopped after a panic feeds no wait any more: deliveries end, and the
/// overlay nothing would lift any more is lifted (published once).
#[test]
fn a_failed_monitor_ends_deliveries() {
    let mut st = state(Some(Arc::new(Broken)));
    st.launch(Instant::now());
    st.sync_events();
    st.outbox.clear();
    assert_eq!(st.cancel_reason(), None);
    assert!(st.blocking());
    st.monitor_failed = true;
    assert_eq!(st.cancel_reason(), Some(AbortReason::NotConnected));
    assert_eq!(
        st.agent_state(),
        AgentState::Starting,
        "the state is frozen"
    );
    assert!(!st.blocking());
    st.sync_events();
    let events: Vec<Event> = st.outbox.drain(..).map(|(_, event)| event).collect();
    assert_eq!(
        events,
        [Event::AgentState {
            id: SessionId(7),
            state: AgentState::Starting,
            blocking: false,
        }]
    );
}

/// The shell in front: `ShellOnly`, nothing masked, no timers, activity idle — until
/// the next launch.
#[test]
fn an_agent_that_left_is_shell_only_until_launched_again() {
    let mut st = state(Some(Arc::new(Broken)));
    let now = Instant::now();
    st.launch(now);
    assert_eq!(
        (st.agent_state(), st.gate.blocking()),
        (AgentState::Starting, true)
    );
    st.agent_left();
    assert_eq!(
        (st.agent_state(), st.gate.blocking()),
        (AgentState::ShellOnly, false)
    );
    assert!(!st.gate.running());
    assert_eq!(st.gate.next_deadline(), None);
    assert_eq!(st.activity.state(), Activity::Idle);
    st.launch(now);
    assert_eq!(st.agent_state(), AgentState::Starting);
}

/// Every launch line clears the shell's input line first (cmd.exe has no clear).
#[test]
fn a_launch_clears_the_input_line_first() {
    assert_eq!(launch_keys(ShellKind::Posix, "agent\n"), " \u{15}agent\n");
    assert_eq!(launch_keys(ShellKind::Cmd, "agent\r"), "agent\r");
    let mut st = state(None);
    let now = Instant::now();
    let keys = LaunchKeys::Now(" \u{15}agent\n".into());
    assert_eq!(
        st.type_launch("agent\n", now, &front(false), Leftover::default()),
        keys
    );
    assert_eq!(
        st.type_launch("agent\n", now, &front(false), Leftover::default()),
        keys
    );
}

/// The keys written by `type_keys` and the monitor (`Action::Write`), taken.
fn writes(st: &mut CtlState) -> Vec<Vec<u8>> {
    std::mem::take(&mut st.actions)
        .into_iter()
        .filter_map(|a| match a {
            Action::Write(bytes) => Some(bytes),
            Action::Log(..) => None,
        })
        .collect()
}

/// A Ctrl-C precedes a launch line typed with the shell in front — but not the first
/// thing the crate types into the session (the shell may still read its startup files).
#[test]
fn a_ctrl_c_goes_first_once_the_crate_typed_something() {
    let now = Instant::now();
    let interrupts = |st: &mut CtlState, shell| {
        matches!(
            st.type_launch("agent\n", now, &front(shell), Leftover::default()),
            LaunchKeys::AfterInterrupt(_)
        )
    };
    let mut st = state(None);
    assert!(!interrupts(&mut st, true), "first launch");
    assert!(interrupts(&mut st, true));
    assert!(
        !interrupts(&mut st, false),
        "a program, or an unknown owner, in front"
    );
    // A launch with another owner in front counts as typed too, and so does a paste.
    let mut st = state(None);
    assert!(!interrupts(&mut st, false));
    assert!(interrupts(&mut st, true));
    let mut st = state(None);
    st.note_paste();
    assert!(interrupts(&mut st, true));
}

/// A Ctrl-C the crate wrote a moment ago (before an Enter it then did not write) and
/// still being handled: a launch line now gets its own Ctrl-C only once that one
/// settled — never two interrupts in one handling — and is typed once the shell took
/// the second. A settled one, or one a paste followed, changes nothing.
#[test]
fn a_launch_right_after_a_ctrl_c_spaces_its_own() {
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let shell = front(true);
    let mut st = state(None);
    st.note_paste();
    st.output_for_test(b"pasted", None, t0);
    st.note_interrupt(Interrupt { fence: 1, at: t0 });
    assert_eq!(
        st.type_launch("agent\n", t0 + ms(5), &shell, Leftover::default()),
        LaunchKeys::Held
    );
    // The shell answers the first at +30 ms: settled 150 ms later; then the second,
    // which the monitor writes (the number) and fences after the write.
    // (The armed gate keeps deadlines of its own: probe with the deadline pass.)
    st.output_for_test(b"^C\r\n$ ", None, t0 + ms(30));
    assert_eq!(st.next_deadline(), Some(t0 + ms(180)));
    assert_eq!(st.deadlines_for_test(t0 + ms(179), "", Some(&shell)), None);
    assert!(!st.launch_due(t0 + ms(179)) && st.launch_due(t0 + ms(180)));
    let number = st.deadlines_for_test(t0 + ms(180), "", Some(&shell));
    assert!(number.is_some());
    assert!(writes(&mut st).is_empty(), "the monitor writes it");
    // While it is written nothing is due; then its fence.
    assert!(!st.launch_due(t0 + ms(9000)));
    assert!(st.launch_held());
    st.launch_interrupted(
        number.unwrap(),
        Some(Interrupt {
            fence: 2,
            at: t0 + ms(180),
        }),
    );
    // Nothing after the second yet: only its cap would type the keys.
    assert_eq!(st.deadlines_for_test(t0 + ms(1179), "", Some(&shell)), None);
    assert!(writes(&mut st).is_empty());
    // Its answer at +200 ms; the keys 150 ms later.
    st.output_for_test(b"^C\r\n$ ", None, t0 + ms(200));
    st.deadlines_for_test(t0 + ms(349), "", Some(&shell));
    assert!(writes(&mut st).is_empty());
    st.deadlines_for_test(t0 + ms(350), "", Some(&shell));
    assert_eq!(writes(&mut st), [b" \x15agent\n".to_vec()]);
    st.deadlines_for_test(t0 + ms(2000), "", Some(&shell));
    assert!(writes(&mut st).is_empty(), "typed once");
    // Typed after it: the next launch interrupts at once again.
    assert!(matches!(
        st.type_launch("agent\n", t0 + ms(400), &front(true), Leftover::default()),
        LaunchKeys::AfterInterrupt(_)
    ));
    // A settled Ctrl-C (here: its cap passed) holds nothing back; nor does one a paste
    // followed.
    st.note_interrupt(Interrupt { fence: 3, at: t0 });
    assert!(matches!(
        st.type_launch("agent\n", t0 + ms(1000), &front(true), Leftover::default()),
        LaunchKeys::AfterInterrupt(_)
    ));
    st.note_interrupt(Interrupt {
        fence: 3,
        at: t0 + ms(1000),
    });
    st.note_paste();
    assert!(matches!(
        st.type_launch("agent\n", t0 + ms(1010), &front(true), Leftover::default()),
        LaunchKeys::AfterInterrupt(_)
    ));
}

/// A launch typed with the shell in front once the crate typed into the session: its
/// Ctrl-C (`type_keys` writes it) was written with the fence `after`. The keys are held.
fn hold(st: &mut CtlState, at: Instant, after: Interrupt) {
    st.note_paste();
    let launch = st.type_launch("agent\n", at, &front(true), Leftover::default());
    let LaunchKeys::AfterInterrupt(number) = launch else {
        panic!("{launch:?}");
    };
    st.launch_interrupted(number, Some(after));
}

/// The launch line after a Ctrl-C waits for the shell's answer and a short quiet (or
/// the cap); then the monitor's deadline pass types it once.
#[test]
fn a_held_back_launch_line_is_typed_once_the_shell_took_the_ctrl_c() {
    let mut st = state(None);
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let after = |fence| Interrupt { fence, at: t0 };
    let shell = front(true);
    st.output_for_test(b"$ ", None, t0);
    hold(&mut st, t0, after(1));
    assert!(st.launch_held());
    assert_eq!(st.next_deadline(), Some(t0 + ms(1000)), "the cap");
    // The shell's answer at +20 ms: due 150 ms later; nothing typed before.
    st.output_for_test(b"^C\r\n$ ", None, t0 + ms(20));
    assert_eq!(st.next_deadline(), Some(t0 + ms(170)));
    st.deadlines_for_test(t0 + ms(100), "", Some(&shell));
    assert!(writes(&mut st).is_empty());
    // Due, but not looked at yet: the next pass reads the owner first.
    st.deadlines_for_test(t0 + ms(170), "", None);
    assert!(writes(&mut st).is_empty() && st.launch_held());
    st.deadlines_for_test(t0 + ms(170), "", Some(&shell));
    assert_eq!(writes(&mut st), [b" \x15agent\n".to_vec()]);
    assert!(!st.launch_held());
    assert!(!st.launch_due(t0 + ms(9000)));
    st.deadlines_for_test(t0 + ms(2000), "", Some(&shell));
    assert!(writes(&mut st).is_empty(), "typed once");

    // A new launch replaces a held-back line; the session's end drops it.
    hold(&mut st, t0, after(2));
    let _ = st.type_launch("new\n", t0, &front(false), Leftover::default());
    let _ = writes(&mut st);
    st.deadlines_for_test(t0 + ms(2000), "", Some(&shell));
    assert!(writes(&mut st).is_empty());
    hold(&mut st, t0, after(2));
    st.on_session_end();
    st.deadlines_for_test(t0 + ms(2000), "", Some(&shell));
    assert!(writes(&mut st).is_empty());
    // A failed Ctrl-C write (the session is going away) drops it too.
    st.note_paste();
    let launch = st.type_launch("agent\n", t0, &shell, Leftover::default());
    let LaunchKeys::AfterInterrupt(number) = launch else {
        panic!("{launch:?}");
    };
    st.launch_interrupted(number, None);
    assert!(!st.launch_held());

    // After a monitor failure the held launch must not bypass automation cancellation.
    let mut st = state(None);
    st.monitor_failed = true;
    hold(&mut st, t0, after(0));
    assert!(writes(&mut st).is_empty());
    assert!(!st.launch_held());
}

/// A held step goes only to the shell the launch was decided with: a program in front
/// when it is due (started in the gap: it would get SIGINT and the launch line), or a
/// line the host submitted to the shell meanwhile (its program may not own the
/// terminal yet), drops the launch — nothing more typed, no agent runs, its program is
/// not expected any more.
#[test]
fn a_held_step_goes_only_to_the_shell_it_was_meant_for() {
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let after = Interrupt { fence: 1, at: t0 };
    let program = look(Some(4400), 0);
    let line_since = look(None, 1);
    for (seen, why) in [
        (&program, "the shell no longer owns the terminal"),
        (&front(false), "the shell no longer owns the terminal"),
        (&line_since, "a line was submitted to the shell meanwhile"),
    ] {
        // The keys after the Ctrl-C.
        let mut st = state(Some(Arc::new(Marker)));
        hold(&mut st, t0, after);
        assert_eq!(st.agent_state(), AgentState::Starting);
        assert_eq!(st.deadlines_for_test(t0 + ms(1000), "", Some(seen)), None);
        let traced = std::mem::take(&mut st.actions);
        assert!(
            !traced.iter().any(|a| matches!(a, Action::Write(_))),
            "{traced:?}"
        );
        assert!(!st.launch_held());
        assert_eq!(st.agent_state(), AgentState::ShellOnly);
        assert_eq!(st.program, AgentProgram::Unknown);
        // Its own Ctrl-C, held behind an earlier one.
        let mut st = state(Some(Arc::new(Marker)));
        st.note_paste();
        st.note_interrupt(after);
        let held = st.type_launch("agent\n", t0, &front(true), Leftover::default());
        assert_eq!(held, LaunchKeys::Held);
        assert_eq!(
            st.deadlines_for_test(t0 + ms(1000), "", Some(seen)),
            None,
            "{why}"
        );
        assert!(writes(&mut st).is_empty());
        assert!(!st.launch_held());
        assert_eq!(st.agent_state(), AgentState::ShellOnly);
    }
    // Traced with the reason.
    let mut st = CtlState::new(Arc::new(Config {
        trace: true,
        ..config(None, ForegroundPolicy::default())
    }));
    hold(&mut st, t0, after);
    st.actions.clear();
    st.deadlines_for_test(t0 + ms(1000), "", Some(&line_since));
    let lines: Vec<String> = std::mem::take(&mut st.actions)
        .into_iter()
        .map(|a| match a {
            Action::Log(_, line) => line,
            Action::Write(bytes) => panic!("wrote {bytes:?}"),
        })
        .collect();
    assert_eq!(
        lines,
        ["launch: not typed after the Ctrl-C, a line was submitted to the shell meanwhile"]
    );
}

/// The fence of a held launch's own Ctrl-C is the one reported after its write: output
/// counted before it — the tail of the shell's answer to the earlier one — is not the
/// shell's answer to this one. A report for a launch replaced meanwhile only makes the
/// new launch's own Ctrl-C wait for it.
#[test]
fn a_held_ctrl_c_is_fenced_after_its_write() {
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let shell = front(true);
    let mut st = state(None);
    st.note_paste();
    st.note_interrupt(Interrupt { fence: 0, at: t0 });
    let _ = st.type_launch("agent\n", t0, &shell, Leftover::default());
    st.output_for_test(b"^C\r\n$ ", None, t0 + ms(20));
    let number = st
        .deadlines_for_test(t0 + ms(170), "", Some(&shell))
        .unwrap();
    // More of the earlier answer arrives before the write, then the write: fence 2.
    st.output_for_test(b"\x1b[K", None, t0 + ms(175));
    st.launch_interrupted(
        number,
        Some(Interrupt {
            fence: 2,
            at: t0 + ms(176),
        }),
    );
    // (The armed gate keeps deadlines of its own: probe the launch's.)
    assert!(
        !st.launch_due(t0 + ms(1175)) && st.launch_due(t0 + ms(1176)),
        "no answer yet: the cap"
    );
    st.output_for_test(b"^C\r\n$ ", None, t0 + ms(190));
    assert!(!st.launch_due(t0 + ms(339)) && st.launch_due(t0 + ms(340)));

    // Replaced while its Ctrl-C is written: the new launch, itself held behind a
    // still-unsettled Ctrl-C, waits for this one before its own.
    let mut st = state(None);
    st.note_paste();
    st.note_interrupt(Interrupt { fence: 0, at: t0 });
    let _ = st.type_launch("agent\n", t0, &shell, Leftover::default());
    st.output_for_test(b"^C\r\n$ ", None, t0 + ms(20));
    let first = st
        .deadlines_for_test(t0 + ms(170), "", Some(&shell))
        .unwrap();
    st.note_interrupt(Interrupt {
        fence: 1,
        at: t0 + ms(170),
    });
    let held = st.type_launch("agent\n", t0 + ms(171), &shell, Leftover::default());
    assert_eq!(held, LaunchKeys::Held);
    st.launch_interrupted(
        first,
        Some(Interrupt {
            fence: 1,
            at: t0 + ms(172),
        }),
    );
    assert!(st.launch_held());
    assert!(
        !st.launch_due(t0 + ms(1171)) && st.launch_due(t0 + ms(1172)),
        "waits for the reported one"
    );
    assert!(writes(&mut st).is_empty());
}

/// A launch line held back after its Ctrl-C starts the agent only once it is typed: the
/// startup it armed (the trust window, the cap) restarts then, and until then a
/// delivery waits (`launch_held`).
#[test]
fn a_held_launch_line_starts_the_startup_when_typed() {
    let mut st = state(Some(Arc::new(Marker)));
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    hold(&mut st, t0, Interrupt { fence: 0, at: t0 });
    assert!(st.launch_held());
    st.output_for_test(b"^C\r\n$ ", None, t0 + ms(20));
    st.deadlines_for_test(t0 + ms(170), "", Some(&front(true)));
    assert_eq!(writes(&mut st), [b" \x15agent\n".to_vec()]);
    assert!(!st.launch_held());
    let cap = Timing::default().startup_max;
    assert_eq!(st.gate.next_deadline(), Some(t0 + ms(170) + cap));
}

/// The gate waits for an agent that may have left unseen once its program was seen, or
/// while a dialog is on screen — never while the launch line is held back, before the
/// agent was seen (the shell is in front until it started it), or once it is ready.
#[test]
fn an_agent_may_have_left_once_seen_or_while_a_dialog_is_up() {
    let t0 = Instant::now();
    let mut st = state(Some(Arc::new(Marker)));
    assert!(!st.could_have_left(), "nothing launched");
    let _ = st.type_launch("agent\n", t0, &look(None, 0), Leftover::default());
    assert!(!st.could_have_left(), "not seen yet");
    st.observe(&look(Some(4300), 0));
    assert!(st.could_have_left());
    st.output_for_test(b"READY", Some("READY"), t0);
    assert!(st.gate.ready() && !st.could_have_left(), "ready");

    let mut st = state(Some(Arc::new(Marker)));
    let _ = st.type_launch("agent\n", t0, &look(None, 0), Leftover::default());
    st.output_for_test(b"x", Some("DIALOG"), t0);
    assert_eq!(st.agent_state(), AgentState::TrustPrompt);
    assert!(st.could_have_left(), "a dialog, program not seen");

    let mut st = state(Some(Arc::new(Marker)));
    hold(&mut st, t0, Interrupt { fence: 0, at: t0 });
    st.program = AgentProgram::Known(ProgramId(4300));
    assert!(!st.could_have_left(), "held back");
}

/// A profile panic is caught by the gate: one `warn`, no key, and the
/// state keeps working (nothing poisoned).
#[test]
fn a_profile_panic_logs_one_warning() {
    let mut st = state(Some(Arc::new(Broken)));
    let now = Instant::now();
    st.launch(now);
    st.output_for_test(b"x", Some("trust dialog"), now);
    st.output_for_test(b"y", Some("trust dialog"), now);
    let warnings: Vec<String> = std::mem::take(&mut st.actions)
        .into_iter()
        .filter_map(|a| match a {
            Action::Log(log::Level::Warn, line) => Some(line),
            Action::Write(bytes) => panic!("wrote {bytes:?}"),
            Action::Log(..) => None,
        })
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].starts_with("profile broken panicked: profile bug; "),
        "{}",
        warnings[0]
    );
    assert_eq!(st.agent_state(), AgentState::Starting);
}

#[test]
fn admission_counts_running_jobs_and_unpublished_completions() {
    let cfg = Config {
        limits: AgentLimits {
            max_message_bytes: 4,
            max_pending_bytes: 4,
            max_pending_jobs: 2,
        },
        ..config(None, ForegroundPolicy::default())
    };
    let mut st = CtlState::new(Arc::new(cfg));
    let (id, reason) = st.reserve_delivery(4, 4);
    assert_eq!(reason, None);
    st.running = Some(id);
    assert_eq!(st.reserve_delivery(1, 1).1, Some(AbortReason::QueueFull));
    st.delivery_ended(id, DeliveryOutcome::Delivered);
    assert_eq!(st.reserve_delivery(1, 1).1, Some(AbortReason::QueueFull));
    assert_eq!(st.outbox.len(), 1);
    for _ in 0..10_000 {
        assert_eq!(
            st.reserve_delivery(5, 5).1,
            Some(AbortReason::InputTooLarge)
        );
    }
    assert_eq!(st.outbox.len(), 1, "refusals add no completion events");
    st.release_delivery(id);
    let (one, reason) = st.reserve_delivery(0, 0);
    assert_eq!(reason, None);
    let (two, reason) = st.reserve_delivery(0, 0);
    assert_eq!(reason, None);
    assert_eq!(st.reserve_delivery(0, 0).1, Some(AbortReason::QueueFull));
    st.release_delivery(one);
    st.release_delivery(two);
    assert_eq!(st.pending_bytes, 0);
    assert!(st.admissions.is_empty());
}

#[test]
fn control_write_failure_cancels_only_deliveries_already_admitted() {
    let mut st = state(None);
    let (old, _) = st.reserve_delivery(1, 1);
    st.running = Some(old);
    st.control_write_failed();
    assert_eq!(st.cancel_reason(), Some(AbortReason::WriteFailed));
    assert_eq!(
        st.delivery_ended(old, DeliveryOutcome::Delivered),
        DeliveryOutcome::Aborted(AbortReason::WriteFailed)
    );
    let (new, _) = st.reserve_delivery(1, 1);
    st.running = Some(new);
    assert_eq!(
        st.cancel_reason(),
        None,
        "subsequent delivery is not poisoned"
    );
}

#[cfg(unix)]
fn limit_controller() -> (
    crate::Terminal,
    Arc<Controller>,
    crossbeam_channel::Receiver<Event>,
) {
    let _spawn = crate::pty::test_spawn_lock();
    let spec = crate::SpawnSpec::direct(
        "/bin/sh",
        ["-c", "stty raw -echo; printf ready; exec sleep 60"],
        std::env::temp_dir(),
        crate::PtySize { rows: 24, cols: 80 },
    );
    let (term, events) = crate::Terminal::spawn(spec).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut text = String::new();
    while !text.contains("ready") {
        if let Event::Output { bytes, .. } = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            text.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    let (tx, _rx) = crossbeam_channel::bounded(1);
    let cfg = Config {
        id: term.id(),
        limits: AgentLimits {
            max_message_bytes: 4,
            max_pending_bytes: 8,
            max_pending_jobs: 2,
        },
        ..config(None, ForegroundPolicy::default())
    };
    let ctl = Arc::new(Controller::new(cfg, term.handle(), tx));
    (term, ctl, events)
}

#[cfg(unix)]
#[test]
fn publisher_holds_admission_while_receiver_is_full_then_releases_it() {
    let (_term, ctl, events) = limit_controller();
    // Fill fanout without involving a controller monitor or racing PTY output.
    for _ in events.len()..events.capacity().unwrap() {
        ctl.term.publish(Event::Frame { id: ctl.cfg.id });
    }
    for _ in 0..2 {
        let (id, reason) = ctl.reserve_delivery(4, 4);
        assert_eq!(reason, None);
        ctl.update_quietly(|st, _| st.delivery_ended(id, DeliveryOutcome::Delivered));
    }
    let publisher = {
        let ctl = Arc::clone(&ctl);
        std::thread::spawn(move || ctl.publish_all())
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ctl.read(|st| st.outbox.is_empty()) {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(
        ctl.read(|st| st.admissions.len()),
        2,
        "drained outbox is still in flight"
    );
    for _ in 0..10_000 {
        assert_eq!(ctl.reserve_delivery(0, 0).1, Some(AbortReason::QueueFull));
    }
    assert!(ctl.read(|st| st.outbox.is_empty()));
    while !publisher.is_finished() {
        let _ = events.recv_timeout(Duration::from_millis(10));
        assert!(Instant::now() < deadline);
    }
    publisher.join().unwrap();
    assert_eq!(
        ctl.read(|st| (st.admissions.len(), st.pending_bytes)),
        (0, 0)
    );
    assert_eq!(ctl.reserve_delivery(4, 4).1, None);
    ctl.dispose();
}

#[cfg(unix)]
#[test]
fn saturated_pty_control_write_fails_existing_delivery_without_poisoning_new_ones() {
    let (term, ctl, _events) = limit_controller();
    // Four maximum chunks occupy the default 4 MiB budget; the child never reads.
    let chunk = vec![b'x'; 1024 * 1024];
    for _ in 0..4 {
        term.write(&chunk).unwrap();
    }
    let (ticket, _) = ctl.reserve_delivery(1, 1);
    ctl.update_quietly(|st, now| {
        st.running = Some(ticket);
        st.launch(now);
    });
    ctl.perform(vec![Action::Write(b"\r".to_vec())]);
    assert_eq!(
        ctl.read(CtlState::cancel_reason),
        Some(AbortReason::WriteFailed)
    );
    assert_eq!(ctl.read(CtlState::agent_state), AgentState::ShellOnly);
    assert_eq!(
        ctl.update_quietly(|st, _| st.delivery_ended(ticket, DeliveryOutcome::Delivered)),
        DeliveryOutcome::Aborted(AbortReason::WriteFailed)
    );
    let (next, reason) = ctl.reserve_delivery(1, 1);
    assert_eq!(reason, None);
    ctl.update_quietly(|st, _| st.running = Some(next));
    assert_eq!(ctl.read(CtlState::cancel_reason), None);
    ctl.dispose();
}

#[cfg(unix)]
#[test]
fn monitor_owned_launch_interrupt_reports_saturated_input() {
    let (term, ctl, _events) = limit_controller();
    let chunk = vec![b'x'; 1024 * 1024];
    for _ in 0..4 {
        term.write(&chunk).unwrap();
    }
    let (ticket, _) = ctl.reserve_delivery(1, 1);
    ctl.update_quietly(|st, now| {
        st.running = Some(ticket);
        st.launch(now);
        st.pending_launch = Some(PendingLaunch {
            keys: "echo launch\n".into(),
            after: None,
            interrupt: true,
            shell_lines: 0,
            number: 7,
        });
    });
    assert!(matches!(ctl.interrupt_launch(7), Err(Error::QueueFull)));
    assert_eq!(
        ctl.read(CtlState::cancel_reason),
        Some(AbortReason::WriteFailed)
    );
    assert_eq!(ctl.read(CtlState::agent_state), AgentState::ShellOnly);
    assert!(!ctl.read(CtlState::launch_held));
    ctl.dispose();
}

// Pure-state adapters keep these tests explicit about profile evaluation. Production
// callers must go through Controller::evaluate_gate, outside its state mutex.
impl CtlState {
    fn output_for_test(&mut self, bytes: &[u8], screen: Option<&str>, now: Instant) {
        self.on_output(bytes, now);
        if let (Some(screen), Some(profile)) = (screen, self.cfg.profile.clone()) {
            let screen = self.leftover.fresh(screen);
            let effects = self.gate.evaluate(screen, profile.as_ref(), now);
            self.apply(effects, screen);
        }
    }

    fn deadlines_for_test(
        &mut self,
        now: Instant,
        screen: &str,
        front: Option<&FrontRead>,
    ) -> Option<u64> {
        if let Some(profile) = self.cfg.profile.clone() {
            let screen = self.leftover.fresh(screen);
            let effects = self.gate.on_deadline(now, screen, profile.as_ref());
            self.apply(effects, screen);
        }
        self.on_deadlines(now, front)
    }
}

#[cfg(unix)]
mod profile_isolation {
    use super::*;
    use crate::agent::{DialogKind, TrustKey};
    use crossbeam_channel::{Receiver, Sender, bounded};

    struct Blocked {
        dialog: bool,
        entered: Sender<()>,
        resume: Receiver<()>,
    }

    impl Blocked {
        fn block(&self) {
            let _ = self.entered.send(());
            let _ = self.resume.recv();
        }
    }

    impl AgentProfile for Blocked {
        fn id(&self) -> &str {
            "blocked"
        }
        fn dialog(&self, _: &str) -> Option<Dialog> {
            if self.dialog {
                self.block();
                Some(Dialog {
                    kind: DialogKind::Trust,
                    key: Some(TrustKey::Enter),
                })
            } else {
                None
            }
        }
        fn input_ready(&self, _: &str) -> bool {
            self.block();
            true
        }
    }

    struct Resume(Sender<()>);
    impl Drop for Resume {
        fn drop(&mut self) {
            let _ = self.0.try_send(());
        }
    }

    fn controller(
        dialog: bool,
    ) -> (
        crate::Terminal,
        Arc<Controller>,
        Receiver<Event>,
        Receiver<()>,
        Resume,
    ) {
        let spec = crate::SpawnSpec::direct(
            "/bin/sh",
            ["-c", "stty raw -echo; printf ready; exec cat"],
            std::env::temp_dir(),
            crate::PtySize { rows: 24, cols: 80 },
        );
        let (term, events) = {
            let _spawn = crate::pty::test_spawn_lock();
            crate::Terminal::spawn(spec).unwrap()
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut text = String::new();
        while !text.contains("ready") {
            if let Event::Output { bytes, .. } = events.recv_deadline(deadline).unwrap() {
                text.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        let (entered_tx, entered) = bounded(1);
        let (resume, resumed) = bounded(1);
        let profile: Arc<dyn AgentProfile> = Arc::new(Blocked {
            dialog,
            entered: entered_tx,
            resume: resumed,
        });
        let mut cfg = config(Some(profile), ForegroundPolicy::default());
        cfg.id = term.id();
        cfg.timing.dialog_settle = Duration::ZERO;
        cfg.trust.steer_dialogs = true;
        let (tx, _rx) = bounded(1);
        let ctl = Arc::new(Controller::new(cfg, term.handle(), tx));
        ctl.update_quietly(|st, now| st.launch(now));
        (term, ctl, events, entered, Resume(resume))
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Change {
        None,
        Relaunch,
        Resize,
        Dispose,
    }

    #[test]
    fn delayed_profile_results_cannot_ready_or_type_after_invalidation() {
        for dialog in [false, true] {
            for change in [
                Change::None,
                Change::Relaunch,
                Change::Resize,
                Change::Dispose,
            ] {
                let (term, ctl, events, entered, resume) = controller(dialog);
                let worker = {
                    let ctl = Arc::clone(&ctl);
                    std::thread::spawn(move || ctl.evaluate_gate(false))
                };
                entered.recv_timeout(Duration::from_secs(5)).unwrap();
                let before = ctl.term().output_chunks();
                match change {
                    Change::None => {}
                    Change::Relaunch => ctl.update_quietly(|st, now| st.launch(now)),
                    Change::Resize => term.resize(crate::PtySize { rows: 25, cols: 81 }).unwrap(),
                    Change::Dispose => ctl.dispose(),
                }
                drop(resume);
                assert_eq!(
                    worker.join().unwrap(),
                    change == Change::None,
                    "{change:?}, dialog {dialog}"
                );
                if change == Change::None && dialog {
                    let echoed = events.recv_timeout(Duration::from_secs(5)).unwrap();
                    assert!(matches!(echoed, Event::Output { bytes, .. } if bytes == b"\r"));
                } else if change == Change::None {
                    assert_eq!(ctl.read(CtlState::agent_state), AgentState::Ready);
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    assert_eq!(
                        ctl.term().output_chunks(),
                        before,
                        "stale dialog key was typed after {change:?}"
                    );
                    assert_ne!(ctl.read(CtlState::agent_state), AgentState::Ready);
                }
                ctl.dispose();
            }
        }
    }

    #[test]
    fn underlying_terminal_close_cancels_without_waiting_for_profile_return() {
        let (term, ctl, _events, entered, resume) = controller(false);
        let worker = {
            let ctl = Arc::clone(&ctl);
            std::thread::spawn(move || ctl.evaluate_gate(false))
        };
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let waiter = {
            let ctl = Arc::clone(&ctl);
            std::thread::spawn(move || {
                ctl.wait_until(Instant::now() + Duration::from_secs(5), |st| {
                    st.cancel_reason()
                })
            })
        };
        term.kill();
        let cancelled = waiter.join().unwrap();
        let refusal = ctl.reserve_delivery(1, 1).1;
        let state = ctl.read(CtlState::agent_state);
        drop(resume);
        assert!(!worker.join().unwrap());
        ctl.dispose();
        assert_eq!(cancelled, Some(AbortReason::NotConnected));
        assert_eq!(refusal, Some(AbortReason::NotConnected));
        assert_eq!(state, AgentState::Exited);
    }
}
