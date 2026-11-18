//! Private Unix PTY experiment. Each receiver observes one controller launch and at most
//! one corroborated writer/session/file binding. After loss, a new attachment starts at
//! a new EOF. A ticket, idle screen or quiet file supplies no turn boundary.
//!
//! The provider must be able to distinguish its writer, including in-process session
//! switches. Pulling queued text samples that evidence, the pinned file identity
//! and terminal lifecycle independently of the parser.
//! The core pins the launch directory and opened regular file, checks an overlap
//! window at the cursor and rereads each candidate batch before committing it.
//! These samples cannot prove arbitrary historical rewrites or distinguish concurrent
//! writers with identical provider evidence.

mod reader;
mod stream;
mod transcript;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use super::state::Controller;
use super::{AgentState, LaunchPlan};
use crate::{SessionId, SpawnSpec};

pub(super) use stream::TurnReceiver;
pub(super) use transcript::Opened;

/// Runs before child creation on the actual Direct argv or prepared shell plan and the
/// merged environment. The transcript directory must exist here so its identity
/// covers later attachments. The transcript file itself may appear after launch.
pub(super) type CaptureLaunch = fn(&SpawnSpec, &LaunchPlan) -> crate::Result<Claim>;

pub(super) struct Claim {
    pub(super) initial: ProviderId,
    pub(super) cwd: PathBuf,
    pub(super) root: transcript::Root,
}

impl Claim {
    pub(super) fn new(initial: ProviderId, cwd: &Path, root: &Path) -> crate::Result<Self> {
        if root.as_os_str().is_empty() {
            return Err(crate::Error::InvalidInput("missing transcript root".into()));
        }
        let cwd = std::path::absolute(cwd).map_err(|_| {
            crate::Error::InvalidInput("unavailable transcript working directory".into())
        })?;
        let root = transcript::Root::open(cwd.join(root)).map_err(|source| crate::Error::Io {
            context: "pin transcript root".into(),
            source,
        })?;
        Ok(Self { initial, cwd, root })
    }
}

// Opaque to the core. A provider decides what constitutes a valid identifier.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct ProviderId(Box<str>);

impl ProviderId {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn new(id: &str) -> Result<Self, Gap> {
        if id.is_empty() || id.len() > 256 {
            return Err(Gap::Uncorroborated);
        }
        Ok(Self(id.into()))
    }
}

pub(super) struct Launch {
    pub(super) session: SessionId,
    pub(super) lifecycle: u64,
    pub(super) claim: Arc<Claim>,
}

/// The provider corroborates the original launch and its current conversation. A
/// writer revision can distinguish a switch away and back to the very same file.
/// `relative` names the transcript beneath the corroborated effective root.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Binding {
    pub(super) initial: ProviderId,
    pub(super) current: ProviderId,
    pub(super) revision: u64,
    pub(super) relative: PathBuf,
}

pub(super) trait Witness: Send + Sync + 'static {
    /// Sample independent current-writer evidence. Never infer it from a filename, PID
    /// or working directory. A shell's actual writer must corroborate the launch root.
    /// A pull may sample concurrently with the reader. This must use bounded local
    /// evidence and must not wait on the worker, parser, or other provider callbacks.
    fn current(&self, launch: &Launch) -> Result<Binding, Gap>;
}

pub(super) trait Source: Send + 'static {
    /// Obtain the independent sampler before the worker starts.
    fn witness(&self) -> Arc<dyn Witness>;

    fn current(&mut self, launch: &Launch) -> Result<Binding, Gap> {
        self.witness().current(launch)
    }

    /// A provider can additionally check metadata records on this opened object.
    fn open(&mut self, launch: &Launch) -> Result<Opened, Gap> {
        let binding = self.current(launch)?;
        Opened::new(launch, binding)
    }
}

/// Receives complete records only. Retained parser state must fit one bounded reply
/// candidate plus bounded provider metadata (including identifier and block counts).
pub(super) trait Parser: Send + 'static {
    fn record(&mut self, binding: &Binding, line: &[u8], reply_limit: usize)
    -> Result<Record, Gap>;
    fn reset(&mut self);
}

pub(super) enum Record {
    Ignore,
    User,
    End(Option<Reply>),
}

/// Builder shared with parsers so truncation happens before copying each text block.
#[derive(Default)]
pub(super) struct Reply {
    pub(super) text: String,
    pub(super) truncated: bool,
}

impl Reply {
    pub(super) fn push(&mut self, text: &str, limit: usize) {
        if self.truncated {
            return;
        }
        let mut n = text.len().min(limit.saturating_sub(self.text.len()));
        self.truncated = n < text.len();
        while !text.is_char_boundary(n) {
            n -= 1;
        }
        self.text.reserve_exact(n);
        self.text.push_str(&text[..n]);
    }
}

impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply")
            .field("bytes", &self.text.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Gap {
    Missing,
    Uncorroborated,
    SessionChanged,
    Replaced,
    Rewritten,
    Truncated,
    Io,
    Format,
    Oversized,
    Overflow,
    Lifecycle,
    Monitor,
    WorkerPanic,
}

impl From<std::io::Error> for Gap {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::NotFound => Self::Missing,
            std::io::ErrorKind::UnexpectedEof => Self::Truncated,
            _ => Self::Io,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Status {
    Waiting,
    Active {
        start: u64,
        offset: u64,
        incomplete_turns: u32,
    },
    Gap(Gap),
    Closed,
}

impl Status {
    fn live(self) -> bool {
        matches!(self, Self::Waiting | Self::Active { .. })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AttachError {
    InvalidLimits,
    NoLaunch,
    Busy,
    Spawn,
}

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub(super) read_bytes: usize,
    pub(super) records: usize,
    pub(super) line_bytes: usize,
    pub(super) reply_bytes: usize,
    pub(super) queue_items: usize,
    pub(super) queue_bytes: usize,
}

// Pilot ceilings, not measured provider defaults.
const CEILINGS: Limits = Limits {
    read_bytes: 64 * 1024,
    records: 256,
    line_bytes: 1024 * 1024,
    reply_bytes: 256 * 1024,
    queue_items: 16,
    queue_bytes: 1024 * 1024,
};

impl Limits {
    fn validate(self) -> Result<(), AttachError> {
        for (value, max) in [
            (self.read_bytes, CEILINGS.read_bytes),
            (self.records, CEILINGS.records),
            (self.line_bytes, CEILINGS.line_bytes),
            (self.reply_bytes, CEILINGS.reply_bytes),
            (self.queue_items, CEILINGS.queue_items),
            (self.queue_bytes, CEILINGS.queue_bytes),
        ] {
            if !(1..=max).contains(&value) {
                return Err(AttachError::InvalidLimits);
            }
        }
        if self.reply_bytes > self.queue_bytes {
            return Err(AttachError::InvalidLimits);
        }
        Ok(())
    }
}

pub(super) struct Options {
    pub(super) limits: Limits,
    // Tests can advance a manual clock. The ordinary clock coalesces missed ticks.
    ticks: Receiver<Instant>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            limits: CEILINGS,
            ticks: crossbeam_channel::tick(Duration::from_millis(50)),
        }
    }
}

/// A cached shell launch line can reuse a provider ID. The claim covers only the first
/// launch. Reattaching within it still requires a fresh current-session witness.
pub(super) struct Slot {
    claim: Arc<Claim>,
    first_launch: u64,
    occupied: Arc<AtomicBool>,
}

const MAX_WORKERS: usize = 4;
static WORKERS: AtomicUsize = AtomicUsize::new(0);

struct Permit(Arc<AtomicBool>);

impl Permit {
    fn acquire(slot: &Slot) -> Result<Self, AttachError> {
        slot.occupied
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| AttachError::Busy)?;
        if WORKERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_WORKERS).then_some(n + 1)
            })
            .is_err()
        {
            slot.occupied.store(false, Ordering::Release);
            return Err(AttachError::Busy);
        }
        Ok(Self(slot.occupied.clone()))
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::AcqRel);
        self.0.store(false, Ordering::Release);
    }
}

impl Slot {
    pub(super) fn new(claim: Claim, ctl: &Controller) -> Self {
        Self {
            claim: Arc::new(claim),
            first_launch: ctl.read(|st| st.lifecycle_revision().wrapping_add(1)),
            occupied: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn attach(
        &self,
        ctl: &Arc<Controller>,
        source: impl Source,
        parser: impl Parser,
        options: Options,
    ) -> Result<TurnReceiver, AttachError> {
        options.limits.validate()?;
        let permit = Permit::acquire(self)?;
        let launch = ctl.read(|st| {
            let lifecycle = st.lifecycle_revision();
            if lifecycle != self.first_launch
                || st.disposed
                || st.monitor_unavailable()
                || st.launch_held()
                || ctl.term().turns_ended()
                || matches!(st.agent_state(), AgentState::ShellOnly | AgentState::Exited)
            {
                return Err(AttachError::NoLaunch);
            }
            Ok(Launch {
                claim: self.claim.clone(),
                lifecycle,
                session: ctl.cfg.id,
            })
        })?;
        reader::spawn(ctl, launch, source, parser, options, permit)
    }
}

#[cfg(test)]
mod tests;
