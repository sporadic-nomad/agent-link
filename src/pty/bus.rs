//! A session's event bus: fans each event out to every subscriber, in order, and keeps the
//! session log.
//!
//! Subscribers are bounded channels; a full one blocks the publisher, which is the
//! back-pressure that stops a session reading output nobody consumes. The state lock is held
//! to register a subscriber (`attach`: plus the log snapshot), to number an output chunk,
//! append it to the log and copy the subscriber list, and to close — never while sending.
//! So `subscribe`, `attach` and `close` never wait on a consumer, but can wait behind
//! another holder: an `attach` copying the memory log (milliseconds for 32 MiB) or the
//! reader writing one chunk (at most 8 KiB) to the file log.
//!
//! A separate publication lock serializes fanout across output and controller events.
//! Closing cancels blocked fanout and never waits for that lock.
//! Recovery uses a separate boundary lock for model processing plus log commit, resize,
//! and snapshot capture; it is released before fanout, so a full receiver cannot prevent
//! recovery attachment. Ordinary raw attachment still needs only the state lock.
//!
//! **The attach fence.** Numbering, logging and copying the subscriber list happen in one
//! critical section, and `attach` snapshots the log and registers its receiver in another.
//! A chunk logged before the snapshot is in the replay and not sent to the new receiver; a
//! chunk logged after it is sent to it: no gap, no overlap.
//!
//! **After the exit.** The reader publishes nothing after the child's `Exit`; only an agent
//! controller can (its reaction to the exit — `AgentState` `Exited`, `Activity` — and later
//! controller events). A receiver registered later gets that `Exit` queued (after its
//! replay), so a late receiver waiting for the end does not hang. Its channel is small
//! ([`AFTER_EXIT`] events): enough for the controller's reaction without blocking it, and
//! dropped receivers are pruned only when an event is delivered — rare after the exit — so
//! each costs a small channel until teardown, not a 1 024-slot one.
//!
//! **The controller's receiver** ([`watch`](EventBus::watch), at most one) is served after
//! every other receiver, whenever those were registered: when the controller sees an event,
//! every host receiver already holds it, so an event the controller publishes in reaction
//! never overtakes its cause on any receiver.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded, select_biased};

use super::log::{Replay, SessionLog};
use super::model::ModelSlot;
use super::{Event, PtySize, RecoverySnapshot, ReplayCursor, ReplayError, ReplayGap, SessionId};
use crate::error::{Error, Result};

/// Capacity of every subscriber channel, in events.
pub(crate) const CAPACITY: usize = 1024;

/// Internal monitoring never blocks publication or retains more than this many events.
/// Payloads are capped too, including titles supplied by a custom screen model.
const WATCH_CAPACITY: usize = 1024;
const WATCH_EVENT_BYTES: usize = 8192;

/// Capacity of a channel registered after the `Exit`: the `Exit` plus an agent
/// controller's reaction to it (and a few later controller events).
pub(crate) const AFTER_EXIT: usize = 16;

pub(crate) struct EventBus {
    id: SessionId,
    /// Reader EOF and observed process exit cut off turn observation before Exit fanout.
    #[cfg(all(unix, feature = "agent", any(test, feature = "turns")))]
    pub(super) turns_ended: Arc<AtomicBool>,
    state: Mutex<BusState>,
    /// Serializes model processing + output logging, resize, and recovery attachment.
    /// Lock order: publishing -> boundary -> model/master -> state (model/master are
    /// released before state). Never held across event fanout. Close needs only state.
    boundary: Mutex<PtySize>,
    /// Serializes complete fanout. Shutdown never waits for this lock: it cancels a
    /// blocked publisher and omits final events if a fanout is still in flight.
    publishing: Mutex<()>,
    /// Dropped by [`close`](EventBus::close); `closed_rx` then reports ready, which frees a
    /// publisher blocked on a full subscriber.
    closed_tx: Mutex<Option<Sender<()>>>,
    closed_rx: Receiver<()>,
    /// Sticky: losing any monitored event disables automation for this session.
    watch_overflow: Arc<AtomicBool>,
}

struct BusState {
    subscribers: Vec<Sender<Event>>,
    /// The agent controller's receiver: served after every subscriber.
    watcher: Option<Sender<Event>>,
    /// The `seq` of the next output chunk.
    next_seq: u64,
    /// Bytes published so far, independent of log retention and output chunk sizes.
    output_bytes: u64,
    /// Every output chunk, appended under the lock that numbers it. Dropped on close.
    log: SessionLog,
    /// The child's `Exit`, once published; queued for every later receiver.
    exit: Option<Event>,
    closed: bool,
}

impl BusState {
    /// A channel for a new receiver: small once the `Exit` was published.
    fn channel(&self) -> (Sender<Event>, Receiver<Event>) {
        bounded(if self.exit.is_some() {
            AFTER_EXIT
        } else {
            CAPACITY
        })
    }

    /// Every receiver in delivery order: the subscribers, then the controller's.
    fn recipients(&self) -> Vec<Sender<Event>> {
        let mut all = self.subscribers.clone();
        all.extend(self.watcher.clone());
        all
    }

    /// Adds a receiver's sender unless the bus is closed (the receiver is then already
    /// disconnected); after the `Exit` it gets that `Exit` first.
    fn register(&mut self, tx: Sender<Event>) {
        if self.closed {
            return;
        }
        if let Some(exit) = &self.exit {
            // A fresh channel always has room.
            let _ = tx.try_send(exit.clone());
        }
        self.subscribers.push(tx);
    }
}

/// How one send ended.
enum Delivery {
    Sent,
    /// The receiver was dropped.
    Gone,
    /// The bus closed while the send waited.
    Closed,
}

impl EventBus {
    pub(crate) fn new(id: SessionId, log: SessionLog, size: PtySize) -> Self {
        let (closed_tx, closed_rx) = bounded(0);
        Self {
            id,
            #[cfg(all(unix, feature = "agent", any(test, feature = "turns")))]
            turns_ended: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(BusState {
                subscribers: Vec::new(),
                watcher: None,
                next_seq: 0,
                output_bytes: 0,
                log,
                exit: None,
                closed: false,
            }),
            boundary: Mutex::new(size),
            publishing: Mutex::new(()),
            closed_tx: Mutex::new(Some(closed_tx)),
            closed_rx,
            watch_overflow: Arc::new(AtomicBool::new(false)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BusState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new receiver for every later event; after the `Exit` it holds that `Exit`. After
    /// [`close`](Self::close) it is already disconnected.
    pub(crate) fn subscribe(&self) -> Receiver<Event> {
        let mut state = self.lock();
        let (tx, rx) = state.channel();
        state.register(tx);
        rx
    }

    /// The agent controller's bounded receiver, served last (see the module docs).
    /// A full queue or oversized event disconnects it and disables automation; publication
    /// never waits for this receiver, because the controller publishes into the same bus.
    /// The host's raw terminal stream is unaffected. Replaces an earlier receiver.
    #[cfg_attr(not(feature = "agent"), allow(dead_code))]
    pub(crate) fn watch(&self) -> Receiver<Event> {
        let (tx, rx) = bounded(WATCH_CAPACITY);
        let mut state = self.lock();
        if !state.closed {
            state.watcher = Some(tx);
        }
        rx
    }

    #[cfg(feature = "agent")]
    pub(crate) fn watch_overflow(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.watch_overflow)
    }

    #[cfg(feature = "agent")]
    pub(crate) fn closed_signal(&self) -> Receiver<()> {
        self.closed_rx.clone()
    }

    /// The log so far and a receiver for every later event, fenced against
    /// [`publish_output_with`](Self::publish_output_with) (see the module docs); after the `Exit` the
    /// receiver holds that `Exit`. After [`close`](Self::close): nothing to replay and an
    /// already disconnected receiver.
    pub(crate) fn attach(&self) -> (Replay, Receiver<Event>) {
        let mut state = self.lock();
        let (tx, rx) = state.channel();
        if state.closed {
            return (Replay::None, rx);
        }
        let replay = state.log.replay();
        state.register(tx);
        (replay, rx)
    }

    pub(crate) fn attach_recovery(
        &self,
        from: Option<ReplayCursor>,
        screen: &ModelSlot,
    ) -> Result<(RecoverySnapshot, Receiver<Event>), ReplayError> {
        self.attach_recovery_inner(from, screen, false, true)
    }

    #[cfg(feature = "observers")]
    pub(crate) fn attach_live_recovery(
        &self,
        screen: &ModelSlot,
    ) -> Result<(RecoverySnapshot, Receiver<Event>), ReplayError> {
        self.attach_recovery_inner(None, screen, true, true)
    }

    #[cfg(feature = "observers")]
    pub(crate) fn attach_live_recovery_metadata(
        &self,
        screen: &ModelSlot,
    ) -> Result<(RecoverySnapshot, Receiver<Event>), ReplayError> {
        self.attach_recovery_inner(None, screen, true, false)
    }

    fn attach_recovery_inner(
        &self,
        from: Option<ReplayCursor>,
        screen: &ModelSlot,
        live_only: bool,
        include_display: bool,
    ) -> Result<(RecoverySnapshot, Receiver<Event>), ReplayError> {
        let mut requested = from.unwrap_or(ReplayCursor {
            session: self.id,
            offset: 0,
        });
        if requested.session != self.id {
            return Err(ReplayError::SessionMismatch {
                expected: self.id,
                actual: requested.session,
            });
        }
        let size = self.boundary.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(feature = "screen")]
        let display = if include_display {
            screen.snapshot()
        } else {
            None
        };
        #[cfg(not(feature = "screen"))]
        let _ = (screen, include_display);
        let mut state = self.lock();
        if state.closed {
            return Err(ReplayError::Closed);
        }
        let end = state.output_bytes;
        if live_only {
            requested.offset = end;
        }
        if requested.offset > end {
            return Err(ReplayError::FutureCursor {
                requested: requested.offset,
                end,
            });
        }
        let (retained, reason) = state.log.retained(end);
        let start = requested.offset.max(retained.start).min(retained.end);
        let replay_range = if requested.offset > retained.end {
            requested.offset..requested.offset
        } else {
            start..retained.end
        };
        let mut gaps = Vec::new();
        if requested.offset < replay_range.start {
            gaps.push(ReplayGap {
                range: requested.offset..replay_range.start,
                reason,
            });
        }
        if replay_range.end < end {
            gaps.push(ReplayGap {
                range: replay_range.end..end,
                reason,
            });
        }
        let replay = state
            .log
            .replay_range(retained.clone(), replay_range.clone());
        let snapshot = RecoverySnapshot {
            requested,
            boundary: ReplayCursor {
                session: self.id,
                offset: end,
            },
            retained_start: ReplayCursor {
                session: self.id,
                offset: if retained.is_empty() {
                    end
                } else {
                    retained.start
                },
            },
            replay_range,
            replay,
            gaps,
            next_output_seq: state.next_seq,
            size: *size,
            #[cfg(feature = "screen")]
            display,
        };
        let (tx, rx) = state.channel();
        state.register(tx);
        Ok((snapshot, rx))
    }

    /// Commit geometry only after both PTY and model resize succeed.
    pub(crate) fn resize(&self, size: PtySize, resize: impl FnOnce() -> Result<()>) -> Result<()> {
        let mut current = self.boundary.lock().unwrap_or_else(|e| e.into_inner());
        resize()?;
        *current = size;
        Ok(())
    }

    /// Publishes one output chunk with the next `seq` and logs it. Only the session's
    /// reader thread calls this, so chunks reach every subscriber in `seq` order.
    #[cfg(test)]
    pub(crate) fn publish_output(&self, bytes: Vec<u8>) {
        self.publish_output_with(bytes, || {});
    }

    /// `process` updates the model at the same recovery boundary as the log. It is
    /// internal reader work, never a host callback, and runs without the state lock.
    /// The reader must announce the chunk to its output fence before calling this:
    /// a controller publisher can hold `publishing` while blocked on a host receiver.
    pub(crate) fn publish_output_with(&self, bytes: Vec<u8>, process: impl FnOnce()) {
        let _publishing = self.publishing.lock().unwrap_or_else(|e| e.into_inner());
        let (seq, subscribers, watcher) = {
            let _boundary = self.boundary.lock().unwrap_or_else(|e| e.into_inner());
            process();
            let mut state = self.lock();
            if state.closed {
                return;
            }
            let seq = state.next_seq;
            state.next_seq += 1;
            state.output_bytes += bytes.len() as u64;
            state.log.append(&bytes, self.id);
            (seq, state.subscribers.clone(), state.watcher.clone())
        };
        let event = Event::Output {
            id: self.id,
            seq,
            bytes,
        };
        self.deliver(&subscribers, watcher.as_ref(), event);
    }

    /// Publishes an event that is not output. An `Exit` is also kept for later receivers;
    /// after it only an agent controller publishes.
    pub(crate) fn publish(&self, event: Event) {
        #[cfg(all(unix, feature = "agent", any(test, feature = "turns")))]
        if matches!(event, Event::Exit { .. }) {
            self.turns_ended.store(true, Ordering::Release);
        }
        let _publishing = self.publishing.lock().unwrap_or_else(|e| e.into_inner());
        let (subscribers, watcher) = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            if matches!(event, Event::Exit { .. }) {
                state.exit = Some(event.clone());
            }
            (state.subscribers.clone(), state.watcher.clone())
        };
        self.deliver(&subscribers, watcher.as_ref(), event);
    }

    /// Sends outside the lock, then prunes receivers that were dropped.
    fn deliver(
        &self,
        subscribers: &[Sender<Event>],
        watcher: Option<&Sender<Event>>,
        event: Event,
    ) {
        #[cfg(feature = "questions")]
        let watcher = watcher
            .filter(|_| !matches!(event, Event::Question { .. } | Event::QuestionClosed { .. }));
        let mut gone = Vec::new();
        for tx in subscribers {
            match self.send(tx, event.clone()) {
                Delivery::Sent => {}
                Delivery::Gone => gone.push(tx),
                Delivery::Closed => return,
            }
        }
        if let Some(watcher) = watcher {
            if matches!(self.closed_rx.try_recv(), Err(TryRecvError::Disconnected)) {
                return;
            }
            let payload = match &event {
                Event::Output { bytes, .. } => bytes.len(),
                Event::Title { title, .. } => title.len(),
                Event::ScreenReset { reason, .. } => reason.len(),
                _ => 0,
            };
            let sent = if payload > WATCH_EVENT_BYTES {
                Err(TrySendError::Full(event))
            } else {
                watcher.try_send(event)
            };
            match sent {
                Ok(()) => {}
                Err(TrySendError::Disconnected(_)) => gone.push(watcher),
                Err(TrySendError::Full(_)) => {
                    self.watch_overflow.store(true, Ordering::Release);
                    gone.push(watcher);
                    log::error!(
                        "[agent-link:{}] agent monitor queue overflow (limit {WATCH_CAPACITY} events, {WATCH_EVENT_BYTES} payload bytes per event); automation disabled, raw terminal events continue",
                        self.id
                    );
                }
            }
        }
        if !gone.is_empty() {
            let mut state = self.lock();
            state
                .subscribers
                .retain(|s| !gone.iter().any(|g| g.same_channel(s)));
            if let Some(watcher) = &state.watcher {
                if gone.iter().any(|g| g.same_channel(watcher)) {
                    state.watcher = None;
                }
            }
        }
    }

    /// Blocks while `tx` is full, until it has room, its receiver is dropped or the bus
    /// closes. Closed is checked first, so nothing is delivered once `close` has run.
    fn send(&self, tx: &Sender<Event>, event: Event) -> Delivery {
        select_biased! {
            recv(self.closed_rx) -> _ => Delivery::Closed,
            send(tx, event) -> sent => if sent.is_ok() { Delivery::Sent } else { Delivery::Gone },
        }
    }

    /// Stops all delivery: later publishes are dropped, a blocked publish returns, and every
    /// receiver disconnects once it has drained what it already holds. The log is dropped
    /// (memory freed, file closed).
    pub(crate) fn close(&self) {
        self.close_with_final(std::iter::empty());
    }

    /// Closes the bus without waiting for any subscriber. Final controller events are
    /// best effort: only when no earlier fanout is still in flight, and only while each
    /// receiver has capacity. A receiver missing an event gets no later final event.
    /// No publish can overtake this batch or resume after it.
    pub(crate) fn close_with_final(&self, events: impl IntoIterator<Item = Event>) {
        // Release blocked fanout before attempting its lock. Never wait for a publisher:
        // it may be descheduled, or waiting on a receiver owned by this dropping thread.
        drop(
            self.closed_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        );
        let publishing = match self.publishing.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        let (mut subscribers, log) = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            let recipients = if publishing.is_some() {
                state.recipients()
            } else {
                Vec::new()
            };
            state.subscribers.clear();
            state.watcher = None;
            (
                recipients,
                std::mem::replace(&mut state.log, SessionLog::Off),
            )
        };
        drop(log);
        for event in events {
            if subscribers.is_empty() {
                break;
            }
            subscribers.retain(|tx| tx.try_send(event.clone()).is_ok());
        }
    }
}

/// Calls `f` with every event from `rx`, in order, on a new thread (`agent-link-forward`),
/// until the session is torn down (or every other sender is gone). Use it to bridge a
/// session into a host's own event system.
///
/// `f` runs on the forwarding thread; a slow `f` back-pressures the session like any
/// undrained receiver. Never join the returned handle from a thread `f` waits on.
pub fn forward_to(
    rx: Receiver<Event>,
    mut f: impl FnMut(Event) + Send + 'static,
) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("agent-link-forward".into())
        .spawn(move || {
            for event in rx {
                f(event);
            }
        })
        .map_err(|e| Error::Spawn(format!("spawn forward thread: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);
    const SIZE: PtySize = PtySize { rows: 24, cols: 80 };

    fn model() -> ModelSlot {
        let (writer, _rx) = super::super::input::Input::new(super::super::InputLimits::default());
        ModelSlot::new(
            &super::super::ScreenConfig::default(),
            SessionId(1),
            SIZE,
            &writer,
        )
    }

    #[test]
    fn recovery_reports_eviction_and_resumes_inside_a_retained_chunk() {
        let log =
            SessionLog::open(&super::super::SessionLogConfig::Memory { cap_bytes: 5 }).unwrap();
        let bus = EventBus::new(SessionId(1), log, SIZE);
        let screen = model();
        bus.publish_output(b"abc".to_vec());
        bus.publish_output(b"defg".to_vec());
        let (snapshot, live) = bus.attach_recovery(None, &screen).unwrap();
        assert_eq!(snapshot.requested.offset, 0);
        assert_eq!(snapshot.retained_start.offset, 3);
        assert_eq!(snapshot.boundary.offset, 7);
        assert_eq!(snapshot.replay_range, 3..7);
        assert_eq!(snapshot.replay.into_bytes().unwrap(), b"defg");
        assert_eq!(
            snapshot.gaps,
            [ReplayGap {
                range: 0..3,
                reason: super::super::ReplayGapReason::Evicted
            }]
        );
        assert_eq!(snapshot.next_output_seq, 2);
        bus.publish_output(b"hi".to_vec());
        assert_eq!(
            output(live.recv_timeout(WAIT).unwrap()),
            (2, b"hi".to_vec())
        );
        drop(live);

        let (snapshot, _) = bus
            .attach_recovery(
                Some(ReplayCursor {
                    session: SessionId(1),
                    offset: 5,
                }),
                &screen,
            )
            .unwrap();
        assert_eq!(snapshot.replay_range, 7..9);
        assert_eq!(snapshot.gaps[0].range, 5..7);
        assert_eq!(snapshot.replay.into_bytes().unwrap(), b"hi");
        let (snapshot, _) = bus
            .attach_recovery(
                Some(ReplayCursor {
                    session: SessionId(1),
                    offset: 8,
                }),
                &screen,
            )
            .unwrap();
        assert_eq!(snapshot.replay_range, 8..9);
        assert!(snapshot.gaps.is_empty());
        assert_eq!(snapshot.replay.into_bytes().unwrap(), b"i");
        let (empty, _) = bus
            .attach_recovery(Some(snapshot.boundary), &screen)
            .unwrap();
        assert_eq!(empty.replay_range, 9..9);
        assert!(empty.gaps.is_empty());
        assert_eq!(empty.replay.into_bytes().unwrap(), b"");
    }

    #[test]
    fn recovery_rejects_foreign_future_and_closed_cursors_without_subscribing() {
        let bus = EventBus::new(SessionId(1), SessionLog::Off, SIZE);
        let screen = model();
        assert_eq!(
            bus.attach_recovery(
                Some(ReplayCursor {
                    session: SessionId(2),
                    offset: 0
                }),
                &screen
            )
            .unwrap_err(),
            ReplayError::SessionMismatch {
                expected: SessionId(1),
                actual: SessionId(2)
            }
        );
        assert_eq!(
            bus.attach_recovery(
                Some(ReplayCursor {
                    session: SessionId(1),
                    offset: 1
                }),
                &screen
            )
            .unwrap_err(),
            ReplayError::FutureCursor {
                requested: 1,
                end: 0
            }
        );
        assert!(bus.lock().subscribers.is_empty());
        bus.close();
        assert_eq!(
            bus.attach_recovery(None, &screen).unwrap_err(),
            ReplayError::Closed
        );
    }

    #[test]
    fn recovery_without_recording_reports_loss_but_retains_eof_and_geometry() {
        let bus = EventBus::new(SessionId(1), SessionLog::Off, SIZE);
        let screen = model();
        let (empty, _) = bus.attach_recovery(None, &screen).unwrap();
        assert!(empty.gaps.is_empty());
        assert_eq!(empty.boundary.offset, 0);
        bus.publish_output(b"lost".to_vec());
        let size = PtySize { rows: 10, cols: 20 };
        bus.resize(size, || screen.resize(size, || Ok(()))).unwrap();
        assert!(bus.resize(SIZE, || Err(Error::Closed)).is_err());
        bus.publish(Event::Exit {
            id: SessionId(1),
            status: None,
        });
        let (snapshot, rx) = bus.attach_recovery(None, &screen).unwrap();
        assert_eq!(snapshot.size, size);
        assert_eq!(snapshot.retained_start, snapshot.boundary);
        assert_eq!(snapshot.replay_range, 4..4);
        assert_eq!(snapshot.replay, Replay::None);
        assert_eq!(
            snapshot.gaps,
            [ReplayGap {
                range: 0..4,
                reason: super::super::ReplayGapReason::RecordingDisabled
            }]
        );
        #[cfg(feature = "screen")]
        assert_eq!(snapshot.display.unwrap().size, size);
        assert!(matches!(rx.recv_timeout(WAIT).unwrap(), Event::Exit { .. }));
    }

    #[test]
    fn recovery_fences_model_and_log_without_waiting_for_full_receiver() {
        let log = SessionLog::open(&super::super::SessionLogConfig::Memory {
            cap_bytes: CAPACITY * 2,
        })
        .unwrap();
        let bus = Arc::new(EventBus::new(SessionId(1), log, SIZE));
        let screen = Arc::new(model());
        let full = bus.subscribe();
        for _ in 0..CAPACITY {
            bus.publish_output_with(b"x".to_vec(), || {
                screen.process(SessionId(1), b"x");
            });
        }
        let (processing_tx, processing_rx) = bounded(0);
        let (release_tx, release_rx) = bounded(0);
        let publisher = {
            let bus = Arc::clone(&bus);
            let screen = Arc::clone(&screen);
            std::thread::spawn(move || {
                bus.publish_output_with(b"Z".to_vec(), || {
                    screen.process(SessionId(1), b"Z");
                    processing_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(WAIT);
                })
            })
        };
        processing_rx.recv_timeout(WAIT).unwrap();
        let (attached_tx, attached_rx) = bounded(1);
        let attacher = {
            let bus = Arc::clone(&bus);
            let screen = Arc::clone(&screen);
            std::thread::spawn(move || {
                let _ = attached_tx.send(bus.attach_recovery(None, &screen));
            })
        };
        let _ = release_tx.send(());
        let result = attached_rx.recv_timeout(WAIT);
        // Always unblock publisher before asserting, even when attachment failed.
        bus.close();
        drop(full);
        publisher.join().unwrap();
        attacher.join().unwrap();
        let (snapshot, live) = result.unwrap().unwrap();
        assert_eq!(snapshot.boundary.offset, CAPACITY as u64 + 1);
        assert_eq!(snapshot.next_output_seq, CAPACITY as u64 + 1);
        let replay = snapshot.replay.into_bytes().unwrap();
        assert_eq!(replay.len(), CAPACITY + 1);
        assert_eq!(replay.last(), Some(&b'Z'));
        #[cfg(feature = "screen")]
        assert!(snapshot.display.unwrap().text().contains('Z'));
        assert!(
            live.try_recv().is_err(),
            "already logged chunk must not also be live"
        );
    }

    fn output(event: Event) -> (u64, Vec<u8>) {
        match event {
            Event::Output { seq, bytes, .. } => (seq, bytes),
            other => panic!("expected output, got {other:?}"),
        }
    }

    #[test]
    fn every_subscriber_gets_every_chunk_in_order() {
        let bus = EventBus::new(SessionId(1), SessionLog::Off, SIZE);
        let a = bus.subscribe();
        let b = bus.subscribe();
        for i in 0..5u8 {
            bus.publish_output(vec![i]);
        }
        for rx in [&a, &b] {
            for i in 0..5u8 {
                assert_eq!(
                    output(rx.recv_timeout(WAIT).unwrap()),
                    (u64::from(i), vec![i])
                );
            }
        }
    }

    #[test]
    fn a_late_subscriber_starts_at_the_current_seq() {
        let bus = EventBus::new(SessionId(1), SessionLog::Off, SIZE);
        let early = bus.subscribe();
        bus.publish_output(b"a".to_vec());
        let late = bus.subscribe();
        bus.publish_output(b"b".to_vec());
        assert_eq!(output(early.recv_timeout(WAIT).unwrap()).0, 0);
        assert_eq!(output(early.recv_timeout(WAIT).unwrap()).0, 1);
        assert_eq!(output(late.recv_timeout(WAIT).unwrap()), (1, b"b".to_vec()));
    }

    #[test]
    fn a_dropped_receiver_is_pruned() {
        let bus = EventBus::new(SessionId(1), SessionLog::Off, SIZE);
        let kept = bus.subscribe();
        drop(bus.subscribe());
        bus.publish_output(b"x".to_vec());
        assert_eq!(bus.lock().subscribers.len(), 1);
        assert_eq!(output(kept.recv_timeout(WAIT).unwrap()).1, b"x");
    }

    #[test]
    fn close_disconnects_receivers_and_drops_later_events() {
        let bus = EventBus::new(SessionId(1), SessionLog::Off, SIZE);
        let rx = bus.subscribe();
        bus.publish_output(b"before".to_vec());
        bus.close();
        bus.publish_output(b"after".to_vec());
        bus.publish(Event::Exit {
            id: SessionId(1),
            status: None,
        });
        assert_eq!(output(rx.recv_timeout(WAIT).unwrap()).1, b"before");
        assert!(
            rx.recv_timeout(WAIT).is_err(),
            "disconnected, nothing after"
        );
        assert!(bus.subscribe().recv_timeout(WAIT).is_err());
    }

    /// A publisher blocked on a full receiver is released by `close`.
    #[test]
    fn close_frees_a_publisher_blocked_on_a_full_receiver() {
        let bus = std::sync::Arc::new(EventBus::new(SessionId(1), SessionLog::Off, SIZE));
        let full = bus.subscribe();
        for _ in 0..CAPACITY {
            bus.publish_output(Vec::new());
        }
        assert_eq!(full.len(), CAPACITY);
        let publisher = {
            let bus = bus.clone();
            std::thread::spawn(move || bus.publish_output(b"blocked".to_vec()))
        };
        // Give the publisher time to park on the full channel, so the blocked path is the
        // one exercised. The assertion holds either way: a publish that starts after the
        // close returns at once.
        std::thread::sleep(Duration::from_millis(50));
        bus.close();
        let deadline = std::time::Instant::now() + WAIT;
        while !publisher.is_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "close did not free the publisher"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        publisher.join().unwrap();
    }

    #[test]
    fn close_offers_final_events_only_to_receivers_with_capacity() {
        let bus = EventBus::new(SessionId(1), SessionLog::Off, SIZE);
        let full = bus.subscribe();
        for _ in 0..CAPACITY {
            bus.publish_output(Vec::new());
        }
        let empty = bus.subscribe();
        let final_event = Event::Title {
            id: SessionId(1),
            title: "final".into(),
        };
        bus.close_with_final([final_event.clone()]);
        assert_eq!(full.len(), CAPACITY, "full receiver was never awaited");
        assert_eq!(empty.recv_timeout(WAIT).unwrap(), final_event);
        assert!(empty.recv_timeout(WAIT).is_err());
        bus.publish_output(b"after close".to_vec());
        assert_eq!(full.len(), CAPACITY);
    }

    #[test]
    fn final_events_never_overtake_an_in_flight_fanout() {
        let bus = EventBus::new(SessionId(1), SessionLog::Off, SIZE);
        let rx = bus.subscribe();
        // Models an earlier publisher which is descheduled during fanout. Shutdown
        // cannot wait for it and must not put later final events ahead of it.
        let publishing = bus.publishing.lock().unwrap();
        bus.close_with_final([Event::Title {
            id: SessionId(1),
            title: "final".into(),
        }]);
        assert!(
            rx.recv_timeout(WAIT).is_err(),
            "final event overtook prior fanout"
        );
        drop(publishing);
        bus.publish_output(b"cancelled publisher".to_vec());
        assert!(rx.try_recv().is_err());
    }

    fn memory_bus() -> EventBus {
        let log =
            SessionLog::open(&crate::pty::SessionLogConfig::Memory { cap_bytes: 1 << 20 }).unwrap();
        EventBus::new(SessionId(1), log, SIZE)
    }

    #[test]
    fn attach_replays_what_came_before_and_streams_the_rest() {
        let bus = memory_bus();
        bus.publish_output(b"ab".to_vec());
        bus.publish_output(b"cd".to_vec());
        let (replay, rx) = bus.attach();
        bus.publish_output(b"ef".to_vec());
        assert_eq!(replay, Replay::Bytes(b"abcd".to_vec()));
        assert_eq!(output(rx.recv_timeout(WAIT).unwrap()), (2, b"ef".to_vec()));
        assert!(rx.try_recv().is_err(), "nothing else");
    }

    #[test]
    fn attach_without_a_log_replays_nothing() {
        let bus = EventBus::new(SessionId(1), SessionLog::Off, SIZE);
        bus.publish_output(b"ab".to_vec());
        let (replay, rx) = bus.attach();
        bus.publish_output(b"cd".to_vec());
        assert_eq!(replay, Replay::None);
        assert_eq!(output(rx.recv_timeout(WAIT).unwrap()), (1, b"cd".to_vec()));
    }

    #[test]
    fn attach_after_close_is_disconnected_and_the_log_is_gone() {
        let bus = memory_bus();
        bus.publish_output(b"ab".to_vec());
        bus.close();
        let (replay, rx) = bus.attach();
        assert_eq!(replay, Replay::None);
        assert!(rx.recv_timeout(WAIT).is_err());
        assert!(matches!(bus.lock().log, SessionLog::Off));
    }

    /// A receiver added after the `Exit` gets it too, once: nothing else will come.
    #[test]
    fn a_receiver_added_after_the_exit_gets_the_exit() {
        let bus = memory_bus();
        let early = bus.subscribe();
        bus.publish_output(b"ab".to_vec());
        let exit = Event::Exit {
            id: SessionId(1),
            status: None,
        };
        bus.publish(exit.clone());
        let late = bus.subscribe();
        let (replay, attached) = bus.attach();
        assert_eq!(replay, Replay::Bytes(b"ab".to_vec()));
        assert_eq!(early.capacity(), Some(CAPACITY));
        for rx in [&late, &attached] {
            assert_eq!(rx.capacity(), Some(AFTER_EXIT), "a small channel");
            assert_eq!(rx.recv_timeout(WAIT).unwrap(), exit);
            assert!(rx.try_recv().is_err(), "only the Exit");
        }
        assert_eq!(output(early.recv_timeout(WAIT).unwrap()).1, b"ab");
        assert_eq!(early.recv_timeout(WAIT).unwrap(), exit);
        assert!(early.try_recv().is_err(), "the Exit only once");
        // Teardown still disconnects them; a receiver after it gets nothing.
        bus.close();
        assert!(late.recv_timeout(WAIT).is_err());
        assert!(attached.recv_timeout(WAIT).is_err());
        assert!(bus.subscribe().recv_timeout(WAIT).is_err());
        assert_eq!(bus.attach().0, Replay::None);
    }

    /// An agent controller publishes its reaction after the `Exit`: a receiver added after
    /// the `Exit` and not drained yet has room for it, so the publisher does not block.
    #[test]
    fn a_late_receiver_has_room_for_events_after_the_exit() {
        let bus = std::sync::Arc::new(EventBus::new(SessionId(1), SessionLog::Off, SIZE));
        let exit = Event::Exit {
            id: SessionId(1),
            status: None,
        };
        bus.publish(exit.clone());
        let late = bus.subscribe();
        let title = |n: usize| Event::Title {
            id: SessionId(1),
            title: format!("after {n}"),
        };
        let publisher = {
            let bus = bus.clone();
            std::thread::spawn(move || {
                for n in 0..AFTER_EXIT - 1 {
                    bus.publish(title(n));
                }
            })
        };
        let deadline = std::time::Instant::now() + WAIT;
        while !publisher.is_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "the publisher blocked on the late receiver"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        publisher.join().unwrap();
        assert_eq!(late.recv_timeout(WAIT).unwrap(), exit);
        for n in 0..AFTER_EXIT - 1 {
            assert_eq!(late.recv_timeout(WAIT).unwrap(), title(n));
        }
        assert!(late.try_recv().is_err());
    }

    /// The controller's receiver gets an event only after every other receiver got it —
    /// also one subscribed after it.
    #[test]
    fn the_watcher_is_served_after_every_other_receiver() {
        let bus = std::sync::Arc::new(EventBus::new(SessionId(1), SessionLog::Off, SIZE));
        let watch = bus.watch();
        let late = bus.subscribe();
        for _ in 0..CAPACITY {
            bus.publish_output(Vec::new());
        }
        while watch.try_recv().is_ok() {}
        let publisher = {
            let bus = bus.clone();
            std::thread::spawn(move || bus.publish_output(b"next".to_vec()))
        };
        // The publisher parks on the full late receiver; the watcher must not have the
        // chunk yet.
        std::thread::sleep(Duration::from_millis(50));
        assert!(watch.try_recv().is_err(), "the watcher was served first");
        assert_eq!(output(late.recv_timeout(WAIT).unwrap()).0, 0);
        assert_eq!(
            output(watch.recv_timeout(WAIT).unwrap()),
            (CAPACITY as u64, b"next".to_vec())
        );
        publisher.join().unwrap();
        while late.try_recv().is_ok() {}
        // Dropped, it is pruned like any receiver; closed, it disconnects.
        drop(watch);
        bus.publish_output(Vec::new());
        assert!(bus.lock().watcher.is_none());
        let again = bus.watch();
        bus.close();
        assert!(again.recv_timeout(WAIT).is_err());
    }

    /// Attaches race a publisher: for every attach, replay ++ live is exactly the stream.
    #[test]
    fn attach_has_no_gap_and_no_overlap_under_a_concurrent_publisher() {
        const CHUNKS: u32 = 3000;
        let bus = std::sync::Arc::new(memory_bus());
        let full = {
            let rx = bus.subscribe();
            std::thread::spawn(move || rx.iter().flat_map(|e| output(e).1).collect::<Vec<u8>>())
        };
        let publisher = {
            let bus = bus.clone();
            std::thread::spawn(move || {
                for i in 0..CHUNKS {
                    bus.publish_output(format!("<{i}>").into_bytes());
                }
            })
        };
        // Each attached receiver is drained on its own thread, so none can fill up.
        let mut attached = Vec::new();
        while !publisher.is_finished() && attached.len() < 200 {
            let (replay, rx) = bus.attach();
            let drain = std::thread::spawn(move || rx.iter().map(output).collect::<Vec<_>>());
            attached.push((replay, drain));
            std::thread::yield_now();
        }
        publisher.join().unwrap();
        bus.close();
        let stream = full.join().unwrap();
        let expected: Vec<u8> = (0..CHUNKS)
            .flat_map(|i| format!("<{i}>").into_bytes())
            .collect();
        assert_eq!(stream, expected);
        let mut mid_stream = 0;
        for (replay, drain) in attached {
            let Replay::Bytes(mut seen) = replay else {
                panic!("memory replay expected");
            };
            if !seen.is_empty() && seen.len() < stream.len() {
                mid_stream += 1;
            }
            let live = drain.join().unwrap();
            for pair in live.windows(2) {
                assert_eq!(pair[1].0, pair[0].0 + 1, "live seqs are consecutive");
            }
            seen.extend(live.into_iter().flat_map(|(_, bytes)| bytes));
            assert_eq!(seen, stream, "replay ++ live must be the whole stream");
        }
        assert!(mid_stream > 0, "no attach landed mid-stream");
    }

    #[test]
    fn forward_to_delivers_in_order_and_ends_on_disconnect() {
        let bus = EventBus::new(SessionId(1), SessionLog::Off, SIZE);
        let rx = bus.subscribe();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let handle = forward_to(rx, move |event| {
            let _ = seen_tx.send(output(event).0);
        })
        .unwrap();
        for _ in 0..3 {
            bus.publish_output(Vec::new());
        }
        for want in 0..3 {
            assert_eq!(seen_rx.recv_timeout(WAIT).unwrap(), want);
        }
        bus.close();
        handle.join().unwrap();
    }
}

#[cfg(test)]
mod watcher_tests {
    use super::*;
    const SIZE: PtySize = PtySize { rows: 24, cols: 80 };
    use std::time::Duration;

    #[test]
    fn a_stalled_watcher_is_bounded_and_host_output_keeps_its_order() {
        let bus = Arc::new(EventBus::new(SessionId(1), SessionLog::Off, SIZE));
        let watch = bus.watch();
        let host = bus.subscribe();
        let drain = std::thread::spawn(move || {
            host.iter()
                .filter_map(|event| match event {
                    Event::Output { seq, .. } => Some(seq),
                    _ => None,
                })
                .collect::<Vec<_>>()
        });
        for _ in 0..WATCH_CAPACITY * 3 {
            bus.publish_output(vec![b'x'; WATCH_EVENT_BYTES]);
        }
        assert!(bus.watch_overflow.load(Ordering::Acquire));
        assert!(bus.lock().watcher.is_none());
        assert_eq!(watch.len(), WATCH_CAPACITY);
        let retained: usize = watch
            .try_iter()
            .map(|event| match event {
                Event::Output { bytes, .. } => bytes.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(retained, WATCH_CAPACITY * WATCH_EVENT_BYTES);
        assert_eq!(watch.try_recv(), Err(TryRecvError::Disconnected));
        bus.close();
        assert_eq!(
            drain.join().unwrap(),
            (0..(WATCH_CAPACITY * 3) as u64).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_full_watcher_never_deadlocks_controller_publication() {
        let bus = Arc::new(EventBus::new(SessionId(1), SessionLog::Off, SIZE));
        let _watch = bus.watch();
        for _ in 0..WATCH_CAPACITY {
            bus.publish_output(Vec::new());
        }
        let (done, received) = bounded(1);
        let publisher = {
            let bus = Arc::clone(&bus);
            std::thread::spawn(move || {
                bus.publish(Event::Title {
                    id: SessionId(1),
                    title: "controller event".into(),
                });
                let _ = done.send(());
            })
        };
        assert!(received.recv_timeout(Duration::from_secs(2)).is_ok());
        assert!(bus.watch_overflow.load(Ordering::Acquire));
        publisher.join().unwrap();
    }

    #[test]
    fn an_oversized_event_disables_monitoring_without_retaining_its_payload() {
        let bus = EventBus::new(SessionId(1), SessionLog::Off, SIZE);
        let watch = bus.watch();
        let host = bus.subscribe();
        let event = Event::Title {
            id: SessionId(1),
            title: "t".repeat(WATCH_EVENT_BYTES + 1),
        };
        bus.publish(event.clone());
        assert_eq!(host.try_recv().unwrap(), event);
        assert!(bus.watch_overflow.load(Ordering::Acquire));
        assert_eq!(watch.try_recv(), Err(TryRecvError::Disconnected));
    }
}
