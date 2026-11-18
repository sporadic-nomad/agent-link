//! The monitor thread (`agent-link-monitor-<id>`): feeds the session's events and the
//! clock into the controller state — output to the startup gate (which reads the screen
//! model after each chunk) and to the delivery waits' view (with frame ends and cleared
//! progress reports), titles and progress to the activity tracker, the exit to both —
//! fires their deadlines (also the ones that write a launch's held-back Ctrl-C and launch
//! line, each only after a fresh look finds the shell still in front), and publishes the
//! events the updates queued (the delivery outcomes included; at the drop the dropping
//! thread publishes the `Disposed` outcomes itself, `Controller::dispose`). After a launch
//! line went to the shell it
//! also reads who owns the terminal on every wake until the agent's program is known
//! (`CtlState::observe`): from the agent's first output on, before it could exit and leave
//! the terminal to a program the user starts.
//!
//! Events carry no arrival time, so every event already queued is applied before any
//! deadline fires: after a stall (publishing to a full host receiver) a quiet deadline must
//! not fire for output that arrived before it. Applied late, the output counts as arriving
//! now — deadlines move later, never earlier. A run of queued output chunks is noted chunk
//! by chunk but the screen is evaluated once for the run (it is the same screen).
//!
//! It publishes an event only once it counted the output chunks fed when the event was
//! queued (`Controller::publish_queued`): on every receiver an agent event follows the
//! output of the screen it was read from.
//!
//! Profile evaluations run outside the controller lock on a gate snapshot. If a custom
//! callback blocks, state reads and teardown remain available. The internal event queue
//! is bounded; overflow is logged and disables automation independently of this thread.
//!
//! A panic on this thread (a crate bug) is caught and logged at `error`; the controller
//! then stops updating: its state is frozen except that `blocking` is lifted (a host's
//! overlay would never come down) and the session's end still reads `Exited`. It sets
//! `monitor_failed`, so every delivery ends (`NotConnected`) instead of waiting for output
//! nobody counts, and the thread goes on only publishing the outbox (those outcomes and
//! states) and draining the session's events until teardown or dispose.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError, select};

use super::state::{Controller, MonitorMsg};
use crate::error::{Error, Result};
use crate::screen::catch_panic;
use crate::{Event, ProgressState};

/// How long the monitor sleeps when nothing is scheduled (it wakes on any event anyway).
const IDLE_WAIT: Duration = Duration::from_secs(3600);

/// Starts the monitor. `events` is subscribed before the session's first read and never
/// blocks the publisher: a bounded queue overflows by disconnecting and disabling
/// automation. Thus the monitor can publish into the same bus without waiting on itself.
/// It ends when the session is torn down or the terminal is disposed.
pub(crate) fn spawn(
    ctl: Arc<Controller>,
    events: Receiver<Event>,
    msgs: Receiver<MonitorMsg>,
) -> Result<JoinHandle<()>> {
    spawn_running(ctl, events, msgs, run)
}

/// The monitor's main loop (a test swaps in one that panics).
type Loop = fn(&Controller, &Receiver<Event>, &Receiver<MonitorMsg>);

/// [`spawn`] with `run` as the main loop.
fn spawn_running(
    ctl: Arc<Controller>,
    events: Receiver<Event>,
    msgs: Receiver<MonitorMsg>,
    run: Loop,
) -> Result<JoinHandle<()>> {
    let id = ctl.cfg.id;
    std::thread::Builder::new()
        .name(format!("agent-link-monitor-{id}"))
        .spawn(move || {
            if let Err(message) = catch_panic(|| run(&ctl, &events, &msgs)) {
                log::error!("[agent-link:{id}] agent monitor stopped after a panic: {message}");
                ctl.update_quietly(|st, _| st.monitor_failed = true);
                if let Err(message) = catch_panic(|| publish_only(&ctl, &events, &msgs)) {
                    log::error!("[agent-link:{id}] agent monitor publisher panicked: {message}");
                }
            }
        })
        .map_err(|e| Error::Spawn(format!("spawn monitor thread: {e}")))
}

fn run(ctl: &Controller, events: &Receiver<Event>, msgs: &Receiver<MonitorMsg>) {
    loop {
        let alive = apply_queued(ctl, events);
        if ctl.read(|st| st.monitor_unavailable()) {
            ctl.update_quietly(|st, _| st.monitor_failed = true);
            publish_only(ctl, events, msgs);
            return;
        }
        if !alive {
            ctl.update_quietly(|st, _| st.on_session_end());
            publish_only(ctl, events, msgs);
            return;
        }
        observe_front(ctl);
        fire_due(ctl);
        ctl.publish_queued();
        // Disposed: stop (the drop publishes the deliveries it ended).
        if ctl.read(|st| st.disposed) {
            break;
        }
        let wait = ctl
            .read(|st| st.next_deadline())
            .map_or(IDLE_WAIT, |at| at.saturating_duration_since(Instant::now()));
        select! {
            recv(events) -> event => match event {
                Ok(event) => handle(ctl, event),
                Err(_) if ctl.read(|st| st.monitor_unavailable()) => {}
                Err(_) => {
                    ctl.update_quietly(|st, _| st.on_session_end());
                    publish_only(ctl, events, msgs);
                    return;
                },
            },
            recv(msgs) -> msg => match msg {
                Ok(MonitorMsg::Wake) => {}
                Ok(MonitorMsg::Stop) | Err(_) => break,
            },
            default(wait) => {}
        }
    }
    // What the last updates queued (an exit's `Exited`); no more output will be counted.
    ctl.publish_all();
}

/// After a panic: publishes what is queued (the lifted `blocking`, the job thread's
/// `NotConnected` outcomes) and drops the session's events — except the exit, still
/// applied (`Exited`). After overflow the reader disconnected this receiver; keep serving
/// job outcomes and disposal messages without attempting to reconstruct missing output.
fn publish_only(ctl: &Controller, events: &Receiver<Event>, msgs: &Receiver<MonitorMsg>) {
    let id = ctl.cfg.id;
    let disconnected = crossbeam_channel::never();
    let mut events = events;
    loop {
        ctl.publish_all();
        if ctl.read(|st| st.disposed) {
            return;
        }
        select! {
            recv(events) -> event => match event {
                Ok(Event::Exit { .. }) => {
                    let ended = catch_panic(|| ctl.update_quietly(|st, _| st.on_session_end()));
                    if let Err(message) = ended {
                        log::error!("[agent-link:{id}] session end not applied: {message}");
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    if !ctl.read(|st| st.monitor_unavailable()) {
                        ctl.update_quietly(|st, _| st.on_session_end());
                    }
                    events = &disconnected;
                },
            },
            recv(msgs) -> msg => match msg {
                Ok(MonitorMsg::Wake) => {}
                Ok(MonitorMsg::Stop) | Err(_) => break,
            },
        }
    }
    ctl.publish_all();
}

/// Applies the events queued now (not ones that arrive meanwhile, so deadlines are not
/// starved); `false` once the session is torn down. Consecutive output chunks share one
/// screen evaluation, made before the next other event.
fn apply_queued(ctl: &Controller, events: &Receiver<Event>) -> bool {
    let mut unevaluated = false;
    let mut alive = true;
    for _ in 0..events.len() {
        match events.try_recv() {
            Ok(Event::Output { bytes, .. }) => {
                ctl.update_quietly(|st, now| st.on_output(&bytes, now));
                unevaluated = true;
            }
            Ok(event) => {
                if std::mem::take(&mut unevaluated) {
                    evaluate(ctl);
                }
                handle(ctl, event);
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                alive = false;
                break;
            }
        }
    }
    if unevaluated {
        evaluate(ctl);
    }
    alive
}

/// While the agent's program is expected: who owns the terminal now, for the state to
/// learn it.
fn observe_front(ctl: &Controller) {
    if ctl.read(|st| st.expects_program()) {
        let front = ctl.read_front();
        ctl.update_quietly(|st, _| st.observe(&front));
    }
}

/// Evaluates the current screen when the gate wants it.
fn evaluate(ctl: &Controller) {
    if ctl.read(|st| st.gate.wants_screen(Instant::now())) {
        ctl.evaluate_gate(false);
    }
}

/// Fires the deadlines due now. The screen — and, when a held launch step is due, who owns
/// the terminal (it is written only into the shell it was meant for) — are read before
/// taking the controller's lock. A held launch's own Ctrl-C is written here, and its fence
/// read right after the write was queued.
fn fire_due(ctl: &Controller) {
    let now = Instant::now();
    let (due, launch_due) = ctl.read(|st| {
        (
            st.next_deadline().is_some_and(|at| at <= now),
            st.launch_due(now),
        )
    });
    if !due {
        return;
    }
    ctl.evaluate_gate(true);
    let front = launch_due.then(|| ctl.read_front());
    let interrupt = ctl.update_quietly(|st, now| st.on_deadlines(now, front.as_ref()));
    if let Some(number) = interrupt {
        if let Err(e) = ctl.interrupt_launch(number) {
            log::debug!("[agent-link:{}] launch Ctrl-C not written: {e}", ctl.cfg.id);
        }
    }
}

fn handle(ctl: &Controller, event: Event) {
    match event {
        Event::Output { bytes, .. } => {
            ctl.update_quietly(|st, now| st.on_output(&bytes, now));
            evaluate(ctl);
        }
        Event::Frame { .. } => {
            // Published right after its chunk's `Output`: the chunk counted last.
            let open_after = ctl.term().frame_open_after();
            ctl.update_quietly(|st, now| st.on_frame(now, open_after));
        }
        Event::Title { title, .. } => ctl.update_quietly(|st, now| st.on_title(&title, now)),
        Event::Progress { state, .. } => {
            let working = state != ProgressState::Clear;
            ctl.update_quietly(|st, now| st.on_progress(working, now));
        }
        Event::Exit { .. } => ctl.update_quietly(|st, _| {
            st.trace("session ended");
            st.on_session_end();
        }),
        _ => {}
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::{Duration, Instant};

    use crossbeam_channel::unbounded;

    use super::*;
    use crate::agent::controller::{self, Job, MessageJob};
    use crate::agent::state::Config;
    use crate::agent::{
        AbortReason, AgentProfile, DeliveryOutcome, Dialog, ForegroundPolicy, SendOptions,
        ShellKind, Timing, TrustPolicy, delivery,
    };
    use crate::identity::TerminalIdentity;
    use crate::{PtySize, SpawnMode, SpawnSpec, Terminal};

    // An upper bound only: on a busy macOS host an endpoint-security agent vetting each
    // exec has delayed a process start by over 10 s during the parallel tests.
    const WAIT: Duration = Duration::from_secs(60);

    /// Never ready, never a dialog.
    struct Quiet;

    impl AgentProfile for Quiet {
        fn id(&self) -> &str {
            "quiet"
        }

        fn dialog(&self, _: &str) -> Option<Dialog> {
            None
        }

        fn input_ready(&self, _: &str) -> bool {
            false
        }
    }

    fn panicking_loop(_: &Controller, _: &Receiver<Event>, _: &Receiver<MonitorMsg>) {
        panic!("monitor bug");
    }

    /// A controller over `/bin/sh` with the `Quiet` profile and no threads started: the
    /// host's receiver, the monitor's receiver and its message channel.
    fn sh_controller(
        dir: &std::path::Path,
    ) -> (
        Terminal,
        Arc<Controller>,
        Receiver<Event>,
        Receiver<Event>,
        Receiver<MonitorMsg>,
    ) {
        let mut spec = SpawnSpec::shell(dir, PtySize { rows: 24, cols: 80 });
        spec.mode = SpawnMode::Shell(crate::shell::ShellSpec {
            program: Some("/bin/sh".into()),
            login: false,
        });
        spec.env.set("PS1", "$ ").set("ENV", "").set("HOME", dir);
        let (term, events, watch) = {
            let _spawn = crate::pty::test_spawn_lock();
            Terminal::spawn_watched(spec).unwrap()
        };
        let (tx, rx) = unbounded();
        let cfg = Config {
            id: term.id(),
            profile: Some(Arc::new(Quiet)),
            profile_id: "quiet".into(),
            launch_line: None,
            has_model_choice: false,
            reset_command: None,
            shell: ShellKind::Posix,
            trust: TrustPolicy::default(),
            timing: Timing::default(),
            limits: super::super::AgentLimits::default(),
            foreground_policy: ForegroundPolicy::default(),
            trace: false,
            identity: TerminalIdentity::Honest,
        };
        let ctl = Arc::new(Controller::new(cfg, term.handle(), tx));
        (term, ctl, events, watch, rx)
    }

    /// The monitor panics while a delivery waits for an agent that is starting: nothing
    /// would ever make it ready, so the delivery ends (`NotConnected`) — on its ticket and,
    /// published by what is left of the monitor, as an event, after the lifted overlay.
    #[test]
    fn after_a_monitor_panic_deliveries_end_and_are_published() {
        let dir = tempfile::tempdir().unwrap();
        let (_term, ctl, events, watch, rx) = sh_controller(dir.path());
        ctl.update_quietly(|st, now| st.launch(now));
        assert!(ctl.read(|st| st.blocking()), "the overlay is up at launch");
        drop(controller::spawn(Arc::clone(&ctl)).unwrap());
        let ticket = ctl.update_quietly(|st, _| {
            let id = st.next_ticket();
            let (ticket, resolver) = delivery::ticket(id);
            st.jobs.push_back(Job::Message {
                job: MessageJob::new(id, "hello", SendOptions::default()),
                resolver,
            });
            ticket
        });
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(ticket.try_outcome(), None, "waits for the agent");

        drop(spawn_running(Arc::clone(&ctl), watch, rx, panicking_loop).unwrap());
        let not_connected = DeliveryOutcome::Aborted(AbortReason::NotConnected);
        assert_eq!(ticket.wait(WAIT), Some(not_connected.clone()));
        let deadline = Instant::now() + WAIT;
        let mut lifted = false;
        loop {
            match events.recv_deadline(deadline).expect("the delivery event") {
                Event::AgentState { blocking, .. } => lifted = !blocking,
                Event::Delivery { outcome, .. } => break assert_eq!(outcome, not_connected),
                _ => continue,
            }
        }
        assert!(lifted, "the overlay is lifted before the outcome");
        assert!(!ctl.read(|st| st.blocking()));
        ctl.dispose();
    }

    /// An event queued after the reader fed a chunk waits until the monitor counted that
    /// chunk: every host receiver holds its `Output` by then.
    #[test]
    fn an_event_waits_for_the_output_fed_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let (term, ctl, events, _watch, _rx) = sh_controller(dir.path());
        term.write(b"echo hi-$((1+1))\n").unwrap();
        let deadline = Instant::now() + WAIT;
        while !term.screen().is_some_and(|s| s.text().contains("hi-2\n$")) {
            assert!(Instant::now() < deadline, "no prompt after the echo");
            std::thread::sleep(Duration::from_millis(10));
        }
        // The shell is idle at its prompt: no chunk is fed from here on.
        std::thread::sleep(Duration::from_millis(200));
        let fed = term.handle().output_chunks();
        let frame = Event::Frame { id: term.id() };
        ctl.update_quietly(|st, _| st.publish(frame.clone()));
        let frames = || events.try_iter().filter(|e| *e == frame).count();

        ctl.update_quietly(|st, _| st.view.output_serial = fed - 1);
        ctl.publish_queued();
        assert_eq!(frames(), 0, "published before its chunk was counted");
        ctl.update_quietly(|st, _| st.view.output_serial = fed);
        ctl.publish_queued();
        assert_eq!(frames(), 1);
        ctl.dispose();
    }
}
