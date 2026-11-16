//! Bounded transport-neutral raw PTY observers. See `docs/async-and-presentation.md#pty-observer-fanout-observers-feature`.
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use crate::{Event, ReplayCursor, ReplayError, Terminal};

/// Hard retention limits. Byte limits count raw payload; item limits also bound metadata.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum concurrent viewers.
    pub viewers: usize,
    /// Maximum retained history bytes.
    pub history_bytes: usize,
    /// Maximum retained history items.
    pub history_items: usize,
    /// Maximum queued bytes per viewer.
    pub viewer_bytes: usize,
    /// Maximum queued items per viewer.
    pub viewer_items: usize,
    /// Maximum sum of queued viewer bytes (history is separately bounded).
    pub aggregate_bytes: usize,
    /// Maximum sum of queued viewer items.
    pub aggregate_items: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            viewers: 16,
            history_bytes: 1024 * 1024,
            history_items: 1024,
            viewer_bytes: 1024 * 1024,
            viewer_items: 1024,
            aggregate_bytes: 8 * 1024 * 1024,
            aggregate_items: 8192,
        }
    }
}
/// Whitelisted events; arbitrary core diagnostics and agent payloads are never forwarded.
#[derive(Clone, Debug)]
pub enum Item {
    /// Unmodified bytes with their original output sequence and process-local provenance.
    Output {
        /// First byte.
        start: ReplayCursor,
        /// First byte after this chunk.
        end: ReplayCursor,
        /// Core output sequence.
        seq: u64,
        /// Raw output, possibly partial UTF-8 or escape sequences.
        bytes: Arc<[u8]>,
    },
    /// Natural PTY EOF (not teardown).
    Exit {
        /// Process-local session.
        session: crate::SessionId,
        /// Core exit status, when known.
        status: Option<i32>,
    },
}
impl Item {
    fn bytes(&self) -> usize {
        match self {
            Self::Output { bytes, .. } => bytes.len(),
            Self::Exit { .. } => 0,
        }
    }
}
/// Explicit terminal state, readable even when the viewer's queue was full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disconnect {
    /// Viewer exceeded a byte/item budget; reconnect and handle a gap.
    SlowViewer,
    /// Caller detached.
    Detached,
    /// Hub shut down or core receiver ended.
    Closed,
}
/// Refusal before allocating a viewer queue.
#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    /// Invalid process-local cursor.
    #[error(transparent)]
    Replay(#[from] ReplayError),
    /// No viewer admission capacity.
    #[error("observer capacity exhausted")]
    Capacity,
    /// Adapter stopped.
    #[error("observer hub closed")]
    Closed,
}
/// Initialization failure, before a worker is installed.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// Every configured limit must be positive.
    #[error("observer limits must be positive")]
    InvalidLimits,
    /// Core attachment refused.
    #[error(transparent)]
    Replay(#[from] ReplayError),
    /// Worker thread could not be spawned.
    #[error("observer worker: {0}")]
    Spawn(#[from] std::io::Error),
}
struct Queue {
    wake: Arc<Condvar>,
    items: VecDeque<Item>,
    bytes: usize,
    reason: Option<Disconnect>,
}
struct State {
    initial_geometry: (ReplayCursor, crate::PtySize),
    next_output_seq: u64,
    boundary: ReplayCursor,
    history: VecDeque<Item>,
    history_bytes: usize,
    viewers: Vec<Arc<Mutex<Queue>>>,
    closed: bool,
}
struct Shared {
    limits: Limits,
    state: Mutex<State>,
}
/// Owns one continuously drained core receiver. Drop/shutdown do not tear down the PTY.
pub struct ObserverHub {
    shared: Arc<Shared>,
    stop: crossbeam_channel::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}
/// Atomic attachment fence and explicit unrecoverable prefix. Replay precedes live items.
pub struct Viewer {
    /// First live byte at attachment.
    pub boundary: ReplayCursor,
    /// Next raw core output sequence at the live fence.
    pub next_output_seq: u64,
    /// Geometry captured when the hub started, paired with its original byte fence.
    /// Subsequent resize is not observed; transport hosts must supply resize history.
    pub initial_geometry: (ReplayCursor, crate::PtySize),
    /// AdapterHistoryUnavailable: missing half-open byte range in this adapter,
    /// whether predating startup or evicted. This says nothing about core log retention.
    /// Hosts must explicitly resync their renderer.
    pub gap: Option<std::ops::Range<u64>>,
    queue: Arc<Mutex<Queue>>,
}
impl ObserverHub {
    /// Starts at the core recovery boundary. Older core logs are not adapter history.
    /// Limits must all be positive. Attachment may wait for the core recovery lock;
    /// subsequent fanout never waits for viewers. No file replay is read.
    pub fn new(terminal: &Terminal, limits: Limits) -> Result<Self, StartError> {
        if [
            limits.viewers,
            limits.history_bytes,
            limits.history_items,
            limits.viewer_bytes,
            limits.viewer_items,
            limits.aggregate_bytes,
            limits.aggregate_items,
        ]
        .contains(&0)
        {
            return Err(StartError::InvalidLimits);
        }
        let (snapshot, receiver) = terminal.attach_live_recovery_metadata()?;
        let shared = Arc::new(Shared {
            limits,
            state: Mutex::new(State {
                boundary: snapshot.boundary,
                initial_geometry: (snapshot.boundary, snapshot.size),
                next_output_seq: snapshot.next_output_seq,
                history: VecDeque::new(),
                history_bytes: 0,
                viewers: Vec::new(),
                closed: false,
            }),
        });
        drop(snapshot);
        let (stop, stopped) = crossbeam_channel::bounded(1);
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("agent-link-observers".into())
            .spawn(move || {
                loop {
                    crossbeam_channel::select_biased! {
                        recv(stopped) -> _ => break,
                        recv(receiver) -> event => match event {
                            Ok(event) => publish(&worker_shared, event),
                            Err(_) => break,
                        }
                    }
                }
                close(&worker_shared);
            })?;
        Ok(Self {
            shared,
            stop,
            worker: Some(worker),
        })
    }
    /// Atomically queues retained replay and joins live fanout. Foreign/future cursors
    /// are refused. Missing history is an explicit gap; replay admission is all-or-nothing.
    pub fn attach(&self, from: Option<ReplayCursor>) -> Result<Viewer, AttachError> {
        let mut s = self.shared.state.lock().unwrap();
        if s.closed {
            return Err(AttachError::Closed);
        }
        let requested = from.unwrap_or(ReplayCursor {
            offset: 0,
            ..s.boundary
        });
        if requested.session != s.boundary.session {
            return Err(ReplayError::SessionMismatch {
                expected: s.boundary.session,
                actual: requested.session,
            }
            .into());
        }
        if requested.offset > s.boundary.offset {
            return Err(ReplayError::FutureCursor {
                requested: requested.offset,
                end: s.boundary.offset,
            }
            .into());
        }
        s.viewers.retain(|q| q.lock().unwrap().reason.is_none());
        if s.viewers.len() >= self.shared.limits.viewers {
            return Err(AttachError::Capacity);
        }
        let (total_bytes, total_items) = totals(&s);
        let l = self.shared.limits;
        let mut replay_bytes = 0usize;
        let mut replay_items = 0usize;
        for item in &s.history {
            let amount = match item {
                Item::Output { start, end, .. } if end.offset > requested.offset => {
                    (end.offset - requested.offset.max(start.offset)) as usize
                }
                Item::Exit { .. } => 0,
                _ => continue,
            };
            replay_bytes = replay_bytes
                .checked_add(amount)
                .ok_or(AttachError::Capacity)?;
            replay_items = replay_items.checked_add(1).ok_or(AttachError::Capacity)?;
        }
        if replay_bytes > l.viewer_bytes
            || replay_items > l.viewer_items
            || replay_bytes > l.aggregate_bytes.saturating_sub(total_bytes)
            || replay_items > l.aggregate_items.saturating_sub(total_items)
        {
            return Err(AttachError::Capacity);
        }
        let mut items = VecDeque::new();
        let mut bytes = 0;
        let mut first = s.boundary.offset;
        for item in &s.history {
            match item {
                Item::Output {
                    start,
                    end,
                    seq,
                    bytes: data,
                } if end.offset > requested.offset => {
                    let offset = requested.offset.max(start.offset);
                    first = first.min(offset);
                    let data: Arc<[u8]> = data[(offset - start.offset) as usize..].into();
                    bytes += data.len();
                    items.push_back(Item::Output {
                        start: ReplayCursor { offset, ..*start },
                        end: *end,
                        seq: *seq,
                        bytes: data,
                    });
                }
                Item::Exit { .. } => items.push_back(item.clone()),
                _ => {}
            }
        }
        let queue = Arc::new(Mutex::new(Queue {
            wake: Arc::new(Condvar::new()),
            items,
            bytes,
            reason: None,
        }));
        s.viewers.push(queue.clone());
        Ok(Viewer {
            boundary: s.boundary,
            next_output_seq: s.next_output_seq,
            initial_geometry: s.initial_geometry,
            gap: (requested.offset < first).then_some(requested.offset..first),
            queue,
        })
    }
    /// Idempotently stops fanout and joins its worker; independent of viewers/core I/O.
    pub fn shutdown(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for ObserverHub {
    fn drop(&mut self) {
        self.shutdown();
    }
}
impl Viewer {
    /// Pops one item without blocking. `reason()` distinguishes idle from disconnection.
    pub fn try_recv(&self) -> Option<Item> {
        let mut q = self.queue.lock().unwrap();
        let item = q.items.pop_front()?;
        q.bytes -= item.bytes();
        Some(item)
    }
    /// Waits at most `timeout` for one item or disconnection. Returns `None` on
    /// timeout or disconnect; read `reason()` to distinguish them. Never waits on core I/O.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Item> {
        let q = self.queue.lock().unwrap();
        let wake = q.wake.clone();
        let (mut q, _) = wake
            .wait_timeout_while(q, timeout, |q| q.items.is_empty() && q.reason.is_none())
            .unwrap();
        let item = q.items.pop_front()?;
        q.bytes -= item.bytes();
        Some(item)
    }
    /// Returns the terminal reason independently of queue capacity.
    pub fn reason(&self) -> Option<Disconnect> {
        self.queue.lock().unwrap().reason
    }
    /// Idempotently releases queued retention; no further items are delivered.
    pub fn detach(&self) {
        disconnect(&mut self.queue.lock().unwrap(), Disconnect::Detached);
    }
}
impl Drop for Viewer {
    fn drop(&mut self) {
        self.detach();
    }
}
fn disconnect(q: &mut Queue, reason: Disconnect) {
    if q.reason.is_none() {
        q.reason = Some(reason);
    }
    q.wake.notify_all();
    q.items.clear();
    q.bytes = 0;
}
fn totals(s: &State) -> (usize, usize) {
    s.viewers.iter().fold((0usize, 0usize), |(b, i), q| {
        let q = q.lock().unwrap();
        (b.saturating_add(q.bytes), i.saturating_add(q.items.len()))
    })
}
fn close(shared: &Shared) {
    let mut s = shared.state.lock().unwrap();
    s.closed = true;
    for q in &s.viewers {
        let mut q = q.lock().unwrap();
        if q.reason.is_none() {
            q.reason = Some(Disconnect::Closed);
        }
        q.wake.notify_all();
    }
    s.history.clear();
    s.history_bytes = 0;
}
fn publish(shared: &Shared, event: Event) {
    let mut s = shared.state.lock().unwrap();
    let item = match event {
        Event::Output { id, seq, bytes } => {
            let start = s.boundary;
            s.next_output_seq = seq + 1;
            s.boundary = ReplayCursor {
                session: id,
                offset: start.offset + bytes.len() as u64,
            };
            Item::Output {
                start,
                end: s.boundary,
                seq,
                bytes: bytes.into(),
            }
        }
        Event::Exit { id, status } => Item::Exit {
            session: id,
            status,
        },
        _ => return,
    };
    let bytes = item.bytes();
    let l = shared.limits;
    s.history_bytes += bytes;
    s.history.push_back(item.clone());
    while s.history_bytes > l.history_bytes || s.history.len() > l.history_items {
        if let Some(old) = s.history.pop_front() {
            s.history_bytes -= old.bytes();
        }
    }
    let (mut total_bytes, mut total_items) = totals(&s);
    for q in &s.viewers {
        let mut q = q.lock().unwrap();
        if q.reason.is_some() {
            continue;
        }
        if bytes > l.viewer_bytes.saturating_sub(q.bytes)
            || q.items.len() >= l.viewer_items
            || bytes > l.aggregate_bytes.saturating_sub(total_bytes)
            || total_items >= l.aggregate_items
        {
            total_bytes -= q.bytes;
            total_items -= q.items.len();
            disconnect(&mut q, Disconnect::SlowViewer);
        } else {
            q.bytes += bytes;
            q.items.push_back(item.clone());
            q.wake.notify_all();
            total_bytes += bytes;
            total_items += 1;
        }
    }
    s.viewers.retain(|q| q.lock().unwrap().reason.is_none());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PtySize, SessionId};
    fn hub(limits: Limits) -> ObserverHub {
        let boundary = ReplayCursor {
            session: SessionId(5),
            offset: 10,
        };
        let (stop, _) = crossbeam_channel::bounded(1);
        ObserverHub {
            shared: Arc::new(Shared {
                limits,
                state: Mutex::new(State {
                    boundary,
                    initial_geometry: (boundary, PtySize { rows: 24, cols: 80 }),
                    next_output_seq: 7,
                    history: VecDeque::new(),
                    history_bytes: 0,
                    viewers: Vec::new(),
                    closed: false,
                }),
            }),
            stop,
            worker: None,
        }
    }
    fn output(h: &ObserverHub, seq: u64, bytes: &[u8]) {
        publish(
            &h.shared,
            Event::Output {
                id: SessionId(5),
                seq,
                bytes: bytes.to_vec(),
            },
        );
    }
    #[test]
    fn replay_live_fence_and_partial_cursor_preserve_bytes() {
        let h = hub(Limits::default());
        output(&h, 7, &[0xff, 0x1b, b'[']);
        let v = h
            .attach(Some(ReplayCursor {
                session: SessionId(5),
                offset: 11,
            }))
            .unwrap();
        assert_eq!(v.boundary.offset, 13);
        assert_eq!(v.next_output_seq, 8);
        assert_eq!(v.initial_geometry.0.offset, 10);
        assert!(v.gap.is_none());
        output(&h, 8, b"x");
        let Item::Output {
            start, bytes, seq, ..
        } = v.try_recv().unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (start.offset, seq, bytes.as_ref()),
            (11, 7, &[0x1b, b'['][..])
        );
        let Item::Output {
            start, bytes, seq, ..
        } = v.try_recv().unwrap()
        else {
            panic!()
        };
        assert_eq!((start.offset, seq, bytes.as_ref()), (13, 8, &b"x"[..]));
        assert!(v.try_recv().is_none());
    }
    #[test]
    fn gaps_invalid_cursors_and_retention() {
        let h = hub(Limits {
            history_items: 1,
            ..Limits::default()
        });
        output(&h, 7, b"ab");
        output(&h, 8, b"cd");
        let v = h.attach(None).unwrap();
        assert_eq!(v.gap, Some(0..12));
        assert!(matches!(
            h.attach(Some(ReplayCursor {
                session: SessionId(6),
                offset: 0
            })),
            Err(AttachError::Replay(ReplayError::SessionMismatch { .. }))
        ));
        assert!(matches!(
            h.attach(Some(ReplayCursor {
                session: SessionId(5),
                offset: 15
            })),
            Err(AttachError::Replay(ReplayError::FutureCursor { .. }))
        ));
    }
    #[test]
    fn slow_viewer_disconnect_is_independent_and_detach_idempotent() {
        let h = hub(Limits {
            viewer_items: 1,
            ..Limits::default()
        });
        let slow = h.attach(None).unwrap();
        let fast = h.attach(None).unwrap();
        output(&h, 7, b"a");
        fast.try_recv().unwrap();
        output(&h, 8, b"b");
        assert_eq!(slow.reason(), Some(Disconnect::SlowViewer));
        assert!(slow.try_recv().is_none());
        assert!(fast.try_recv().is_some());
        slow.detach();
        slow.detach();
        assert_eq!(slow.reason(), Some(Disconnect::SlowViewer));
        close(&h.shared);
        assert_eq!(fast.reason(), Some(Disconnect::Closed));
        assert!(matches!(h.attach(None), Err(AttachError::Closed)));
    }
    #[test]
    fn byte_item_aggregate_and_admission_limits() {
        let h = hub(Limits {
            aggregate_bytes: 2,
            aggregate_items: 2,
            viewers: 2,
            ..Limits::default()
        });
        let a = h.attach(None).unwrap();
        let b = h.attach(None).unwrap();
        assert!(matches!(h.attach(None), Err(AttachError::Capacity)));
        output(&h, 7, b"ab");
        assert!(a.try_recv().is_some());
        assert_eq!(b.reason(), Some(Disconnect::SlowViewer));
        assert!(h.attach(None).is_ok());
        let h = hub(Limits {
            viewer_bytes: 1,
            history_bytes: 1,
            ..Limits::default()
        });
        let v = h.attach(None).unwrap();
        output(&h, 7, b"ab");
        assert_eq!(v.reason(), Some(Disconnect::SlowViewer));
        assert_eq!(h.attach(None).unwrap().gap, Some(0..12));
    }
    #[test]
    fn metadata_items_are_bounded_and_eof_survives_disconnect() {
        let h = hub(Limits {
            aggregate_items: 1,
            ..Limits::default()
        });
        let a = h.attach(None).unwrap();
        let b = h.attach(None).unwrap();
        publish(
            &h.shared,
            Event::Exit {
                id: SessionId(5),
                status: Some(0),
            },
        );
        assert_eq!(b.reason(), Some(Disconnect::SlowViewer));
        close(&h.shared);
        assert_eq!(a.reason(), Some(Disconnect::Closed));
        assert!(matches!(
            a.recv_timeout(Duration::ZERO),
            Some(Item::Exit {
                status: Some(0),
                ..
            })
        ));
        assert!(a.recv_timeout(Duration::ZERO).is_none());
    }
    #[test]
    fn closed_wakes_waiter_and_replay_refusal_does_not_take_slot() {
        let h = hub(Limits {
            viewer_bytes: 1,
            viewers: 1,
            ..Limits::default()
        });
        output(&h, 7, b"ab");
        assert!(matches!(h.attach(None), Err(AttachError::Capacity)));
        let v = h
            .attach(Some(ReplayCursor {
                session: SessionId(5),
                offset: 12,
            }))
            .unwrap();
        let (entered, ready) = crossbeam_channel::bounded(1);
        let worker = thread::spawn(move || {
            entered.send(()).unwrap();
            assert!(v.recv_timeout(Duration::from_secs(5)).is_none());
            assert_eq!(v.reason(), Some(Disconnect::Closed));
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        close(&h.shared);
        worker.join().unwrap();
    }
    #[test]
    fn attach_publish_race_has_no_duplicate_or_missing_output() {
        for _ in 0..64 {
            let h = Arc::new(hub(Limits::default()));
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let worker_h = h.clone();
            let worker_barrier = barrier.clone();
            let worker = thread::spawn(move || {
                worker_barrier.wait();
                output(&worker_h, 7, b"race");
            });
            barrier.wait();
            let v = h.attach(None).unwrap();
            worker.join().unwrap();
            let Item::Output {
                start, end, bytes, ..
            } = v.try_recv().unwrap()
            else {
                panic!()
            };
            assert_eq!(
                (start.offset, end.offset, bytes.as_ref()),
                (10, 14, &b"race"[..])
            );
            assert!(v.try_recv().is_none());
        }
    }
}
