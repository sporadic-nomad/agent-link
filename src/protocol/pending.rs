//! Common registration/admission records; wire actions and outcomes remain generic.
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use super::Completion;
use crate::rpc::RpcWriteReceipt;
use crate::{Error, Result};

/// Registered before the dispatcher can see the frame. Admission remains charged
/// until both the write and native response/completion publication have finished.
pub(crate) struct Pending<A, R, C> {
    pub(crate) id: Option<String>,
    pub(crate) action: A,
    pub(crate) line: Option<String>,
    /// Set before leaving the controller lock to publish bytes. A racing shutdown
    /// must preserve uncertainty until a receipt establishes that nothing was written.
    pub(crate) dispatched: bool,
    pub(crate) receipt: Option<RpcWriteReceipt>,
    pub(crate) deadline: Instant,
    pub(crate) bytes: usize,
    pub(crate) control: bool,
    pub(crate) request: Option<Arc<Completion<R>>>,
    /// Committed before publication so a racing shutdown can settle the ticket
    /// with the observed result instead of losing it or inventing uncertainty.
    pub(crate) request_outcome: Option<R>,
    pub(crate) reply: Option<Arc<Completion<C>>>,
    pub(crate) responded: bool,
}

/// Nonblocking admission within the caller's registration lock. Active and responded
/// but unpublished records still count. Callers reserve control capacity separately.
pub(crate) fn admit<A, R, C>(
    pending: &VecDeque<Pending<A, R, C>>,
    bytes: usize,
    control: bool,
    max_count: usize,
    max_bytes: usize,
) -> Result<()> {
    let (count, used) = pending
        .iter()
        .filter(|p| p.control == control)
        .fold((0usize, 0usize), |(count, bytes), p| {
            (count + 1, bytes + p.bytes)
        });
    if count >= max_count || bytes > max_bytes.saturating_sub(used) {
        return Err(Error::QueueFull);
    }
    Ok(())
}
