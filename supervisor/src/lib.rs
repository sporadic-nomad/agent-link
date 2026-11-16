//! Durable coordination for application-hosted agent workers, separate from agent-link.
//!
//! [`Supervisor`] holds an exclusive local-filesystem ownership lock. Each native
//! dispatch is preceded by a committed, synchronized `SubmittedUnknown` receipt.
//! Repeating an operation ID returns the receipt; it never resends, even after a crash.
//! Runtime incarnations, provider conversations and worker identities stay distinct.
//!
//! The application supplies a [`Runtime`], authenticated IPC and a worker process that
//! outlives its viewers. The optional `codex` feature supplies an owned-stdio runtime.
//! This crate does not daemonize, signal saved PIDs or change ordinary agent-link
//! Drop behavior. Methods involving
//! persistence are synchronous and may block on disk. [`StopHandle`] bypasses disk and
//! operation admission. See the crate README for recovery and filesystem assumptions.
//!
//! ```no_run
//! use std::{path::Path, sync::Arc, time::Instant};
//! use agent_link_supervisor::{Binding, Limits, Receipt, Result, Runtime, RuntimeId, Supervisor};
//!
//! // Run in the application-owned worker process. The factory must strictly resume
//! // the supplied existing conversation and enforce the Runtime contract.
//! fn submit_once(
//!     root: &Path,
//!     binding: Binding,
//!     factory: impl FnOnce(&Binding, RuntimeId) -> Result<Arc<dyn Runtime>>,
//! ) -> Result<Receipt> {
//!     let mut worker = Supervisor::open(root, binding, Limits::default())?;
//!     // A pre-existing fence refuses this call; recovery is explicit.
//!     worker.resume(Instant::now(), factory)?;
//!     let receipt = worker.submit("stable-operation-id", b"exact native input")?;
//!     worker.stop_handle()?.request();
//!     // Await native teardown before confirm_stopped; Drop preserves the fence.
//!     Ok(receipt)
//! }
//! ```

#[cfg(feature = "codex")]
pub mod codex;

mod leases;
mod store;
mod worker;

use std::sync::Arc;
use std::time::Duration;

pub use leases::Lease;
pub use worker::Supervisor;

/// Expected refusal or persistence failure. Errors contain no prompt or native output.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Invalid or oversized input/configuration.
    #[error("invalid supervisor input or limits")]
    InvalidInput,
    /// Another owner holds the lifetime lock; no takeover is attempted.
    #[error("worker already owned")]
    Owned,
    /// Provider/session identity differs from the durable binding.
    #[error("provider session binding mismatch")]
    BindingMismatch,
    /// Existing operation ID was used for different bytes or operation kind.
    #[error("operation ID conflicts with its durable digest")]
    Conflict,
    /// A finite operation, disk, result, or lease budget is exhausted.
    #[error("supervisor capacity exhausted")]
    Capacity,
    /// Unknown or active work prevents another turn.
    #[error("worker has unresolved work")]
    Busy,
    /// An old runtime may still exist or its unload has not been confirmed.
    #[error("runtime cleanup evidence required")]
    RecoveryRequired,
    /// Runtime, lease, turn or evidence identity is stale.
    #[error("stale runtime, lease, or native evidence")]
    Stale,
    /// No runtime is attached, or stop was requested.
    #[error("runtime unavailable or stopping")]
    Stopped,
    /// An earlier persistence error may have committed; reopen before more work.
    #[error("persistence failed; reopen and inspect durable state")]
    Poisoned,
    /// Unsupported or corrupt store. Never creates a replacement conversation.
    #[error("invalid or unsupported supervisor store")]
    InvalidStore,
    /// OS-level ownership or filesystem failure.
    #[error("supervisor filesystem error: {0}")]
    Io(#[from] std::io::Error),
    /// SQLite error. SQL contains no user payloads.
    #[error("supervisor database error: {0}")]
    Database(#[from] rusqlite::Error),
    /// OS randomness failed; no predictable identity is substituted.
    #[error("could not generate worker identity")]
    Random,
}

/// Supervisor result.
pub type Result<T> = std::result::Result<T, Error>;

/// Immutable provider conversation binding. The host must supply an existing exact
/// conversation and use strict native resume; fresh-session fallback is forbidden.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// Provider/adapter namespace, up to 128 bytes.
    pub provider: String,
    /// Full provider conversation ID, up to 1024 bytes. Never a display prefix.
    pub session: String,
}

/// Full durable worker identity; generated once from OS randomness, never a PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WorkerId(pub [u8; 16]);

/// Exact runtime birth identity. Owner replacement advances the generation, but an
/// unresolved old incarnation remains fenced until cleanup is positively confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RuntimeId {
    /// Durable store/worker identity.
    pub worker: WorkerId,
    /// Monotonically allocated birth generation, independent of process-local IDs.
    pub generation: u64,
}

/// SHA-256 of domain-separated operation bytes. Prompts are not persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Digest(pub [u8; 32]);

/// Work whose deduplication record is retained for the entire store lifetime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Start a native turn with the exact submitted payload bytes.
    Submit,
    /// Interrupt the exact turn of this submit operation.
    Interrupt(String),
}

/// Durable native evidence; acknowledgement and terminal completion are distinct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Intent committed before dispatch; dispatch or native acceptance may be unknown.
    SubmittedUnknown,
    /// Native acceptance for this exact turn. For interrupt, only acknowledgement.
    Accepted(String),
    /// Exact native terminal evidence, with a bounded opaque result supplied by the host.
    Terminal {
        /// Native turn identity.
        turn: String,
        /// Native outcome bytes; may be sensitive. Not included in Debug output.
        result: NativeResult,
    },
}

/// Bounded by [`Limits::result_bytes`] when persisted. Debug prints only its size.
#[derive(Clone, PartialEq, Eq)]
pub struct NativeResult(pub Vec<u8>);

impl std::fmt::Debug for NativeResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NativeResult({} bytes)", self.0.len())
    }
}

/// Persisted before being returned to an observer. Replays never invoke the runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// Caller-selected exact operation ID, bounded to 128 bytes.
    pub id: String,
    /// Kind and cancellation target, if any.
    pub operation: Operation,
    /// Digest binding this ID to exact bytes and operation semantics.
    pub digest: Digest,
    /// Runtime incarnation at the dispatch boundary.
    pub runtime: RuntimeId,
    /// Strongest committed native evidence.
    pub outcome: Outcome,
}

/// Result of a native submit or interrupt call. Transport errors, timeouts and
/// missing acknowledgements must map to Unknown, never authorize an automatic retry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation {
    /// No exact acceptance evidence.
    Unknown,
    /// Exact provider turn ID (for interrupt this must equal its target).
    Accepted(String),
}

/// Application-owned native runtime. Implementations must preserve exact binding,
/// refuse implicit steering/fresh resume, and keep `stop` independent of blocked
/// submit/interrupt calls and event consumers. The same instance must never be reused
/// for a successor incarnation. All native events still need continuous bounded drain.
///
/// No method runs under a supervisor mutex or a database transaction. Submit and
/// interrupt can block; the application sets native deadlines. Panics leave committed
/// unknown intent and the runtime fence intact. Rust cannot forcibly stop a callback.
pub trait Runtime: Send + Sync + 'static {
    /// Exact binding actually returned by native resume.
    fn binding(&self) -> Binding;
    /// Send once. The receipt contains the operation ID for native correlation where
    /// supported; it is not a claim that the provider implements idempotency.
    fn submit(&self, intent: &Receipt, payload: &[u8]) -> Observation;
    /// Interrupt only this exact native turn, never a successor or generic foreground.
    fn interrupt(&self, intent: &Receipt, turn: &str) -> Observation;
    /// Initiate stop without waiting on ordinary work, persistence or event delivery.
    /// Must be idempotent. This alone does not prove teardown/unsubscribe has completed.
    fn stop(&self);
}

/// Independently admitted stop capability bound to one runtime instance. It does not
/// retain a runtime after supervisor disposal and cannot stop a replacement incarnation.
#[derive(Clone)]
pub struct StopHandle {
    pub(crate) runtime: std::sync::Weak<dyn Runtime>,
    // 0 = available, 1 = stop callback executing, 2 = callback returned/unwound.
    pub(crate) state: Arc<std::sync::atomic::AtomicU8>,
}

impl StopHandle {
    /// Mark stopping before invoking the runtime's nonblocking stop path. No disk I/O,
    /// operation budget, queue or supervisor lock is involved. Clones invoke the stop
    /// callback at most once; repeated requests cannot reach a reused native handle.
    pub fn request(&self) {
        use std::sync::atomic::Ordering;
        if self
            .state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        struct Finish<'a>(&'a std::sync::atomic::AtomicU8);
        impl Drop for Finish<'_> {
            fn drop(&mut self) {
                self.0.store(2, Ordering::Release);
            }
        }
        let _finish = Finish(&self.state);
        if let Some(runtime) = self.runtime.upgrade() {
            runtime.stop();
        }
    }

    /// Whether this incarnation has been asked to stop.
    pub fn is_requested(&self) -> bool {
        self.state.load(std::sync::atomic::Ordering::Acquire) != 0
    }

    pub(crate) fn settled(&self) -> bool {
        self.state.load(std::sync::atomic::Ordering::Acquire) == 2
    }
}

/// Finite per-worker budgets. Persistence budgets are immutable once a store exists;
/// changing them requires an explicit future migration, not deleting deduplication data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Lifetime submit count, including completed records. Default 1024.
    pub operations: u32,
    /// Independent lifetime interrupt count. Default 64; stop needs no record.
    pub controls: u32,
    /// Maximum supplied payload bytes, checked before hashing or calling native code.
    pub payload_bytes: u32,
    /// Reserved maximum result bytes per record, including interrupt receipts.
    pub result_bytes: u32,
    /// SQLite database size cap; rollback journal can additionally approach this size.
    pub database_bytes: u32,
    /// Maximum simultaneous attachment leases. Default 8.
    pub leases: u32,
    /// Lease validity after attach/renew. Default 30 seconds.
    pub lease_ttl: Duration,
    /// Time without a live attachment before stop. Default 5 minutes. Host must call
    /// [`Supervisor::tick`] regularly; active work does not override this explicit expiry.
    pub detached_ttl: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            operations: 1024,
            controls: 64,
            payload_bytes: 1024 * 1024,
            result_bytes: 16 * 1024,
            database_bytes: 32 * 1024 * 1024,
            leases: 8,
            lease_ttl: Duration::from_secs(30),
            detached_ttl: Duration::from_secs(300),
        }
    }
}

pub(crate) fn text_ok(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && !text.contains('\0')
}

pub(crate) fn validate_binding(binding: &Binding) -> Result<()> {
    if text_ok(&binding.provider, 128) && text_ok(&binding.session, 1024) {
        Ok(())
    } else {
        Err(Error::InvalidInput)
    }
}
