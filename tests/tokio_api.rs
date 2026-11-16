//! The `tokio` feature's async adapters against real sessions: an echo round trip through
//! `EventStream` (the spawn receiver bridged, and `Terminal::events_stream`), both ending
//! after `kill_async`; a stream nobody reads stalls the session like an undrained receiver,
//! and dropping it frees the session for the other subscriber; with `agent`, `send_async`
//! and `Ticket::outcome` against the fake agent (`Delivered`; several waiting tasks; the
//! event agrees), the commit-point handshake left unanswered (`revalidate`) and answered
//! from the stream, `kill_async` disposing awaited deliveries; with `rpc`, an `RpcChild`'s
//! lines and `Exit` through a stream. Every wait has a deadline (never a bare sleep).
#![cfg(all(unix, feature = "tokio"))]

mod common;

use std::future::Future;
use std::time::{Duration, Instant};

use agent_link::{Event, EventStream};
use common::LAUNCH_WAIT;
use tokio::time::timeout;

/// A session receiver's capacity (`Terminal::subscribe`); its stream mirrors it.
const CAPACITY: usize = 1024;

/// The flood test's output wait: 24 MB through the session and a stream on a loaded host.
const FLOOD_WAIT: Duration = Duration::from_secs(60);

/// `fut`, failing the test after `limit`.
async fn within<T>(limit: Duration, what: &str, fut: impl Future<Output = T>) -> T {
    timeout(limit, fut)
        .await
        .unwrap_or_else(|_| panic!("timed out after {limit:?} waiting for {what}"))
}

/// Polls `ready` every 10 ms (without blocking the runtime) until it holds; panics after
/// [`LAUNCH_WAIT`]. (`rpc` implies `agent`.)
#[cfg(feature = "agent")]
async fn poll_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + LAUNCH_WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Reads output until it contains `needle`, other events skipped, within `limit`. Matches
/// on a bounded tail, so a flood costs no quadratic search.
async fn output_until(stream: &mut EventStream, needle: &str, limit: Duration) {
    let deadline = Instant::now() + limit;
    let mut tail = String::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, stream.next()).await {
            Ok(Some(Event::Output { bytes, .. })) => {
                tail.push_str(&String::from_utf8_lossy(&bytes));
                if tail.contains(needle) {
                    return;
                }
                if tail.len() > 4096 {
                    let keep = tail.len() - 1024;
                    let cut = (keep..tail.len())
                        .find(|&at| tail.is_char_boundary(at))
                        .unwrap_or(tail.len());
                    tail.drain(..cut);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("the stream ended before {needle:?}; last output: {tail:?}"),
            Err(_) => panic!("no {needle:?} within {limit:?}; last output: {tail:?}"),
        }
    }
}

/// Reads `stream` to its end: a teardown disconnects it, and never reports an `Exit`.
async fn ends_without_exit(stream: &mut EventStream) {
    let deadline = Instant::now() + LAUNCH_WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, stream.next()).await {
            Ok(None) => return,
            Ok(Some(Event::Exit { .. })) => panic!("an Exit after the teardown"),
            Ok(Some(_)) => {}
            Err(_) => panic!("the stream did not end after the teardown"),
        }
    }
}

/// The receiver `spawn` returned, bridged, misses nothing; a stream subscribed later gets
/// what comes after. Both end after `kill_async`, with no `Exit`.
#[tokio::test]
async fn an_echo_round_trips_through_event_streams() {
    let home = tempfile::tempdir().unwrap();
    let (term, events) = common::start_shell(home.path());
    let mut first = EventStream::new(events).unwrap();
    let mut later = term.events_stream().unwrap();
    term.write(b"echo async-$((6*7))\n").unwrap();
    // The echo of the typed line shows `$((6*7))`, never `async-42`.
    output_until(&mut first, "async-42", LAUNCH_WAIT).await;
    output_until(&mut later, "async-42", LAUNCH_WAIT).await;
    let exit = term.kill_async().await;
    assert_eq!(exit.status, None, "killed by a signal");
    ends_without_exit(&mut first).await;
    ends_without_exit(&mut later).await;
}

/// A stream nobody reads stalls the session once it and its receiver are full (1 024
/// events each, plus the one its thread carries) — the back-pressure of an undrained
/// receiver. Dropping it frees the session: the other subscriber gets the rest.
#[tokio::test]
async fn dropping_an_unread_stream_frees_the_session() {
    let home = tempfile::tempdir().unwrap();
    let mut spec = common::sh_spec(home.path());
    spec.screen.enabled = false; // 24 MB of output; nothing reads a screen here
    let (term, events) = common::start_shell_spec(spec);
    let mut kept = EventStream::new(events).unwrap();
    // The shell is idle at its prompt: every later event goes to both streams.
    let unread = term.events_stream().unwrap();
    // At most 8 KiB per `Output`: well over 2 050 events.
    term.write(b"yes 0123456789abcdef | head -c 24000000; echo flood-$((6*7))\n")
        .unwrap();
    // `kept` was subscribed first, so it gets each event before the session waits on
    // `unread`: the 2 049 events `unread` holds, then the one the session is stuck on —
    // plus any the shell printed before `unread` subscribed (none for `sh`; a few allowed).
    for _ in 0..2 * CAPACITY + 1 {
        let event = within(FLOOD_WAIT, "the events before the stall", kept.next()).await;
        assert!(event.is_some(), "the stream ended");
    }
    let mut extra = 0;
    loop {
        match timeout(Duration::from_millis(500), kept.next()).await {
            Ok(Some(_)) => extra += 1,
            Ok(None) => panic!("the stream ended"),
            Err(_) => break, // quiet: the session waits on `unread`
        }
        assert!(extra <= 4, "the session went on past a full stream");
    }
    drop(unread);
    output_until(&mut kept, "flood-42", FLOOD_WAIT).await;
    term.kill_async().await;
    ends_without_exit(&mut kept).await;
}

#[cfg(feature = "agent")]
mod agent {
    use std::sync::Arc;

    use agent_link::ForegroundOwner;
    use agent_link::agent::{
        AbortReason, AgentTerminal, DeliveryOutcome, Revalidate, SendOptions, TicketId,
    };
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
    use tokio::task::JoinHandle;

    use super::common::delivery::{DELIVERY_WAIT, fake, submit};
    use super::*;

    use DeliveryOutcome::{Aborted, Delivered};

    /// A task draining a terminal's stream (a full one would stall the deliveries); it
    /// passes on the commit points and the deliveries, and ends with the stream.
    struct Drained {
        commit_points: UnboundedReceiver<TicketId>,
        deliveries: UnboundedReceiver<(TicketId, DeliveryOutcome)>,
        task: JoinHandle<()>,
    }

    fn drain(mut stream: EventStream) -> Drained {
        let (commit_tx, commit_points) = unbounded_channel();
        let (delivery_tx, deliveries) = unbounded_channel();
        let task = tokio::spawn(async move {
            while let Some(event) = stream.next().await {
                match event {
                    Event::CommitPoint { ticket, .. } => {
                        let _ = commit_tx.send(ticket);
                    }
                    Event::Delivery {
                        ticket, outcome, ..
                    } => {
                        let _ = delivery_tx.send((ticket, outcome));
                    }
                    _ => {}
                }
            }
        });
        Drained {
            commit_points,
            deliveries,
            task,
        }
    }

    /// A submit asking the host at its commit point, waiting up to `timeout`.
    fn handshake(timeout: Duration) -> SendOptions {
        SendOptions {
            revalidate: Revalidate::Handshake { timeout },
            ..submit()
        }
    }

    fn screen_text(term: &AgentTerminal) -> String {
        term.terminal().screen().expect("a screen model").text()
    }

    /// `send_async` launches the stopped agent, delivers, and the event agrees.
    #[tokio::test]
    async fn send_async_delivers_to_the_fake_agent() {
        let (term, events, _home) = fake("composer", &[]);
        let mut drained = drain(EventStream::new(events).unwrap());
        let sent = term.send_async("hello async", submit());
        assert_eq!(within(DELIVERY_WAIT, "the delivery", sent).await, Delivered);
        assert_eq!(
            within(LAUNCH_WAIT, "its event", drained.deliveries.recv()).await,
            Some((TicketId(1), Delivered))
        );
        poll_until("the message in the transcript", || {
            screen_text(&term).contains("you: hello async")
        })
        .await;
        let exit = term.kill_async().await;
        assert_eq!(exit.status, None, "killed by a signal");
        within(LAUNCH_WAIT, "the stream's end", drained.task)
            .await
            .unwrap();
    }

    /// Two tasks await one ticket; both get the outcome, which then answers at once and
    /// matches the blocking API.
    #[tokio::test]
    async fn a_ticket_is_awaited_by_several_tasks() {
        let (term, events, _home) = fake("composer", &[]);
        let mut drained = drain(EventStream::new(events).unwrap());
        let ticket = Arc::new(term.send("hello outcome", submit()));
        let waiter = {
            let ticket = Arc::clone(&ticket);
            tokio::spawn(async move { ticket.outcome().await })
        };
        assert_eq!(
            within(DELIVERY_WAIT, "the outcome", ticket.outcome()).await,
            Delivered
        );
        assert_eq!(
            within(LAUNCH_WAIT, "the other task", waiter).await.unwrap(),
            Delivered
        );
        assert_eq!(ticket.outcome().await, Delivered);
        assert_eq!(ticket.try_outcome(), Some(Delivered));
        assert_eq!(
            within(LAUNCH_WAIT, "its event", drained.deliveries.recv()).await,
            Some((ticket.id(), Delivered))
        );
        term.kill_async().await;
    }

    /// `send_async` answers no handshake: unanswered, the delivery ends `revalidate` after
    /// its timeout, nothing pasted. Answered the documented way — `send`, the ticket's id
    /// matched to the `CommitPoint` the stream task passes on, `confirm`, `outcome` — the
    /// next one is delivered.
    #[tokio::test]
    async fn a_handshake_is_answered_from_the_stream() {
        let (term, events, _home) = fake("composer", &[]);
        let mut drained = drain(EventStream::new(events).unwrap());
        let unanswered = term.send_async("an unanswered message", handshake(ms(400)));
        let asked = within(
            DELIVERY_WAIT,
            "the first commit point",
            drained.commit_points.recv(),
        )
        .await;
        assert_eq!(asked, Some(TicketId(1)));
        assert_eq!(
            within(LAUNCH_WAIT, "the handshake's timeout", unanswered).await,
            Aborted(AbortReason::Revalidate)
        );

        let answered = term.send("a confirmed message", handshake(LAUNCH_WAIT));
        let asked = within(
            DELIVERY_WAIT,
            "the second commit point",
            drained.commit_points.recv(),
        )
        .await
        .expect("the stream ended");
        assert_eq!(asked, answered.id());
        term.confirm(asked, true);
        assert_eq!(
            within(DELIVERY_WAIT, "the delivery", answered.outcome()).await,
            Delivered
        );
        poll_until("the message in the transcript", || {
            screen_text(&term).contains("you: a confirmed message")
        })
        .await;
        assert!(!screen_text(&term).contains("unanswered"));
        term.kill_async().await;
    }

    /// `kill_async` disposes a delivery that waits (the silent fake is not ready within its
    /// trust window) and a queued one, awaited on a task of its own (the future owns its
    /// ticket): both return `disposed`, and both events reach the stream, in ticket order,
    /// before it ends.
    #[tokio::test]
    async fn kill_async_disposes_the_awaited_deliveries() {
        let (term, events, _home) = fake("silent", &[]);
        let mut drained = drain(EventStream::new(events).unwrap());
        let waiting = term.send("the first message", submit());
        let queued = tokio::spawn(term.send_async("the second message", submit()));
        poll_until("the agent in front", || {
            term.terminal().foreground().owner == ForegroundOwner::Agent
        })
        .await;
        assert_eq!(waiting.try_outcome(), None);
        let exit = term.kill_async().await;
        assert_eq!(exit.status, None, "killed by a signal");
        let disposed = Aborted(AbortReason::Disposed);
        assert_eq!(
            within(LAUNCH_WAIT, "the waiting delivery", waiting.outcome()).await,
            disposed
        );
        assert_eq!(
            within(LAUNCH_WAIT, "the queued delivery", queued)
                .await
                .unwrap(),
            disposed
        );
        within(LAUNCH_WAIT, "the stream's end", drained.task)
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Ok(delivery) = drained.deliveries.try_recv() {
            events.push(delivery);
        }
        assert_eq!(
            events,
            [(TicketId(1), disposed.clone()), (TicketId(2), disposed)]
        );
    }

    #[tokio::test]
    async fn kill_async_does_not_wait_for_a_full_retained_stream() {
        use agent_link::agent::AgentSpec;
        let (term, events, _home) = common::delivery::shell(AgentSpec::default());
        let queued = events.clone();
        let stream = EventStream::new(events).unwrap();
        let pid = term.terminal().pid().unwrap() as libc::pid_t;
        term.terminal()
            .write(b"yes async-lifecycle-output\n")
            .unwrap();
        // The forwarding thread drains into the stream until both bounded queues fill.
        poll_until("the unread stream and source queue to fill", || {
            queued.is_full()
        })
        .await;
        within(
            Duration::from_secs(5),
            "teardown with an unread full stream",
            term.kill_async(),
        )
        .await;
        // SAFETY: signal zero only queries whether the child still exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        drop(stream);
        drop(queued);
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }
}

#[cfg(feature = "rpc")]
mod rpc {
    use std::path::Path;

    use agent_link::agent::RpcLaunch;
    use agent_link::rpc::{RpcChild, RpcEvent, RpcSpawnSpec};

    use super::*;

    /// One RPC wait: a child's start is two execs, which endpoint-security vetting under
    /// load has slowed past 20 s (as in `tests/rpc_roundtrip.rs`).
    const RPC_WAIT: Duration = Duration::from_secs(60);

    /// `launcher args…` through `/bin/sh` (no rc files).
    fn sh_spec(dir: &Path, launcher: &str, args: &[&str]) -> RpcSpawnSpec {
        let mut spec = RpcSpawnSpec::new(RpcLaunch::new(launcher, args), dir);
        spec.shell = Some("/bin/sh".into());
        spec.log_tag = "tokio-test".into();
        spec
    }

    async fn next(stream: &mut EventStream<RpcEvent>, what: &str) -> Option<RpcEvent> {
        within(RPC_WAIT, what, stream.next()).await
    }

    /// An RPC child's one receiver, bridged: lines in order; `kill_async` ends the child
    /// (no exit code: SIGKILL) and the stream, with no `Exit`. A child that exits on its
    /// own sends its `Exit` with the stderr tail, then the stream ends.
    #[tokio::test]
    async fn an_rpc_child_streams_its_lines() {
        let dir = tempfile::tempdir().unwrap();
        let (child, events) = RpcChild::spawn(sh_spec(dir.path(), "cat", &[])).unwrap();
        let mut stream = EventStream::new(events).unwrap();
        for (seq, text) in [r#"{"a":1}"#, "two"].into_iter().enumerate() {
            child.send_line(text).unwrap();
            let line = RpcEvent::Line {
                id: child.id(),
                seq: seq as u64,
                line: text.into(),
            };
            assert_eq!(next(&mut stream, text).await, Some(line));
        }
        assert_eq!(child.kill_async().await, None, "SIGKILL: no exit code");
        assert_eq!(next(&mut stream, "the stream's end").await, None);

        let spec = sh_spec(dir.path(), "sh", &["-c", "echo out; echo err >&2; exit 3"]);
        let (child, events) = RpcChild::spawn(spec).unwrap();
        let mut stream = EventStream::new(events).unwrap();
        let id = child.id();
        let out = RpcEvent::Line {
            id,
            seq: 0,
            line: "out".into(),
        };
        assert_eq!(next(&mut stream, "the line").await, Some(out));
        let exit = RpcEvent::Exit {
            id,
            stderr_tail: "err".into(),
        };
        assert_eq!(next(&mut stream, "the exit").await, Some(exit));
        assert_eq!(next(&mut stream, "the stream's end").await, None);
        poll_until("the exit code", || {
            child.try_wait().expect("liveness readable").is_some()
        })
        .await;
        assert_eq!(child.kill_async().await, Some(3));
    }
}
