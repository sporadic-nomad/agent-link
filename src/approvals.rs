//! Opt-in bounded execution of native approval callbacks.
//!
//! Construct one [`Executor`] and explicitly submit capabilities received from native
//! events. Nothing installs a callback automatically. Callbacks run on a fixed number
//! of dedicated threads, never on a protocol dispatcher. Active and queued requests
//! share item and serialized native-payload budgets. Hung Rust callbacks cannot be
//! killed: they retain their worker and admission capacity until they return; no
//! replacement workers are spawned. Shutdown and ticket deadlines do not wait for them.
//! If disposing a panic payload panics, the pool closes and settles tickets. Its
//! secondary panic payload is deliberately retained to prevent recursive panics:
//! at most one per request admitted at closure, with host-owned payload size.
//!
//! Decisions, reply flushes and remote tool execution are separate observations.
//! [`Outcome::Replied`] contains the native write ticket, not an execution receipt.
//! Delegation sends nothing and leaves the host's original capability available.
//! Keep a token clone before submission if delegation is possible. All replies use
//! the original single-use native token, so generation/turn cancellation still wins.
//! Ticket cancellation prevents a reply that has not started admission; a reply
//! already admitted to the adapter cannot be retracted by this executor.

#![cfg_attr(
    not(any(feature = "codex", feature = "acp", feature = "claude", test)),
    allow(dead_code)
)]

use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Fixed worker count and budgets covering queued **and active** callbacks.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Dedicated workers. Must be nonzero.
    pub workers: usize,
    /// Total admitted requests, including callbacks that have timed out but still run.
    pub requests: usize,
    /// Total serialized native request payload bytes, including running callbacks.
    pub payload_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            workers: 2,
            requests: 32,
            payload_bytes: 1024 * 1024,
        }
    }
}

/// Refused admission never invokes a callback or sends a native response.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// Zero or inconsistent worker/item/byte configuration.
    #[error("invalid approval executor limits")]
    InvalidLimits,
    /// A worker or deadline monitor could not be created.
    #[error("approval executor thread could not be created")]
    ThreadSpawn,
    /// The executor has shut down.
    #[error("approval executor is closed")]
    Closed,
    /// Active plus queued requests exceed a finite item or byte budget.
    #[error("approval executor capacity exhausted")]
    Full,
    /// The absolute capability or host deadline has elapsed.
    #[error("approval callback deadline elapsed")]
    Expired,
}

/// Native reply admission results retain their provider-specific write tickets.
#[non_exhaustive]
pub enum Reply {
    /// Codex reply admission. The ticket observes writing, not tool execution.
    #[cfg(feature = "codex")]
    Codex(Result<crate::codex::ReplyTicket, crate::codex::CodexError>),
    /// ACP reply admission. The ticket observes writing, not tool execution.
    #[cfg(feature = "acp")]
    Acp(Result<crate::acp::RequestTicket, crate::acp::AcpError>),
    /// Claude reply admission. The ticket observes writing, not tool execution.
    #[cfg(feature = "claude")]
    Claude(crate::Result<crate::claude::Ticket>),
}

/// Sticky authoritative callback outcome, independent of any native event receiver.
#[non_exhaustive]
pub enum Outcome {
    /// Native reply was attempted through the original guarded token.
    Replied(Reply),
    /// Explicitly delegated to the host; no response was sent.
    Delegated,
    /// Deadline elapsed. A running callback retains its worker and budget.
    Expired,
    /// Host cancellation; a running callback's later decision is discarded.
    Cancelled,
    /// Executor closed; callbacks may still be running on detached workers.
    Closed,
}
struct Completion {
    pool_closed: Arc<AtomicBool>,
    outcome: Mutex<Option<Arc<Outcome>>>,
    changed: Condvar,
    deadline: Instant,
}
impl Completion {
    fn finish(&self, outcome: Outcome) {
        let mut slot = lock(&self.outcome);
        if slot.is_none() {
            *slot = Some(Arc::new(if self.pool_closed.load(Ordering::SeqCst) {
                Outcome::Closed
            } else if Instant::now() >= self.deadline {
                Outcome::Expired
            } else {
                outcome
            }));
        }
        drop(slot);
        self.changed.notify_all();
    }
    fn expired(&self) -> bool {
        if Instant::now() >= self.deadline {
            self.finish(Outcome::Expired);
        }
        lock(&self.outcome).is_some()
    }
    #[cfg(any(feature = "codex", feature = "acp", feature = "claude"))]
    fn reply(&self, response: impl FnOnce() -> Reply) {
        // This gate serializes cancellation against reply admission. It is neither
        // the pool nor the adapter controller lock. The adapter performs its own
        // freshness/expiry checks, and never waits for a physical pipe here.
        let mut slot = lock(&self.outcome);
        if slot.is_none() {
            *slot = Some(Arc::new(if self.pool_closed.load(Ordering::SeqCst) {
                Outcome::Closed
            } else if Instant::now() >= self.deadline {
                Outcome::Expired
            } else {
                Outcome::Replied(response())
            }));
        }
        drop(slot);
        self.changed.notify_all();
    }
}

/// Cloneable observation/cancellation handle. Dropping it does not cancel the callback.
#[derive(Clone)]
pub struct Ticket(Arc<Completion>);
impl Ticket {
    /// Reads the sticky outcome, enforcing the absolute callback deadline.
    pub fn outcome(&self) -> Option<Arc<Outcome>> {
        self.0.expired();
        lock(&self.0.outcome).clone()
    }
    /// Waits until an observation deadline; timeout does not cancel work.
    pub fn wait_until(&self, deadline: Instant) -> Option<Arc<Outcome>> {
        let deadline = deadline.min(self.0.deadline);
        let mut slot = lock(&self.0.outcome);
        while slot.is_none() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            slot = self
                .0
                .changed
                .wait_timeout(slot, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        drop(slot);
        self.outcome()
    }
    /// Settles immediately unless a reply admission has already won the gate.
    pub fn cancel(&self) {
        self.0.finish(Outcome::Cancelled);
    }
}

type Run = Box<dyn FnOnce(&Completion) + Send + 'static>;
struct Job {
    id: u64,
    completion: Arc<Completion>,
    run: Run,
}
struct Entry {
    bytes: usize,
    completion: Arc<Completion>,
    expired: bool,
}
struct State {
    closed: bool,
    next_id: u64,
    bytes: usize,
    entries: HashMap<u64, Entry>,
    queue: VecDeque<Job>,
}
struct Shared {
    closed: Arc<AtomicBool>,
    state: Mutex<State>,
    changed: Condvar,
    limits: Limits,
}

/// Fixed dedicated worker pool. Drop closes admission and settles tickets without
/// joining callbacks. Reuse a pool rather than creating a pool per request.
pub struct Executor {
    shared: Arc<Shared>,
}
impl Executor {
    /// Validates finite limits and starts the fixed workers plus a deadline monitor.
    /// Thread creation failure closes any workers already started.
    pub fn new(limits: Limits) -> Result<Self, Error> {
        if limits.workers == 0 || limits.requests < limits.workers || limits.payload_bytes == 0 {
            return Err(Error::InvalidLimits);
        }
        let executor = Self {
            shared: Arc::new(Shared {
                closed: Arc::new(AtomicBool::new(false)),
                limits,
                changed: Condvar::new(),
                state: Mutex::new(State {
                    closed: false,
                    next_id: 0,
                    bytes: 0,
                    entries: HashMap::new(),
                    queue: VecDeque::new(),
                }),
            }),
        };
        for index in 0..limits.workers {
            let shared = Arc::clone(&executor.shared);
            std::thread::Builder::new()
                .name(format!("approval-{index}"))
                .spawn(move || worker(shared))
                .map_err(|_| Error::ThreadSpawn)?;
        }
        let shared = Arc::clone(&executor.shared);
        std::thread::Builder::new()
            .name("approval-deadlines".into())
            .spawn(move || monitor(shared))
            .map_err(|_| Error::ThreadSpawn)?;
        Ok(executor)
    }
    fn submit(&self, bytes: usize, deadline: Instant, run: Run) -> Result<Ticket, Error> {
        if deadline <= Instant::now() {
            return Err(Error::Expired);
        }
        let completion = Arc::new(Completion {
            pool_closed: Arc::clone(&self.shared.closed),
            outcome: Mutex::new(None),
            changed: Condvar::new(),
            deadline,
        });
        let mut state = lock(&self.shared.state);
        if state.closed {
            return Err(Error::Closed);
        }
        if state.entries.len() >= self.shared.limits.requests
            || bytes > self.shared.limits.payload_bytes.saturating_sub(state.bytes)
        {
            return Err(Error::Full);
        }
        let id = state.next_id;
        state.next_id = state.next_id.checked_add(1).ok_or(Error::Full)?;
        state.bytes += bytes;
        state.entries.insert(
            id,
            Entry {
                bytes,
                completion: Arc::clone(&completion),
                expired: false,
            },
        );
        state.queue.push_back(Job {
            id,
            completion: Arc::clone(&completion),
            run,
        });
        drop(state);
        self.shared.changed.notify_all();
        Ok(Ticket(completion))
    }
    /// Refuses further admission, settles every ticket. Fixed workers discard queued callbacks.
    /// Running callbacks retain their resources until return; no joins occur.
    pub fn close(&self) {
        close_pool(&self.shared);
    }
}
impl Drop for Executor {
    fn drop(&mut self) {
        self.close();
    }
}
fn close_pool(shared: &Shared) {
    let completions: Vec<_> = {
        let mut state = lock(&shared.state);
        state.closed = true;
        shared.closed.store(true, Ordering::SeqCst);
        state
            .entries
            .values()
            .map(|e| Arc::clone(&e.completion))
            .collect()
    };
    for completion in completions {
        completion.finish(Outcome::Closed);
    }
    shared.changed.notify_all();
}
fn worker(shared: Arc<Shared>) {
    loop {
        let (job, closed) = {
            let mut state = lock(&shared.state);
            loop {
                if let Some(job) = state.queue.pop_front() {
                    break (job, state.closed);
                }
                if state.closed {
                    return;
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
        };
        let Job {
            id,
            completion,
            run,
        } = job;
        let result = catch_unwind(AssertUnwindSafe(|| {
            if !closed && !completion.expired() {
                run(&completion);
            } else {
                drop(run);
            }
        }));
        // Provider wrappers normally catch callback panics and deny natively. This
        // outer boundary also catches a skipped capture's destructor panic, so it
        // cannot silently destroy a fixed worker or move queued drops to the owner.
        if let Err(payload) = result {
            completion.finish(Outcome::Cancelled);
            // A panic payload is user-owned too: its destructor can block or panic.
            // Retain admission through disposal, and contain a second panic rather
            // than losing this fixed worker. Never recursively drop panic payloads:
            // close admission before continuing, so forgotten secondary payloads
            // are bounded by the requests already admitted at this failure.
            if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
                std::mem::forget(secondary);
                close_pool(&shared);
            }
        }
        // User callback captures/destructors are dropped before releasing capacity.
        // Even a callback panic is caught by provider wrappers below.
        let entry = {
            let mut state = lock(&shared.state);
            let entry = state.entries.remove(&id);
            if let Some(entry) = &entry {
                state.bytes -= entry.bytes;
            }
            entry
        };
        drop(entry);
        shared.changed.notify_all();
    }
}
fn monitor(shared: Arc<Shared>) {
    loop {
        let completions = {
            let mut state = lock(&shared.state);
            if state.closed {
                return;
            }
            let now = Instant::now();
            let mut completions = Vec::new();
            for entry in state.entries.values_mut() {
                if !entry.expired && entry.completion.deadline <= now {
                    entry.expired = true;
                    completions.push(Arc::clone(&entry.completion));
                }
            }
            if completions.is_empty() {
                let deadline = state
                    .entries
                    .values()
                    .filter(|e| !e.expired)
                    .map(|e| e.completion.deadline)
                    .min();
                state = if let Some(deadline) = deadline {
                    shared
                        .changed
                        .wait_timeout(state, deadline.saturating_duration_since(now))
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                } else {
                    shared
                        .changed
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner())
                };
                drop(state);
                continue;
            }
            completions
        };
        for completion in completions {
            completion.finish(Outcome::Expired);
        }
    }
}

// Count serialization without allocating a second native payload buffer.
#[cfg(any(feature = "codex", feature = "acp", feature = "claude"))]
fn payload_bytes(value: &serde_json::Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    if serde_json::to_writer(&mut count, value).is_err() {
        return usize::MAX;
    }
    count.0
}

/// Codex-specific native choices; exact supported amendment objects are accepted.
#[cfg(feature = "codex")]
pub enum CodexDecision {
    /// Exact offered native decision; invalid choices fall back to cancellation.
    Decision(serde_json::Value),
    /// Native cancellation, or offered decline if cancellation is unavailable.
    Cancel,
    /// Send nothing; host retains the pending token.
    Delegate,
}
#[cfg(feature = "codex")]
impl Executor {
    /// Executes a callback with immutable native identities, details and deadline.
    /// Panic/invalid choice requests native cancellation or offered decline. The earlier host/capability
    /// deadline applies; queue time is included. Token clones remain single-use.
    pub fn codex<F>(
        &self,
        token: crate::codex::Approval,
        deadline: Instant,
        callback: F,
    ) -> Result<Ticket, Error>
    where
        F: FnOnce(&crate::codex::ApprovalRequest) -> CodexDecision + Send + 'static,
    {
        let deadline = deadline.min(token.request().deadline);
        let bytes = payload_bytes(&token.request().params)
            .saturating_add(payload_bytes(&token.request().id))
            .saturating_add(token.request().method.len());
        self.submit(
            bytes,
            deadline,
            Box::new(move |completion| {
                let decision = catch_unwind(AssertUnwindSafe(|| callback(token.request())))
                    .unwrap_or(CodexDecision::Cancel);
                match decision {
                    CodexDecision::Delegate => completion.finish(Outcome::Delegated),
                    decision => {
                        let value = match decision {
                            CodexDecision::Decision(value)
                                if token.request().decisions().contains(&value) =>
                            {
                                value
                            }
                            _ => {
                                let choices = token.request().decisions();
                                if choices.contains(&serde_json::Value::from("cancel")) {
                                    serde_json::Value::from("cancel")
                                } else if choices.contains(&serde_json::Value::from("decline")) {
                                    serde_json::Value::from("decline")
                                } else {
                                    serde_json::Value::from("cancel")
                                }
                            }
                        };
                        completion.reply(|| Reply::Codex(token.respond(value)));
                    }
                }
            }),
        )
    }
}

/// ACP choice IDs carry provider semantics; this executor never interprets labels.
#[cfg(feature = "acp")]
pub enum AcpDecision {
    /// Select an exact offered option ID. Invalid IDs produce native cancellation.
    Select(String),
    /// Explicit native cancelled outcome.
    Cancel,
    /// Send nothing; host retains the pending token.
    Delegate,
}
/// Immutable ACP request details, including local origin and native identity.
#[cfg(feature = "acp")]
pub struct AcpRequest<'a> {
    /// Complete native permission parameters.
    pub params: &'a serde_json::Value,
    /// Exact native numeric/string reverse request ID.
    pub id: &'a serde_json::Value,
    /// Local connection generation and transport sequence.
    pub origin: (crate::SessionId, u64),
    /// Absolute callback deadline (minimum of host and capability deadline).
    pub deadline: Instant,
}
#[cfg(feature = "acp")]
impl Executor {
    /// Executes an ACP permission callback. Panics and invalid option IDs cancel.
    pub fn acp<F>(
        &self,
        token: crate::acp::Permission,
        deadline: Instant,
        callback: F,
    ) -> Result<Ticket, Error>
    where
        F: FnOnce(&AcpRequest<'_>) -> AcpDecision + Send + 'static,
    {
        let deadline = deadline.min(token.deadline());
        let bytes =
            payload_bytes(token.request()).saturating_add(payload_bytes(token.request_id()));
        self.submit(
            bytes,
            deadline,
            Box::new(move |completion| {
                let request = AcpRequest {
                    params: token.request(),
                    id: token.request_id(),
                    origin: token.origin(),
                    deadline,
                };
                let decision = catch_unwind(AssertUnwindSafe(|| callback(&request)))
                    .unwrap_or(AcpDecision::Cancel);
                match decision {
                    AcpDecision::Delegate => completion.finish(Outcome::Delegated),
                    decision => completion.reply(|| {
                        Reply::Acp(match decision {
                            AcpDecision::Select(id) => match token.select(&id) {
                                Err(crate::acp::AcpError::InvalidDecision) => token.cancel(),
                                result => result,
                            },
                            _ => token.cancel(),
                        })
                    }),
                }
            }),
        )
    }
}

/// Claude's native updated-input/deny choices, without invented ACP/Codex semantics.
#[cfg(feature = "claude")]
pub enum ClaudeDecision {
    /// Allow with an object containing the complete native updated tool input.
    Allow(serde_json::Value),
    /// Deny with a native message.
    Deny(String),
    /// Send nothing; host retains the pending token.
    Delegate,
}
/// Immutable Claude control request and origin, with no response capability.
#[cfg(feature = "claude")]
pub struct ClaudeRequest<'a> {
    /// Complete native request, including tool input and extension fields.
    pub native: &'a serde_json::Value,
    /// Exact native control request ID.
    pub id: &'a str,
    /// Local connection generation and transport sequence.
    pub origin: (crate::SessionId, u64),
    /// Absolute callback deadline (minimum of host and capability deadline).
    pub deadline: Instant,
}
#[cfg(feature = "claude")]
impl Executor {
    /// Executes a Claude callback. Panic or malformed input produces denial; a
    /// bounded-admission failure is exposed in the native reply result, never retried.
    pub fn claude<F>(
        &self,
        token: crate::claude::Approval,
        deadline: Instant,
        callback: F,
    ) -> Result<Ticket, Error>
    where
        F: FnOnce(&ClaudeRequest<'_>) -> ClaudeDecision + Send + 'static,
    {
        let deadline = deadline.min(token.deadline());
        let bytes = payload_bytes(token.request()).saturating_add(token.request_id().len());
        self.submit(
            bytes,
            deadline,
            Box::new(move |completion| {
                let request = ClaudeRequest {
                    native: token.request(),
                    id: token.request_id(),
                    origin: token.origin(),
                    deadline,
                };
                let decision = catch_unwind(AssertUnwindSafe(|| callback(&request)))
                    .unwrap_or_else(|_| ClaudeDecision::Deny("Approval callback failed".into()));
                match decision {
                    ClaudeDecision::Delegate => completion.finish(Outcome::Delegated),
                    decision => completion.reply(|| {
                        let result = match decision {
                            ClaudeDecision::Allow(input) if input.is_object() => {
                                token.respond(Some(&input), None)
                            }
                            ClaudeDecision::Deny(message) => token.respond(None, Some(&message)),
                            _ => token.respond(None, Some("Invalid approval callback decision")),
                        };
                        Reply::Claude(match result {
                            Err(crate::Error::InvalidInput(_)) => {
                                token.respond(None, Some("Invalid approval callback decision"))
                            }
                            result => result,
                        })
                    }),
                }
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }
    fn pool(requests: usize, bytes: usize) -> Executor {
        Executor::new(Limits {
            workers: 1,
            requests,
            payload_bytes: bytes,
        })
        .unwrap()
    }
    fn delegated(counter: Arc<AtomicUsize>) -> Run {
        Box::new(move |completion| {
            counter.fetch_add(1, Ordering::SeqCst);
            completion.finish(Outcome::Delegated);
        })
    }
    #[test]
    fn hung_worker_keeps_active_and_queued_budgets_and_close_settles_tickets() {
        let executor = pool(2, 10);
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, blocked) = crossbeam_channel::bounded::<()>(1);
        let active = executor
            .submit(
                6,
                deadline(),
                Box::new(move |completion| {
                    entered.send(()).unwrap();
                    let _ = blocked.recv_timeout(Duration::from_secs(5));
                    completion.finish(Outcome::Delegated);
                }),
            )
            .unwrap();
        started.recv_deadline(deadline()).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        assert!(matches!(
            executor.submit(5, deadline(), delegated(Arc::clone(&counter))),
            Err(Error::Full)
        ));
        let queued = executor
            .submit(4, deadline(), delegated(Arc::clone(&counter)))
            .unwrap();
        assert!(matches!(
            executor.submit(0, deadline(), delegated(Arc::clone(&counter))),
            Err(Error::Full)
        ));
        executor.close();
        assert!(matches!(
            &*active.wait_until(deadline()).unwrap(),
            Outcome::Closed
        ));
        assert!(matches!(
            &*queued.wait_until(deadline()).unwrap(),
            Outcome::Closed
        ));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert!(matches!(
            executor.submit(0, deadline(), delegated(counter)),
            Err(Error::Closed)
        ));
        drop(release);
    }
    #[test]
    fn queued_and_active_expiry_settle_without_worker_and_late_results_cannot_replace() {
        let executor = pool(3, 10);
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, blocked) = crossbeam_channel::bounded::<()>(1);
        let expiry = Instant::now() + Duration::from_secs(1);
        let active = executor
            .submit(
                5,
                expiry,
                Box::new(move |completion| {
                    entered.send(()).unwrap();
                    let _ = blocked.recv_timeout(Duration::from_secs(5));
                    completion.finish(Outcome::Delegated);
                }),
            )
            .unwrap();
        started.recv_deadline(deadline()).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let queued = executor
            .submit(5, expiry, delegated(Arc::clone(&counter)))
            .unwrap();
        // Observe the independent monitor directly, without Ticket's deadline fallback.
        let monitor_deadline = deadline();
        for ticket in [&active, &queued] {
            let mut value = lock(&ticket.0.outcome);
            while value.is_none() {
                let remaining = monitor_deadline
                    .checked_duration_since(Instant::now())
                    .unwrap();
                let (next, timed) = ticket.0.changed.wait_timeout(value, remaining).unwrap();
                assert!(!timed.timed_out(), "deadline monitor did not settle ticket");
                value = next;
            }
            assert!(matches!(&**value.as_ref().unwrap(), Outcome::Expired));
        }
        assert!(matches!(
            executor.submit(1, deadline(), delegated(Arc::clone(&counter))),
            Err(Error::Full)
        ));
        drop(release);
        // The trailing marker establishes that the skipped queued closure was disposed.
        let (done, finished) = crossbeam_channel::bounded(1);
        executor
            .submit(
                0,
                deadline(),
                Box::new(move |c| {
                    c.finish(Outcome::Delegated);
                    done.send(()).unwrap();
                }),
            )
            .unwrap();
        finished.recv_deadline(deadline()).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert!(matches!(&*active.outcome().unwrap(), Outcome::Expired));
    }
    #[test]
    fn queued_cancellation_skips_callback_and_refused_admission_never_invokes() {
        let executor = pool(3, 100);
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, blocked) = crossbeam_channel::bounded::<()>(1);
        executor
            .submit(
                1,
                deadline(),
                Box::new(move |c| {
                    entered.send(()).unwrap();
                    let _ = blocked.recv_timeout(Duration::from_secs(5));
                    c.finish(Outcome::Delegated);
                }),
            )
            .unwrap();
        started.recv_deadline(deadline()).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let queued = executor
            .submit(1, deadline(), delegated(Arc::clone(&counter)))
            .unwrap();
        queued.clone().cancel();
        assert!(matches!(&*queued.outcome().unwrap(), Outcome::Cancelled));
        assert!(matches!(
            executor.submit(101, deadline(), delegated(Arc::clone(&counter))),
            Err(Error::Full)
        ));
        assert!(matches!(
            executor.submit(1, Instant::now(), delegated(Arc::clone(&counter))),
            Err(Error::Expired)
        ));
        let (done, finished) = crossbeam_channel::bounded(1);
        executor
            .submit(
                1,
                deadline(),
                Box::new(move |c| {
                    c.finish(Outcome::Delegated);
                    done.send(()).unwrap();
                }),
            )
            .unwrap();
        drop(release);
        finished.recv_deadline(deadline()).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn shutdown_never_drops_user_captures_on_caller_and_keeps_destructor_charged() {
        struct BlockingDrop {
            shared: Arc<Shared>,
            entered: crossbeam_channel::Sender<()>,
            resume: crossbeam_channel::Receiver<()>,
        }
        impl Drop for BlockingDrop {
            fn drop(&mut self) {
                assert!(
                    self.shared.state.try_lock().is_ok(),
                    "user destructor ran under pool lock"
                );
                self.entered.send(()).unwrap();
                let _ = self.resume.recv_timeout(Duration::from_secs(5));
            }
        }
        let executor = pool(2, 2);
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, blocked) = crossbeam_channel::bounded::<()>(1);
        executor
            .submit(
                1,
                deadline(),
                Box::new(move |c| {
                    entered.send(()).unwrap();
                    let _ = blocked.recv_timeout(Duration::from_secs(5));
                    c.finish(Outcome::Delegated);
                }),
            )
            .unwrap();
        started.recv_deadline(deadline()).unwrap();
        let (destructor_entered, destructor_started) = crossbeam_channel::bounded(1);
        let (destructor_release, destructor_blocked) = crossbeam_channel::bounded(1);
        let probe = BlockingDrop {
            shared: Arc::clone(&executor.shared),
            entered: destructor_entered,
            resume: destructor_blocked,
        };
        let queued = executor
            .submit(
                1,
                deadline(),
                Box::new(move |c| {
                    let _ = &probe;
                    c.finish(Outcome::Delegated);
                }),
            )
            .unwrap();
        executor.close();
        assert!(matches!(&*queued.outcome().unwrap(), Outcome::Closed));
        assert_eq!(lock(&executor.shared.state).entries.len(), 2);
        drop(release);
        destructor_started.recv_deadline(deadline()).unwrap();
        assert_eq!(lock(&executor.shared.state).bytes, 1);
        executor.close();
        drop(destructor_release);
    }
    #[test]
    fn close_invalidates_decisions_before_all_ticket_publications_finish() {
        let executor = Arc::new(
            Executor::new(Limits {
                workers: 2,
                requests: 2,
                payload_bytes: 2,
            })
            .unwrap(),
        );
        let (entered, started) = crossbeam_channel::bounded(2);
        let mut tickets = Vec::new();
        let mut releases = Vec::new();
        for _ in 0..2 {
            let entered = entered.clone();
            let (release, resume) = crossbeam_channel::bounded::<()>(1);
            releases.push(release);
            tickets.push(
                executor
                    .submit(
                        1,
                        deadline(),
                        Box::new(move |c| {
                            entered.send(()).unwrap();
                            let _ = resume.recv_deadline(deadline());
                            c.finish(Outcome::Delegated);
                        }),
                    )
                    .unwrap(),
            );
        }
        for _ in 0..2 {
            started.recv_deadline(deadline()).unwrap();
        }
        // Hold the first publication in close's stable map iteration. The other
        // callback must still observe closure before its own ticket is visited.
        let first = Arc::clone(
            &lock(&executor.shared.state)
                .entries
                .values()
                .next()
                .unwrap()
                .completion,
        );
        let first_index = tickets
            .iter()
            .position(|t| Arc::ptr_eq(&t.0, &first))
            .unwrap();
        let publication = lock(&first.outcome);
        let closer = Arc::clone(&executor);
        let closing = std::thread::spawn(move || closer.close());
        let end = deadline();
        while !executor.shared.closed.load(Ordering::SeqCst) {
            assert!(Instant::now() < end, "close did not invalidate admission");
            std::thread::yield_now();
        }
        let other_index = 1 - first_index;
        releases[other_index].send(()).unwrap();
        assert!(matches!(
            &*tickets[other_index].wait_until(deadline()).unwrap(),
            Outcome::Closed
        ));
        drop(publication);
        closing.join().unwrap();
        drop(releases);
    }
    #[test]
    fn panicking_skipped_capture_does_not_destroy_fixed_worker() {
        struct PanicDrop;
        impl Drop for PanicDrop {
            fn drop(&mut self) {
                panic!("synthetic capture destructor panic");
            }
        }
        let executor = pool(3, 3);
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, resume) = crossbeam_channel::bounded::<()>(1);
        executor
            .submit(
                1,
                deadline(),
                Box::new(move |c| {
                    entered.send(()).unwrap();
                    let _ = resume.recv_deadline(deadline());
                    c.finish(Outcome::Delegated);
                }),
            )
            .unwrap();
        started.recv_deadline(deadline()).unwrap();
        let capture = PanicDrop;
        let skipped = executor
            .submit(
                1,
                deadline(),
                Box::new(move |c| {
                    let _ = &capture;
                    c.finish(Outcome::Delegated);
                }),
            )
            .unwrap();
        skipped.cancel();
        let marker = executor
            .submit(1, deadline(), Box::new(|c| c.finish(Outcome::Delegated)))
            .unwrap();
        drop(release);
        assert!(matches!(
            &*marker.wait_until(deadline()).unwrap(),
            Outcome::Delegated
        ));
        assert!(matches!(&*skipped.outcome().unwrap(), Outcome::Cancelled));
    }
    #[test]
    fn panic_payload_disposal_failure_closes_pool_without_caller_destruction() {
        struct Secondary(Arc<AtomicUsize>);
        impl Drop for Secondary {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("recursive payload destructor");
            }
        }
        struct Primary(Arc<AtomicUsize>);
        impl Drop for Primary {
            fn drop(&mut self) {
                std::panic::panic_any(Secondary(Arc::clone(&self.0)));
            }
        }
        struct BlockingCapture {
            entered: crossbeam_channel::Sender<()>,
            resume: crossbeam_channel::Receiver<()>,
        }
        impl Drop for BlockingCapture {
            fn drop(&mut self) {
                self.entered.send(()).unwrap();
                let _ = self.resume.recv_deadline(deadline());
            }
        }
        let executor = pool(2, 2);
        let secondary_drops = Arc::new(AtomicUsize::new(0));
        let payload = Primary(Arc::clone(&secondary_drops));
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, resume) = crossbeam_channel::bounded::<()>(1);
        let failed = executor
            .submit(
                1,
                deadline(),
                Box::new(move |_| {
                    entered.send(()).unwrap();
                    let _ = resume.recv_deadline(deadline());
                    std::panic::panic_any(payload);
                }),
            )
            .unwrap();
        started.recv_deadline(deadline()).unwrap();
        let (capture_entered, capture_started) = crossbeam_channel::bounded(1);
        let (capture_release, capture_resume) = crossbeam_channel::bounded::<()>(1);
        let capture = BlockingCapture {
            entered: capture_entered,
            resume: capture_resume,
        };
        let invocations = Arc::new(AtomicUsize::new(0));
        let callback_invocations = Arc::clone(&invocations);
        let queued = executor
            .submit(
                1,
                deadline(),
                Box::new(move |_| {
                    let _ = &capture;
                    callback_invocations.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .unwrap();
        drop(release);
        assert!(matches!(
            &*queued.wait_until(deadline()).unwrap(),
            Outcome::Closed
        ));
        assert!(matches!(&*failed.outcome().unwrap(), Outcome::Cancelled));
        assert!(matches!(
            executor.submit(0, deadline(), Box::new(|_| {})),
            Err(Error::Closed)
        ));
        capture_started.recv_deadline(deadline()).unwrap();
        assert_eq!(secondary_drops.load(Ordering::SeqCst), 0);
        assert_eq!(invocations.load(Ordering::SeqCst), 0);
        // The worker is still disposing a queued user capture; owner Drop must return.
        drop(executor);
        drop(capture_release);
    }
    #[test]
    fn invalid_limits_are_refused_before_threads() {
        for limits in [
            Limits {
                workers: 0,
                ..Limits::default()
            },
            Limits {
                requests: 1,
                workers: 2,
                payload_bytes: 1,
            },
            Limits {
                payload_bytes: 0,
                ..Limits::default()
            },
        ] {
            assert!(matches!(Executor::new(limits), Err(Error::InvalidLimits)));
        }
    }
}
