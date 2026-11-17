//! A JSON-lines-over-stdio child: an agent CLI run headless (`claude --print` in
//! stream-json mode, `codex app-server`, `pi --mode rpc`, `opencode acp`), framed as lines
//! both ways. Transport only: lines are passed verbatim, nothing is parsed.
//!
//! - [`RpcChild::spawn`] starts an [`RpcSpawnSpec`] — on unix `exec <launcher> <args>`
//!   through the user's interactive login shell in a new session (`setsid`); on Windows the
//!   launcher resolved on the child's `PATH`, with no console window — and returns the
//!   handle plus an [`RpcEvent`] receiver: one `Line` per stdout line (with `seq`), then an
//!   `Exit` with the stderr tail on normal EOF, or a terminal `Error` on transport failure.
//!   Stdin I/O failures emit nonterminal `StdinError` and leave output draining.
//! - [`RpcChild::send_line`] queues one line for stdin (`\n` appended by the writer),
//!   refusing oversized input or a full queue without waiting on the child.
//! - [`RpcChild::send_line_tracked`] adds bounded write receipts with absolute deadlines.
//!   [`RpcUtf8Mode::Strict`] rejects corrupt stdout text before a host decodes JSON.
//! - [`RpcChild::close_stdin`] finishes accepted writes and closes stdin, continuing to
//!   drain output. This is independent of provider turn completion.
//! - Dropping the handle (or [`RpcChild::kill`]) kills the child's process group (Windows:
//!   its process tree) and reaps it.
//! - [`preflight`] validates the spec and builds its platform-specific command without
//!   starting a process. A host can check a replacement before tearing down the current
//!   child, but success does not guarantee spawn or agent startup.
//! - [`forward_rpc`] runs a callback per event on a thread of its own.
//!
//! Threads per child, detached, never joined: `agent-link-rpc-stderr-<id>` (the last
//! 32 lines / 4 KiB kept), `agent-link-rpc-reader-<id>`, `agent-link-rpc-writer-<id>`.
//! The first tracked admission also starts `agent-link-rpc-deadlines-<id>` (receipt
//! deadlines, independent of blocked pipes); untracked-only children have no deadline
//! thread. The worker is reused until input closes or the transport stops.
//! [`RpcLimits`] bounds frames and queues. Log lines carry `[<log_tag>:<log_key>]`;
//! payloads and command arguments are logged only with explicit `trace: true`. Stderr
//! additionally supports the independent `trace_stderr: true` opt-in.
//! Built-in profile launches default to YOLO/unattended permissions. Use
//! [`AgentProfile::rpc_launch_with_options`](crate::agent::AgentProfile::rpc_launch_with_options)
//! to select native/custom policy before spawning. The transport does not parse or answer
//! approval requests; hosts choosing a policy that asks must implement those replies.
//!
//! ```no_run
//! use agent_link::agent::{ProfileRegistry, RpcLaunchOptions};
//! use agent_link::rpc::{forward_rpc, preflight, RpcChild, RpcEvent, RpcSpawnSpec};
//!
//! let launch = ProfileRegistry::builtin()
//!     .get("pi")
//!     .expect("a built-in profile")
//!     .rpc_launch_with_options(&RpcLaunchOptions::default())?;
//! let spec = RpcSpawnSpec::new(launch, "/path/to/project");
//! preflight(&spec)?; // before tearing down whatever the new child replaces
//! let (child, events) = RpcChild::spawn(spec)?;
//! child.send_line(r#"{"type":"get_state"}"#)?;
//! forward_rpc(events, |event| match event {
//!     RpcEvent::Line { seq, line, .. } => println!("{seq}: {line}"),
//!     RpcEvent::Exit { stderr_tail, .. } => eprintln!("exited: {stderr_tail}"),
//!     RpcEvent::Error { error, .. } => eprintln!("transport failed: {error}"),
//!     _ => {}
//! })?;
//! drop(child); // kills the child's process group and reaps it
//! # Ok::<(), agent_link::Error>(())
//! ```

mod child;
mod command;
mod framing;
mod input;
mod pump;
mod receipt;
mod stderr_ring;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::process::{ChildStderr, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

use crossbeam_channel::{Receiver, bounded};

pub use self::receipt::{RpcWriteFailure, RpcWriteOutcome, RpcWriteReceipt, RpcWriteStatus};

use self::child::{ChildGuard, ChildState};
use self::input::Input;
use self::pump::Publisher;
use self::stderr_ring::StderrRing;
use crate::agent::RpcLaunch;
use crate::env::EnvPolicy;
use crate::error::{Error, Result};
use crate::pty::SessionId;

/// Memory limits for one RPC transport. Frame sizes count bytes, including CR/LF
/// delimiters when present. Limits must be nonzero; rejected input is never queued.
///
/// Defaults are library resource policy, not vendor protocol maxima. The default
/// output queue holds eight frames (64 MiB raw stdout bytes); lossy UTF-8 can
/// expand queued strings to 192 MiB, excluding the reader frame and metadata.
/// The input budget includes the line currently being written. Output is bounded by
/// `event_capacity` frames, each at most `max_stdout_line_bytes` raw bytes (invalid
/// UTF-8 replacement can expand the string to at most three times that size), plus one
/// reader frame. Pending receipt metadata shares the stdin item budget; there is no
/// transport-owned completion history. Stderr additionally retains only the fixed
/// 4 KiB diagnostic tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RpcLimits {
    /// Maximum raw stdout frame, including its newline. Default: 8 MiB.
    pub max_stdout_line_bytes: usize,
    /// Maximum raw stderr frame, including its newline. Default: 64 KiB.
    /// Larger diagnostic lines are discarded through newline or EOF and replaced
    /// with `[agent-link: oversized stderr line dropped]` in the bounded exit tail.
    /// This does not stop stdout or stdin.
    pub max_stderr_line_bytes: usize,
    /// Maximum stdin frame, including the newline added by `send_line`. Default: 8 MiB.
    pub max_stdin_line_bytes: usize,
    /// Bytes admitted to stdin but not yet completely written. Default: 32 MiB.
    pub max_pending_stdin_bytes: usize,
    /// Lines admitted to stdin but not yet completely written. Default: 1,024.
    pub max_pending_stdin_lines: usize,
    /// Maximum number of pending output events. Default: 8.
    pub event_capacity: usize,
}

impl Default for RpcLimits {
    fn default() -> Self {
        Self {
            max_stdout_line_bytes: 8 * 1024 * 1024,
            max_stderr_line_bytes: 64 * 1024,
            max_stdin_line_bytes: 8 * 1024 * 1024,
            max_pending_stdin_bytes: 32 * 1024 * 1024,
            max_pending_stdin_lines: 1024,
            event_capacity: 8,
        }
    }
}

impl RpcLimits {
    fn validate(self) -> Result<()> {
        let values = [
            self.max_stdout_line_bytes,
            self.max_stderr_line_bytes,
            self.max_stdin_line_bytes,
            self.max_pending_stdin_bytes,
            self.max_pending_stdin_lines,
            self.event_capacity,
        ];
        if values.contains(&0) || values.iter().any(|&n| n > isize::MAX as usize / 4) {
            return Err(Error::InvalidInput(
                "rpc limits must be nonzero and fit in memory size arithmetic".into(),
            ));
        }
        if self.max_pending_stdin_bytes < self.max_stdin_line_bytes {
            return Err(Error::InvalidInput(
                "rpc stdin queue byte budget must hold one maximum-size frame".into(),
            ));
        }
        if self
            .event_capacity
            .checked_mul(std::mem::size_of::<RpcEvent>())
            .is_none_or(|bytes| bytes > isize::MAX as usize)
            || self
                .max_pending_stdin_lines
                .checked_mul(std::mem::size_of::<input::Line>())
                .is_none_or(|bytes| bytes > isize::MAX as usize)
        {
            return Err(Error::InvalidInput(
                "rpc queue capacities exceed allocation size arithmetic".into(),
            ));
        }
        Ok(())
    }
}

/// Which pipe caused a transport error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RpcStream {
    /// The child's standard input.
    Stdin,
    /// The child's standard output.
    Stdout,
    /// The child's standard error.
    Stderr,
}

/// A terminal transport failure, containing metadata only, never payload text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RpcTransportError {
    /// A stdout frame exceeded its configured byte limit; it was not delivered or truncated.
    #[error("RPC {stream:?} frame exceeds the {limit}-byte limit")]
    LineTooLong {
        /// Pipe that produced the oversized frame.
        stream: RpcStream,
        /// Maximum raw frame size in bytes.
        limit: usize,
    },
    /// Stdout contained invalid UTF-8 in strict mode. The frame was not delivered.
    #[error("RPC stdout frame contains invalid UTF-8")]
    InvalidUtf8,
    /// An active stdin write exceeded its deadline; the channel is invalidated.
    #[error("RPC stdin write deadline exceeded; emission may be partial")]
    WriteDeadlineExceeded,
    /// A pipe read or write failed.
    #[error("RPC {stream:?} I/O failed: {kind:?}")]
    Io {
        /// Pipe whose operation failed.
        stream: RpcStream,
        /// Operating-system error category; no payload-bearing error string is retained.
        kind: std::io::ErrorKind,
    },
}

/// Stdout text decoding. Framing always assembles raw bytes before decoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum RpcUtf8Mode {
    /// Replace invalid bytes with U+FFFD (the compatibility default).
    #[default]
    Lossy,
    /// Reject invalid UTF-8 with [`RpcTransportError::InvalidUtf8`]. Stderr remains
    /// lossy diagnostic text; it is never a protocol frame.
    Strict,
}

/// Everything [`RpcChild::spawn`] needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcSpawnSpec {
    /// What to run, usually from
    /// [`AgentProfile::rpc_launch`](crate::agent::AgentProfile::rpc_launch). Its
    /// `env_defaults` apply only for keys `env` does not set; explicit launch `env`
    /// entries override host values. Host removals take precedence over both.
    pub launch: RpcLaunch,
    /// The child's working directory; it must be a directory.
    pub cwd: PathBuf,
    /// The child's environment.
    pub env: EnvPolicy,
    /// unix only: the shell that runs the launch (`<shell> -l -i -c 'exec …'`, `<shell> -c`
    /// for a shell named `sh`). `None`: the user's login shell (`shell::login_shell`).
    /// The selected program is used as given; a nonexistent shell fails to spawn.
    pub shell: Option<String>,
    /// First half of the log prefix `[<log_tag>:<log_key>]`, e.g. `agent-rpc`.
    pub log_tag: String,
    /// Second half of the log prefix, e.g. the host's pane or probe key.
    pub log_key: String,
    /// Explicitly opts in to logging payloads, stderr and launch arguments (cut to 400
    /// characters). These may contain credentials or private prompts. Default `false`:
    /// logs contain lifecycle metadata only unless `trace_stderr` is enabled. Stderr
    /// remains available in the exit event regardless of either trace setting.
    pub trace: bool,
    /// Independently opts in to logging stderr at warning level, cut to 400 characters.
    /// Stderr may contain credentials or private prompts. Default `false`. Setting
    /// `trace` to `true` also logs stderr, regardless of this setting. This does not
    /// enable logging of launch arguments or stdin/stdout payloads, or affect the
    /// bounded stderr tail retained in exit events.
    pub trace_stderr: bool,
    /// Finite frame and queue limits, validated before the child starts.
    pub limits: RpcLimits,
    /// Stdout decoding policy. Default: lossily replace invalid UTF-8.
    pub stdout_utf8: RpcUtf8Mode,
}

impl RpcSpawnSpec {
    /// `launch` in `cwd` with the inherited environment, the login shell, the log prefix
    /// `[agent-rpc:<launcher>]` and no trace.
    pub fn new(launch: RpcLaunch, cwd: impl Into<PathBuf>) -> Self {
        let log_key = launch.launcher.clone();
        Self {
            launch,
            cwd: cwd.into(),
            env: EnvPolicy::default(),
            shell: None,
            log_tag: "agent-rpc".into(),
            log_key,
            trace: false,
            trace_stderr: false,
            limits: RpcLimits::default(),
            stdout_utf8: RpcUtf8Mode::default(),
        }
    }
}

/// What an [`RpcChild`] reports on its receiver.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RpcEvent {
    /// One stdout line: trailing `\n` and then `\r` stripped. Invalid UTF-8 is
    /// replaced in the default lossy mode, or rejected in strict mode.
    Line {
        /// The child.
        id: SessionId,
        /// 0 for the child's first line, +1 per line (wrapping).
        seq: u64,
        /// The line.
        line: String,
    },
    /// Stdin failed and is closed; stdout and stderr continue draining. Nonterminal:
    /// subsequent stdout lines and the final `Exit` or terminal `Error` can follow.
    /// Active write receipts are already settled as possibly written and queued writes
    /// as not written; new sends return `Closed`. A child can remain alive indefinitely
    /// after closing stdin; use host deadlines or kill/drop to bound its lifetime.
    /// Concurrent output termination or teardown can suppress this notification.
    StdinError {
        /// The child.
        id: SessionId,
        /// I/O failure category, without payload content.
        kind: std::io::ErrorKind,
    },
    /// The transport failed. The last event: no `Exit` follows, and `send_line` returns
    /// `Closed`. The process may still be running; drop or kill its handle to terminate it.
    /// Oversized stdout is never emitted as a partial `Line`. Oversized stderr is
    /// discarded with a marker in the bounded exit tail and is not a transport error.
    Error {
        /// The child.
        id: SessionId,
        /// Structured failure metadata, without payload content.
        error: RpcTransportError,
    },
    /// stdout ended while the child was not being torn down: it exited (or closed its
    /// stdout) on its own, without a terminal transport failure. Such a failure sends
    /// [`RpcEvent::Error`] instead. A preceding [`RpcEvent::StdinError`] permits this exit.
    /// The last event; never sent once teardown started, so a kill is
    /// never reported as an exit. [`RpcChild::try_wait`] has the exit code (stdout can end
    /// a moment before the exit is visible).
    Exit {
        /// The child.
        id: SessionId,
        /// The last stderr lines (at most 32 lines / 4 KiB), oldest first, joined by `\n`.
        stderr_tail: String,
    },
}

impl RpcEvent {
    /// The child the event belongs to.
    pub fn id(&self) -> SessionId {
        match self {
            Self::Line { id, .. }
            | Self::Exit { id, .. }
            | Self::Error { id, .. }
            | Self::StdinError { id, .. } => *id,
        }
    }
}

#[derive(Clone, Default)]
/// Observation of the owned direct child's actual reap, independent of teardown return.
pub struct RpcReapObservation(Arc<std::sync::atomic::AtomicU8>);

/// Direct-child reap evidence. This does not prove descendant or pipe-thread exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RpcReapStatus {
    /// No successful owned wait has completed yet.
    Pending,
    /// This handle's own OS wait completed successfully.
    Reaped,
    /// An external reaper or OS/thread error prevents positive evidence.
    Unknown,
}

impl RpcReapObservation {
    /// Nonblocking status; never infers success from a PID check.
    pub fn status(&self) -> RpcReapStatus {
        match self.0.load(Ordering::Acquire) {
            1 => RpcReapStatus::Reaped,
            2 => RpcReapStatus::Unknown,
            _ => RpcReapStatus::Pending,
        }
    }
}

/// A child speaking lines over stdio. `Send + Sync`; dropping it — or [`kill`](Self::kill)
/// — tears it down:
///
/// 1. stop input and settle outstanding write receipts; stop publishing (no `Exit`),
///    release a reader blocked on a full receiver, and disconnect it once drained;
/// 2. kill the tree, unless the child was reaped outside this handle: unix `SIGKILL` to its
///    process group — also after a confirmed exit, which is safe because the pid is still
///    unreaped (see [`try_wait`](Self::try_wait)) and reaches what the child left in its
///    group; Windows `taskkill /T /F`, skipped once the exit is confirmed;
/// 3. kill and reap the child, bounded at ~2 s — then a detached
///    `agent-link-rpc-reaper-<id>` thread finishes and a `warn` is logged; `child reaped:
///    exit code …` is logged;
/// 4. drop the input admission handle and detach the threads (never joined).
///
/// The dropping thread blocks for this — **not free on Windows**: one `taskkill` round trip
/// (~100–200 ms). Drop it outside your own locks, and off a UI thread on Windows.
pub struct RpcChild {
    id: SessionId,
    pid: u32,
    tag: String,
    publisher: Arc<Publisher>,
    /// Enqueue side of the bounded writer queue: `send_line` never waits for capacity.
    /// Dropped in teardown.
    writer: Mutex<Option<Input>>,
    child: Mutex<ChildState>,
    /// Dropped (detached) in teardown, never joined: each may be blocked on a pipe.
    threads: Mutex<Vec<JoinHandle<()>>>,
    torn_down: AtomicBool,
    // Files outlive process teardown; cleanup never runs under the child/writer mutex.
    #[cfg(feature = "tools")]
    _tools: Option<crate::tools::ToolAttachment>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl RpcChild {
    /// Retains actual owned-reap observation across kill or Drop.
    pub fn reap_observation(&self) -> RpcReapObservation {
        lock(&self.child).reap_status.clone()
    }

    /// Starts `spec`'s child and returns it with its bounded event receiver:
    /// **drain it or drop it** — a full one stops the reader and so the child's stdout; a
    /// dropped one discards the lines).
    ///
    /// Errors: everything [`preflight`] refuses, refused the same way before anything
    /// starts; [`Error::Spawn`] when the process (`spawn <launcher> rpc child: …`) or one of
    /// its threads cannot be started — a child already running is then killed and reaped
    /// before the error returns.
    ///
    /// On Unix, success means the login shell started; the shell resolves and executes
    /// the launcher afterward. A missing launcher can therefore produce a successful
    /// spawn followed by [`RpcEvent::Exit`]. Inspect its stderr tail and use
    /// [`Self::try_wait`] to observe the shell's exit status.
    pub fn spawn(spec: RpcSpawnSpec) -> Result<(RpcChild, Receiver<RpcEvent>)> {
        let mut planned = command::plan(&spec)?;
        let launcher = spec.launch.launcher.as_str();
        // Minted before the child exists; a failed spawn skips an id.
        let id = SessionId::next();
        let tag = format!("[{}:{}]", spec.log_tag, spec.log_key);
        #[cfg(not(unix))]
        if spec.trace {
            log::info!(
                "{tag} resolved {launcher} launcher: {}",
                planned.resolved.display()
            );
        }
        let cmd = &mut planned.command;
        let isolation = if cfg!(unix) { "setsid" } else { "process-tree" };
        log::info!(
            "{tag} spawning {launcher}: {} argument(s) (isolation={isolation}, trace={})",
            spec.launch.args.len(),
            spec.trace,
        );
        if spec.trace {
            log::info!(
                "{tag} launch details: {}",
                pump::trace_snip(&format!(
                    "program={:?} args={:?} cwd={:?}",
                    cmd.get_program(),
                    cmd.get_args().collect::<Vec<_>>(),
                    spec.cwd
                ))
            );
        }
        let child = cmd
            .spawn()
            .map_err(|e| Error::Spawn(format!("spawn {launcher} rpc child: {e}")))?;
        log::info!("{tag} {launcher} child spawned: pid={}", child.id());

        // Guarded until every initial pipe thread is up.
        let mut guard = ChildGuard::new(ChildState::new(child, id, tag.clone()));
        let pid = guard.state().pid();
        let (stdin, stdout, stderr) = guard.state().take_stdio(launcher)?;
        let writer_tx = Input::new(spec.limits);
        let (publisher, events) = Publisher::new(spec.limits.event_capacity, writer_tx.clone());
        let threads = start_threads(id, &tag, &publisher, (stdin, stdout, stderr), &spec)
            .inspect_err(|_| {
                // A failed spawn is a teardown: no `Exit` for the child the guard kills.
                publisher.close();
            })?;
        let child = guard.disarm();
        Ok((
            RpcChild {
                id,
                pid,
                tag,
                publisher,
                writer: Mutex::new(Some(writer_tx)),
                child: Mutex::new(child),
                threads: Mutex::new(threads),
                torn_down: AtomicBool::new(false),
                #[cfg(feature = "tools")]
                _tools: spec.launch.tools.clone(),
            },
            events,
        ))
    }

    /// The child's id: from the same process-wide counter as [`Terminal`](crate::Terminal)
    /// session ids (one id space; every event carries it).
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// The child's process id. It stays reserved for this child, exited or not, until
    /// teardown reaps it (unless something else in the process reaps it first).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Queues `line` for the child's stdin; returns at once, the writer thread writes it
    /// plus `\n` and flushes, in call order. [`Error::InvalidInput`] for a line holding a
    /// `\n` (nothing is queued); [`Error::InputTooLarge`] above the frame limit;
    /// [`Error::QueueFull`] while the input budget is exhausted (retry after progress);
    /// [`Error::Closed`] after stdin half-close, teardown, EOF or a transport failure.
    /// No input is queued on an error, and admission never waits for the child to read.
    /// Once input is closed, `Closed` takes precedence over input validation errors.
    /// A later stdin I/O failure emits nonterminal [`RpcEvent::StdinError`] while output
    /// continues draining; use tracked writes for authoritative per-write outcomes.
    ///
    /// unix: a write to the stdin of a child that is gone raises `SIGPIPE`, and the write
    /// fails only while the host process ignores that signal — a Rust `main` does by
    /// default. A host that restores its default action (or embeds the crate under a
    /// non-Rust `main`) is killed by it: ignore `SIGPIPE` there.
    pub fn send_line(&self, line: &str) -> Result<()> {
        if self.publisher.stopped() {
            return Err(Error::Closed);
        }
        let writer = lock(&self.writer);
        let tx = writer.as_ref().ok_or(Error::Closed)?;
        tx.send(line)
    }

    /// Admits one line and returns an independent transport receipt. Admission errors
    /// match [`send_line`](Self::send_line); an already elapsed `deadline` returns
    /// [`Error::InvalidInput`] while input is open (`Closed` takes precedence otherwise).
    /// The first tracked admission starts a dedicated deadline worker;
    /// [`Error::Spawn`] if it cannot be created. Every error refuses the whole
    /// line without a receipt; a later call can retry worker creation.
    ///
    /// `deadline` is absolute and covers queueing, writing and flushing. Queued expiry
    /// removes the line without emitting it. Expiry after the writer claims it reports
    /// possible emission and invalidates stdin: no subsequent frame can be written.
    /// OS pipe writes cannot always be interrupted; bytes already in flight may still
    /// arrive after expiry. Drop or kill the child to terminate it. No automatic retries.
    /// Completion time is sampled immediately after write/flush; any outcome already
    /// settled by the deadline worker or shutdown remains final.
    ///
    /// Receipts distinguish flush completion from provider acceptance. The transport
    /// retains only admitted receipts within the stdin item budget, including active
    /// writes; host-retained finished receipts are the host's responsibility. This
    /// call never waits for child I/O. Dropping a receipt does not cancel its write.
    pub fn send_line_tracked(&self, line: &str, deadline: Instant) -> Result<RpcWriteReceipt> {
        if self.publisher.stopped() {
            return Err(Error::Closed);
        }
        let writer = lock(&self.writer);
        let tx = writer.as_ref().ok_or(Error::Closed)?;
        tx.send_tracked(line, deadline, || {
            // The deadline worker is detached like the pipe pumps. Input admission
            // starts at most one and teardown/EOF wakes it independently of I/O.
            pump::spawn_deadlines(self.id, self.tag.clone(), Arc::clone(&self.publisher)).map(drop)
        })
    }

    /// Requests an ordered stdin half-close without waiting: reject later sends, finish
    /// accepted lines in FIFO order, then drop the stdin pipe. Stdout/stderr continue
    /// draining. Repeated requests succeed until transport stop; after EOF, failure or
    /// teardown this returns [`Error::Closed`]. No queue slot is needed for the request.
    ///
    /// This acknowledges the request, not observed EOF at the child. A blocked accepted
    /// write can delay physical pipe closure indefinitely, even after a deadline has
    /// invalidated input. A write failure aborts the remaining queue. Hard kill/drop
    /// remain independent and bounded.
    pub fn close_stdin(&self) -> Result<()> {
        if self.publisher.stopped() {
            return Err(Error::Closed);
        }
        lock(&self.writer)
            .as_ref()
            .ok_or(Error::Closed)?
            .half_close()
    }

    /// Whether the child has exited; never blocks. `Ok(Some(code))`: exited, `code` `None`
    /// for a signal (or when something else in the process reaped the child, so the code
    /// is unknown); sticky. `Ok(None)`: running. `Err`: liveness cannot be read — teardown
    /// still kills the tree.
    ///
    /// Never reaps: on unix the exit is read with `waitid(WNOWAIT)`, so the pid and its
    /// process group stay reserved until teardown, which can then still kill the group —
    /// everything the child left in it — without risking a reused pid. Dropping the
    /// handle after a confirmed exit is therefore always safe. On Windows the open process
    /// handle keeps the pid the same way.
    pub fn try_wait(&self) -> std::io::Result<Option<Option<i32>>> {
        lock(&self.child).try_wait()
    }

    /// Tears the child down now (what dropping it does) and returns its exit code: `None`
    /// when a signal ended it (unix: also the kill itself) or it could not be reaped within
    /// the bound. Windows reports a killed child's terminate code (1).
    pub fn kill(self) -> Option<i32> {
        self.teardown()
    }

    /// Idempotent; the steps are on [`RpcChild`].
    fn teardown(&self) -> Option<i32> {
        if self.torn_down.swap(true, Ordering::AcqRel) {
            return lock(&self.child).exit_code();
        }
        self.publisher.close();
        let code = lock(&self.child).kill_and_reap();
        drop(lock(&self.writer).take());
        drop(std::mem::take(&mut *lock(&self.threads)));
        code
    }
}

impl Drop for RpcChild {
    fn drop(&mut self) {
        self.teardown();
    }
}

impl std::fmt::Debug for RpcChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcChild")
            .field("id", &self.id)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

/// The initial stderr, reader and writer threads. Deadline tracking starts on demand.
fn start_threads(
    id: SessionId,
    tag: &str,
    publisher: &Arc<Publisher>,
    (stdin, stdout, stderr): (ChildStdin, ChildStdout, ChildStderr),
    spec: &RpcSpawnSpec,
) -> Result<Vec<JoinHandle<()>>> {
    let ring = StderrRing::default();
    // Dropped by the stderr thread at its EOF: the tail is then complete for the `Exit`.
    let (stderr_done_tx, stderr_done_rx) = bounded::<()>(0);
    let config = pump::Config {
        id,
        tag: tag.into(),
        publisher: Arc::clone(publisher),
        trace: spec.trace,
        trace_stderr: spec.trace || spec.trace_stderr,
        limits: spec.limits,
        stdout_utf8: spec.stdout_utf8,
    };
    let stderr = pump::spawn_stderr(config.clone(), stderr, ring.clone(), stderr_done_tx)?;
    let reader = pump::spawn_reader(config.clone(), stdout, ring, stderr_done_rx)?;
    let writer = pump::spawn_writer(config, stdin)?;
    Ok(vec![stderr, reader, writer])
}

/// Frames one raw stdout/stderr line: the trailing `\n`, then a trailing `\r`, stripped
/// (each independently, so a lone `\r` at EOF goes too), invalid UTF-8 replaced by U+FFFD.
pub fn frame_line(raw: &[u8]) -> String {
    strip_line_ending(String::from_utf8_lossy(raw).into_owned())
}

fn strip_line_ending(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
    }
    if s.ends_with('\r') {
        s.pop();
    }
    s
}

/// Validates `spec` and builds its platform-specific command without starting a process.
/// [`RpcChild::spawn`] performs the same validation before attempting to start the child.
///
/// Returns [`Error::InvalidInput`] for invalid frame/queue limits, an empty launcher, a cwd
/// that is not a directory, and on Unix an empty shell or a launcher/argument the POSIX
/// quoting refuses (a control character). On Windows, also resolves the launcher on the
/// child's effective `PATH`, returning [`Error::NotFound`] if it cannot be found.
///
/// On Unix, this does not check that the shell or launcher exists or can execute. The
/// launcher is resolved by the login shell after it starts. A missing launcher can pass
/// preflight and [`RpcChild::spawn`], then fail with [`RpcEvent::Exit`].
///
/// Call before tearing down a child being replaced so a validation error leaves that
/// child running. Success does not reserve resources or guarantee spawn or agent startup
/// on either platform. Nothing is logged.
pub fn preflight(spec: &RpcSpawnSpec) -> Result<()> {
    command::plan(spec).map(|_| ())
}

/// Calls `f` with every event from `rx`, in order, on a new thread (`agent-link-rpc-forward`),
/// until the receiver disconnects: after the child's `Exit`, a transport `Error`, or its
/// teardown, once drained.
/// A slow `f` back-pressures the child like any undrained receiver. Never join the
/// returned handle from a thread `f` waits on.
pub fn forward_rpc(
    rx: Receiver<RpcEvent>,
    mut f: impl FnMut(RpcEvent) + Send + 'static,
) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("agent-link-rpc-forward".into())
        .spawn(move || {
            for event in rx {
                f(event);
            }
        })
        .map_err(|e| Error::Spawn(format!("spawn rpc forward thread: {e}")))
}
