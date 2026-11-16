//! Async adapters (feature `tokio`): consume sessions from an async host without blocking a
//! runtime thread. The blocking core is unchanged; these only wrap it.
//!
//! - [`EventStream`]: an event receiver as an async stream. [`Terminal::events_stream`]
//!   subscribes a new one; [`EventStream::new`] bridges a receiver you hold — the one a
//!   `spawn` returned misses nothing from the start, and an RPC child's events come only on
//!   the receiver its `spawn` returned.
//! - `Ticket::outcome` (feature `agent`) awaits a delivery's outcome;
//!   `AgentTerminal::send_async` is `send` plus that.
//! - `kill_async` on [`Terminal`], `AgentTerminal` and `RpcChild` (feature `rpc`) runs the
//!   blocking teardown on Tokio's blocking pool.
//!
//! **Bridging.** One forwarding thread per stream (`agent-link-stream-<id>`;
//! `agent-link-stream` for [`EventStream::new`]) moves events from the receiver into a
//! bounded Tokio channel of the receiver's own capacity (1 024; 16 for one added after the
//! `Exit`). The receive blocks, so it is a thread, not a runtime task, and works whatever
//! context creates the stream. Back-pressure is kept: a stream nobody reads stalls the
//! session like an undrained receiver once both are full. Order is kept; the stream ends
//! (`None`) once the receiver disconnects (teardown; an RPC child's `Exit`) and it was read
//! to the end. Dropping the stream ends its thread at once, which drops the receiver (the
//! session prunes it with its next event).
//!
//! **No `Stream` trait.** [`EventStream`] has `async fn next` and
//! [`poll_next`](EventStream::poll_next) rather than `futures::Stream`: that trait lives in
//! the `futures` crate, which this crate does not depend on. A host that wants it wraps the
//! stream and forwards `poll_next` (the stream is `Unpin`).
//!
//! **Tickets.** `Ticket::outcome` costs no thread: the ticket's slot wakes its waiting tasks
//! when the delivery ends. A full receiver or stream stalls a running delivery
//! (back-pressure), so never await a delivery in the only task that drains that terminal's
//! events.
//!
//! **Teardown blocks.** Dropping a `Terminal`, an `AgentTerminal` or an `RpcChild` kills and
//! reaps the child (bounded at about 2 s; on Windows also a `taskkill` round trip); an
//! `AgentTerminal` resolves its `Disposed` tickets first; final events are best effort and
//! never wait on a full receiver. In async code call `kill_async`, or drop it inside
//! `tokio::task::spawn_blocking`.
//!
//! The library uses only Tokio's `sync` and `rt` features. This timeout example also
//! requires the application's Tokio dependency to enable `time` and its runtime to
//! enable time.
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use std::time::Duration;
//! use agent_link::{Event, EventStream, PtySize, SpawnSpec, Terminal};
//!
//! let spec = SpawnSpec::shell(std::env::temp_dir(), PtySize { rows: 24, cols: 80 });
//! let (term, events) = Terminal::spawn(spec)?;
//! let result = tokio::time::timeout(Duration::from_secs(30), async {
//!     let mut events = EventStream::new(events)?; // from the session's first byte on
//!     // Split the echoed word so only command output can match `hello`.
//!     let line: &[u8] = if cfg!(windows) {
//!         b"echo hel^lo\r"
//!     } else {
//!         b"echo hel''lo\n"
//!     };
//!     term.write(line)?;
//!
//!     let mut seen = Vec::new();
//!     while let Some(event) = events.next().await {
//!         match event {
//!             Event::Output { bytes, .. } => {
//!                 seen.extend_from_slice(&bytes);
//!                 if seen.windows(5).any(|word| word == b"hello") {
//!                     return Ok::<(), Box<dyn std::error::Error>>(());
//!                 }
//!                 // Retain only the suffix needed to match across chunk boundaries.
//!                 seen.drain(..seen.len().saturating_sub(4));
//!             }
//!             Event::Exit { .. } => return Err("shell exited before output".into()),
//!             _ => {}
//!         }
//!     }
//!     Err("event stream ended before output".into())
//! }).await;
//!
//! // Tear down off the runtime thread on success, error, and timeout.
//! term.kill_async().await;
//! result??;
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::future::Future;
use std::task::{Context, Poll};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, bounded, select_biased};
use tokio::sync::mpsc;

#[cfg(feature = "agent")]
use crate::agent::{AgentTerminal, DeliveryOutcome, SendOptions, Ticket};
use crate::error::{Error, Result};
#[cfg(feature = "rpc")]
use crate::rpc::RpcChild;
use crate::{Event, ExitInfo, Terminal};

/// A stream's capacity when the receiver it bridges is unbounded (a session's receivers
/// hold 1 024 events).
const UNBOUNDED_CAPACITY: usize = 1024;

/// An event receiver as an async stream (feature `tokio`): a session's [`Event`]s (the
/// default) or an RPC child's `RpcEvent`s. Read it with [`next`](Self::next).
///
/// **Drain it or drop it**, like the receiver it bridges: once the stream and that receiver
/// are both full, the session stops reading output (an RPC child: its stdout) until there is
/// room. Dropping it discards what it holds and drops the receiver at once. It ends (`None`)
/// once the receiver disconnects — the session's teardown, an RPC child's `Exit` or teardown
/// — and everything was read. One forwarding thread per stream; see the
/// [module docs](crate::async_api).
pub struct EventStream<E = Event> {
    rx: mpsc::Receiver<E>,
    /// Never sent on: dropped with the stream, it wakes the forwarding thread, which ends and
    /// drops the receiver.
    _stop: Sender<()>,
}

impl<E: Send + 'static> EventStream<E> {
    /// Bridges `rx` into a stream of the same capacity on a new `agent-link-stream` thread.
    /// `rx` is usually one of the crate's receivers: from [`Terminal::spawn`], `subscribe`
    /// or `attach`, `AgentTerminal::spawn`, `RpcChild::spawn`. Bridging the receiver a
    /// `spawn` returned misses nothing from the session's start, which
    /// [`Terminal::events_stream`] — a new subscription — would.
    ///
    /// [`Error::Spawn`] when the thread cannot start (`rx` is dropped then).
    pub fn new(rx: Receiver<E>) -> Result<Self> {
        Self::bridge(rx, "agent-link-stream".into()).map(|(stream, _thread)| stream)
    }

    /// The stream and its forwarding thread (detached by the callers; tests watch it end).
    fn bridge(rx: Receiver<E>, name: String) -> Result<(Self, JoinHandle<()>)> {
        let capacity = rx.capacity().unwrap_or(UNBOUNDED_CAPACITY).max(1);
        let (tx, out) = mpsc::channel(capacity);
        let (stop_tx, stop_rx) = bounded(0);
        let thread = std::thread::Builder::new()
            .name(name)
            .spawn(move || forward(&rx, &tx, &stop_rx))
            .map_err(|e| Error::Spawn(format!("spawn stream thread: {e}")))?;
        let stream = Self {
            rx: out,
            _stop: stop_tx,
        };
        Ok((stream, thread))
    }
}

impl<E> EventStream<E> {
    /// The next event, in order; `None` once the receiver disconnected and everything it
    /// delivered was read. Cancel-safe: a dropped future (a `select!` branch that lost)
    /// loses no event.
    pub async fn next(&mut self) -> Option<E> {
        self.rx.recv().await
    }

    /// The polling form of [`next`](Self::next) — for a host implementing
    /// `futures::Stream` on a wrapper of its own: `Ready(None)` at the end; `Pending`
    /// registers `cx`'s waker.
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<E>> {
        self.rx.poll_recv(cx)
    }
}

impl<E> fmt::Debug for EventStream<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStream")
            .field("queued", &self.rx.len())
            .finish_non_exhaustive()
    }
}

/// The forwarding thread: moves events from `rx` to `tx`, in order, until `rx` disconnects
/// or the stream is dropped — `stop` disconnects (checked first, so a dropped stream wins
/// over a waiting event) or `tx`'s channel closes while the send waits for room.
fn forward<E>(rx: &Receiver<E>, tx: &mpsc::Sender<E>, stop: &Receiver<()>) {
    loop {
        select_biased! {
            recv(stop) -> _ => return,
            recv(rx) -> event => {
                let Ok(event) = event else {
                    return;
                };
                // Blocks while the stream is full (back-pressure); fails once it is dropped.
                if tx.blocking_send(event).is_err() {
                    return;
                }
            }
        }
    }
}

impl Terminal {
    /// A new receiver for every later event — [`subscribe`](Terminal::subscribe) — as an
    /// [`EventStream`] (feature `tokio`), bridged on an `agent-link-stream-<id>` thread. Same
    /// rules as `subscribe`: it starts at the next event (after the `Exit`, with that
    /// `Exit`) and must be drained or dropped. To miss nothing from the start, bridge the
    /// receiver [`spawn`](Terminal::spawn) returned with [`EventStream::new`] instead.
    ///
    /// [`Error::Spawn`] when the thread cannot start.
    pub fn events_stream(&self) -> Result<EventStream> {
        let name = format!("agent-link-stream-{}", self.id());
        EventStream::bridge(self.subscribe(), name).map(|(stream, _thread)| stream)
    }

    /// [`kill`](Terminal::kill) on Tokio's blocking pool (feature `tokio`): the teardown —
    /// the kill, then a reap bounded at about 2 s, which would block a runtime thread —
    /// starts now, and the future returns how the child ended. Dropping the future does not
    /// stop the teardown. Panics outside a Tokio runtime, like
    /// `tokio::task::spawn_blocking`.
    pub fn kill_async(self) -> impl Future<Output = ExitInfo> + Send + 'static {
        on_blocking_pool(move || self.kill(), ExitInfo::default())
    }
}

#[cfg(feature = "agent")]
impl Ticket {
    /// Waits for the delivery's outcome without blocking a thread (feature `tokio`): the
    /// ticket's slot wakes the task when the delivery ends — no thread per ticket. Once
    /// resolved, every call returns the same outcome at once, as [`wait`](Ticket::wait)
    /// does; any number of tasks may wait on one ticket. Cancel-safe.
    ///
    /// A receiver or stream of the terminal that nobody drains stalls the running delivery,
    /// and so this future: keep draining them meanwhile — never await a delivery in the only
    /// task that reads the terminal's events.
    pub async fn outcome(&self) -> DeliveryOutcome {
        loop {
            // Created before the check: a resolve after the check still wakes it
            // (`notify_waiters` reaches every `Notified` created before the call).
            let resolved = self.resolved().notified();
            if let Some(outcome) = self.try_outcome() {
                return outcome;
            }
            resolved.await;
        }
    }
}

#[cfg(feature = "agent")]
impl AgentTerminal {
    /// [`send`](AgentTerminal::send), then the ticket's [`outcome`](Ticket::outcome)
    /// (feature `tokio`). The delivery is queued by this call — in call order with `send` —
    /// not when the future is first polled. The future owns its ticket (`'static`: it can
    /// be spawned), and dropping it does not cancel the delivery; its
    /// [`Event::Delivery`] still comes.
    ///
    /// Every [`SendOptions`] works, but the future answers no `Revalidate::Handshake`, and
    /// it keeps its ticket, so a reader of the events cannot tell which
    /// [`Event::CommitPoint`] is this delivery's. For a handshake call
    /// [`send`](AgentTerminal::send) instead: match [`Ticket::id`] against the commit
    /// point's `ticket`, [`confirm`](AgentTerminal::confirm) it, and await
    /// [`Ticket::outcome`]. Unanswered, the delivery ends `Aborted(Revalidate)` after the
    /// handshake's timeout. Same drain rule as [`Ticket::outcome`].
    pub fn send_async(
        &self,
        text: &str,
        opts: SendOptions,
    ) -> impl Future<Output = DeliveryOutcome> + Send + 'static + use<> {
        let ticket = self.send(text, opts);
        async move { ticket.outcome().await }
    }

    /// What dropping the terminal does, on Tokio's blocking pool (feature `tokio`): the
    /// running and queued tickets resolve `Aborted(Disposed)` immediately; their final
    /// events are best effort and never wait on a full receiver. The session is then torn
    /// down. Starts
    /// now; the future returns how the child ended, as [`Terminal::kill`] does. Dropping
    /// the future does not stop it. Panics outside a Tokio runtime.
    ///
    /// Full retained streams do not delay teardown. Read tickets for the cancellation
    /// outcome: a full stream or an in-flight publisher can omit final delivery events.
    pub fn kill_async(self) -> impl Future<Output = ExitInfo> + Send + 'static {
        let session = self.terminal().handle();
        on_blocking_pool(
            move || {
                drop(self);
                // Torn down: the exit the teardown recorded.
                session.try_wait().unwrap_or_default()
            },
            ExitInfo::default(),
        )
    }
}

#[cfg(feature = "rpc")]
impl RpcChild {
    /// [`kill`](RpcChild::kill) on Tokio's blocking pool (feature `tokio`): the teardown —
    /// the kill, a reap bounded at about 2 s, on Windows a `taskkill` round trip — starts
    /// now, and the future returns the exit code. Dropping the future does not stop it.
    /// Panics outside a Tokio runtime.
    ///
    /// The child's events come on the one receiver [`spawn`](RpcChild::spawn) returned:
    /// bridge it with [`EventStream::new`].
    pub fn kill_async(self) -> impl Future<Output = Option<i32>> + Send + 'static {
        on_blocking_pool(move || self.kill(), None)
    }
}

/// Runs `f` on Tokio's blocking pool now; the future returns its result. A panic in `f`
/// resumes in the awaiting task. If the runtime shuts down before `f` ran, `f` is dropped
/// unrun — what it owns is dropped there, a teardown included — and the future returns
/// `fallback`.
fn on_blocking_pool<R: Send + 'static>(
    f: impl FnOnce() -> R + Send + 'static,
    fallback: R,
) -> impl Future<Output = R> + Send + 'static {
    let task = tokio::task::spawn_blocking(f);
    async move {
        match task.await {
            Ok(value) => value,
            Err(e) => match e.try_into_panic() {
                Ok(panic) => std::panic::resume_unwind(panic),
                Err(_) => fallback,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use crossbeam_channel::TrySendError;

    const WAIT: Duration = Duration::from_secs(10);

    /// Polls `ready` every 5 ms until it holds; panics after [`WAIT`].
    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    async fn next<E>(stream: &mut EventStream<E>) -> Option<E> {
        tokio::time::timeout(WAIT, stream.next())
            .await
            .expect("no event in time")
    }

    fn disconnected(tx: &Sender<u32>) -> bool {
        matches!(tx.try_send(u32::MAX), Err(TrySendError::Disconnected(_)))
    }

    /// Movable across threads and tasks; `Unpin`, so a host's `Stream` wrapper can forward
    /// `poll_next` without pinning.
    #[test]
    fn streams_cross_threads_and_are_unpin() {
        fn assert_traits<T: Send + Sync + Unpin>() {}
        assert_traits::<EventStream>();
        assert_traits::<EventStream<u32>>();
    }

    #[test]
    fn the_capacity_follows_the_receiver() {
        let (_tx, rx) = bounded::<u32>(5);
        assert_eq!(EventStream::new(rx).unwrap().rx.max_capacity(), 5);
        let (_tx, rx) = crossbeam_channel::unbounded::<u32>();
        let stream = EventStream::new(rx).unwrap();
        assert_eq!(stream.rx.max_capacity(), UNBOUNDED_CAPACITY);
        let (_tx, rx) = bounded::<u32>(0);
        assert_eq!(EventStream::new(rx).unwrap().rx.max_capacity(), 1);
        assert_eq!(format!("{stream:?}"), "EventStream { queued: 0, .. }");
    }

    /// In order, through a stream smaller than the run; then the stream ends with the
    /// sender, and its thread with it.
    #[tokio::test]
    async fn events_arrive_in_order_then_the_stream_ends() {
        let (tx, rx) = bounded(4);
        let (mut stream, thread) = EventStream::bridge(rx, "test-stream".into()).unwrap();
        let sender = std::thread::spawn(move || {
            for i in 0..100u32 {
                tx.send(i).unwrap();
            }
        });
        for want in 0..100u32 {
            assert_eq!(next(&mut stream).await, Some(want));
        }
        assert_eq!(next(&mut stream).await, None);
        sender.join().unwrap();
        wait_for("the thread to end", || thread.is_finished());
    }

    /// Unread, the bridge holds its capacity twice (the stream and the receiver) plus the
    /// event its thread carries; then the sender waits — back-pressure, as for an
    /// undrained receiver. One event read makes room for one more. Dropping the stream
    /// while its thread waits for room ends the thread, and the receiver goes with it.
    #[tokio::test]
    async fn an_unread_stream_back_pressures_the_sender_until_dropped() {
        const CAP: usize = 4;
        let (tx, rx) = bounded(CAP);
        let (mut stream, thread) = EventStream::bridge(rx, "test-stream".into()).unwrap();
        let mut sent = 0u32;
        let deadline = Instant::now() + WAIT;
        while sent < (2 * CAP + 1) as u32 {
            match tx.try_send(sent) {
                Ok(()) => sent += 1,
                Err(TrySendError::Full(_)) => {
                    assert!(Instant::now() < deadline, "the bridge took only {sent}");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("{e}"),
            }
        }
        assert!(
            matches!(tx.try_send(u32::MAX), Err(TrySendError::Full(_))),
            "nothing more fits while nobody reads"
        );
        assert_eq!(stream.rx.len(), CAP);
        assert_eq!(next(&mut stream).await, Some(0));
        wait_for("room for one more", || tx.try_send(sent).is_ok());
        assert!(matches!(tx.try_send(u32::MAX), Err(TrySendError::Full(_))));

        drop(stream);
        wait_for("the thread to end", || thread.is_finished());
        assert!(disconnected(&tx), "the receiver was dropped");
    }

    /// No event needed: a dropped stream wakes its idle thread at once.
    #[test]
    fn dropping_an_idle_stream_ends_its_thread_and_drops_the_receiver() {
        let (tx, rx) = bounded::<u32>(4);
        let (stream, thread) = EventStream::bridge(rx, "test-stream".into()).unwrap();
        drop(stream);
        wait_for("the thread to end", || thread.is_finished());
        assert!(disconnected(&tx));
    }

    #[tokio::test]
    async fn the_blocking_pool_returns_the_value_and_resumes_a_panic() {
        assert_eq!(on_blocking_pool(|| 7, 0).await, 7);
        let panicked = tokio::spawn(on_blocking_pool(|| -> u32 { panic!("teardown bug") }, 0));
        let err = panicked.await.expect_err("the panic resumes in the task");
        assert!(err.is_panic());
    }
}
