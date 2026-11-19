//! Byte and chunk admission, including the write in flight.

use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

use crate::{Error, Result};

/// Finite limits for host writes and the screen model's replies, shared by one PTY.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputLimits {
    /// Largest individual write, in bytes (default 1 MiB).
    pub max_write_bytes: usize,
    /// Queued and actively written bytes combined (default 4 MiB).
    pub max_pending_bytes: usize,
    /// Queued and actively written chunks combined (default 1,024).
    pub max_pending_chunks: usize,
}

impl Default for InputLimits {
    fn default() -> Self {
        Self {
            max_write_bytes: 1024 * 1024,
            max_pending_bytes: 4 * 1024 * 1024,
            max_pending_chunks: 1024,
        }
    }
}

impl InputLimits {
    pub(crate) fn validate(self) -> Result<()> {
        let values = [
            self.max_write_bytes,
            self.max_pending_bytes,
            self.max_pending_chunks,
        ];
        if values.iter().any(|&n| n > isize::MAX as usize / 4)
            || self
                .max_pending_chunks
                .checked_mul(std::mem::size_of::<Chunk>() + std::mem::size_of::<usize>())
                .is_none_or(|n| n > isize::MAX as usize)
        {
            return Err(Error::InvalidInput(
                "input limits exceed allocation size arithmetic".into(),
            ));
        }
        if self.max_write_bytes == 0
            || self.max_pending_bytes < self.max_write_bytes
            || self.max_pending_chunks == 0
        {
            return Err(Error::InvalidInput(
                "input limits must be nonzero and pending bytes must cover one maximum write"
                    .into(),
            ));
        }
        Ok(())
    }
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
struct Used {
    bytes: usize,
    chunks: usize,
    closed: bool,
}

#[derive(Clone)]
pub(crate) struct Input {
    tx: Sender<Chunk>,
    used: Arc<Mutex<Used>>,
    limits: InputLimits,
}

pub(crate) struct Chunk {
    pub(crate) bytes: Vec<u8>,
    used: Arc<Mutex<Used>>,
}

pub(crate) struct InputReceiver {
    pub(crate) rx: Receiver<Chunk>,
    used: Arc<Mutex<Used>>,
}

impl Drop for InputReceiver {
    fn drop(&mut self) {
        // Serialize closure with admission. Crossbeam otherwise retains queued values
        // until the last sender drops, even though no receiver can consume them.
        let mut used = lock(&self.used);
        used.closed = true;
        let pending: Vec<_> = self.rx.try_iter().collect();
        drop(used);
        drop(pending);
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        let mut used = lock(&self.used);
        used.bytes -= self.bytes.len();
        used.chunks -= 1;
    }
}

impl Input {
    #[cfg(feature = "questions")]
    pub(super) fn max_write_bytes(&self) -> usize {
        self.limits.max_write_bytes
    }
    pub(crate) fn new(limits: InputLimits) -> (Self, InputReceiver) {
        let (tx, rx) = bounded(limits.max_pending_chunks);
        let used = Arc::default();
        (
            Self {
                tx,
                used: Arc::clone(&used),
                limits,
            },
            InputReceiver { rx, used },
        )
    }

    pub(crate) fn send(&self, bytes: &[u8]) -> Result<()> {
        self.send_with(bytes, || {})
    }

    /// Screen replies also refuse lock contention: the reader must not wait behind a
    /// host admission hook (which can inspect the foreground process on Windows).
    #[cfg(feature = "screen")]
    pub(crate) fn try_send(&self, bytes: &[u8]) -> Result<()> {
        self.send_using(bytes, || {}, true)
    }

    /// `admitted` runs before the writer can see the chunk, only after admission succeeds.
    pub(crate) fn send_with(&self, bytes: &[u8], admitted: impl FnOnce()) -> Result<()> {
        self.send_using(bytes, admitted, false)
    }

    fn send_using(&self, bytes: &[u8], admitted: impl FnOnce(), try_only: bool) -> Result<()> {
        let size = bytes.len();
        if size > self.limits.max_write_bytes {
            return Err(Error::InputTooLarge {
                size,
                limit: self.limits.max_write_bytes,
            });
        }
        let result = {
            let mut used = if try_only {
                match self.used.try_lock() {
                    Ok(guard) => guard,
                    Err(TryLockError::Poisoned(e)) => e.into_inner(),
                    Err(TryLockError::WouldBlock) => return Err(Error::QueueFull),
                }
            } else {
                lock(&self.used)
            };
            if used.closed {
                return Err(Error::Closed);
            }
            if used.chunks >= self.limits.max_pending_chunks
                || size > self.limits.max_pending_bytes - used.bytes
            {
                return Err(Error::QueueFull);
            }
            // Allocation and the hook precede reservation so unwinding cannot leak it.
            let bytes = bytes.to_vec();
            admitted();
            used.bytes += size;
            used.chunks += 1;
            self.tx.try_send(Chunk {
                bytes,
                used: Arc::clone(&self.used),
            })
        };
        // A failed send owns a Chunk whose destructor takes the budget lock.
        result.map_err(|e| match e {
            TrySendError::Full(_) => Error::QueueFull,
            TrySendError::Disconnected(_) => Error::Closed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "screen")]
    #[test]
    fn replies_do_not_wait_for_a_host_admission_lock() {
        let (input, _rx) = Input::new(InputLimits::default());
        let held = lock(&input.used);
        assert!(matches!(input.try_send(b"reply"), Err(Error::QueueFull)));
        drop(held);
        input.try_send(b"reply").unwrap();
    }

    #[test]
    fn active_writes_hold_bytes_and_count_and_exit_releases_queued_data() {
        let (input, rx) = Input::new(InputLimits {
            max_write_bytes: 4,
            max_pending_bytes: 4,
            max_pending_chunks: 2,
        });
        input.send(b"1234").unwrap();
        let active = rx.rx.recv().unwrap();
        assert!(matches!(input.send(b"x"), Err(Error::QueueFull)));
        drop(active);
        input.send(b"").unwrap();
        let active = rx.rx.recv().unwrap();
        input.send(b"x").unwrap();
        assert!(matches!(input.send(b""), Err(Error::QueueFull)));
        drop(rx);
        assert!(matches!(input.send(b"x"), Err(Error::Closed)));
        assert_eq!(lock(&input.used).bytes, 0);
        assert_eq!(lock(&input.used).chunks, 1);
        drop(active);
        assert_eq!(lock(&input.used).chunks, 0);
    }

    #[test]
    fn refusals_do_not_call_the_admission_hook_or_use_capacity() {
        let (input, rx) = Input::new(InputLimits {
            max_write_bytes: 1,
            max_pending_bytes: 1,
            max_pending_chunks: 1,
        });
        assert!(matches!(
            input.send_with(b"xx", || panic!()),
            Err(Error::InputTooLarge { .. })
        ));
        input.send(b"x").unwrap();
        assert!(matches!(
            input.send_with(b"x", || panic!()),
            Err(Error::QueueFull)
        ));
        drop(rx.rx.recv().unwrap());
        input.send(b"y").unwrap();
    }
}
