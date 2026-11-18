//! A distinct allocation is the observation generation. It binds at most once; a gap
//! is terminal for that generation. Its queue, including any uncommitted worker batch,
//! can never migrate to a subsequent receiver. Lock order is controller then queue.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

use crossbeam_channel::Sender;

use super::{AgentState, Controller, Gap, Launch, Limits, Opened, Reply, Status, Witness};

pub(super) struct Batch {
    replies: VecDeque<Reply>,
    bytes: usize,
}

impl Batch {
    pub(super) fn new() -> Self {
        Self {
            replies: VecDeque::new(),
            bytes: 0,
        }
    }
}

struct Queue {
    status: Status,
    opened: Weak<Opened>,
    pending: Batch,
}

pub(super) struct Stream {
    ctl: Weak<Controller>,
    lifecycle: u64,
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
            queue: Mutex::new(Queue {
                status: Status::Waiting,
                opened: Weak::new(),
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
            if let Some(end) = end.filter(|_| was_live) {
                q.status = end;
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
        if bytes == 0 && !reply.truncated {
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

    pub(super) fn commit(&self, status: Status, mut batch: Batch) -> bool {
        self.access(|q| {
            if !q.status.live() {
                return false;
            }
            q.pending.replies.append(&mut batch.replies);
            q.pending.bytes += batch.bytes;
            q.status = status;
            true
        })
    }
}

/// One pull receiver. Status is coalesced outside the queue; the first gap remains
/// visible even if the worker has exited or the caller has never polled.
pub(in crate::agent) struct TurnReceiver {
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

    pub(in crate::agent) fn status(&self) -> Status {
        self.stream.status()
    }

    /// Pull one queued reply after sampling the writer, file identity and terminal.
    /// An empty queue needs no I/O. A competing pull returns immediately.
    pub(in crate::agent) fn try_recv(&self) -> Option<Reply> {
        // Keep the same front item in place across the sample. The worker can append
        // or invalidate the queue in the meantime.
        let _pull = self.pulling.try_lock().ok()?;
        let opened = self.stream.access(|q| {
            if !q.status.live() || q.pending.replies.is_empty() {
                return None;
            }
            q.opened.upgrade()
        })?;
        let launch = self.launch.upgrade()?;
        let witness = self.witness.upgrade()?;
        if let Err(reason) = witness
            .current(&launch)
            .and_then(|current| opened.confirm(&launch, current))
        {
            self.stream.finish(Status::Gap(reason));
            return None;
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

    pub(in crate::agent) fn stop(&self) {
        self.stream.finish(Status::Closed);
    }
}

impl Drop for TurnReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}
