//! Process-local recovery metadata for raw terminal observers.

use std::ops::Range;

use super::{PtySize, Replay, SessionId};

/// A position between bytes in one PTY's output, suitable for reconnecting an observer.
///
/// `offset` counts raw bytes from spawn, not UTF-8 characters or [`super::Event::Output`]
/// chunks. Advance it by each consumed output chunk's byte length. The session ID also
/// serves as the generation: replacing a PTY always mints a new ID, even when resuming
/// the same provider conversation. These IDs are process-local; a remote host must scope
/// cursors to its own server incarnation across restarts. This is not an authentication
/// token or a durable provider resume token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReplayCursor {
    /// PTY identity and process-local generation.
    pub session: SessionId,
    /// Number of raw output bytes preceding this position.
    pub offset: u64,
}

/// Why a range of requested output cannot be replayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplayGapReason {
    /// The memory log evicted older output.
    Evicted,
    /// Recording is disabled.
    RecordingDisabled,
    /// A file write failed; subsequent output was not recorded.
    FileFailed,
}

/// Missing bytes, reported as metadata rather than inserted into the child's stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayGap {
    /// Half-open byte offsets in the snapshot's session.
    pub range: Range<u64>,
    /// Why these bytes are unavailable.
    pub reason: ReplayGapReason,
}

/// An atomic replay/live boundary from [`super::Terminal::attach_recovery`].
///
/// `replay_range` and `gaps` together cover the requested range through `boundary`.
/// Ranges use raw byte offsets; `next_output_seq` separately identifies the first live
/// output chunk. Non-output events are not replayed, except the sticky exit event.
/// No per-chunk index or extra output history is retained for this API.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RecoverySnapshot {
    /// Start requested by the caller (offset zero when no cursor was supplied).
    pub requested: ReplayCursor,
    /// First live byte, and the position represented by `display` and `size`.
    pub boundary: ReplayCursor,
    /// Oldest retained byte, or the boundary if nothing is retained.
    pub retained_start: ReplayCursor,
    /// Available bytes actually returned in `replay` (half-open offsets).
    pub replay_range: Range<u64>,
    /// Raw bytes or a bounded file range. File data has the same path/lifetime caveats as
    /// [`Replay::File`]; read it while the session's log remains open.
    pub replay: Replay,
    /// Requested bytes unavailable in this snapshot (at most one contiguous gap).
    pub gaps: Vec<ReplayGap>,
    /// The sequence number of the next [`super::Event::Output`], if another is produced.
    pub next_output_seq: u64,
    /// Current successfully applied PTY geometry, including resizes with no output.
    /// Subsequent resizes require another snapshot; raw events do not report them.
    pub size: PtySize,
    /// Display view at exactly `boundary`, or `None` with the model disabled
    /// or when explicitly requesting metadata-only live recovery.
    ///
    /// This text/cursor/mode view omits cell styling and is not a parser checkpoint:
    /// it omits pending escape/UTF-8 fragments, parser state and hidden buffers.
    /// Do not feed `replay` after drawing this view: the replay
    /// precedes it. Exact parser continuation requires a host-maintained parser and an
    /// uninterrupted raw stream, including host-applied resize history.
    #[cfg(feature = "screen")]
    pub display: Option<crate::screen::ScreenSnapshot>,
}

/// A recovery attachment refused before registering a receiver or copying the log.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ReplayError {
    /// A cursor names a different PTY generation.
    #[error("replay cursor belongs to session {actual}, expected {expected}")]
    SessionMismatch {
        /// The current PTY session.
        expected: SessionId,
        /// The cursor's session.
        actual: SessionId,
    },
    /// A cursor is beyond the current output boundary.
    #[error("replay cursor offset {requested} is ahead of output boundary {end}")]
    FutureCursor {
        /// Requested byte offset.
        requested: u64,
        /// Current output byte count.
        end: u64,
    },
    /// The session was torn down; natural output EOF alone does not close recovery.
    #[error("session closed")]
    Closed,
}
