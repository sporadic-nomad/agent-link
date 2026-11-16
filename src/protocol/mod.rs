//! Wire-independent bounded lifecycle primitives. Provider routing stays in adapters.
//! No host callbacks or wake functions run under these locks.

#[cfg(any(feature = "codex", feature = "acp"))]
mod pending;
#[cfg(any(feature = "codex", feature = "acp"))]
pub(crate) use pending::{Pending, admit};

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

#[cfg(all(test, any(feature = "codex", feature = "acp")))]
use crossbeam_channel::bounded;
#[cfg(test)]
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) struct Completion<T> {
    value: Mutex<Option<Arc<T>>>,
    changed: Condvar,
    #[cfg(test)]
    before_finish: Mutex<Option<(Sender<()>, Receiver<()>)>>,
}

impl<T> Completion<T> {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            value: Mutex::new(None),
            changed: Condvar::new(),
            #[cfg(test)]
            before_finish: Mutex::new(None),
        })
    }

    pub(crate) fn finish(&self, value: T) {
        #[cfg(test)]
        {
            let pause = lock(&self.before_finish).take();
            if let Some((entered, resume)) = pause {
                let _ = entered.send(());
                // Dropping the sender also releases the worker on test failure.
                assert!(
                    !matches!(
                        resume.recv_timeout(std::time::Duration::from_secs(10)),
                        Err(RecvTimeoutError::Timeout)
                    ),
                    "completion publication pause timed out"
                );
            }
        }
        let mut slot = lock(&self.value);
        if slot.is_none() {
            *slot = Some(Arc::new(value));
            self.changed.notify_all();
        }
    }

    #[cfg(all(test, any(feature = "codex", feature = "acp")))]
    pub(crate) fn pause_next_finish(&self) -> (Receiver<()>, Sender<()>) {
        let (entered, paused) = bounded(1);
        let (resume, released) = bounded(1);
        *lock(&self.before_finish) = Some((entered, released));
        (paused, resume)
    }

    pub(crate) fn get(&self) -> Option<Arc<T>> {
        lock(&self.value).clone()
    }

    pub(crate) fn wait_until(&self, deadline: Instant) -> Option<Arc<T>> {
        let mut slot = lock(&self.value);
        loop {
            if slot.is_some() {
                return slot.clone();
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            slot = self
                .changed
                .wait_timeout(slot, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

struct QueueState<T> {
    items: VecDeque<(T, usize)>,
    bytes: usize,
    closed: bool,
}

/// One consumer; storage is released on pop, never on a separate acknowledgement.
pub(crate) struct Queue<T> {
    state: Mutex<QueueState<T>>,
    ready: Condvar,
    max_items: usize,
    max_bytes: usize,
}

impl<T> Queue<T> {
    pub(crate) fn new(max_items: usize, max_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(QueueState {
                items: VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            ready: Condvar::new(),
            max_items,
            max_bytes,
        })
    }

    pub(crate) fn push(&self, item: T, bytes: usize) -> bool {
        let mut state = lock(&self.state);
        if state.closed
            || state.items.len() >= self.max_items
            || bytes > self.max_bytes - state.bytes
        {
            return false;
        }
        state.items.push_back((item, bytes));
        state.bytes += bytes;
        self.ready.notify_one();
        true
    }

    pub(crate) fn recv_until(&self, deadline: Instant) -> Option<T> {
        let mut state = lock(&self.state);
        loop {
            if let Some((item, bytes)) = state.items.pop_front() {
                state.bytes -= bytes;
                return Some(item);
            }
            if state.closed {
                return None;
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            state = self
                .ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    pub(crate) fn close(&self) {
        lock(&self.state).closed = true;
        self.ready.notify_all();
    }
}
