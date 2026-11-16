//! Nonblocking admission, ordered half-close, and independent deadline settlement.
//!
//! Lock order: input state -> receipt state. Neither lock guards pipe I/O or event
//! publication. Receipts use condition variables, never host-owned wake callbacks.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use super::receipt::ReceiptState;
use super::{
    RpcLimits, RpcStream, RpcTransportError, RpcWriteFailure, RpcWriteOutcome, RpcWriteReceipt,
    RpcWriteStatus, lock,
};
use crate::{Error, Result};

struct Metadata {
    size: usize,
    deadline: Option<Instant>,
    receipt: Option<Arc<ReceiptState>>,
}

impl Metadata {
    fn status(&self, status: RpcWriteStatus) {
        if let Some(receipt) = &self.receipt {
            receipt.set(status);
        }
    }

    fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| deadline <= now)
    }
}

pub(super) struct Line {
    pub(super) text: String,
    meta: Metadata,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Line>,
    active: Option<Metadata>,
    bytes: usize,
    accepting: bool,
    stopped: bool,
    deadlines_started: bool,
}

struct Shared {
    state: Mutex<State>,
    writer_ready: Condvar,
    deadlines_changed: Condvar,
    limits: RpcLimits,
}

/// Whether the writer owns a new terminal failure or must honor an earlier stop.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum WriteCompletion {
    Written,
    Stopped,
    Failed(RpcTransportError),
}

#[derive(Clone)]
pub(super) struct Input(Arc<Shared>);

impl Input {
    pub(super) fn new(limits: RpcLimits) -> Self {
        Self(Arc::new(Shared {
            state: Mutex::new(State {
                accepting: true,
                ..State::default()
            }),
            writer_ready: Condvar::new(),
            deadlines_changed: Condvar::new(),
            limits,
        }))
    }

    pub(super) fn send(&self, text: &str) -> Result<()> {
        let mut state = self.admit(text, None, || Ok(()))?;
        self.enqueue(&mut state, text, None, None);
        Ok(())
    }

    pub(super) fn send_tracked(
        &self,
        text: &str,
        deadline: Instant,
        start_deadlines: impl FnOnce() -> Result<()>,
    ) -> Result<RpcWriteReceipt> {
        let mut state = self.admit(text, Some(deadline), start_deadlines)?;
        // Allocate a receipt only after admission succeeds; no rejection history.
        let receipt = ReceiptState::new();
        self.enqueue(&mut state, text, Some(deadline), Some(Arc::clone(&receipt)));
        Ok(RpcWriteReceipt { state: receipt })
    }

    fn line_size(&self, text: &str) -> Result<usize> {
        if text.contains('\n') {
            return Err(Error::InvalidInput(
                "rpc line contains an interior newline".into(),
            ));
        }
        let size = text.len().saturating_add(1);
        if size > self.0.limits.max_stdin_line_bytes {
            return Err(Error::InputTooLarge {
                size,
                limit: self.0.limits.max_stdin_line_bytes,
            });
        }
        Ok(size)
    }

    /// Retain the admission lock until enqueue so capacity and worker startup are atomic.
    fn admit(
        &self,
        text: &str,
        deadline: Option<Instant>,
        start_deadlines: impl FnOnce() -> Result<()>,
    ) -> Result<MutexGuard<'_, State>> {
        // Scan host input outside the state lock, but report closure before validation
        // errors for both entry points.
        let size = self.line_size(text);
        let mut state = lock(&self.0.state);
        if !state.accepting || state.stopped {
            return Err(Error::Closed);
        }
        let size = size?;
        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            return Err(Error::InvalidInput(
                "rpc write deadline has already elapsed".into(),
            ));
        }
        if state.queue.len() + usize::from(state.active.is_some())
            >= self.0.limits.max_pending_stdin_lines
            || size > self.0.limits.max_pending_stdin_bytes - state.bytes
        {
            return Err(Error::QueueFull);
        }
        if deadline.is_some() && !state.deadlines_started {
            // Internal thread starter, never a host callback. Start under the admission
            // lock so no frame can become visible without a worker, and simultaneous
            // first tracked sends start only one worker. Failure leaves admission intact.
            start_deadlines()?;
            state.deadlines_started = true;
        }
        Ok(state)
    }

    fn enqueue(
        &self,
        state: &mut State,
        text: &str,
        deadline: Option<Instant>,
        receipt: Option<Arc<ReceiptState>>,
    ) {
        let size = text.len() + 1; // Validated by admit while holding this lock.
        state.queue.push_back(Line {
            text: text.to_owned(),
            meta: Metadata {
                size,
                deadline,
                receipt,
            },
        });
        state.bytes += size;
        self.0.writer_ready.notify_one();
        if deadline.is_some() {
            self.0.deadlines_changed.notify_one();
        }
    }

    pub(super) fn half_close(&self) -> Result<()> {
        let mut state = lock(&self.0.state);
        if state.stopped {
            return Err(Error::Closed);
        }
        state.accepting = false;
        self.0.writer_ready.notify_one();
        self.0.deadlines_changed.notify_one();
        Ok(())
    }

    fn stop_locked(&self, state: &mut State, reason: RpcWriteFailure) {
        state.stopped = true;
        state.accepting = false;
        if let Some(active) = &state.active {
            active.status(RpcWriteStatus::Finished(RpcWriteOutcome::PossiblyWritten(
                reason,
            )));
        }
        while let Some(line) = state.queue.pop_front() {
            state.bytes -= line.meta.size;
            line.meta
                .status(RpcWriteStatus::Finished(RpcWriteOutcome::NotWritten(
                    RpcWriteFailure::Closed,
                )));
        }
        // Keep the active payload's byte/item reservation until its writer returns.
        self.0.writer_ready.notify_one();
        self.0.deadlines_changed.notify_one();
    }

    pub(super) fn stop(&self, reason: RpcWriteFailure) {
        self.stop_locked(&mut lock(&self.0.state), reason);
    }

    fn expire_queued(&self, state: &mut State, now: Instant) {
        let State { queue, bytes, .. } = state;
        // One linear pass even when many interleaved deadlines expire together.
        queue.retain(|line| {
            if !line.meta.expired(now) {
                return true;
            }
            *bytes -= line.meta.size;
            line.meta
                .status(RpcWriteStatus::Finished(RpcWriteOutcome::NotWritten(
                    RpcWriteFailure::DeadlineExceeded,
                )));
            false
        });
    }

    /// Only the writer calls this. Claims a frame before attempting any pipe operation.
    pub(super) fn next(&self) -> Option<String> {
        let mut state = lock(&self.0.state);
        loop {
            if state.stopped {
                return None;
            }
            self.expire_queued(&mut state, Instant::now());
            if let Some(line) = state.queue.pop_front() {
                line.meta.status(RpcWriteStatus::Writing);
                state.active = Some(line.meta);
                // Claiming only moves a deadline from queued to active; the monitor's
                // existing timer remains valid and does not need a wakeup.
                return Some(line.text);
            }
            if !state.accepting {
                return None;
            }
            state = self
                .0
                .writer_ready
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Settle this write and report a new failure only if no other path stopped input.
    /// `completed_at` is sampled immediately after write/flush, before lock contention
    /// or tracing can make an on-time completion appear late. An earlier stop still wins.
    pub(super) fn finish(
        &self,
        text: String,
        result: std::io::Result<()>,
        completed_at: Instant,
    ) -> WriteCompletion {
        drop(text); // Release payload storage before its admission reservation.
        let mut state = lock(&self.0.state);
        let Some(active) = state.active.take() else {
            return WriteCompletion::Stopped;
        };
        state.bytes -= active.size;
        // Removing a deadline cannot bring the next timeout forward. Let an existing
        // timer expire unless the monitor needs to exit after a drained half-close.
        if !state.accepting && state.queue.is_empty() {
            self.0.deadlines_changed.notify_one();
        }
        if state.stopped {
            // EOF may be waiting for the stderr tail. Its already-settled receipts and
            // terminal event take precedence over a late EPIPE or deadline observation.
            return WriteCompletion::Stopped;
        }
        let failure = if active.expired(completed_at) {
            Some((
                RpcWriteFailure::DeadlineExceeded,
                RpcTransportError::WriteDeadlineExceeded,
            ))
        } else if let Err(err) = result {
            Some((
                RpcWriteFailure::Io(err.kind()),
                RpcTransportError::Io {
                    stream: RpcStream::Stdin,
                    kind: err.kind(),
                },
            ))
        } else {
            None
        };
        if let Some((reason, error)) = failure {
            active.status(RpcWriteStatus::Finished(RpcWriteOutcome::PossiblyWritten(
                reason,
            )));
            self.stop_locked(&mut state, reason);
            WriteCompletion::Failed(error)
        } else {
            active.status(RpcWriteStatus::Finished(RpcWriteOutcome::Written));
            WriteCompletion::Written
        }
    }

    /// Dedicated deadline worker. Never waits for pipe I/O or event publication. Returns
    /// true only when an active timeout invalidates the channel; queued timeouts are safe
    /// to remove without affecting the stream. `now` is sampled under the input lock.
    pub(super) fn monitor(&self) -> bool {
        let mut state = lock(&self.0.state);
        loop {
            if state.stopped
                || (!state.accepting && state.queue.is_empty() && state.active.is_none())
            {
                return false;
            }
            let now = Instant::now();
            self.expire_queued(&mut state, now);
            if !state.accepting && state.queue.is_empty() && state.active.is_none() {
                return false;
            }
            if state
                .active
                .as_ref()
                .is_some_and(|active| active.expired(now))
            {
                self.stop_locked(&mut state, RpcWriteFailure::DeadlineExceeded);
                return true;
            }
            let deadline = state
                .queue
                .iter()
                .filter_map(|line| line.meta.deadline)
                .chain(state.active.as_ref().and_then(|active| active.deadline))
                .min();
            state = match deadline {
                Some(deadline) => {
                    self.0
                        .deadlines_changed
                        .wait_timeout(state, deadline.saturating_duration_since(now))
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                None => self
                    .0
                    .deadlines_changed
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner()),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }
    fn tracked(input: &Input, text: &str) -> RpcWriteReceipt {
        input.send_tracked(text, deadline(), || Ok(())).unwrap()
    }

    #[test]
    fn deadline_worker_starts_only_for_admitted_tracked_work_and_retries_after_spawn_failure() {
        let input = Input::new(RpcLimits {
            max_stdin_line_bytes: 8,
            max_pending_stdin_lines: 1,
            ..RpcLimits::default()
        });
        let no_start = || -> Result<()> { panic!("refused or untracked work started a worker") };
        input.send("plain").unwrap();
        assert!(matches!(
            input.send_tracked("full", deadline(), no_start),
            Err(Error::QueueFull)
        ));
        let text = input.next().unwrap();
        input.finish(text, Ok(()), Instant::now());
        assert!(matches!(
            input.send_tracked("too large", deadline(), no_start),
            Err(Error::InputTooLarge { .. })
        ));
        assert!(matches!(
            input.send_tracked("x\ny", deadline(), no_start),
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            input.send_tracked("expired", Instant::now(), no_start),
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            input.send_tracked("failed", deadline(), || Err(Error::Spawn(
                "injected".into()
            ))),
            Err(Error::Spawn(_))
        ));
        // Failed startup admitted no bytes and consumed no capacity. The next attempt
        // must retry startup, then emit only its own frame.
        let mut starts = 0;
        let receipt = input
            .send_tracked("retry", deadline(), || {
                starts += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(starts, 1);
        let text = input.next().unwrap();
        assert_eq!(text, "retry");
        input.finish(text, Ok(()), Instant::now());
        assert_eq!(
            receipt.wait_until(deadline()),
            Some(RpcWriteOutcome::Written)
        );
        let receipt = input.send_tracked("reuse", deadline(), no_start).unwrap();
        let text = input.next().unwrap();
        input.finish(text, Ok(()), Instant::now());
        assert_eq!(
            receipt.wait_until(deadline()),
            Some(RpcWriteOutcome::Written)
        );
        input.half_close().unwrap();
        assert!(input.next().is_none());
        assert!(matches!(
            input.send_tracked("closed", deadline(), no_start),
            Err(Error::Closed)
        ));
    }

    #[test]
    fn simultaneous_first_tracked_admissions_start_one_deadline_worker() {
        use std::sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        };
        let input = Input::new(RpcLimits::default());
        let starts = AtomicUsize::new(0);
        let barrier = Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    barrier.wait();
                    input
                        .send_tracked("tracked", deadline(), || {
                            starts.fetch_add(1, Ordering::Relaxed);
                            Ok(())
                        })
                        .unwrap();
                });
            }
        });
        assert_eq!(starts.load(Ordering::Relaxed), 1);
        input.half_close().unwrap();
        let mut written = 0;
        while let Some(text) = input.next() {
            input.finish(text, Ok(()), Instant::now());
            written += 1;
        }
        assert_eq!(written, 4);
    }

    #[test]
    fn admission_budgets_cover_active_writes_and_refusals_publish_nothing() {
        let input = Input::new(RpcLimits {
            max_stdin_line_bytes: 5,
            max_pending_stdin_bytes: 5,
            max_pending_stdin_lines: 2,
            ..RpcLimits::default()
        });
        assert!(matches!(
            input.send_tracked("12345", deadline(), || Ok(())),
            Err(Error::InputTooLarge { .. })
        ));
        assert!(matches!(
            input.send_tracked("x\ny", deadline(), || Ok(())),
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            input.send_tracked("x", Instant::now(), || Ok(())),
            Err(Error::InvalidInput(_))
        ));
        let receipt = tracked(&input, "1234");
        assert_eq!(receipt.status(), RpcWriteStatus::Queued);
        let text = input.next().unwrap();
        assert_eq!(receipt.status(), RpcWriteStatus::Writing);
        assert!(matches!(input.send(""), Err(Error::QueueFull)));
        input.finish(text, Ok(()), Instant::now());
        assert_eq!(receipt.wait(), RpcWriteOutcome::Written);
        let a = tracked(&input, "");
        let b = tracked(&input, "");
        assert!(matches!(input.send(""), Err(Error::QueueFull)));
        input.half_close().unwrap();
        for _ in 0..2 {
            let text = input.next().unwrap();
            assert_eq!(text, "");
            input.finish(text, Ok(()), Instant::now());
        }
        assert!(input.next().is_none());
        assert_eq!(a.wait(), RpcWriteOutcome::Written);
        assert_eq!(b.wait(), RpcWriteOutcome::Written);
    }

    #[test]
    fn closed_input_precedes_validation_for_both_send_methods() {
        for stopped in [false, true] {
            let input = Input::new(RpcLimits {
                max_stdin_line_bytes: 4,
                ..RpcLimits::default()
            });
            if stopped {
                input.stop(RpcWriteFailure::Closed);
            } else {
                input.half_close().unwrap();
            }
            for text in ["x\ny", "oversized", "ok"] {
                assert!(matches!(input.send(text), Err(Error::Closed)));
                for expires in [Instant::now(), deadline()] {
                    assert!(matches!(
                        input.send_tracked(text, expires, || panic!(
                            "closed input started a worker"
                        )),
                        Err(Error::Closed)
                    ));
                }
            }
            assert!(input.next().is_none(), "refused lines must not be queued");
        }
    }

    #[test]
    fn on_time_flush_stays_written_when_settlement_waits_for_the_input_lock() {
        use crossbeam_channel::{bounded, never};

        let input = Input::new(RpcLimits::default());
        let expires = Instant::now() + Duration::from_secs(1);
        let receipt = input.send_tracked("on time", expires, || Ok(())).unwrap();
        let text = input.next().unwrap();
        let state = lock(&input.0.state);
        let (started_tx, started) = bounded(1);
        let worker_input = input.clone();
        let writer = std::thread::spawn(move || {
            let completed_at = Instant::now();
            started_tx.send(completed_at).unwrap();
            worker_input.finish(text, Ok(()), completed_at)
        });
        let completed_at = started.recv_deadline(deadline());
        // Hold the state lock through expiry, simulating contention after flush.
        let _ = never::<()>().recv_deadline(expires);
        drop(state);
        let completion = writer.join().unwrap();
        assert!(completed_at.unwrap() < expires);
        assert_eq!(completion, WriteCompletion::Written);
        assert_eq!(
            receipt.wait_until(deadline()),
            Some(RpcWriteOutcome::Written)
        );
        // The late settlement must not invalidate the stream or discard later work.
        let later = tracked(&input, "later");
        input.half_close().unwrap();
        let text = input.next().unwrap();
        assert_eq!(text, "later");
        input.finish(text, Ok(()), Instant::now());
        assert_eq!(later.wait_until(deadline()), Some(RpcWriteOutcome::Written));
        assert!(input.next().is_none());
    }

    #[test]
    fn flush_at_or_after_deadline_invalidates_input_without_waiting_for_monitor() {
        for lateness in [Duration::ZERO, Duration::from_nanos(1)] {
            let input = Input::new(RpcLimits::default());
            let expires = deadline();
            let receipt = input.send_tracked("late", expires, || Ok(())).unwrap();
            let text = input.next().unwrap();
            let queued = tracked(&input, "queued");
            assert_eq!(
                input.finish(text, Ok(()), expires + lateness),
                WriteCompletion::Failed(RpcTransportError::WriteDeadlineExceeded)
            );
            assert_eq!(
                receipt.wait_until(deadline()),
                Some(RpcWriteOutcome::PossiblyWritten(
                    RpcWriteFailure::DeadlineExceeded
                ))
            );
            assert_eq!(
                queued.wait_until(deadline()),
                Some(RpcWriteOutcome::NotWritten(RpcWriteFailure::Closed))
            );
            assert!(input.next().is_none());
            assert!(!input.monitor(), "the writer already owns the failure");
        }
    }

    #[test]
    fn queued_expiry_skips_only_the_expired_frame_and_releases_capacity() {
        let input = Input::new(RpcLimits {
            max_pending_stdin_lines: 2,
            ..RpcLimits::default()
        });
        let active = tracked(&input, "active");
        let text = input.next().unwrap();
        let expires = deadline();
        let queued = input.send_tracked("expired", expires, || Ok(())).unwrap();
        input.expire_queued(&mut lock(&input.0.state), expires);
        assert_eq!(
            queued.wait(),
            RpcWriteOutcome::NotWritten(RpcWriteFailure::DeadlineExceeded)
        );
        let later = tracked(&input, "later");
        assert_eq!(
            input.finish(text, Ok(()), Instant::now()),
            WriteCompletion::Written
        );
        input.half_close().unwrap();
        let text = input.next().unwrap();
        assert_eq!(text, "later");
        input.finish(text, Ok(()), Instant::now());
        assert!(input.next().is_none());
        assert_eq!(active.wait(), RpcWriteOutcome::Written);
        assert_eq!(later.wait(), RpcWriteOutcome::Written);
    }

    #[test]
    fn deadline_monitor_wakes_for_an_earlier_admission() {
        use crossbeam_channel::bounded;

        let input = Input::new(RpcLimits::default());
        input.send("blocked untracked write").unwrap();
        let text = input.next().unwrap();
        let first = input
            .send_tracked(
                "first expiry",
                Instant::now() + Duration::from_millis(100),
                || Ok(()),
            )
            .unwrap();
        let later = input.send_tracked("later", deadline(), || Ok(())).unwrap();
        let worker_input = input.clone();
        let (done_tx, done) = bounded(1);
        let monitor = std::thread::spawn(move || done_tx.send(worker_input.monitor()).unwrap());
        let limit = Instant::now() + Duration::from_secs(5);
        let first_outcome = first.wait_until(limit);
        // Expiry settles the receipt under this lock. Acquiring it proves the monitor
        // reached its next wait, which is currently for the distant `later` deadline.
        drop(lock(&input.0.state));
        let earlier = input.send_tracked(
            "earlier",
            Instant::now() + Duration::from_millis(100),
            || Ok(()),
        );
        let earlier_outcome = earlier
            .as_ref()
            .ok()
            .and_then(|receipt| receipt.wait_until(limit));
        let later_status = later.status();
        input.stop(RpcWriteFailure::Closed);
        let finished = done.recv_deadline(deadline());
        monitor.join().unwrap();
        input.finish(text, Ok(()), Instant::now());
        earlier.unwrap();
        for outcome in [first_outcome, earlier_outcome] {
            assert_eq!(
                outcome,
                Some(RpcWriteOutcome::NotWritten(
                    RpcWriteFailure::DeadlineExceeded
                ))
            );
        }
        assert_eq!(later_status, RpcWriteStatus::Queued);
        assert!(!finished.unwrap());
    }

    #[test]
    fn half_close_wakes_deadline_monitor_after_last_untracked_write() {
        use crossbeam_channel::bounded;

        let input = Input::new(RpcLimits::default());
        input.send("active").unwrap();
        let text = input.next().unwrap();
        let queued = input
            .send_tracked(
                "expires",
                Instant::now() + Duration::from_millis(100),
                || Ok(()),
            )
            .unwrap();
        input.half_close().unwrap();
        let worker_input = input.clone();
        let (done_tx, done) = bounded(1);
        let monitor = std::thread::spawn(move || done_tx.send(worker_input.monitor()).unwrap());
        let limit = Instant::now() + Duration::from_secs(5);
        let outcome = queued.wait_until(limit);
        // After this expiry the monitor has no timer; only final drain can wake it.
        drop(lock(&input.0.state));
        let completion = input.finish(text, Ok(()), Instant::now());
        let finished = done.recv_deadline(limit);
        // Always release the worker, even if the drain notification was lost.
        input.stop(RpcWriteFailure::Closed);
        monitor.join().unwrap();
        assert_eq!(
            outcome,
            Some(RpcWriteOutcome::NotWritten(
                RpcWriteFailure::DeadlineExceeded
            ))
        );
        assert_eq!(completion, WriteCompletion::Written);
        assert!(!finished.unwrap());
    }

    #[test]
    fn stop_settles_active_and_pending_without_waiting_for_the_writer() {
        for reason in [RpcWriteFailure::Closed, RpcWriteFailure::DeadlineExceeded] {
            let input = Input::new(RpcLimits::default());
            let active = tracked(&input, "active");
            let text = input.next().unwrap();
            let queued = tracked(&input, "queued");
            input.stop(reason);
            assert_eq!(active.wait(), RpcWriteOutcome::PossiblyWritten(reason));
            assert_eq!(
                queued.wait(),
                RpcWriteOutcome::NotWritten(RpcWriteFailure::Closed)
            );
            assert!(matches!(input.send("late"), Err(Error::Closed)));
            assert!(input.next().is_none());
            // A late completion cannot overwrite uncertainty or report a second failure.
            assert_eq!(
                input.finish(text, Ok(()), Instant::now()),
                WriteCompletion::Stopped
            );
            assert_eq!(active.wait(), RpcWriteOutcome::PossiblyWritten(reason));
        }
    }

    #[test]
    fn observing_or_dropping_a_receipt_does_not_cancel_the_write() {
        let input = Input::new(RpcLimits::default());
        let receipt = tracked(&input, "kept");
        assert_eq!(receipt.wait_until(Instant::now()), None);
        drop(receipt);
        input.half_close().unwrap();
        input.half_close().unwrap();
        assert!(matches!(input.send("late"), Err(Error::Closed)));
        let text = input.next().unwrap();
        assert_eq!(text, "kept");
        input.finish(text, Ok(()), Instant::now());
        assert!(input.next().is_none());
        assert!(!input.monitor());
    }
}
