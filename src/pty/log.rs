//! The session log: a raw copy of the child's output for late attach and recordings, and
//! the [`Replay`] that [`Terminal::attach`](super::Terminal::attach) hands out.
//!
//! The log lives inside the event bus and is appended under the bus lock, in the same
//! critical section that numbers the chunk; `attach` snapshots it under that lock too. So a
//! chunk is either in the snapshot or sent to the new receiver, never both, never neither.

use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::PathBuf;

use super::{ReplayGapReason, SessionId, SessionLogConfig};
use crate::error::{Error, Result};

/// Largest memory-log block: small chunks are merged up to `cap / 8` (at most this), so a
/// stream of tiny reads does not cost one allocation and one queue entry per read, and
/// evicting one block never drops more than an eighth of a small cap.
const BLOCK: usize = 8 * 1024;

/// What a session recorded before a receiver from [`Terminal::attach`] starts: everything
/// up to, not including, the first `Output` that receiver gets.
///
/// [`Terminal::attach`]: super::Terminal::attach
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Replay {
    /// Nothing is recorded ([`SessionLogConfig::Off`]).
    None,
    /// The newest output, from the memory log. Once older output was evicted it starts
    /// mid-stream — possibly inside an escape sequence, so a renderer should reset first.
    Bytes(Vec<u8>),
    /// The first `len` bytes of the file log at `path`; read them with
    /// [`Replay::into_bytes`] or stream them yourself. They stay valid while the session's
    /// log is open: the file only grows, and the path is locked against other sessions.
    /// After teardown a new session logging to the same path empties it.
    File {
        /// The log file, as given in [`SessionLogConfig::File`].
        path: PathBuf,
        /// How many bytes of it precede the first live event.
        len: u64,
    },
    /// The file log stopped at a write error. Its first `len` bytes are the output up to
    /// the error; the output after that was not recorded, so it is missing between this
    /// replay and the live events.
    FileFailed {
        /// The log file.
        path: PathBuf,
        /// How many bytes were written before the error.
        len: u64,
        /// The write error.
        error: String,
    },
    /// A retained file segment returned by [`super::Terminal::attach_recovery`]. Gaps
    /// are reported separately in its snapshot, including after a file write failure.
    /// The same path/lifetime limitations as [`File`](Self::File) apply.
    FileRange {
        /// The log file.
        path: PathBuf,
        /// Byte offset at which to start reading.
        offset: u64,
        /// Number of bytes to read, excluding any later appends.
        len: u64,
    },
}

impl Replay {
    /// The replayed bytes: empty for [`None`](Self::None), the file's first `len` bytes for
    /// [`File`](Self::File), or the specified segment for [`FileRange`](Self::FileRange).
    /// Errors: the file cannot be read or the requested range is truncated, or the
    /// log failed ([`FileFailed`](Self::FileFailed)), so the bytes would not be complete.
    pub fn into_bytes(self) -> std::io::Result<Vec<u8>> {
        match self {
            Self::None => Ok(Vec::new()),
            Self::Bytes(bytes) => Ok(bytes),
            Self::File { path, len } => Self::FileRange {
                path,
                offset: 0,
                len,
            }
            .into_bytes(),
            Self::FileRange { path, offset, len } => {
                let mut bytes = Vec::new();
                let mut file = File::open(&path)?;
                file.seek(SeekFrom::Start(offset))?;
                file.take(len).read_to_end(&mut bytes)?;
                if (bytes.len() as u64) < len {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                            "session log {} holds {} of {len} bytes",
                            path.display(),
                            bytes.len()
                        ),
                    ));
                }
                Ok(bytes)
            }
            Self::FileFailed { path, error, .. } => Err(std::io::Error::other(format!(
                "session log {} is incomplete: {error}",
                path.display()
            ))),
        }
    }
}

/// Prints sizes, not contents: a replay can hold megabytes of output, secrets included.
impl fmt::Debug for Replay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Bytes(bytes) => write!(f, "Bytes({} bytes)", bytes.len()),
            Self::File { path, len } => f
                .debug_struct("File")
                .field("path", path)
                .field("len", len)
                .finish(),
            Self::FileFailed { path, len, error } => f
                .debug_struct("FileFailed")
                .field("path", path)
                .field("len", len)
                .field("error", error)
                .finish(),
            Self::FileRange { path, offset, len } => f
                .debug_struct("FileRange")
                .field("path", path)
                .field("offset", offset)
                .field("len", len)
                .finish(),
        }
    }
}

/// A session's log, built from its [`SessionLogConfig`] at spawn.
pub(crate) enum SessionLog {
    Off,
    Memory(Ring),
    File(FileLog),
}

impl SessionLog {
    /// Validates `config` and opens it, before the child starts. The file log is created
    /// or opened but not emptied yet ([`begin`](Self::begin) does that). Errors:
    /// [`Error::InvalidInput`] for a zero cap or a relative path; [`Error::Io`] when the file
    /// cannot be opened, is not a regular file or is in use by another session.
    pub(crate) fn open(config: &SessionLogConfig) -> Result<Self> {
        match config {
            SessionLogConfig::Off => Ok(Self::Off),
            SessionLogConfig::Memory { cap_bytes } => {
                if *cap_bytes == 0 {
                    return Err(Error::InvalidInput(
                        "session log cap_bytes must be at least 1 (SessionLogConfig::Off records nothing)"
                            .into(),
                    ));
                }
                Ok(Self::Memory(Ring::new(*cap_bytes)))
            }
            SessionLogConfig::File { path } => {
                if !path.is_absolute() {
                    return Err(Error::InvalidInput(format!(
                        "session log path {} is not absolute",
                        path.display()
                    )));
                }
                FileLog::open(path.clone()).map(Self::File)
            }
        }
    }

    /// Empties the file log. Called once the child has started (before anything is
    /// read), so a spawn that fails earlier leaves the previous log intact.
    pub(crate) fn begin(&mut self) -> Result<()> {
        match self {
            Self::File(file) => file.begin(),
            Self::Off | Self::Memory(_) => Ok(()),
        }
    }

    /// Records one chunk. Called under the bus lock.
    pub(crate) fn append(&mut self, bytes: &[u8], id: SessionId) {
        match self {
            Self::Off => {}
            Self::Memory(ring) => ring.append(bytes),
            Self::File(file) => file.append(bytes, id),
        }
    }

    /// Everything recorded so far. Called under the bus lock: the memory log is copied
    /// here (milliseconds for the 32 MiB default); the file log is not read.
    pub(crate) fn replay(&self) -> Replay {
        match self {
            Self::Off => Replay::None,
            Self::Memory(ring) => Replay::Bytes(ring.snapshot()),
            Self::File(file) => file.replay(),
        }
    }

    /// Retained byte interval and the reason output outside it is missing.
    pub(crate) fn retained(&self, end: u64) -> (Range<u64>, ReplayGapReason) {
        match self {
            Self::Off => (end..end, ReplayGapReason::RecordingDisabled),
            Self::Memory(ring) => (end - ring.len as u64..end, ReplayGapReason::Evicted),
            Self::File(file) => (0..file.len, ReplayGapReason::FileFailed),
        }
    }

    /// Copy only the requested retained bytes; never read file data under the bus lock.
    pub(crate) fn replay_range(&self, retained: Range<u64>, range: Range<u64>) -> Replay {
        if range.is_empty() {
            return Replay::None;
        }
        match self {
            Self::Off => Replay::None,
            Self::Memory(ring) => {
                Replay::Bytes(ring.snapshot_from((range.start - retained.start) as usize))
            }
            Self::File(file) => Replay::FileRange {
                path: file.path.clone(),
                offset: range.start,
                len: range.end - range.start,
            },
        }
    }
}

/// The memory log: the newest output, at most `cap` bytes (or the newest chunk alone when
/// it is bigger), oldest blocks evicted first.
pub(crate) struct Ring {
    blocks: VecDeque<Vec<u8>>,
    /// Bytes in `blocks`.
    len: usize,
    cap: usize,
    /// Merge limit: `cap / 8`, between 1 and [`BLOCK`].
    block: usize,
}

impl Ring {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            blocks: VecDeque::new(),
            len: 0,
            cap,
            block: (cap / 8).clamp(1, BLOCK),
        }
    }

    /// Appends, then evicts the oldest blocks while over the cap. The newest block always
    /// stays, so more than `cap` is kept only when the newest chunk alone is bigger.
    pub(crate) fn append(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        match self.blocks.back_mut() {
            Some(last) if last.len() + bytes.len() <= self.block => {
                last.extend_from_slice(bytes);
            }
            _ => {
                if let Some(last) = self.blocks.back_mut() {
                    last.shrink_to_fit();
                }
                self.blocks.push_back(bytes.to_vec());
            }
        }
        self.len += bytes.len();
        while self.len > self.cap && self.blocks.len() > 1 {
            if let Some(old) = self.blocks.pop_front() {
                self.len -= old.len();
            }
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<u8> {
        self.snapshot_from(0)
    }

    fn snapshot_from(&self, mut skip: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len - skip);
        for block in &self.blocks {
            let n = skip.min(block.len());
            out.extend_from_slice(&block[n..]);
            skip -= n;
        }
        out
    }
}

/// How long [`lock_exclusive`] retries a held lock before refusing the path.
#[cfg(unix)]
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// `flock(LOCK_EX)` on `fd`, retried for up to [`LOCK_WAIT`]. The lock belongs to the open
/// file description, which a child that another thread is forking shares until its `exec`
/// closes it (`O_CLOEXEC`) — so a log a session just dropped can look held for a moment
/// (seconds, under an endpoint-security agent that holds each `exec`).
#[cfg(unix)]
fn lock_exclusive(fd: std::os::unix::io::RawFd) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        // SAFETY: `fd` is open for the caller's `File`; the lock is released when it closes.
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::EWOULDBLOCK) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Some(libc::EWOULDBLOCK) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "in use by another session",
                ));
            }
            _ => return Err(error),
        }
    }
}

/// The file log: every chunk appended, unbuffered, so `len` is always what the file holds.
pub(crate) struct FileLog {
    path: PathBuf,
    /// Kept open even after a write error, preserving the path lock for recovery reads.
    file: Option<File>,
    len: u64,
    /// The write error that stopped the log.
    error: Option<String>,
}

impl FileLog {
    /// Opens or creates `path` without emptying it. It must be a regular file that no other
    /// session logs to (unix: `flock`; Windows: opened without write sharing), so two
    /// sessions never write one file. unix: a symlink is refused (`O_NOFOLLOW`: a planted
    /// link cannot aim the log at another file), a FIFO fails at once instead of blocking
    /// (`O_NONBLOCK`, cleared once the file is known to be regular), and the file is made
    /// mode 0600 — new or not — since the log holds whatever the child printed, secrets
    /// included. Windows follows the path as given and the file keeps its directory's ACL.
    fn open(path: PathBuf) -> Result<Self> {
        let failed = |source| Error::Io {
            context: format!("open session log {}", path.display()),
            source,
        };
        let mut options = std::fs::OpenOptions::new();
        // Emptied by `begin`, once the child runs.
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_SHARE_READ only: another writer (a second session) is refused, readers
            // (`Replay::into_bytes`) are not.
            options.share_mode(0x1);
        }
        let file = options.open(&path).map_err(failed)?;
        if !file.metadata().map_err(failed)?.is_file() {
            return Err(failed(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a regular file",
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            use std::os::unix::io::AsRawFd;
            let fd = file.as_raw_fd();
            // SAFETY: `fd` is open for the lifetime of `file`; F_GETFL/F_SETFL only change
            // its status flags.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags == -1
                // SAFETY: as above.
                || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1
            {
                return Err(failed(std::io::Error::last_os_error()));
            }
            lock_exclusive(fd).map_err(failed)?;
            // `mode` applies only to a new file.
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(failed)?;
        }
        Ok(Self {
            path,
            file: Some(file),
            len: 0,
            error: None,
        })
    }

    /// Empties the file (nothing was written yet, so the write position is 0).
    fn begin(&mut self) -> Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        file.set_len(0).map_err(|source| Error::Io {
            context: format!("truncate session log {}", self.path.display()),
            source,
        })
    }

    /// Writes the chunk under the bus lock (one chunk, at most 8 KiB). A failed write stops
    /// the log for good — a later success would leave a hole in the file — and is logged
    /// once; `len` counts only the bytes the file took.
    fn append(&mut self, mut bytes: &[u8], id: SessionId) {
        if self.error.is_some() {
            return;
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        while !bytes.is_empty() {
            match file.write(bytes) {
                Ok(0) => {
                    self.fail(std::io::ErrorKind::WriteZero.into(), id);
                    return;
                }
                Ok(n) => {
                    self.len += n as u64;
                    bytes = &bytes[n..];
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.fail(e, id);
                    return;
                }
            }
        }
    }

    fn fail(&mut self, error: std::io::Error, id: SessionId) {
        log::warn!(
            "[agent-link:{id}] session log {} stopped after {} bytes: {error}",
            self.path.display(),
            self.len
        );
        self.error = Some(error.to_string());
    }

    fn replay(&self) -> Replay {
        match &self.error {
            None => Replay::File {
                path: self.path.clone(),
                len: self.len,
            },
            Some(error) => Replay::FileFailed {
                path: self.path.clone(),
                len: self.len,
                error: error.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: SessionId = SessionId(1);

    fn bytes(replay: Replay) -> Vec<u8> {
        replay.into_bytes().expect("readable replay")
    }

    #[test]
    fn off_replays_nothing() {
        let mut log = SessionLog::open(&SessionLogConfig::Off).unwrap();
        log.append(b"abc", ID);
        assert_eq!(log.replay(), Replay::None);
        assert_eq!(bytes(Replay::None), b"");
    }

    #[test]
    fn a_zero_memory_cap_is_refused() {
        let err = SessionLog::open(&SessionLogConfig::Memory { cap_bytes: 0 })
            .err()
            .expect("refused");
        assert!(matches!(err, Error::InvalidInput(_)), "{err}");
    }

    #[test]
    fn the_ring_keeps_everything_under_the_cap() {
        let mut ring = Ring::new(1024);
        for chunk in [&b"one "[..], b"two ", b"three"] {
            ring.append(chunk);
        }
        assert_eq!(ring.snapshot(), b"one two three");
        // Tiny chunks are merged into one block.
        assert_eq!(ring.blocks.len(), 1);
    }

    #[test]
    fn the_ring_evicts_the_oldest_and_keeps_the_newest() {
        let mut ring = Ring::new(10);
        let mut stream = Vec::new();
        for i in 0..50u8 {
            let chunk = vec![b'a' + i % 26; usize::from(i % 7) + 1];
            stream.extend_from_slice(&chunk);
            ring.append(&chunk);
            let kept = ring.snapshot();
            assert!(kept.len() <= 10, "{} kept", kept.len());
            assert!(stream.ends_with(&kept), "the kept bytes are the newest");
            assert!(kept.ends_with(&chunk), "the newest chunk is always kept");
            assert_eq!(kept.len(), ring.len);
        }
    }

    /// Blocks are at most an eighth of the cap, so evicting one keeps most of a small cap.
    #[test]
    fn a_small_cap_keeps_most_of_its_bytes() {
        let mut ring = Ring::new(1024);
        for _ in 0..10 {
            ring.append(&[b'x'; 300]);
        }
        assert_eq!(ring.snapshot().len(), 900);
        let mut ring = Ring::new(1024);
        for _ in 0..100 {
            ring.append(&[b'y'; 30]);
        }
        assert!(ring.snapshot().len() >= 1024 - 128, "{}", ring.len);
    }

    #[test]
    fn a_chunk_bigger_than_the_cap_is_kept_alone() {
        let mut ring = Ring::new(4);
        ring.append(b"ab");
        ring.append(b"0123456789");
        assert_eq!(ring.snapshot(), b"0123456789");
        ring.append(b"cd");
        assert_eq!(ring.snapshot(), b"cd");
    }

    #[test]
    fn the_file_log_truncates_appends_and_replays_its_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        std::fs::write(&path, b"stale content").unwrap();
        let mut log = SessionLog::open(&SessionLogConfig::File { path: path.clone() }).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"stale content",
            "kept until the session begins"
        );
        log.begin().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert_eq!(
            log.replay(),
            Replay::File {
                path: path.clone(),
                len: 0
            }
        );
        log.append(b"hello ", ID);
        let replay = log.replay();
        log.append(b"world", ID);
        assert_eq!(bytes(replay), b"hello ", "only the first len bytes");
        assert_eq!(bytes(log.replay()), b"hello world");
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
    }

    #[cfg(unix)]
    fn mode(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn a_file_log_is_private_new_or_not() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        SessionLog::open(&SessionLogConfig::File { path: path.clone() }).unwrap();
        assert_eq!(mode(&path), 0o600);
        let old = dir.path().join("old.log");
        std::fs::write(&old, b"x").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644)).unwrap();
        SessionLog::open(&SessionLogConfig::File { path: old.clone() }).unwrap();
        assert_eq!(mode(&old), 0o600);
    }

    /// A planted symlink (dangling or not) is refused; its target is untouched.
    #[cfg(unix)]
    #[test]
    fn a_file_log_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("precious");
        std::fs::write(&target, b"keep me").unwrap();
        let link = dir.path().join("session.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = SessionLog::open(&SessionLogConfig::File { path: link })
            .err()
            .expect("refused");
        assert!(matches!(err, Error::Io { .. }), "{err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me");

        let dangling = dir.path().join("dangling.log");
        std::os::unix::fs::symlink(dir.path().join("created"), &dangling).unwrap();
        assert!(SessionLog::open(&SessionLogConfig::File { path: dangling }).is_err());
        assert!(
            !dir.path().join("created").exists(),
            "nothing created through it"
        );
    }

    /// A FIFO (or anything but a regular file) is refused at once, without blocking.
    #[cfg(unix)]
    #[test]
    fn a_file_log_refuses_a_fifo_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("session.log");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert!(SessionLog::open(&SessionLogConfig::File { path: fifo }).is_err());
        // A directory opens for writing nowhere; refused too.
        let err = SessionLog::open(&SessionLogConfig::File {
            path: dir.path().to_path_buf(),
        })
        .err()
        .expect("refused");
        assert!(matches!(err, Error::Io { .. }), "{err}");
    }

    #[test]
    fn a_file_log_that_cannot_be_opened_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-dir").join("session.log");
        let err = SessionLog::open(&SessionLogConfig::File { path: path.clone() })
            .err()
            .expect("refused");
        assert!(matches!(err, Error::Io { .. }), "{err}");
        assert!(
            err.to_string()
                .starts_with(&format!("open session log {}: ", path.display())),
            "{err}"
        );
    }

    #[test]
    fn a_relative_file_log_path_is_refused() {
        let err = SessionLog::open(&SessionLogConfig::File {
            path: PathBuf::from("session.log"),
        })
        .err()
        .expect("refused");
        assert!(matches!(err, Error::InvalidInput(_)), "{err}");
    }

    /// Two live sessions never log to one file; the path is free again once the first
    /// log closes.
    #[test]
    fn a_file_log_in_use_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        let config = SessionLogConfig::File { path: path.clone() };
        let mut first = SessionLog::open(&config).unwrap();
        first.begin().unwrap();
        first.append(b"first", ID);
        let err = SessionLog::open(&config).err().expect("refused");
        assert!(matches!(err, Error::Io { .. }), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"first", "left alone");
        drop(first);
        SessionLog::open(&config).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_file_log_writes_blocking() {
        use std::os::unix::io::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let log = FileLog::open(dir.path().join("session.log")).unwrap();
        let fd = log.file.as_ref().unwrap().as_raw_fd();
        // SAFETY: `fd` is open while `log` lives.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_eq!(flags & libc::O_NONBLOCK, 0);
    }

    #[test]
    fn a_failed_write_stops_the_log_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        let mut log = FileLog::open(path.clone()).unwrap();
        log.append(b"kept", ID);
        log.fail(std::io::Error::other("disk full"), ID);
        log.append(b"lost", ID);
        let replay = log.replay();
        assert_eq!(
            replay,
            Replay::FileFailed {
                path: path.clone(),
                len: 4,
                error: "disk full".into()
            }
        );
        assert!(replay.into_bytes().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"kept");
    }

    #[test]
    fn recovery_preserves_partial_file_prefix_and_declares_the_missing_suffix() {
        use super::super::{
            InputLimits, PtySize, ReplayCursor, ReplayGap, ReplayGapReason, ScreenConfig,
        };
        use super::super::{bus::EventBus, input::Input, model::ModelSlot};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        let mut file = FileLog::open(path.clone()).unwrap();
        file.append(b"kept", ID);
        file.fail(std::io::Error::other("disk full"), ID);
        let size = PtySize { rows: 5, cols: 20 };
        let (writer, _rx) = Input::new(InputLimits::default());
        let screen = ModelSlot::new(&ScreenConfig::default(), ID, size, &writer);
        let bus = EventBus::new(ID, SessionLog::File(file), size);
        // Simulate a log write failing part way through this output chunk.
        bus.publish_output(b"kept-lost".to_vec());
        bus.publish_output(b"-also-lost".to_vec());
        let (snapshot, _) = bus
            .attach_recovery(
                Some(ReplayCursor {
                    session: ID,
                    offset: 2,
                }),
                &screen,
            )
            .unwrap();
        assert_eq!(snapshot.boundary.offset, 19);
        assert_eq!(snapshot.retained_start.offset, 0);
        assert_eq!(snapshot.replay_range, 2..4);
        assert_eq!(
            snapshot.gaps,
            [ReplayGap {
                range: 4..19,
                reason: ReplayGapReason::FileFailed
            }]
        );
        assert_eq!(snapshot.replay.into_bytes().unwrap(), b"pt");
        let (lost, _) = bus
            .attach_recovery(
                Some(ReplayCursor {
                    session: ID,
                    offset: 8,
                }),
                &screen,
            )
            .unwrap();
        assert_eq!(lost.replay_range, 8..8);
        assert_eq!(lost.gaps[0].range, 8..19);
        assert_eq!(lost.replay.into_bytes().unwrap(), b"");
        // The failed log still owns the path: another session cannot erase the prefix.
        assert!(SessionLog::open(&SessionLogConfig::File { path: path.clone() }).is_err());
        bus.close();
        assert!(SessionLog::open(&SessionLogConfig::File { path }).is_ok());
    }

    #[test]
    fn recovery_file_range_stops_at_its_boundary_and_detects_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        let mut log = SessionLog::open(&SessionLogConfig::File { path: path.clone() }).unwrap();
        log.append(b"before", ID);
        let replay = log.replay_range(0..6, 3..6);
        log.append(b"after", ID);
        assert_eq!(bytes(replay.clone()), b"ore");
        drop(log); // Windows denies external writers while the session owns the path.
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_len(4)
            .unwrap();
        assert_eq!(
            replay.into_bytes().unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn a_file_shorter_than_its_replay_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        std::fs::write(&path, b"abc").unwrap();
        let err = Replay::File { path, len: 10 }.into_bytes().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn debug_shows_sizes_not_contents() {
        let shown = format!("{:?}", Replay::Bytes(b"secret-token".to_vec()));
        assert_eq!(shown, "Bytes(12 bytes)");
    }
}
