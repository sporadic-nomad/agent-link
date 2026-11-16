//! Per-admission observations. No history is retained by the transport.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use super::lock;

/// Why an admitted stdin write did not receive a successful flush receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RpcWriteFailure {
    /// The absolute deadline elapsed, including time waiting in the queue.
    DeadlineExceeded,
    /// EOF, transport failure, or teardown stopped input.
    Closed,
    /// Writing or flushing failed. No payload-bearing error string is retained.
    Io(std::io::ErrorKind),
}

/// Terminal transport observation for one admitted line; never a provider receipt.
///
/// Treat unknown future variants (the `_` match arm) like [`Self::PossiblyWritten`]:
/// bytes may have reached the child, so never automatically resend. Only
/// [`Self::NotWritten`] establishes that no bytes from this line were emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RpcWriteOutcome {
    /// The entire line and newline were written and flushed before the deadline.
    Written,
    /// The writer never started this line. No bytes from it were emitted.
    NotWritten(RpcWriteFailure),
    /// Writing started, but successful flush was not established. Zero, some, or all
    /// bytes may have reached the child. Never automatically resend this operation.
    PossiblyWritten(RpcWriteFailure),
}

/// Snapshot of an admitted write. A successful admission initially queues the line;
/// the writer may advance it before the caller first observes the receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RpcWriteStatus {
    /// Accepted, waiting for the writer.
    Queued,
    /// The writer has claimed the line; emission may have begun.
    Writing,
    /// A sticky terminal outcome.
    Finished(RpcWriteOutcome),
}

#[derive(Debug)]
pub(super) struct ReceiptState {
    status: Mutex<RpcWriteStatus>,
    changed: Condvar,
}

impl ReceiptState {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(RpcWriteStatus::Queued),
            changed: Condvar::new(),
        })
    }

    pub(super) fn set(&self, status: RpcWriteStatus) {
        let mut current = lock(&self.status);
        if !matches!(*current, RpcWriteStatus::Finished(_)) {
            *current = status;
            self.changed.notify_all();
        }
    }
}

/// A bounded, independently readable write observation from
/// [`RpcChild::send_line_tracked`](super::RpcChild::send_line_tracked).
///
/// Dropping this handle does not cancel the write. The transport retains at most one
/// receipt per admitted line, with no completion history; finished receipts retained
/// by the host consume host-owned memory. Waiting does not drain the event receiver.
/// No async runtime is required; use a blocking worker when waiting from an async host.
#[derive(Debug)]
pub struct RpcWriteReceipt {
    pub(super) state: Arc<ReceiptState>,
}

impl RpcWriteReceipt {
    #[cfg(all(test, any(feature = "codex", feature = "acp")))]
    pub(crate) fn finished_for_test(outcome: RpcWriteOutcome) -> Self {
        let state = ReceiptState::new();
        state.set(RpcWriteStatus::Finished(outcome));
        Self { state }
    }

    /// Reads the current status without waiting for child I/O.
    pub fn status(&self) -> RpcWriteStatus {
        *lock(&self.state.status)
    }

    /// Blocks until a terminal observation, including deadline expiry or teardown.
    /// A full event receiver or blocked pipe cannot prevent settlement.
    pub fn wait(&self) -> RpcWriteOutcome {
        let mut status = lock(&self.state.status);
        loop {
            if let RpcWriteStatus::Finished(outcome) = *status {
                return outcome;
            }
            status = self
                .state
                .changed
                .wait(status)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Waits only until `deadline`. `None` means observation timed out; it does not
    /// cancel the write or alter the write's own deadline or outcome.
    pub fn wait_until(&self, deadline: Instant) -> Option<RpcWriteOutcome> {
        let mut status = lock(&self.state.status);
        loop {
            if let RpcWriteStatus::Finished(outcome) = *status {
                return Some(outcome);
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            status = self
                .state
                .changed
                .wait_timeout(status, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}
