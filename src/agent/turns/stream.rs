//! A distinct allocation is the observation generation. It binds at most once; a gap
//! is terminal for that generation. Its queue, including any uncommitted worker batch,
//! can never migrate to a subsequent receiver. Lock order is controller then queue.

use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};

use crossbeam_channel::Sender;

use super::transcript::Cursor;
use super::{AgentState, Controller, Gap, Launch, Limits, Opened, Reply, Status, Witness};

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
        }
    }

    /// Enqueue, dequeue, status and cancellation all sample the same local cutoff.
    /// Sender wakeups and disposal of detached reply buffers happen after both locks.
    fn access<T>(&self, act: impl FnOnce(&mut Queue) -> T) -> T {
        let access = |end: Option<Status>| {
            let mut q = self.queue.lock().unwrap_or_else(|err| err.into_inner());
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
            let detached = (was_live && !q.status.live())
                .then(|| std::mem::replace(&mut q.pending, Batch::new()));
            (value, detached)
        };
        let (value, detached) = match self.ctl.upgrade() {
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
        value
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
                    self.finish(Status::Gap(Gap::Lifecycle));
                }
            }
        }
        self.status().live()
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

    pub(super) fn commit_at(&self, status: Status, cursor: Position, mut batch: Batch) -> bool {
        self.access(|q| {
            if !q.status.live() {
                return false;
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
            true
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
    /// A nonempty pull does bounded synchronous local filesystem and process I/O. Hosts
    /// should poll status to distinguish an empty queue from a gap or closure.
    pub fn try_recv(&self) -> Option<super::TurnReply> {
        // Keep the same front item in place across the sample. The worker can append
        // or invalidate the queue in the meantime.
        let _pull = self.pulling.try_lock().ok()?;
        let (opened, cursor) = self.stream.access(|q| {
            if !q.status.live() || q.pending.replies.is_empty() {
                return None;
            }
            Some((q.opened.upgrade()?, q.cursor.clone()?))
        })?;
        let launch = self.launch.upgrade()?;
        let witness = self.witness.upgrade()?;
        match witness
            .current(&launch)
            .and_then(|current| opened.confirm(&launch, current))
            .and_then(|()| cursor.confirm(&opened))
            .and_then(|valid| {
                if valid {
                    opened.confirm(&launch, witness.current(&launch)?)?;
                }
                Ok(valid)
            }) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(reason) => {
                self.stream.finish(Status::Gap(reason));
                return None;
            }
        }
        if !self.stream.sample_terminal(true) {
            return None;
        }
        self.stream.access(|q| {
            if !q.status.live() {
                return None;
            }
            let reply = q.pending.replies.pop_front()?;
            q.pending.bytes -= reply.text.len();
            Some(reply)
        })
    }

    /// Close this attachment, discard queued text, and wake its worker. Drop also stops it.
    pub fn stop(&self) {
        self.stream.finish(Status::Closed);
    }
}

impl Drop for TurnReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}
