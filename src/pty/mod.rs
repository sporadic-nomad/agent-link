//! PTY sessions: run a child on a pseudo-terminal, stream its output as [`Event`]s, write
//! to it, resize it, ask who owns its foreground, and tear it down.
//!
//! [`Terminal::spawn`] takes a [`SpawnSpec`] and returns the session plus its first event
//! receiver; [`Terminal::attach`] adds a receiver plus a [`Replay`] of what it missed.
//! Dropping the [`Terminal`] kills the child's process group and the foreground job
//! (Windows: the child's Job Object — everything started from the session, detached
//! programs included — or its process tree) and reaps the child.
//! [`SessionRegistry`] keeps terminals by key, one per key.
//!
//! Rules a host keeps:
//! - Drain every receiver or drop it: they are bounded (1 024 events), and a full one stalls
//!   the session until it has room.
//! - Drop a [`Terminal`] outside your own locks (and off async runtime threads): teardown
//!   blocks for the kill and a reap bounded at about 2 s.
//! - A line typed into a shell ends with `\n` on unix and `\r` for `cmd.exe`. Match a
//!   command's output, never the shell's echo of the line you typed.
//! - macOS: serialise spawns made from several threads at once (or retry the error) —
//!   `openpty` called concurrently now and then fails (`openpty: … Unknown error: -6`).
//! - [`Event::Exit`] means the output ended on its own; a teardown never sends one.
//!   [`SessionId`]s are never reused, so a host can tell a replacement session from the one
//!   it replaced.
//!
//! [`Terminal`] has an example that reads the receiver in a loop; [`forward_to`] runs a
//! callback per event on a thread of its own instead:
//!
//! ```no_run
//! use agent_link::{forward_to, Event, PtySize, SpawnSpec, Terminal};
//!
//! let spec = SpawnSpec::shell(std::env::temp_dir(), PtySize { rows: 24, cols: 80 });
//! let (term, events) = Terminal::spawn(spec)?;
//! forward_to(events, |event| {
//!     if let Event::Output { bytes, .. } = event {
//!         print!("{}", String::from_utf8_lossy(&bytes)); // what a renderer would draw
//!     }
//! })?;
//! term.write(b"ls\n")?;
//! std::thread::sleep(std::time::Duration::from_secs(1));
//! drop(term); // kills the shell and its foreground job, then reaps the shell
//! # Ok::<(), agent_link::Error>(())
//! ```

mod bus;
mod child;
mod foreground;
pub(crate) mod input;
#[cfg(all(feature = "turns", target_os = "macos", target_arch = "aarch64"))]
mod leased;
mod log;
mod model;
mod recovery;
mod registry;
mod session;

pub use bus::forward_to;
// The RPC child shares the exit check that keeps a pid reserved and the reap bound.
#[cfg(feature = "rpc")]
pub(crate) use child::REAP_WAIT;
#[cfg(unix)]
pub(crate) use child::peek_exit;
pub use input::InputLimits;
pub use log::Replay;
pub use recovery::{RecoverySnapshot, ReplayCursor, ReplayError, ReplayGap, ReplayGapReason};
pub use registry::SessionRegistry;
pub use session::Terminal;
#[cfg(feature = "agent")]
pub(crate) use session::TerminalHandle;

/// Held around each PTY spawn in the unit tests, one at a time per test binary: on macOS,
/// `openpty` called from several threads at once now and then fails ("Unknown error: -6"),
/// and parallel tests start their sessions in the same instant. The integration tests use
/// `tests/common::spawn_lock` for the same reason.
#[cfg(all(test, unix))]
pub(crate) fn test_spawn_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::env::EnvPolicy;
use crate::identity::TerminalIdentity;
use crate::shell::ShellSpec;

/// A session's id: process-wide, monotonic, starts at 1, minted once per spawn and never
/// reused. Hosts can use it as an epoch that tells a replacement session from the one it
/// replaced.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

impl SessionId {
    /// Mints the next id. Its counter is the crate's only process-wide mutable state.
    pub(crate) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A terminal size in character cells. Both sides must be at least 1.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PtySize {
    /// Rows (lines).
    pub rows: u16,
    /// Columns.
    pub cols: u16,
}

impl PtySize {
    /// The size portable-pty takes (no pixel size).
    pub(crate) fn to_portable(self) -> portable_pty::PtySize {
        portable_pty::PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// What the PTY runs.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpawnMode {
    /// An interactive shell; the host types commands (an agent's launch line) into it.
    /// The foreground query tells the shell from a program it started.
    Shell(ShellSpec),
    /// The program itself is the child (no shell). `program` is resolved on the child's
    /// `PATH` when it has no directory part.
    Direct {
        /// The executable.
        program: PathBuf,
        /// Its arguments.
        args: Vec<OsString>,
    },
}

/// Everything [`Terminal::spawn`] needs.
#[derive(Clone, Debug)]
pub struct SpawnSpec {
    /// What runs in the PTY.
    pub mode: SpawnMode,
    /// The child's working directory; it must exist and be a directory.
    pub cwd: PathBuf,
    /// The initial terminal size.
    pub size: PtySize,
    /// The child's environment, applied on top of [`identity`](Self::identity).
    pub env: EnvPolicy,
    /// The terminal the child is told it runs in.
    pub identity: TerminalIdentity,
    /// The headless screen model and its query replies (feature `screen`).
    pub screen: ScreenConfig,
    /// The raw output log for late attach and recording.
    pub log: SessionLogConfig,
    /// Admission limits for writes and automatic terminal query replies.
    pub input_limits: InputLimits,
}

impl SpawnSpec {
    /// The user's login shell (`ShellSpec { program: None, login: true }`) in `cwd`, with
    /// the inherited environment, [`TerminalIdentity::Honest`] and the default screen and
    /// log settings.
    pub fn shell(cwd: impl Into<PathBuf>, size: PtySize) -> Self {
        Self::with_mode(
            SpawnMode::Shell(ShellSpec {
                program: None,
                login: true,
            }),
            cwd.into(),
            size,
        )
    }

    /// `program args…` run directly in `cwd`, otherwise like [`shell`](Self::shell).
    pub fn direct(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
        cwd: impl Into<PathBuf>,
        size: PtySize,
    ) -> Self {
        let mode = SpawnMode::Direct {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        };
        Self::with_mode(mode, cwd.into(), size)
    }

    fn with_mode(mode: SpawnMode, cwd: PathBuf, size: PtySize) -> Self {
        Self {
            mode,
            cwd,
            size,
            env: EnvPolicy::default(),
            identity: TerminalIdentity::default(),
            screen: ScreenConfig::default(),
            log: SessionLogConfig::default(),
            input_limits: InputLimits::default(),
        }
    }
}

/// Settings of the headless screen model a session feeds its output to (module
/// `screen`). Without the `screen` feature there is no model and these are
/// ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenConfig {
    /// Keep a screen model (default `true`): each chunk is fed to it before it is
    /// published, `Terminal::screen` reads it, and the session publishes
    /// [`Event::Title`], [`Event::Progress`], [`Event::Frame`] and [`Event::ScreenReset`].
    pub enabled: bool,
    /// Rows of scrollback the model keeps (default 200).
    pub scrollback_lines: usize,
    /// Answer the child's terminal capability queries (primary device attributes, status
    /// and cursor position reports, the synchronized-output mode report) from the model
    /// (default `true`). Turn it off when the host's own renderer already answers them, or
    /// the child gets every reply twice. Unverified on Windows: ConPTY asks for the cursor
    /// position at startup and may wait for the reply, so a host with neither this nor a
    /// renderer of its own answering may see no output.
    /// Replies share the input budget. A busy, full or closed writer drops a reply with a debug
    /// log; processing output never waits for input capacity.
    pub answer_queries: bool,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scrollback_lines: 200,
            answer_queries: true,
        }
    }
}

/// Where a session records its raw output, for [`Terminal::attach`] and recordings. Each
/// chunk is recorded under the session's bus lock, before any receiver gets it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionLogConfig {
    /// Record nothing; [`Terminal::attach`] replays nothing.
    Off,
    /// Keep the newest output in memory. Reads are merged into blocks of up to
    /// `cap_bytes / 8` (at most 8 KiB), evicted oldest first past `cap_bytes`: at most
    /// `cap_bytes` is kept, or the newest read alone (at most 8 KiB) when it is bigger.
    Memory {
        /// The cap, in bytes; at least 1.
        cap_bytes: usize,
    },
    /// Append all output to a file, unbounded. It holds whatever the child printed,
    /// secrets included — which is why the host picks the path; pick a private directory.
    /// Opened (or created) at spawn before the child starts — on unix a symlink or a
    /// non-regular file is refused and the file is set to mode 0600; Windows follows the
    /// path as given and keeps the directory's ACL — and emptied only once the child
    /// started, so a failed spawn leaves an earlier log intact. One session per path: a
    /// path another live session logs to is refused at spawn (unix: after up to 1 s of
    /// retries, since a concurrent fork briefly shares a just-closed log's lock). Written unbuffered under the
    /// bus lock (one chunk per write) — **use a local disk**: a stalled filesystem stalls
    /// the reader with the lock held, and with it `subscribe`, attachment, resize and teardown. A
    /// write error stops the log for good (one `warn`; [`Replay::FileFailed`] from then
    /// on for ordinary attachment). The path remains locked until teardown, including
    /// after failure; [`Terminal::attach_recovery`] can still expose its retained prefix.
    File {
        /// The log file; an absolute path (a relative one is refused at spawn).
        path: PathBuf,
    },
}

impl Default for SessionLogConfig {
    /// `Memory` with a 32 MiB cap.
    fn default() -> Self {
        Self::Memory {
            cap_bytes: 32 * 1024 * 1024,
        }
    }
}

/// What a session reports on its event receivers.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// A chunk of child output, exactly as read from the PTY.
    Output {
        /// The session.
        id: SessionId,
        /// 0 for the session's first chunk, +1 per chunk; a receiver subscribed later
        /// starts at a higher value.
        seq: u64,
        /// The bytes.
        bytes: Vec<u8>,
    },
    /// The PTY output ended on its own: the child exited and nothing else holds the
    /// terminal open (on macOS the child's exit alone ends it). The session's last event —
    /// only an agent controller's events (feature `agent`: its reaction, `AgentState`
    /// `Exited` and `Activity`, and the outcomes of deliveries that were still waiting) can
    /// follow; a receiver added later ([`Terminal::subscribe`],
    /// [`Terminal::attach`]) gets it first. Not sent once teardown has started: only one
    /// queued just before can still be read after `kill` returns, so a teardown is never
    /// reported as an exit.
    ///
    /// On Windows, ConPTY can keep the output pipe open after every child exits, until
    /// the session is torn down. Do not rely on this event for Windows process completion:
    /// continue draining output and poll [`Terminal::try_wait`], then explicitly drop or
    /// kill the session when it is no longer needed. Teardown does not synthesize `Exit`.
    Exit {
        /// The session.
        id: SessionId,
        /// The exit code when known. Currently always `None`: the output ends before the
        /// child is reaped; [`Terminal::try_wait`] reports the code.
        status: Option<i32>,
    },
    /// The child set the window title (OSC 0 or 2), also to the same value again. Sent
    /// after the [`Output`](Self::Output) that carried it. Only with a screen model
    /// (feature `screen`, [`ScreenConfig::enabled`]), like the next three.
    Title {
        /// The session.
        id: SessionId,
        /// The title (invalid UTF-8 replaced).
        title: String,
    },
    /// The child reported progress (OSC 9;4). Sent after the `Output` that carried it.
    Progress {
        /// The session.
        id: SessionId,
        /// The report.
        state: ProgressState,
    },
    /// A synchronized-output frame ended (DEC mode 2026 reset after it was set, or a full
    /// reset inside a frame): the redraw is complete at this point of the stream. Sent
    /// after the `Output` that carried it. `Terminal::screen` may already be further on —
    /// the same chunk can open the next frame, and the reader runs ahead of a slow
    /// receiver — so do not take the snapshot read on this event as that redraw.
    Frame {
        /// The session.
        id: SessionId,
    },
    /// The screen model's parser panicked and the model started over with a blank screen
    /// (title, input modes, alternate screen and cursor visibility kept): `Terminal::screen`
    /// misses what was on screen until the child redraws. Sent after
    /// the `Output` whose processing panicked.
    ScreenReset {
        /// The session.
        id: SessionId,
        /// The panic message.
        reason: String,
    },
    /// An [`AgentTerminal`](crate::agent::AgentTerminal)'s state or `blocking` flag changed
    /// (feature `agent`). The starting values are not sent; nothing without a profile. Like
    /// every controller event, it follows the `Output` that drew the screen it was read
    /// from.
    #[cfg(feature = "agent")]
    AgentState {
        /// The session.
        id: SessionId,
        /// The new state.
        state: crate::agent::AgentState,
        /// Whether the host should mask the terminal (a loading overlay).
        blocking: bool,
    },
    /// An [`AgentTerminal`](crate::agent::AgentTerminal)'s activity changed (feature
    /// `agent`; nothing without a profile). Informational only.
    #[cfg(feature = "agent")]
    Activity {
        /// The session.
        id: SessionId,
        /// The new activity.
        state: crate::agent::Activity,
    },
    /// A delivery started with [`AgentTerminal::send`](crate::agent::AgentTerminal::send)
    /// (or `reset`, `reset_and_send`) ended (feature `agent`; also without a profile): the
    /// same outcome its [`Ticket`](crate::agent::Ticket) holds. One per ticket, in the
    /// order the deliveries ran. At teardown every outstanding ticket resolves
    /// `Aborted(Disposed)`; its final event is best effort and may be omitted when a
    /// receiver is full or an earlier publication is in flight.
    #[cfg(feature = "agent")]
    Delivery {
        /// The session.
        id: SessionId,
        /// The delivery.
        ticket: crate::agent::TicketId,
        /// How it ended.
        outcome: crate::agent::DeliveryOutcome,
    },
    /// A delivery sent with [`Revalidate::Handshake`](crate::agent::Revalidate::Handshake)
    /// reached its commit point (feature `agent`): nothing is pasted until the host answers
    /// with [`AgentTerminal::confirm`](crate::agent::AgentTerminal::confirm) for this ticket
    /// — within the handshake's timeout, else the delivery ends `Aborted(Revalidate)`. Can
    /// come more than once per ticket (a delivery that waits for the agent again reaches
    /// the commit point again); answer each.
    #[cfg(feature = "agent")]
    CommitPoint {
        /// The session.
        id: SessionId,
        /// The delivery.
        ticket: crate::agent::TicketId,
    },
}

impl Event {
    /// The session the event belongs to.
    pub fn id(&self) -> SessionId {
        match self {
            Self::Output { id, .. }
            | Self::Exit { id, .. }
            | Self::Title { id, .. }
            | Self::Progress { id, .. }
            | Self::Frame { id }
            | Self::ScreenReset { id, .. } => *id,
            #[cfg(feature = "agent")]
            Self::AgentState { id, .. }
            | Self::Activity { id, .. }
            | Self::Delivery { id, .. }
            | Self::CommitPoint { id, .. } => *id,
        }
    }
}

/// A progress report (OSC 9;4;`state`;`percent`, as Windows Terminal and ConEmu read it).
/// A percentage is 0–100 when the child gave one.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ProgressState {
    /// State 0: no progress shown.
    Clear,
    /// State 1: working.
    Normal(Option<u8>),
    /// State 2: failed.
    Error(Option<u8>),
    /// State 3: working, amount unknown.
    Indeterminate,
    /// State 4: paused.
    Paused(Option<u8>),
}

/// Who owns the terminal's foreground, and how sure the answer is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Foreground {
    /// The owner.
    pub owner: ForegroundOwner,
    /// How the owner was determined.
    pub confidence: Confidence,
}

/// The program that currently receives the terminal's keystrokes.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ForegroundOwner {
    /// The session's shell is at its prompt.
    Shell,
    /// Something other than the shell: a program the shell started, or the child of a
    /// direct session.
    Agent,
    /// Not known (no foreground process group, the session is closed, or the platform
    /// cannot tell).
    Unknown,
}

/// Which program owns the terminal, beyond "not the shell" (crate-internal: the agent
/// controller types only into the program its own launch started). unix: the foreground
/// process group; Windows: the one child of the shell the program runs under. Two answers
/// naming the same id saw the same program — an id is reused only once everything that
/// held it ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(not(feature = "agent"), allow(dead_code))] // read by the agent controller
pub(crate) struct ProgramId(pub(crate) u32);

/// How a [`Foreground`] answer was obtained.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Confidence {
    /// From the kernel's foreground process group of the terminal (unix).
    Exact,
    /// Inferred, e.g. from the child's process tree; or no answer at all.
    Heuristic,
}

/// How a session's child ended.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ExitInfo {
    /// The exit code; `None` when a signal ended it or the code is unknown.
    pub status: Option<i32>,
}

impl From<portable_pty::ExitStatus> for ExitInfo {
    fn from(status: portable_pty::ExitStatus) -> Self {
        Self {
            status: match status.signal() {
                Some(_) => None,
                // Windows exit codes are `u32` (NTSTATUS values have the top bit set);
                // keep their bits.
                None => Some(status.exit_code() as i32),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_monotonic_and_start_above_zero() {
        let a = SessionId::next();
        let b = SessionId::next();
        assert!(a.0 >= 1);
        assert!(b > a);
        assert_eq!(b.to_string(), b.0.to_string());
    }

    #[test]
    fn spawn_spec_defaults() {
        let spec = SpawnSpec::shell("/tmp", PtySize { rows: 24, cols: 80 });
        assert_eq!(
            spec.mode,
            SpawnMode::Shell(ShellSpec {
                program: None,
                login: true
            })
        );
        assert_eq!(spec.identity, TerminalIdentity::Honest);
        assert_eq!(spec.env, EnvPolicy::default());
        assert_eq!(
            spec.screen,
            ScreenConfig {
                enabled: true,
                scrollback_lines: 200,
                answer_queries: true
            }
        );
        assert_eq!(
            spec.log,
            SessionLogConfig::Memory {
                cap_bytes: 32 * 1024 * 1024
            }
        );

        let spec = SpawnSpec::direct("/bin/echo", ["hi"], "/tmp", spec.size);
        assert_eq!(
            spec.mode,
            SpawnMode::Direct {
                program: "/bin/echo".into(),
                args: vec!["hi".into()]
            }
        );
    }

    #[test]
    fn exit_info_keeps_the_code_and_drops_signals() {
        let code = ExitInfo::from(portable_pty::ExitStatus::with_exit_code(3));
        assert_eq!(code.status, Some(3));
        let signal = ExitInfo::from(portable_pty::ExitStatus::with_signal("Killed"));
        assert_eq!(signal.status, None);
        let ntstatus = ExitInfo::from(portable_pty::ExitStatus::with_exit_code(0xC000_013A));
        assert_eq!(ntstatus.status, Some(0xC000_013A_u32 as i32));
    }

    #[test]
    fn event_id_reads_every_variant() {
        let id = SessionId(7);
        let out = Event::Output {
            id,
            seq: 0,
            bytes: Vec::new(),
        };
        assert_eq!(out.id(), id);
        assert_eq!(Event::Exit { id, status: None }.id(), id);
        let title = Event::Title {
            id,
            title: "t".into(),
        };
        let progress = Event::Progress {
            id,
            state: ProgressState::Clear,
        };
        let reset = Event::ScreenReset {
            id,
            reason: "r".into(),
        };
        for event in [title, progress, Event::Frame { id }, reset] {
            assert_eq!(event.id(), id);
        }
        #[cfg(feature = "agent")]
        {
            let state = Event::AgentState {
                id,
                state: crate::agent::AgentState::Ready,
                blocking: false,
            };
            let activity = Event::Activity {
                id,
                state: crate::agent::Activity::Idle,
            };
            let commit = Event::CommitPoint {
                id,
                ticket: crate::agent::TicketId(1),
            };
            assert_eq!(state.id(), id);
            assert_eq!(activity.id(), id);
            assert_eq!(commit.id(), id);
        }
    }
}
