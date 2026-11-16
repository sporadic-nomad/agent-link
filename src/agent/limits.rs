//! Delivery admission limits.

use crate::{Error, Result};

/// Finite admission limits for one agent terminal.
///
/// Reservations cover queued and running jobs, and completed jobs whose delivery event
/// has not finished publishing. Draining the event receiver therefore releases capacity.
/// Rejected calls resolve their ticket immediately and publish no delivery event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentLimits {
    /// Maximum raw text bytes in one message or reset command, before sanitizing
    /// (default 256 KiB). The PTY's maximum write must also fit the paste framing.
    pub max_message_bytes: usize,
    /// Combined raw text bytes of admitted jobs (default 4 MiB). A reset-and-send reserves
    /// both command and message bytes, even though it uses one job slot.
    pub max_pending_bytes: usize,
    /// Admitted jobs, including running jobs and unpublished completions (default 128).
    /// This also bounds the number of retained host callbacks; their own allocations
    /// cannot be measured by the library.
    pub max_pending_jobs: usize,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: 256 * 1024,
            max_pending_bytes: 4 * 1024 * 1024,
            max_pending_jobs: 128,
        }
    }
}

impl AgentLimits {
    pub(crate) fn validate(self) -> Result<()> {
        let values = [
            self.max_message_bytes,
            self.max_pending_bytes,
            self.max_pending_jobs,
        ];
        if values.iter().any(|&n| n > isize::MAX as usize / 4)
            || self
                .max_pending_jobs
                .checked_mul(std::mem::size_of::<super::controller::Job>())
                .is_none_or(|n| n > isize::MAX as usize)
        {
            return Err(Error::InvalidInput(
                "agent limits exceed allocation size arithmetic".into(),
            ));
        }
        if self.max_message_bytes == 0
            || self.max_pending_bytes < self.max_message_bytes
            || self.max_pending_jobs == 0
        {
            return Err(Error::InvalidInput(
                "agent limits must be nonzero and pending bytes must cover one maximum message"
                    .into(),
            ));
        }
        Ok(())
    }
}
