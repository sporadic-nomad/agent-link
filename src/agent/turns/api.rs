use std::fmt;
#[cfg(feature = "turns")]
use std::path::Path;
#[cfg(feature = "turns")]
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};

/// A host's assertion that it controls admission of every writer under one configuration
/// root. Hold the lease while that guarantee is in force. One lease may cover several
/// terminals with distinct sessions. Dropping it revokes all of their observations.
///
/// Before constructing this lease, isolate the root from unadmitted processes and
/// enforce one writer per logical session, including after an interactive session
/// switch. A second Claude process can resume a live UUID and produce indistinguishable
/// records. A fresh directory, Unix mode 0700, or a process-local lock alone permits
/// other processes with the same account to write there.
#[cfg(feature = "turns")]
#[must_use = "keep the lease alive while the host enforces exclusive writer access"]
pub struct ExclusiveClaudeRoot {
    #[cfg(unix)]
    root: Arc<super::transcript::Root>,
    #[cfg(unix)]
    pub(in crate::agent) launch_path: std::path::PathBuf,
    active: Arc<AtomicBool>,
    spawning: Mutex<()>,
}

#[cfg(feature = "turns")]
impl ExclusiveClaudeRoot {
    /// Pins an existing local configuration directory for a host that already enforces
    /// exclusive writer access. Performs synchronous directory I/O. Requires native
    /// macOS arm64. Returns an I/O error for an inaccessible directory or final symlink,
    /// or [crate::Error::Unsupported] on other platforms.
    pub fn new(root: impl AsRef<Path>) -> crate::Result<Self> {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            let root = std::path::absolute(root)
                .and_then(super::transcript::Root::open)
                .map_err(|source| crate::Error::Io {
                    context: "pin exclusive transcript root".into(),
                    source,
                })?;
            // Darwin can name a directory by volume and file ID. Holding the
            // descriptor pins that identity across renames through process creation.
            let (volume, file) = root.directory_id();
            Ok(Self {
                root: Arc::new(root),
                launch_path: format!("/.vol/{volume}/{file}").into(),
                active: Arc::new(AtomicBool::new(true)),
                spawning: Mutex::new(()),
            })
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            let _ = root;
            Err(crate::Error::Unsupported(
                "controlled PTY turns require native macOS arm64".into(),
            ))
        }
    }

    /// Some native providers call realpath on their configuration directory.
    /// Darwin's /.vol path works for descriptor lookup but fails realpath.
    pub(super) fn named(root: &Path) -> crate::Result<Self> {
        #[cfg(unix)]
        {
            let mut lease = Self::new(root)?;
            let name = lease
                .root
                .path
                .canonicalize()
                .map_err(|source| crate::Error::Io {
                    context: "resolve isolated transcript root".into(),
                    source,
                })?;
            lease.root.check_path(&name).map_err(|_| {
                crate::Error::InvalidInput("isolated transcript root changed during setup".into())
            })?;
            lease.launch_path = name;
            Ok(lease)
        }
        #[cfg(not(unix))]
        Self::new(root)
    }

    /// Revoke before releasing the external admission or filesystem isolation. Existing
    /// receivers discard queued replies at their next access. Waits for an admitted
    /// process creation to finish. A logger can revoke during spawn preparation or after
    /// process creation. Terminals already started continue running. Repeated
    /// revocation is harmless. Coordinate all leases for a directory before releasing it.
    pub fn revoke(&self) {
        self.active.store(false, Ordering::Release);
        // No process may start under this lease after revoke returns. An overlapping
        // creation can finish, while receiver accesses already see the cutoff.
        drop(self.spawning.lock().unwrap_or_else(|e| e.into_inner()));
    }

    pub(in crate::agent) fn admit_spawn(&self) -> crate::Result<MutexGuard<'_, ()>> {
        let guard = self.spawning.lock().unwrap_or_else(|e| e.into_inner());
        if !self.active.load(Ordering::Acquire) {
            return Err(crate::Error::InvalidInput(
                "exclusive transcript writer authority was revoked".into(),
            ));
        }
        #[cfg(unix)]
        self.root
            .check_name()
            .and_then(|()| self.root.check_path(&self.launch_path))
            .map_err(|_| {
                crate::Error::InvalidInput("exclusive transcript root unavailable at spawn".into())
            })?;
        Ok(guard)
    }

    #[cfg(unix)]
    pub(in crate::agent) fn bind(&self, claim: &mut super::Claim) -> crate::Result<()> {
        self.root
            .check_name()
            .map_err(|_| crate::Error::InvalidInput("exclusive transcript root changed".into()))?;
        if !self.active.load(Ordering::Acquire)
            || self.root.directory_id() != claim.root.directory_id()
        {
            return Err(crate::Error::InvalidInput(
                "missing authority for effective transcript root".into(),
            ));
        }
        claim.root = self.root.clone();
        claim.authority = Some(self.active.clone());
        Ok(())
    }
}

#[cfg(feature = "turns")]
impl Drop for ExclusiveClaudeRoot {
    fn drop(&mut self) {
        self.revoke();
    }
}

#[cfg(feature = "turns")]
impl fmt::Debug for ExclusiveClaudeRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExclusiveClaudeRoot")
            .field("active", &self.active.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

/// Bounded text of a turn that began after the attachment's verified EOF.
/// For isolated sources the enclosing `TranscriptTurn` specifies its completion evidence and sensitivity.
/// Debug reports the byte count and truncation flag. The host owns the returned text.
#[derive(Default)]
#[non_exhaustive]
pub struct TurnReply {
    /// Nonempty text blocks in order, separated by a blank line.
    pub text: String,
    /// The final reply exceeded the configured byte budget and was cut at a UTF-8 boundary.
    pub truncated: bool,
    #[cfg(unix)]
    pub(in crate::agent) completion_only: bool,
    #[cfg(feature = "turns")]
    pub(in crate::agent) inferred: bool,
}

#[cfg(unix)]
impl TurnReply {
    pub(in crate::agent) fn push(&mut self, text: &str, limit: usize) {
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

impl fmt::Debug for TurnReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnReply")
            .field("bytes", &self.text.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// Why an attachment lost continuity. Queued and partial text are discarded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TurnGap {
    /// A previously observed file or required path disappeared.
    Missing,
    /// The process, project, writer record, session, or exclusive claim was ambiguous.
    Uncorroborated,
    /// The writer switched sessions or another binding generation was observed.
    SessionChanged,
    /// The pinned root or transcript was replaced.
    Replaced,
    /// Sampled bytes changed in place.
    Rewritten,
    /// The file shrank or ended before previously sampled bytes.
    Truncated,
    /// Local file or process evidence could no longer be read.
    Io,
    /// The provider's required record shape or ordering was invalid.
    Format,
    /// A required record or provider metadata exceeded a resource bound.
    Oversized,
    /// A complete reply could not fit the bounded queue.
    Overflow,
    /// The terminal's agent launch or foreground owner changed.
    Lifecycle,
    /// The controller lost its internal PTY monitor.
    Monitor,
    /// The observer worker panicked.
    WorkerPanic,
    /// The host revoked or dropped its exclusive writer lease.
    AuthorityRevoked,
}

/// Coalesced state of one attachment. A gap is sticky. A new attachment takes a fresh
/// EOF baseline after its predecessor's worker retires.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TurnStatus {
    /// Waiting for a new direct child's first writer record or its corroborated transcript header.
    /// Once open, the transcript stays pinned while the header is incomplete.
    Waiting,
    /// Following a single corroborated transcript.
    Active {
        /// Byte length at the initial verified EOF.
        start: u64,
        /// Byte offset through the latest validated batch.
        offset: u64,
        /// Saturating count of armed turns superseded by another prompt before an end.
        incomplete_turns: u32,
    },
    /// Observation ended with a typed reason.
    Gap(TurnGap),
    /// The direct child exited, PTY output ended, or the terminal or receiver closed.
    Closed,
}

#[cfg(unix)]
impl TurnStatus {
    pub(super) fn live(self) -> bool {
        matches!(self, Self::Waiting | Self::Active { .. })
    }
}

/// Synchronous refusal to start a turn reader. The terminal keeps running.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum AttachError {
    /// A limit was zero, above its ceiling, or inconsistent with the queue budget.
    #[error("invalid turn reader limits")]
    InvalidLimits,
    /// The terminal has no eligible launch or that launch ended.
    #[error("no eligible controlled PTY launch")]
    NoLaunch,
    /// An active or retiring worker holds this terminal or the global worker budget.
    #[error("turn reader capacity is in use")]
    Busy,
    /// The operating system could not start the worker thread.
    #[error("could not start turn reader")]
    Spawn,
    /// The exclusive-writer lease was revoked before attachment.
    #[error("exclusive writer authority was revoked")]
    AuthorityRevoked,
}

/// Per-attachment limits. Defaults are also the maximum supported values. Set smaller
/// nonzero values as needed, keeping the reply limit within the queue's byte budget.
/// The worker polls every 50 ms and yields between bounded read passes.
#[derive(Clone, Copy, Debug)]
pub struct TurnOptions {
    /// New data requested per read, within the 64 KiB verification and read budget.
    pub read_bytes: usize,
    /// Maximum complete records consumed per batch, or per full Cursor image (256).
    pub records: usize,
    /// Maximum JSON line bytes excluding its newline (1 MiB). Cursor bounds the
    /// entire rewrite image by this limit and by 16 times `read_bytes`.
    pub line_bytes: usize,
    /// Maximum retained UTF-8 bytes per final reply (256 KiB).
    pub reply_bytes: usize,
    /// Maximum queued and staged replies (16).
    pub queue_items: usize,
    /// Maximum queued and staged reply bytes (1 MiB).
    pub queue_bytes: usize,
}

impl TurnOptions {
    pub(super) const MAX: Self = Self {
        read_bytes: 64 * 1024,
        records: 256,
        line_bytes: 1024 * 1024,
        reply_bytes: 256 * 1024,
        queue_items: 16,
        queue_bytes: 1024 * 1024,
    };

    #[cfg(unix)]
    pub(super) fn validate(self) -> Result<(), AttachError> {
        let max = Self::MAX;
        for (value, ceiling) in [
            (self.read_bytes, max.read_bytes),
            (self.records, max.records),
            (self.line_bytes, max.line_bytes),
            (self.reply_bytes, max.reply_bytes),
            (self.queue_items, max.queue_items),
            (self.queue_bytes, max.queue_bytes),
        ] {
            if !(1..=ceiling).contains(&value) {
                return Err(AttachError::InvalidLimits);
            }
        }
        if self.reply_bytes > self.queue_bytes {
            return Err(AttachError::InvalidLimits);
        }
        Ok(())
    }
}

impl Default for TurnOptions {
    fn default() -> Self {
        Self::MAX
    }
}
