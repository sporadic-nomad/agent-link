//! A distinct allocation is the observation generation. It binds at most once; a gap
//! is terminal for that generation. Its queue, including any uncommitted worker batch,
//! can never migrate to a subsequent receiver. Lock order is pull, controller, then
//! queue. Waiter wakeups follow the release of all three locks, and a blocked waiter
//! holds none of them.

use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex, MutexGuard, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crossbeam_channel::Sender;

use super::transcript::Cursor;
use super::wake::Wakeup;
use super::{
    AgentState, Controller, Gap, Launch, Limits, Opened, Reply, Status, TurnRecvError, Witness,
};

pub(super) struct Batch {
    #[cfg(feature = "questions")]
    pub(super) question: Option<Arc<crate::agent::questions::PendingQuestion>>,
    #[cfg(feature = "questions")]
    pub(super) complete: bool,
    replies: VecDeque<Reply>,
    bytes: usize,
}

impl Batch {
    pub(super) fn new() -> Self {
        Self {
            #[cfg(feature = "questions")]
            question: None,
            #[cfg(feature = "questions")]
            complete: false,
            replies: VecDeque::new(),
            bytes: 0,
        }
    }
}

#[derive(Clone)]
pub(super) enum Position {
    Append(Cursor),
    #[cfg(feature = "turns")]
    Snapshot(Arc<super::snapshots::Proof>),
}

impl Position {
    // Only the append-only Claude and Codex transcripts contain question records.
    #[cfg(feature = "questions")]
    fn append_cursor(&self) -> Option<Cursor> {
        match self {
            Self::Append(cursor) => Some(*cursor),
            Self::Snapshot(_) => None,
        }
    }

    fn confirm(&self, opened: &Opened) -> Result<bool, Gap> {
        match self {
            Self::Append(c) => c.confirm(opened).map(|()| true),
            #[cfg(feature = "turns")]
            Self::Snapshot(s) => s.confirm(opened),
        }
    }
}

struct Queue {
    status: Status,
    opened: Weak<Opened>,
    cursor: Option<Position>,
    pending: Batch,
}

pub(super) struct Stream {
    ctl: Weak<Controller>,
    lifecycle: u64,
    authority: Option<Arc<AtomicBool>>,
    queue: Mutex<Queue>,
    cancel: Sender<()>,
    limits: Limits,
    wakeup: Wakeup,
}

impl Stream {
    pub(super) fn new(
        ctl: &Arc<Controller>,
        launch: &Launch,
        limits: Limits,
        cancel: Sender<()>,
    ) -> Self {
        Self {
            ctl: Arc::downgrade(ctl),
            lifecycle: launch.lifecycle,
            authority: launch.claim.authority.clone(),
            queue: Mutex::new(Queue {
                status: Status::Waiting,
                opened: Weak::new(),
                cursor: None,
                pending: Batch::new(),
            }),
            cancel,
            limits,
            wakeup: Wakeup::default(),
        }
    }

    /// Enqueue, dequeue, status and cancellation all sample the same local cutoff.
    /// Sender wakeups, disposal of detached reply buffers and waiter wakeups happen after
    /// both locks. Any change of status or queue length wakes waiters.
    fn access<T>(&self, act: impl FnOnce(&mut Queue) -> T) -> T {
        self.access_and_wake(|q| (act(q), false))
    }

    /// As [`access`](Self::access); `act` can also wake waiters without a visible change.
    fn access_and_wake<T>(&self, act: impl FnOnce(&mut Queue) -> (T, bool)) -> T {
        let ((value, force_wake), changed) = self.access_deferred(act);
        if changed || force_wake {
            self.wakeup.wake_all();
        }
        value
    }

    /// Returns the notification obligation to a caller that may still hold the pull lock.
    fn access_deferred<T>(&self, act: impl FnOnce(&mut Queue) -> T) -> (T, bool) {
        let access = |end: Option<Status>| {
            let mut q = self.queue.lock().unwrap_or_else(|err| err.into_inner());
            let before = (q.status, q.pending.replies.len());
            let was_live = q.status.live();
            if was_live {
                if self
                    .authority
                    .as_ref()
                    .is_some_and(|lease| !lease.load(Ordering::Acquire))
                {
                    q.status = Status::Gap(Gap::AuthorityRevoked);
                } else if let Some(end) = end {
                    q.status = end;
                }
            }
            let value = act(&mut q);
            let wake = before != (q.status, q.pending.replies.len());
            let detached = (was_live && !q.status.live())
                .then(|| std::mem::replace(&mut q.pending, Batch::new()));
            (value, detached, wake)
        };
        let (value, detached, wake) = match self.ctl.upgrade() {
            None => access(Some(Status::Closed)),
            Some(ctl) => ctl.read(|st| {
                let end = if st.disposed
                    || ctl.term().turns_ended()
                    || st.agent_state() == AgentState::Exited
                {
                    Some(Status::Closed)
                } else if st.monitor_unavailable() {
                    Some(Status::Gap(Gap::Monitor))
                } else if st.lifecycle_revision() != self.lifecycle {
                    Some(Status::Gap(Gap::Lifecycle))
                } else {
                    None
                };
                access(end)
            }),
        };
        if detached.is_some() {
            let _ = self.cancel.try_send(());
        }
        drop(detached);
        (value, wake)
    }

    pub(super) fn status(&self) -> Status {
        self.access(|q| q.status)
    }

    pub(super) fn finish(&self, status: Status) {
        self.access(|q| {
            if q.status.live() {
                q.status = status;
            }
        });
    }

    /// Descendants can hold the PTY open after process exit. A shell can also regain
    /// the foreground while its old writer record still exists. Neither kernel query
    /// needs a controller lock or a strong controller reference.
    pub(super) fn sample_terminal(&self, bound: bool) -> bool {
        if let Some(gap) = self.terminal_gap(bound) {
            self.finish(Status::Gap(gap));
        }
        self.status().live()
    }

    fn terminal_gap(&self, bound: bool) -> Option<Gap> {
        if let Some((term, shell_program)) = self.ctl.upgrade().map(|ctl| {
            let program =
                (bound && ctl.cfg.launch_line.is_some()).then(|| ctl.read(|st| st.program));
            (ctl.term().clone(), program)
        }) {
            term.try_wait();
            if let Some(program) = shell_program {
                let (owner, foreground) = term.foreground_program();
                let replaced = matches!((program, foreground),
                    (crate::agent::state::AgentProgram::Known(old), Some(current)) if old != current);
                if owner.owner == crate::ForegroundOwner::Shell || replaced {
                    return Some(Gap::Lifecycle);
                }
            }
        }
        None
    }

    /// The worker is the sole producer. Charge staged replies against the same
    /// capacity as queued replies; dequeues can only free space before commit. One
    /// unadmitted parser candidate has its own (smaller) reply limit.
    pub(super) fn stage(&self, staged: &mut Batch, reply: Reply) -> Result<(), Gap> {
        let bytes = reply.text.len();
        if bytes == 0 && !reply.truncated && !reply.completion_only {
            return Ok(());
        }
        if bytes > self.limits.reply_bytes {
            return Err(Gap::Oversized);
        }
        let room = self.access(|q| {
            if !q.status.live() {
                return false;
            }
            if q.pending.replies.len() + staged.replies.len() >= self.limits.queue_items
                || q.pending.bytes + staged.bytes + bytes > self.limits.queue_bytes
            {
                q.status = Status::Gap(Gap::Overflow);
                return false;
            }
            true
        });
        if !room {
            return Err(Gap::Overflow);
        }
        staged.bytes += bytes;
        staged.replies.push_back(reply);
        Ok(())
    }

    pub(super) fn bind(&self, status: Status, opened: &Arc<Opened>) -> bool {
        self.access(|q| {
            if q.status != Status::Waiting {
                return false;
            }
            q.opened = Arc::downgrade(opened);
            q.status = status;
            true
        })
    }

    pub(super) fn commit(&self, status: Status, cursor: Cursor, batch: Batch) -> bool {
        self.commit_at(status, Position::Append(cursor), batch)
    }

    /// A new position wakes waiters even when status and length are unchanged. It can
    /// release a pull withheld while a Cursor rewrite was short.
    pub(super) fn commit_at(&self, status: Status, cursor: Position, mut batch: Batch) -> bool {
        self.access_and_wake(|q| {
            if !q.status.live() {
                return (false, false);
            }
            #[cfg(feature = "questions")]
            {
                q.pending.question = batch.question.take();
                q.pending.complete = batch.complete;
            }
            q.pending.replies.append(&mut batch.replies);
            q.pending.bytes += batch.bytes;
            q.cursor = Some(cursor);
            q.status = status;
            (true, true)
        })
    }
}

/// One pull receiver. Status is coalesced outside the queue; the first gap remains
/// visible even if the worker has exited or the caller has never polled.
#[must_use = "dropping the receiver stops observation"]
pub struct TurnReceiver {
    stream: Arc<Stream>,
    launch: Weak<Launch>,
    witness: Weak<dyn Witness>,
    pulling: Mutex<()>,
}

impl TurnReceiver {
    pub(super) fn new(
        stream: Arc<Stream>,
        launch: &Arc<Launch>,
        witness: &Arc<dyn Witness>,
    ) -> Self {
        Self {
            stream,
            launch: Arc::downgrade(launch),
            witness: Arc::downgrade(witness),
            pulling: Mutex::new(()),
        }
    }

    /// Coalesced status. Samples local lifecycle and lease state without filesystem I/O.
    pub fn status(&self) -> super::TurnStatus {
        self.stream.status()
    }

    #[cfg(feature = "questions")]
    pub(in crate::agent) fn belongs_to(&self, controller: &Arc<Controller>) -> bool {
        self.stream
            .ctl
            .upgrade()
            .is_some_and(|ctl| Arc::ptr_eq(&ctl, controller))
            && self.status().live()
    }

    #[cfg(feature = "questions")]
    pub(in crate::agent) fn pending_question(
        &self,
    ) -> Result<
        Option<Arc<crate::agent::questions::PendingQuestion>>,
        crate::agent::questions::AnswerError,
    > {
        use crate::agent::questions::AnswerError;
        let Some((question, opened, cursor, complete)) = self.stream.access(|q| {
            if !q.status.live() {
                return Err(AnswerError::NotOpen);
            }
            Ok(q.pending
                .question
                .clone()
                .zip(q.opened.upgrade())
                .zip(q.cursor.as_ref().and_then(Position::append_cursor))
                .map(|((qst, opened), cursor)| (qst, opened, cursor, q.pending.complete)))
        })?
        else {
            return Ok(None);
        };
        if !complete {
            return Err(AnswerError::Changed);
        }
        let launch = self.launch.upgrade().ok_or(AnswerError::NotOpen)?;
        let witness = self.witness.upgrade().ok_or(AnswerError::NotOpen)?;
        let checked = witness
            .current(&launch)
            .and_then(|cur| opened.confirm(&launch, cur))
            .and_then(|()| cursor.confirm(&opened))
            .and_then(|()| opened.confirm(&launch, witness.current(&launch)?))
            .and_then(|()| opened.observed_len());
        let length = checked.map_err(|reason| {
            self.stream.finish(Status::Gap(reason));
            AnswerError::NotOpen
        })?;
        if length != cursor.offset {
            return Err(AnswerError::Changed);
        }
        if !self.stream.sample_terminal(true) {
            return Err(AnswerError::NotOpen);
        }
        self.stream.access(|q| {
            if !q.status.live() {
                return Err(AnswerError::NotOpen);
            }
            if !q.pending.complete
                || q.cursor
                    .as_ref()
                    .and_then(Position::append_cursor)
                    .is_none_or(|c| c.offset != cursor.offset)
                || !q
                    .pending
                    .question
                    .as_ref()
                    .is_some_and(|p| Arc::ptr_eq(p, &question))
            {
                return Err(AnswerError::Changed);
            }
            Ok(Some(question))
        })
    }

    /// Pull one queued reply after sampling the writer, file identity, cursor and terminal.
    /// An empty queue needs no I/O. A competing pull returns immediately.
    /// A nonempty pull does bounded synchronous local filesystem and process I/O. Check
    /// status to distinguish an empty queue from a gap or closure, or wait with
    /// [`recv_timeout`](Self::recv_timeout) instead of polling.
    pub fn try_recv(&self) -> Option<super::TurnReply> {
        self.pull(self.pulling.try_lock().ok()?)
    }

    /// Wait up to `timeout` for the next reply instead of polling. Returns as soon as a
    /// reply passes the same checks as [`try_recv`](Self::try_recv), or observation ends.
    /// A zero timeout makes one attempt.
    ///
    /// - `Ok(reply)`: one reply, removed from the queue.
    /// - [`TurnRecvError::Gap`] or [`TurnRecvError::Closed`]: the sticky end of this
    ///   attachment, as [`status`](Self::status) reports it. Queued replies were discarded.
    /// - [`TurnRecvError::Timeout`]: still waiting or active, and nothing was pulled.
    ///
    /// Blocks the calling thread and spawns nothing. Publication, a gap detected by the
    /// worker or by a pull, [`stop`](Self::stop) and terminal teardown wake waiters at
    /// once. Lease revocation, child exit, the end of PTY output, a launch change and
    /// monitor loss are sampled by the worker's next pass, every 50 ms, which then wakes
    /// waiters; a result at the deadline samples them as well. A reply withheld during a
    /// Cursor rewrite is retried at the worker's next commit. Pulls are serialized: a
    /// waiter also waits for a concurrent pull on this receiver, and the final pull's
    /// bounded synchronous checks can extend the call past `timeout`. Any number of
    /// threads may wait. Each reply goes to one of them.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<super::TurnReply, TurnRecvError> {
        self.stream
            .wakeup
            .wait_for(timeout, || self.attempt())
            .unwrap_or_else(|| Err(self.ended().unwrap_or(TurnRecvError::Timeout)))
    }

    /// Wait up to `timeout` until [`status`](Self::status) differs from `seen`, for
    /// example from [`TurnStatus::Waiting`](super::TurnStatus::Waiting) to `Active`.
    /// Returns the current status at once when it differs or is a final gap or closure.
    /// `None` when it stayed `seen` for the whole timeout. Each validated batch advances
    /// the `Active` offset. Blocks the calling thread and performs no filesystem I/O.
    /// Wakeups and sampling latency are as for [`recv_timeout`](Self::recv_timeout).
    pub fn wait_status_change(
        &self,
        seen: super::TurnStatus,
        timeout: Duration,
    ) -> Option<super::TurnStatus> {
        let changed = |now: Status| (now != seen || !now.live()).then_some(now);
        self.stream
            .wakeup
            .wait_for(timeout, || changed(self.status()))
            .or_else(|| changed(self.status()))
    }

    /// Await the next reply (feature `tokio`). As [`recv_timeout`](Self::recv_timeout)
    /// without a deadline, so it never returns [`TurnRecvError::Timeout`]; wrap it in a
    /// timeout of your own. The wait occupies no thread and spawns nothing, and the
    /// receiver wakes every waiting task. Cancel-safe: dropping the future never loses a
    /// reply. Each attempt runs on the polling thread with the bounded synchronous checks
    /// of [`try_recv`](Self::try_recv), after any concurrent pull on this receiver.
    #[cfg(feature = "tokio")]
    pub async fn recv(&self) -> Result<super::TurnReply, TurnRecvError> {
        self.stream.wakeup.until(|| self.attempt()).await
    }

    /// Await a status other than `seen` (feature `tokio`). As
    /// [`wait_status_change`](Self::wait_status_change) without a deadline. Cancel-safe.
    #[cfg(feature = "tokio")]
    pub async fn status_change(&self, seen: super::TurnStatus) -> super::TurnStatus {
        self.stream
            .wakeup
            .until(|| {
                let now = self.status();
                (now != seen || !now.live()).then_some(now)
            })
            .await
    }

    /// One serialized pull. `None` while observation stays live without a reply.
    fn attempt(&self) -> Option<Result<super::TurnReply, TurnRecvError>> {
        let pull = self.pulling.lock().unwrap_or_else(|err| err.into_inner());
        match self.pull(pull) {
            Some(reply) => Some(Ok(reply)),
            None => self.ended().map(Err),
        }
    }

    fn ended(&self) -> Option<TurnRecvError> {
        match self.status() {
            Status::Gap(gap) => Some(TurnRecvError::Gap(gap)),
            Status::Closed => Some(TurnRecvError::Closed),
            Status::Waiting | Status::Active { .. } => None,
        }
    }

    /// The guard excludes competing pulls for the whole sample.
    fn pull(&self, pull: MutexGuard<'_, ()>) -> Option<super::TurnReply> {
        // Keep the same front item in place across the sample. A worker may append
        // or invalidate it. Even the first access may discover a lifecycle change.
        let (front, mut wake) = self.stream.access_deferred(|q| {
            if !q.status.live() || q.pending.replies.is_empty() {
                return None;
            }
            q.opened.upgrade().zip(q.cursor.clone())
        });
        let reply = front.and_then(|(opened, cursor)| {
            let launch = self.launch.upgrade()?;
            let witness = self.witness.upgrade()?;
            let valid = witness
                .current(&launch)
                .and_then(|current| opened.confirm(&launch, current))
                .and_then(|()| cursor.confirm(&opened))
                .and_then(|valid| {
                    if valid {
                        opened.confirm(&launch, witness.current(&launch)?)?;
                        if let Some(gap) = self.stream.terminal_gap(true) {
                            return Err(gap);
                        }
                    }
                    Ok(valid)
                });
            let (reply, changed) = self.stream.access_deferred(|q| {
                if !q.status.live() {
                    return None;
                }
                match valid {
                    Ok(true) => {
                        let reply = q.pending.replies.pop_front()?;
                        q.pending.bytes -= reply.text.len();
                        Some(reply)
                    }
                    Ok(false) => None,
                    Err(reason) => {
                        q.status = Status::Gap(reason);
                        None
                    }
                }
            });
            wake |= changed;
            reply
        });
        drop(pull);
        if wake {
            self.stream.wakeup.wake_all();
        }
        reply
    }

    /// Threads blocked in a wait on this receiver.
    #[cfg(test)]
    pub(super) fn parked(&self) -> usize {
        self.stream.wakeup.parked()
    }

    /// Hold the next waiter after its failed check, before it blocks or awaits.
    #[cfg(test)]
    pub(super) fn pause_next_wait(&self, pause: super::tests::peer::Pause) {
        self.stream.wakeup.pause_next(move || pause.park());
    }

    /// Close this attachment, discard queued text, and wake its worker and every waiter.
    /// Drop also stops it.
    pub fn stop(&self) {
        self.stream.finish(Status::Closed);
    }
}

impl Drop for TurnReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}
