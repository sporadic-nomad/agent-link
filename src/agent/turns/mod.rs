//! Turn observations from host-controlled direct PTYs on macOS arm64, and a separate
//! best-effort tier for every other terminal.
//!
//! The [`ambient`] tier reads an agent CLI's own transcript for any [`AgentTerminal`]
//! (shell or direct spawn, Unix or Windows) without a lease, an isolated root or a pinned
//! version. It provides no writer exclusion and no attribution guarantee; everything else
//! in this module is the controlled, verified tier described below.
//!
//! [`AgentTerminal`]: crate::agent::AgentTerminal
//!
//! Codex, Pi and Cursor use an [`IsolatedTranscriptRoot`] per terminal. A
//! [`TranscriptReceiver`] yields provider-specific [`TranscriptTurn`] values that
//! distinguish explicit and inferred completions and Cursor's sensitive model text.
//! Claude Code 2.1.296 has its own final reply API, illustrated below.
//!
//! The host first enforces exclusive writer access to each conversation in a dedicated
//! configuration root. Keep its [`ExclusiveClaudeRoot`] alive while admitting launches
//! there. Each [`TurnReceiver`] observes one direct launch from a verified EOF. The
//! caller can distinguish a quiet transcript from loss of continuity with [`TurnStatus`].
//!
//! ```no_run
//! use agent_link::agent::{AgentSpec, AgentTerminal, profiles::ClaudeProfile};
//! use agent_link::agent::turns::{ExclusiveClaudeRoot, TurnOptions, TurnReceiver};
//! use agent_link::{Event, PtySize, SpawnSpec};
//! use crossbeam_channel::Receiver;
//! use std::{path::Path, sync::Arc};
//!
//! // The caller controls all writers in config and supplies a fresh v4 UUID per launch.
//! fn observe(claude: &Path, project: &Path, config: &Path, uuid: &str)
//!     -> Result<(ExclusiveClaudeRoot, AgentTerminal, Receiver<Event>, TurnReceiver),
//!               Box<dyn std::error::Error>>
//! {
//!     let lease = ExclusiveClaudeRoot::new(config)?;
//!     let mut spec = SpawnSpec::direct(claude, ["--session-id", uuid], project,
//!                                     PtySize { rows: 40, cols: 120 });
//!     spec.env.set("CLAUDE_CONFIG_DIR", config);
//!     let agent = AgentSpec::new(Arc::new(ClaudeProfile::default()));
//!     let (term, events) = AgentTerminal::spawn_with_claude_turns(spec, agent, &lease)?;
//!     let turns = term.attach_claude_turns(TurnOptions::default())?;
//!     Ok((lease, term, events, turns))
//! }
//! ```
//!
//! Receivers are pulled: [`TurnReceiver::try_recv`] never blocks, while
//! [`TurnReceiver::recv_timeout`] and [`TurnReceiver::wait_status_change`] block between
//! pulls until the receiver changes, instead of polling. With `tokio`,
//! `TurnReceiver::recv` and `status_change` await the same wakeups.
//!
//! On a nonempty pull the receiver resamples the writer's current session, pinned file,
//! cursor and terminal. A gap discards pending replies. A fresh attachment takes a new EOF
//! baseline. These checks can miss a switch away and back between samples and edits to
//! historical bytes outside the verified region. An external CLI can resume a live UUID
//! and append indistinguishable replies, so hosts must keep enforcing writer exclusion
//! until they revoke the lease.

#[cfg(feature = "turns")]
pub mod ambient;
mod api;
#[cfg(feature = "turns")]
mod providers;
#[cfg(feature = "turns")]
pub use providers::{IsolatedTranscriptRoot, TranscriptReceiver, TranscriptSource, TranscriptTurn};
mod reader;
#[cfg(feature = "turns")]
mod snapshots;
mod stream;
mod transcript;
mod wake;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use super::AgentState;
#[cfg(test)]
use super::LaunchPlan;
use super::state::Controller;
use crate::SessionId;
#[cfg(test)]
use crate::SpawnSpec;

#[cfg(feature = "turns")]
pub use api::ExclusiveClaudeRoot;
pub use api::{AttachError, TurnGap, TurnOptions, TurnRecvError, TurnReply, TurnStatus};
pub(super) use api::{
    TurnGap as Gap, TurnOptions as Limits, TurnReply as Reply, TurnStatus as Status,
};
pub use stream::TurnReceiver;
pub(super) use transcript::DirectoryId;
pub(super) use transcript::Opened;

/// Runs before child creation on the actual Direct argv or prepared shell plan and the
/// merged environment. The transcript directory must exist here so its identity
/// covers later attachments. The transcript file itself may appear after launch.
#[cfg(test)]
pub(super) type CaptureLaunch =
    Arc<dyn Fn(&mut SpawnSpec, &LaunchPlan) -> crate::Result<Claim> + Send + Sync>;

pub(super) struct Claim {
    pub(super) initial: ProviderId,
    pub(super) cwd: PathBuf,
    pub(super) root: Arc<transcript::Root>,
    authority: Option<Arc<AtomicBool>>,
    #[cfg(feature = "turns")]
    pub(super) source: Option<TranscriptSource>,
    /// Present when the provider reserved a new launch before spawn. The slot and
    /// retiring worker share its lifetime, including across a fresh attachment.
    pub(super) writer: Option<Arc<dyn Witness>>,
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
        Ok(Self {
            initial,
            cwd,
            root: Arc::new(root),
            authority: None,
            #[cfg(feature = "turns")]
            source: None,
            writer: None,
        })
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
    /// Mark a reserved UUID as a possible writer immediately before process creation.
    /// Called under the spawn admission guard. Must not run host code.
    fn begin_spawn(&self) {}

    /// Capture the direct child before returning the terminal. Called only in Direct mode.
    fn spawned(&self, _term: &crate::Terminal) {}

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

    /// Select and open the transcript. Missing evidence may be retried until first open.
    fn open(&mut self, launch: &Launch) -> Result<Opened, Gap> {
        let binding = self.current(launch)?;
        Opened::new(launch, binding)
    }

    /// Cursor rewrites its history in place. Other sources append records.
    #[cfg(feature = "turns")]
    fn snapshot_marker(&self) -> Option<snapshots::Marker> {
        None
    }
    /// Return false to wait for a partial header on this same pinned file.
    fn header_ready(&mut self, _opened: &mut Opened) -> Result<bool, Gap> {
        Ok(true)
    }
}

/// Receives complete records only. Retained parser state must fit one bounded reply
/// candidate plus bounded provider metadata (including identifier and block counts).
pub(super) trait Parser: Send + 'static {
    #[cfg(feature = "questions")]
    fn question_record(&self, _line: &[u8]) -> crate::agent::questions::TranscriptChange {
        crate::agent::questions::TranscriptChange::Keep
    }
    fn record(&mut self, binding: &Binding, line: &[u8], reply_limit: usize)
    -> Result<Record, Gap>;
    // Only Pi implements an expressly inferred quiet-time boundary.
    fn has_quiet_candidate(&self) -> bool {
        false
    }
    fn quiet(&mut self) -> Option<Reply> {
        None
    }
    fn reset(&mut self);
}

pub(super) enum Record {
    Ignore,
    User,
    End(Option<Reply>),
    #[cfg(feature = "turns")]
    Next(Option<Reply>),
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

const CEILINGS: Limits = Limits::MAX;

pub(super) struct Options {
    pub(super) limits: Limits,
    // Tests can advance a manual clock. The ordinary clock coalesces missed ticks.
    ticks: Receiver<Instant>,
}

#[cfg(feature = "turns")]
impl From<TurnOptions> for Options {
    fn from(limits: TurnOptions) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }
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
    pub(super) claim: Arc<Claim>,
    first_launch: u64,
    occupied: Arc<AtomicBool>,
}

pub(super) const MAX_WORKERS: usize = 4;
static WORKERS: AtomicUsize = AtomicUsize::new(0);

struct Permit(Arc<AtomicBool>);

impl Permit {
    fn acquire(slot: &Slot) -> Result<Self, AttachError> {
        slot.occupied
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| AttachError::Busy)?;
        #[allow(deprecated, reason = "try_update is unavailable on the Rust 1.85 MSRV")]
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
        if self
            .claim
            .authority
            .as_ref()
            .is_some_and(|a| !a.load(Ordering::Acquire))
        {
            return Err(AttachError::AuthorityRevoked);
        }
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
pub(super) mod tests;
