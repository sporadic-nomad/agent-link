//! [`Terminal`]: spawn, the reader and writer threads, the session calls, and teardown.

use std::io::{Read, Write};
#[cfg(feature = "agent")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use crossbeam_channel::Receiver;
use portable_pty::{CommandBuilder, MasterPty, native_pty_system};

#[cfg(feature = "agent")]
use super::ForegroundOwner;
use super::bus::EventBus;
use super::child::{ChildGuard, ChildSlot};
use super::foreground::{self, ChildKind};
use super::input::{Input, InputReceiver};
use super::log::{Replay, SessionLog};
use super::model::ModelSlot;
use super::{Event, ExitInfo, Foreground, ProgramId, PtySize, SessionId, SpawnMode, SpawnSpec};
use crate::error::{Error, Result};
#[cfg(windows)]
use crate::proc::job::Job;
#[cfg(feature = "screen")]
use crate::screen::ScreenSnapshot;

/// Bytes per PTY read; one read is one [`Event::Output`] at most.
const READ_CHUNK: usize = 8 * 1024;

/// A child running on a pseudo-terminal. Dropping it — or [`kill`](Self::kill) — tears
/// the session down: the terminal's foreground job and the child's process group are
/// killed (Windows: the child's Job Object, or its process tree) and the child reaped. A
/// background job the shell started in its own group (`cmd &` in an interactive unix
/// shell) is not killed. On Windows the job holds everything started from the session,
/// so a detached program launched from it (an editor opened with `code .`, a browser a
/// sign-in flow opened) is killed too.
///
/// - Calls never wait on the child: output is read and input written on the session's own
///   threads (`agent-link-reader-<id>`, `agent-link-writer-<id>`). Teardown detaches them,
///   never joins them.
/// - Output arrives as [`Event`]s on bounded receivers: [`spawn`](Self::spawn) returns
///   the first, [`subscribe`](Self::subscribe) and [`attach`](Self::attach) add more. **A
///   receiver you do not drain eventually stalls the session; drop receivers you do not
///   read.**
/// - The session log ([`SessionLogConfig`](super::SessionLogConfig)) records the output
///   for [`attach`](Self::attach): a late receiver gets what it missed as a [`Replay`].
/// - With a screen model (feature `screen`, [`ScreenConfig`](super::ScreenConfig)) each
///   chunk is processed by the model before its `Output` is published, so `screen()` is
///   at or ahead of any receiver; the model's signals follow as events, and it may answer
///   the child's terminal queries.
/// - Teardown blocks the dropping thread for the kill and a reap bounded at about 2 s;
///   on Windows also a `taskkill` round trip without a Job Object, or with one a process
///   snapshot and a job query (plus `taskkill` when the shell has a child outside the job
///   or that check fails). Drop a `Terminal` outside your own locks.
/// - A line typed into a shell session ends with `\n` on unix and `\r` for `cmd.exe`.
///
/// ```no_run
/// use agent_link::{Event, PtySize, SpawnSpec, Terminal};
///
/// let spec = SpawnSpec::shell(std::env::temp_dir(), PtySize { rows: 24, cols: 80 });
/// let (term, events) = Terminal::spawn(spec)?;
/// // The echoed command line holds `hel%s`; only the command's output holds `hello`.
/// term.write(b"printf 'hel%s\\n' lo\n")?;
/// for event in &events {
///     if let Event::Output { bytes, .. } = event {
///         if String::from_utf8_lossy(&bytes).contains("hello") {
///             break;
///         }
///     }
/// }
/// drop(term); // kills the shell and everything it started, then reaps it
/// # Ok::<(), agent_link::Error>(())
/// ```
pub struct Terminal {
    inner: Arc<Inner>,
}

/// A cloneable, crate-internal reference to a session for controller threads. It never
/// tears the session down; once the owning [`Terminal`] is gone its calls answer
/// `Err(Closed)` (or a closed receiver, `Unknown`).
#[derive(Clone)]
#[cfg_attr(not(feature = "agent"), allow(dead_code))] // held by the agent controller
pub(crate) struct TerminalHandle {
    inner: Arc<Inner>,
}

struct Inner {
    id: SessionId,
    kind: ChildKind,
    /// The child's pid from spawn (the shell in shell mode).
    pid: Option<u32>,
    /// Windows: the child's Job Object (the foreground heuristic reads its members; the
    /// child slot holds it for the teardown kill).
    #[cfg(windows)]
    job: Option<Arc<Job>>,
    bus: Arc<EventBus>,
    /// Shared with the reader: set first thing in teardown so nothing read afterwards is
    /// published and no `Exit` follows the teardown.
    stop: Arc<AtomicBool>,
    torn_down: AtomicBool,
    /// Lines the host submitted to the shell, once an agent controller asked for them.
    #[cfg(feature = "agent")]
    shell_lines: ShellLines,
    /// Kept for resize and the foreground query; dropped in teardown after the kill.
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    child: Mutex<ChildSlot>,
    /// Enqueue side of the bounded writer channel. `write` sends here — a
    /// non-blocking enqueue — and the writer thread, which owns the PTY writer, does the
    /// blocking `write_all` + `flush` in arrival order. So a caller never blocks on a child
    /// that stopped reading, and writes from one thread keep their order. Dropped in
    /// teardown: the idle writer thread, parked in `recv`, then wakes and exits.
    writer: Mutex<Option<Input>>,
    /// The screen model, shared with the reader. A model that answers queries holds a
    /// writer sender too, so teardown empties it with the writer.
    screen: Arc<ModelSlot>,
    /// Reader and writer threads. Dropped (detached) in teardown, never joined: either may
    /// be parked in blocking IO, and each exits on its own once the PTY closes.
    threads: Mutex<Vec<JoinHandle<()>>>,
}

/// Lines the host submitted while no program owned the terminal: each may have started one
/// (the agent controller then cannot tell that program from the agent its launch line
/// started). Off until a controller turns it on: counting reads the foreground per write.
#[cfg(feature = "agent")]
#[derive(Default)]
struct ShellLines {
    on: AtomicBool,
    count: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Seams [`Terminal::spawn_with`] takes so tests can force a fallback path. Empty off
/// Windows.
#[cfg_attr(not(windows), derive(Default))]
pub(crate) struct SpawnHooks {
    /// Creates the Job Object the child joins.
    #[cfg(windows)]
    pub(crate) make_job: fn() -> std::io::Result<Job>,
}

#[cfg(windows)]
impl Default for SpawnHooks {
    fn default() -> Self {
        Self {
            make_job: Job::create,
        }
    }
}

impl Terminal {
    /// Starts `spec`'s child on a new PTY and returns the session with its first event
    /// receiver, subscribed before the first byte is read (nothing is missed).
    ///
    /// On Windows the child joins a new Job Object; when that fails (one `warn`), the
    /// session runs without one and teardown uses `taskkill`.
    ///
    /// Errors: [`Error::InvalidInput`] for a `cwd` that is not a directory (portable-pty
    /// would silently start the child in `$HOME`), an empty size, an empty direct program,
    /// invalid input limits, or a memory log cap of 0; [`Error::Io`] when the file log cannot
    /// be opened (`open session log <path>: …`, before anything starts) or emptied once the child started
    /// (`truncate session log <path>: …`); [`Error::Spawn`] when a step fails, its message
    /// starting with the step (`openpty: …`, `spawn shell: …`, `spawn <program>: …`). A
    /// child started before a later step failed is killed and reaped before the error
    /// returns. macOS: `openpty` called from several threads at the same moment now and then
    /// fails (`openpty: … Unknown error: -6`) — serialise concurrent spawns or retry it.
    pub fn spawn(spec: SpawnSpec) -> Result<(Terminal, Receiver<Event>)> {
        Self::spawn_with(spec, SpawnHooks::default())
    }

    /// [`spawn`](Self::spawn) with test seams.
    pub(crate) fn spawn_with(
        spec: SpawnSpec,
        hooks: SpawnHooks,
    ) -> Result<(Terminal, Receiver<Event>)> {
        let (term, events, _) = Self::spawn_inner(spec, hooks, false)?;
        Ok((term, events))
    }

    /// [`spawn`](Self::spawn) plus the agent controller's bounded receiver, subscribed
    /// before the first read and served last without blocking publication (see `bus`).
    #[cfg(feature = "agent")]
    pub(crate) fn spawn_watched(
        spec: SpawnSpec,
    ) -> Result<(Terminal, Receiver<Event>, Receiver<Event>)> {
        let (term, events, watch) = Self::spawn_inner(spec, SpawnHooks::default(), true)?;
        let watch = watch.ok_or_else(|| Error::Spawn("no controller receiver".into()))?;
        Ok((term, events, watch))
    }

    fn spawn_inner(
        spec: SpawnSpec,
        hooks: SpawnHooks,
        watch: bool,
    ) -> Result<(Terminal, Receiver<Event>, Option<Receiver<Event>>)> {
        #[cfg(not(windows))]
        let SpawnHooks {} = hooks;
        validate(&spec)?;
        let mut log = SessionLog::open(&spec.log)?;
        let (cmd, kind, what) = command(&spec);
        // Minted before the child exists, so every log line of the spawn is tagged; a
        // failed spawn just skips an id.
        let id = SessionId::next();
        let pair = native_pty_system()
            .openpty(spec.size.to_portable())
            .map_err(|e| Error::Spawn(format!("openpty: {e:#}")))?;
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| Error::Spawn(format!("spawn {what}: {e:#}")))?;
        // The host's copy of the slave must go, or the output never ends when the child
        // does.
        drop(pair.slave);
        let master = pair.master;
        // Declared after `master`, so on an early return it is dropped first: the child
        // is killed before the PTY closes, as in teardown.
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut guard = ChildGuard::new(child, id);
        #[cfg(windows)]
        let job = guard.join_job(hooks.make_job);
        // Only now that the child runs: a failed spawn must not wipe an earlier log. Nothing
        // is read before the bus exists, so the log still sees every chunk.
        log.begin()?;
        let reader = master
            .try_clone_reader()
            .map_err(|e| Error::Spawn(format!("clone pty reader: {e:#}")))?;
        let writer = pty_writer(master.as_ref())?;

        let bus = Arc::new(EventBus::new(id, log, spec.size));
        let events = bus.subscribe();
        let watched = watch.then(|| bus.watch());
        let stop = Arc::new(AtomicBool::new(false));
        let (writer_tx, writer_rx) = Input::new(spec.input_limits);
        let screen = Arc::new(ModelSlot::new(&spec.screen, id, spec.size, &writer_tx));
        let writer_thread = spawn_writer(id, writer, writer_rx)?;
        let reader_thread = spawn_reader(
            id,
            reader,
            Arc::clone(&bus),
            Arc::clone(&screen),
            Arc::clone(&stop),
        )?;

        let child = guard.disarm();
        let pid = child.pid();
        log::debug!(
            "[agent-link:{id}] spawned {what} (pid {}) in {}",
            pid.map_or_else(|| "?".into(), |p| p.to_string()),
            spec.cwd.display()
        );
        let inner = Inner {
            id,
            kind,
            pid,
            #[cfg(windows)]
            job,
            bus,
            stop,
            torn_down: AtomicBool::new(false),
            #[cfg(feature = "agent")]
            shell_lines: ShellLines::default(),
            master: Mutex::new(Some(master)),
            child: Mutex::new(child),
            writer: Mutex::new(Some(writer_tx)),
            screen,
            threads: Mutex::new(vec![reader_thread, writer_thread]),
        };
        Ok((
            Terminal {
                inner: Arc::new(inner),
            },
            events,
            watched,
        ))
    }

    /// The session's id (also in every [`Event`] and log line).
    pub fn id(&self) -> SessionId {
        self.inner.id
    }

    /// The child's process id (the shell in shell mode), as spawned. It stays reserved for
    /// this child, exited or not, until teardown reaps it, unless the host reaps it first.
    pub fn pid(&self) -> Option<u32> {
        self.inner.pid
    }

    /// Queues `bytes` for the child; returns at once, the session's writer thread delivers
    /// them in call order. The shared [`InputLimits`](super::InputLimits) include the
    /// write in flight. Oversized writes return [`Error::InputTooLarge`]; exhausted bytes
    /// or chunks return [`Error::QueueFull`], without copying or accepting any input.
    /// `Err(Closed)` once the writer has stopped (a failed PTY write:
    /// the terminal is gone). Under an `AgentTerminal` (feature `agent`) that
    /// launches its agent into a shell, a write holding a line end (CR or LF) first reads
    /// the foreground (one `ioctl` on unix, a process snapshot on Windows): a line the shell
    /// gets may start a program the controller must not take for its agent.
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let writer = lock(&self.inner.writer);
        let tx = writer.as_ref().ok_or(Error::Closed)?;
        tx.send_with(bytes, || {
            #[cfg(feature = "agent")]
            self.inner.note_host_line(bytes);
        })
    }

    /// Resizes the terminal; the child gets `SIGWINCH` (unix). The screen model follows in
    /// the same step: `screen()` never shows the new size with the old layout.
    pub fn resize(&self, size: PtySize) -> Result<()> {
        self.inner.resize(size)
    }

    /// The screen model's view now; `None` when the session keeps no model
    /// (`ScreenConfig::enabled` off). It has processed every chunk any receiver got — and
    /// possibly one more. Briefly holds the model's lock, which the reader takes per chunk.
    #[cfg(feature = "screen")]
    pub fn screen(&self) -> Option<ScreenSnapshot> {
        self.inner.screen.snapshot()
    }

    /// Bytes that redraw the model's screen on a blank terminal of the same size (for a
    /// renderer attached late); `None` without a model.
    #[cfg(feature = "screen")]
    pub fn repaint_stream(&self) -> Option<Vec<u8>> {
        self.inner.screen.repaint_stream()
    }

    /// Who owns the terminal's foreground right now — see [`Foreground`]. Exact on unix;
    /// on Windows a heuristic from the child's process tree in a process snapshot (only
    /// Job Object members when there is a job): anything running below the shell means
    /// `Agent`. A detached program whose launcher exited (an editor opened with `code .`)
    /// is not below it and does not count.
    pub fn foreground(&self) -> Foreground {
        self.inner.foreground()
    }

    /// A new receiver for every later event; once the child's [`Event::Exit`] was sent it
    /// starts with that `Exit`. **Drain it or drop it**: once it holds 1 024 undelivered
    /// events (16 when added after the `Exit`) the session stops reading output, or an
    /// agent controller stops publishing, until it has room.
    pub fn subscribe(&self) -> Receiver<Event> {
        self.inner.bus.subscribe()
    }

    /// The output recorded so far plus a receiver for every later event, with no gap and
    /// no overlap: the [`Replay`] ends right before the first `Output` the receiver gets.
    /// With the log off it is [`Replay::None`]. After the child's [`Event::Exit`] the
    /// replay is everything still recorded and the receiver holds that `Exit`. Same draining rule
    /// as [`subscribe`](Self::subscribe).
    ///
    /// Briefly holds the session's bus lock, which the reader also takes per chunk: a
    /// memory log is copied under it (milliseconds for the 32 MiB default).
    pub fn attach(&self) -> (Replay, Receiver<Event>) {
        self.inner.bus.attach()
    }

    /// Recover from an optional byte cursor and attach to subsequent events atomically.
    /// `None` requests output from spawn; a cursor requests bytes at and after its offset.
    /// Eviction, disabled recording and file failures produce explicit missing ranges in
    /// the snapshot. Cross-session and future cursors return typed [`super::ReplayError`]s
    /// without registering a receiver. Natural EOF still allows replay and a sticky exit;
    /// teardown returns `ReplayError::Closed`.
    ///
    /// Geometry and the optional display view are captured at the exact raw output
    /// boundary. The view is not a terminal parser checkpoint; see [`super::RecoverySnapshot`].
    /// Session IDs are process-local generations, so remote hosts must additionally scope
    /// cursors to their server incarnation. No provider resume or durable recovery is implied.
    ///
    /// Like [`attach`](Self::attach), copies retained memory under the bus lock and returns
    /// a back-pressuring receiver: drain or drop it. Also waits for in-progress screen
    /// processing or resize, but never for subscriber capacity. File replay is a descriptor
    /// of a byte range, not a file read under the lock. No additional history is allocated.
    pub fn attach_recovery(
        &self,
        from: Option<super::ReplayCursor>,
    ) -> Result<(super::RecoverySnapshot, Receiver<Event>), super::ReplayError> {
        self.inner.bus.attach_recovery(from, &self.inner.screen)
    }

    /// Attach at the current output fence without copying retained replay.
    ///
    /// Available with `observers`. Returns geometry and optional display at the same
    /// boundary as `attach_recovery`, but requests the boundary itself: replay and gaps
    /// are empty. Older bytes may still exist in the core log. The receiver must be
    /// continuously drained or dropped; its ordinary core backpressure is unchanged.
    #[cfg(feature = "observers")]
    pub fn attach_live_recovery(
        &self,
    ) -> Result<(super::RecoverySnapshot, Receiver<Event>), super::ReplayError> {
        self.inner.bus.attach_live_recovery(&self.inner.screen)
    }

    /// Attach at the current geometry/output fence without copying replay or display.
    ///
    /// Available with `observers`. Like [`attach_live_recovery`](Self::attach_live_recovery),
    /// returns empty replay/gaps and a live core receiver, but `display` is always `None`
    /// even with an enabled screen model. Intended for bounded geometry-only transports:
    /// no terminal rows or title strings are cloned. Still waits for in-progress core
    /// screen/log processing or resize; never waits for subscriber capacity. Drain or
    /// drop the receiver. Returns [`super::ReplayError::Closed`] after teardown.
    #[cfg(feature = "observers")]
    pub fn attach_live_recovery_metadata(
        &self,
    ) -> Result<(super::RecoverySnapshot, Receiver<Event>), super::ReplayError> {
        self.inner
            .bus
            .attach_live_recovery_metadata(&self.inner.screen)
    }

    /// Writes Ctrl-C (`\x03`); the terminal turns it into `SIGINT` for the foreground job.
    pub fn interrupt(&self) -> Result<()> {
        self.write(b"\x03")
    }

    /// The child's exit if it has exited; never blocks. Once `Some`, always `Some`. On unix
    /// the child is not reaped here but in teardown, so teardown can still kill what it
    /// left in its process group. A child already reaped by the host is reported with
    /// unknown status and is not signalled during teardown. Hosts that reap children
    /// concurrently must coordinate their reaper with teardown: the ownership check and
    /// PID-based signals cannot be atomic.
    pub fn try_wait(&self) -> Option<ExitInfo> {
        self.inner.try_wait()
    }

    /// Tears the session down now (what dropping it does) and returns how the child ended.
    pub fn kill(self) -> ExitInfo {
        self.inner.teardown()
    }

    /// A crate-internal reference for controller threads.
    #[cfg_attr(not(feature = "agent"), allow(dead_code))] // used by the agent controller
    pub(crate) fn handle(&self) -> TerminalHandle {
        TerminalHandle {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.inner.teardown();
    }
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminal")
            .field("id", &self.inner.id)
            .field("pid", &self.inner.pid)
            .finish_non_exhaustive()
    }
}

#[allow(dead_code)] // the agent controller and its delivery jobs use a subset each
impl TerminalHandle {
    pub(crate) fn id(&self) -> SessionId {
        self.inner.id
    }

    pub(crate) fn write(&self, bytes: &[u8]) -> Result<()> {
        self.inner.write(bytes)
    }

    pub(crate) fn resize(&self, size: PtySize) -> Result<()> {
        self.inner.resize(size)
    }

    /// `None` once the session is torn down.
    #[cfg(feature = "screen")]
    pub(crate) fn screen(&self) -> Option<ScreenSnapshot> {
        self.inner.screen.snapshot()
    }

    /// Output chunks read, including those waiting for publication or rendering. Counted
    /// before those waits: a [`screen`](Self::screen) read before this count holds no chunk
    /// past it. The `n`-th `Output` a receiver subscribed before the first read gets is
    /// chunk `n` (from 1).
    #[cfg(feature = "agent")]
    pub(crate) fn output_chunks(&self) -> u64 {
        self.inner.screen.fed()
    }

    /// Shared cancellation flag: the monitor missed an event and cannot automate safely.
    #[cfg(feature = "agent")]
    pub(crate) fn monitor_overflow(&self) -> Arc<AtomicBool> {
        self.inner.bus.watch_overflow()
    }

    #[cfg(feature = "agent")]
    pub(crate) fn closed_signal(&self) -> Receiver<()> {
        self.inner.bus.closed_signal()
    }

    #[cfg(feature = "agent")]
    pub(crate) fn screen_observation(&self) -> Option<(ScreenSnapshot, u64, u64)> {
        self.inner.screen.observation()
    }

    #[cfg(feature = "agent")]
    pub(crate) fn screen_observation_current(&self, rendered: u64, revision: u64) -> bool {
        self.inner.screen.observation_current(rendered, revision)
    }

    /// The number of the last chunk after which a synchronized-output frame was still open
    /// (0: none yet).
    #[cfg(feature = "agent")]
    pub(crate) fn frame_open_after(&self) -> u64 {
        self.inner.screen.open_after()
    }

    /// The number of the last chunk in which the child turned bracketed paste on
    /// (`ESC[?2004h`; 0: none yet), numbered like [`output_chunks`](Self::output_chunks).
    #[cfg(feature = "agent")]
    pub(crate) fn paste_on_at(&self) -> u64 {
        self.inner.screen.paste_on_at()
    }

    pub(crate) fn foreground(&self) -> Foreground {
        self.inner.foreground()
    }

    /// The foreground and the program it names ([`ProgramId`]: not for the shell, an
    /// unknown owner, or a Windows shell running several programs).
    pub(crate) fn foreground_program(&self) -> (Foreground, Option<ProgramId>) {
        self.inner.foreground_program()
    }

    /// Counts the lines the host submits to the shell from now on ([`shell_lines`](Self::shell_lines)).
    #[cfg(feature = "agent")]
    pub(crate) fn count_shell_lines(&self) {
        self.inner.shell_lines.on.store(true, Ordering::SeqCst);
    }

    /// Lines the host wrote ([`Terminal::write`], a CR or LF in the write) while no program
    /// owned the terminal — the shell, or an owner it could not tell — since
    /// [`count_shell_lines`](Self::count_shell_lines). Each is counted before it is queued:
    /// read after a foreground that shows a program such a line started, the count holds it.
    #[cfg(feature = "agent")]
    pub(crate) fn shell_lines(&self) -> u64 {
        self.inner.shell_lines.count.load(Ordering::SeqCst)
    }

    pub(crate) fn subscribe(&self) -> Receiver<Event> {
        self.inner.bus.subscribe()
    }

    /// Publishes a controller event to every receiver; blocks while one is full (only crate
    /// threads call it). Dropped once the session is torn down. The controller's own
    /// receiver gets it too (and ignores it).
    pub(crate) fn publish(&self, event: Event) {
        self.inner.bus.publish(event);
    }

    /// Stops publishing and releases blocked publishers, offering final controller
    /// events without waiting for subscriber capacity. The child is killed by teardown.
    pub(crate) fn close_events_with_final(&self, events: impl IntoIterator<Item = Event>) {
        self.inner.bus.close_with_final(events);
    }

    /// As [`Terminal::try_wait`], but it can wait up to about 2 s while a teardown holds
    /// the child (the bounded reap).
    pub(crate) fn try_wait(&self) -> Option<ExitInfo> {
        self.inner.try_wait()
    }
}

impl Inner {
    fn write(&self, bytes: &[u8]) -> Result<()> {
        let writer = lock(&self.writer);
        let tx = writer.as_ref().ok_or(Error::Closed)?;
        tx.send(bytes)
    }

    /// The PTY and then the model, under the model's lock (taken before the master's).
    fn resize(&self, size: PtySize) -> Result<()> {
        check_size(size)?;
        self.bus.resize(size, || {
            self.screen.resize(size, || {
                let master = lock(&self.master);
                let master = master.as_ref().ok_or(Error::Closed)?;
                master.resize(size.to_portable()).map_err(|e| Error::Io {
                    context: "resize pty".into(),
                    source: e
                        .downcast::<std::io::Error>()
                        .unwrap_or_else(|e| std::io::Error::other(format!("{e:#}"))),
                })
            })
        })
    }

    fn foreground(&self) -> Foreground {
        self.foreground_program().0
    }

    #[cfg(unix)]
    fn foreground_program(&self) -> (Foreground, Option<ProgramId>) {
        let master = lock(&self.master);
        foreground::query(self.kind, master.as_deref(), self.pid)
    }

    /// The master lock is released before the process query: it only tells whether the
    /// session is still open.
    #[cfg(windows)]
    fn foreground_program(&self) -> (Foreground, Option<ProgramId>) {
        let open = lock(&self.master).is_some();
        foreground::query(self.id, self.kind, open, self.pid, self.job.as_deref())
    }

    #[cfg(not(any(unix, windows)))]
    fn foreground_program(&self) -> (Foreground, Option<ProgramId>) {
        let unknown = Foreground {
            owner: super::ForegroundOwner::Unknown,
            confidence: super::Confidence::Heuristic,
        };
        (unknown, None)
    }

    /// A host write that ends a line while no program owns the terminal (the shell, or an
    /// owner it cannot tell) is counted when counting is on (`ShellLines`) — before it is
    /// queued, so no program it starts runs before the count holds it.
    #[cfg(feature = "agent")]
    fn note_host_line(&self, bytes: &[u8]) {
        let lines = &self.shell_lines;
        if lines.on.load(Ordering::SeqCst)
            && bytes.iter().any(|b| matches!(b, b'\r' | b'\n'))
            && self.foreground().owner != ForegroundOwner::Agent
        {
            lines.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn try_wait(&self) -> Option<ExitInfo> {
        lock(&self.child).try_wait()
    }

    /// Idempotent; the order matters:
    /// 1. stop the reader and close the bus — nothing is published afterwards, a reader
    ///    blocked on a full receiver is released, and every receiver disconnects once
    ///    drained;
    /// 2. kill the tree — skipped once reaped. unix: the foreground job's group, then the
    ///    child's group. Windows: terminate the Job Object (even after a confirmed exit);
    ///    `taskkill` instead without a job, and first when the shell has a child outside
    ///    it — `taskkill` only while the child's exit is unconfirmed;
    /// 3. kill and reap the child (bounded);
    /// 4. drop the writer sender and the screen model (its reply sender), so the writer
    ///    thread exits, then the master;
    /// 5. detach the threads.
    fn teardown(&self) -> ExitInfo {
        if self.torn_down.swap(true, Ordering::AcqRel) {
            return lock(&self.child).exit().unwrap_or_default();
        }
        let id = self.id;
        self.stop.store(true, Ordering::Release);
        self.bus.close();
        let fg = self.foreground_group();
        let exit = lock(&self.child).kill_and_reap(fg);
        drop(lock(&self.writer).take());
        self.screen.clear();
        drop(lock(&self.master).take());
        drop(std::mem::take(&mut *lock(&self.threads)));
        log::debug!("[agent-link:{id}] torn down (exit {:?})", exit.status);
        exit
    }

    /// The terminal's foreground process group, if any.
    #[cfg(unix)]
    fn foreground_group(&self) -> Option<i32> {
        lock(&self.master)
            .as_ref()
            .and_then(|m| m.process_group_leader())
    }

    #[cfg(not(unix))]
    fn foreground_group(&self) -> Option<i32> {
        None
    }
}

/// Refuses a spec portable-pty would run wrongly or not at all.
fn validate(spec: &SpawnSpec) -> Result<()> {
    spec.input_limits.validate()?;
    if !spec.cwd.is_dir() {
        return Err(Error::InvalidInput(format!(
            "cwd {} is not a directory",
            spec.cwd.display()
        )));
    }
    check_size(spec.size)?;
    if let SpawnMode::Direct { program, .. } = &spec.mode {
        if program.as_os_str().is_empty() {
            return Err(Error::InvalidInput("direct mode needs a program".into()));
        }
    }
    Ok(())
}

fn check_size(size: PtySize) -> Result<()> {
    if size.rows == 0 || size.cols == 0 {
        return Err(Error::InvalidInput(format!(
            "pty size must be at least 1x1, got {}x{}",
            size.rows, size.cols
        )));
    }
    Ok(())
}

/// The command for `spec`, its kind, and how spawn errors name it. Environment: the
/// identity first, then the host's policy over it (the host wins).
fn command(spec: &SpawnSpec) -> (CommandBuilder, ChildKind, String) {
    let (mut cmd, kind, what) = match &spec.mode {
        SpawnMode::Shell(shell) => {
            let (program, args) = shell.resolve();
            let mut cmd = CommandBuilder::new(program);
            cmd.args(args);
            (cmd, ChildKind::Shell, "shell".to_string())
        }
        SpawnMode::Direct { program, args } => {
            let mut cmd = CommandBuilder::new(program);
            cmd.args(args);
            (cmd, ChildKind::Direct, program.display().to_string())
        }
    };
    cmd.cwd(&spec.cwd);
    let (set, remove) = spec.identity.env();
    spec.env.apply_over(&mut cmd, |cmd| {
        for (key, val) in &set {
            cmd.env(key, val);
        }
        for key in &remove {
            cmd.env_remove(key);
        }
    });
    (cmd, kind, what)
}

/// The PTY's input side. On unix a duplicate of the master fd, not portable-pty's
/// `take_writer`: that writer types Enter + EOF (VEOF) into the terminal when it is
/// dropped, which ends a shell or confirms a dialog if the writer thread ever exits while
/// something still reads the terminal. A plain duplicate closes silently.
#[cfg(unix)]
fn pty_writer(master: &dyn MasterPty) -> Result<Box<dyn Write + Send>> {
    use std::os::fd::FromRawFd;

    let fd = master.as_raw_fd().ok_or_else(|| {
        Error::Spawn("take pty writer: the pty master has no file descriptor".into())
    })?;
    // SAFETY: `fd` is the master's open descriptor; F_DUPFD_CLOEXEC only creates a new one.
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(Error::Spawn(format!(
            "take pty writer: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `dup` is a fresh descriptor nothing else owns.
    Ok(Box::new(unsafe { std::fs::File::from_raw_fd(dup) }))
}

/// The PTY's input side: the pseudoconsole's input pipe (closing it only closes the pipe).
#[cfg(not(unix))]
fn pty_writer(master: &dyn MasterPty) -> Result<Box<dyn Write + Send>> {
    master
        .take_writer()
        .map_err(|e| Error::Spawn(format!("take pty writer: {e:#}")))
}

/// Writes queued chunks in order until the channel closes (teardown) or a write fails.
fn spawn_writer(
    id: SessionId,
    mut writer: Box<dyn Write + Send>,
    rx: InputReceiver,
) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("agent-link-writer-{id}"))
        .spawn(move || {
            for chunk in &rx.rx {
                if let Err(e) = writer.write_all(&chunk.bytes).and_then(|()| writer.flush()) {
                    log::debug!("[agent-link:{id}] pty write failed, writer stops: {e}");
                    break;
                }
            }
        })
        .map_err(|e| Error::Spawn(format!("spawn pty writer thread: {e}")))
}

/// Publishes every chunk until the output ends; then `Exit`, unless teardown started.
/// Each read advances the delivery fence before any publication lock can block it.
/// Each chunk goes to the screen model first, then out as `Output`, then the events the
/// model derived from it — never with the model's lock held. After teardown it keeps
/// reading and discards: on Windows, closing the pseudo-console can wait until its output
/// pipe is drained.
///
/// The child is not reaped here: reaping at end of output would free its pid before
/// teardown's group kill, which could then hit a reused pid.
fn spawn_reader(
    id: SessionId,
    mut reader: Box<dyn Read + Send>,
    bus: Arc<EventBus>,
    screen: Arc<ModelSlot>,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("agent-link-reader-{id}"))
        .spawn(move || {
            let mut buf = vec![0u8; READ_CHUNK];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        log::debug!("[agent-link:{id}] pty read ended: {e}");
                        break;
                    }
                };
                if stop.load(Ordering::Acquire) {
                    continue;
                }
                let chunk = screen.announce_output();
                let mut derived = Vec::new();
                bus.publish_output_with(buf[..n].to_vec(), || {
                    derived = screen.process_announced(id, chunk, &buf[..n]);
                });
                for event in derived {
                    bus.publish(event);
                }
            }
            if !stop.load(Ordering::Acquire) {
                bus.publish(Event::Exit { id, status: None });
            }
        })
        .map_err(|e| Error::Spawn(format!("spawn pty reader thread: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::EnvPolicy;
    use crate::identity::TerminalIdentity;
    use std::ffi::OsStr;

    fn size() -> PtySize {
        PtySize { rows: 24, cols: 80 }
    }

    #[test]
    fn handles_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Terminal>();
        assert_send_sync::<TerminalHandle>();
    }

    /// A controller publisher can stall the reader before it renders newly read bytes.
    /// Delivery must see that pending output, while recovery still snapshots the last
    /// rendered/logged boundary. This uses the real reader loop and no child timing.
    #[cfg(feature = "agent")]
    #[test]
    fn pending_output_invalidates_the_screen_during_blocked_controller_fanout() {
        use std::time::{Duration, Instant};

        use crate::agent::AgentState;
        use crate::pty::{ScreenConfig, SessionLogConfig};

        const WAIT: Duration = Duration::from_secs(5);
        let id = SessionId(1);
        let log = SessionLog::open(&SessionLogConfig::Memory { cap_bytes: 4096 }).unwrap();
        let bus = Arc::new(EventBus::new(id, log, size()));
        let (input, _input_rx) = Input::new(super::super::InputLimits::default());
        let screen = Arc::new(ModelSlot::new(&ScreenConfig::default(), id, size(), &input));
        // Witness is served before the full host receiver. Seeing the controller event
        // on it proves the non-output publisher owns the publication lock.
        let witness = bus.subscribe();
        let full = bus.subscribe();
        let watch = bus.watch();
        let draft = b"composer: pasted message";
        bus.publish_output_with(draft.to_vec(), || {
            screen.process(id, draft);
        });
        witness.recv_timeout(WAIT).unwrap();
        full.recv_timeout(WAIT).unwrap();
        watch.recv_timeout(WAIT).unwrap();
        let (old_screen, rendered, revision) = screen.observation().unwrap();
        assert!(screen.observation_current(rendered, revision));

        let controller_event = Event::AgentState {
            id,
            state: AgentState::Ready,
            blocking: false,
        };
        for _ in 0..super::super::bus::CAPACITY {
            bus.publish(controller_event.clone());
            witness.recv_timeout(WAIT).unwrap();
            watch.recv_timeout(WAIT).unwrap();
        }
        let publisher = {
            let bus = Arc::clone(&bus);
            let event = controller_event.clone();
            std::thread::spawn(move || bus.publish(event))
        };
        // Unwinding drops the full receiver too, so neither worker remains stranded.
        assert_eq!(witness.recv_timeout(WAIT).unwrap(), controller_event);
        let dialog = b"\r\nDIALOG: do not submit";
        let reader = spawn_reader(
            id,
            Box::new(std::io::Cursor::new(dialog.to_vec())),
            Arc::clone(&bus),
            Arc::clone(&screen),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let deadline = Instant::now() + WAIT;
        while screen.fed() == rendered && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let announced = screen.fed();
        let stale = !screen.observation_current(rendered, revision);
        let before_render = screen.observation().unwrap();
        let (attached_tx, attached_rx) = crossbeam_channel::bounded(1);
        let attacher = {
            let bus = Arc::clone(&bus);
            let screen = Arc::clone(&screen);
            std::thread::spawn(move || {
                let _ = attached_tx.send(bus.attach_recovery(None, &screen));
            })
        };
        let attached = attached_rx.recv_timeout(WAIT);
        // Always release fanout before assertions, including the pre-fix failure path.
        drop(full);
        publisher.join().unwrap();
        reader.join().unwrap();
        attacher.join().unwrap();
        let after = bus.attach_recovery(None, &screen).unwrap().0;
        bus.close();

        assert_eq!(
            announced,
            rendered + 1,
            "already-read output must advance the delivery fence before waiting on publication"
        );
        assert!(
            stale,
            "an old composer must not authorize delivery while a dialog is pending"
        );
        assert_eq!(before_render, (old_screen.clone(), rendered, revision));
        let (snapshot, live) = attached.unwrap().unwrap();
        assert_eq!(snapshot.boundary.offset, draft.len() as u64);
        assert_eq!(snapshot.next_output_seq, 1);
        assert_eq!(snapshot.display.unwrap(), old_screen);
        assert_eq!(snapshot.replay.into_bytes().unwrap(), draft);
        assert_eq!(
            live.recv_timeout(WAIT).unwrap(),
            Event::Output {
                id,
                seq: 1,
                bytes: dialog.to_vec()
            }
        );
        assert!(matches!(
            live.recv_timeout(WAIT).unwrap(),
            Event::Exit { .. }
        ));
        assert_eq!(after.boundary.offset, (draft.len() + dialog.len()) as u64);
        assert_eq!(
            after.replay.into_bytes().unwrap(),
            [draft.as_slice(), dialog.as_slice()].concat()
        );
        assert!(after.display.unwrap().text().contains("DIALOG"));
        let (_, now_rendered, now_revision) = screen.observation().unwrap();
        assert_eq!(
            now_rendered,
            rendered + 1,
            "an announced chunk is counted only once"
        );
        assert!(screen.observation_current(now_rendered, now_revision));
    }

    #[test]
    fn a_cwd_that_is_not_a_directory_is_refused() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let err = Terminal::spawn(SpawnSpec::shell(file.path(), size())).unwrap_err();
        assert!(
            matches!(&err, Error::InvalidInput(m) if m.ends_with("is not a directory")),
            "{err}"
        );
        let missing = file.path().with_extension("missing");
        let err = Terminal::spawn(SpawnSpec::shell(missing, size())).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{err}");
    }

    #[test]
    fn an_empty_size_or_program_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [PtySize { rows: 0, cols: 80 }, PtySize { rows: 24, cols: 0 }] {
            let err = Terminal::spawn(SpawnSpec::shell(dir.path(), bad)).unwrap_err();
            assert!(matches!(err, Error::InvalidInput(_)), "{err}");
        }
        let spec = SpawnSpec::direct("", Vec::<String>::new(), dir.path(), size());
        let err = Terminal::spawn(spec).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{err}");
    }

    #[test]
    fn a_missing_direct_program_names_it_in_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("no-such-program");
        let spec = SpawnSpec::direct(&program, Vec::<String>::new(), dir.path(), size());
        let err = Terminal::spawn(spec).unwrap_err();
        let shown = err.to_string();
        assert!(matches!(err, Error::Spawn(_)), "{shown}");
        assert!(
            shown.starts_with(&format!("spawn {}: ", program.display())),
            "{shown}"
        );
    }

    /// Identity first, then the policy: a policy value beats the identity's, removals of
    /// inherited terminal names stay, and `inherit: false` keeps the identity.
    #[test]
    fn identity_is_applied_under_the_host_policy() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = SpawnSpec::shell(dir.path(), size());
        spec.identity = TerminalIdentity::Custom {
            vars: vec![("TERM".into(), "dumb".into()), ("A".into(), "id".into())],
        };
        spec.env.set("TERM", "vt100").set("FOO", "bar");
        let (cmd, kind, what) = command(&spec);
        assert_eq!((kind, what.as_str()), (ChildKind::Shell, "shell"));
        assert_eq!(cmd.get_env("TERM"), Some(OsStr::new("vt100")));
        assert_eq!(cmd.get_env("A"), Some(OsStr::new("id")));
        assert_eq!(cmd.get_env("FOO"), Some(OsStr::new("bar")));

        spec.identity = TerminalIdentity::Honest;
        spec.env = EnvPolicy::default();
        let (cmd, ..) = command(&spec);
        assert_eq!(cmd.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(cmd.get_env("COLORTERM"), Some(OsStr::new("truecolor")));
        assert_eq!(cmd.get_env("TERM_PROGRAM"), None);

        spec.env = EnvPolicy {
            inherit: false,
            ..EnvPolicy::default()
        };
        let (cmd, ..) = command(&spec);
        assert_eq!(cmd.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(cmd.get_env("HOME"), None, "nothing inherited");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::time::{Duration, Instant};

        // An upper bound only: right after a build (fresh binaries scanned, the parallel PTY
        // tests starting shells) a process start has taken over 10 s on a loaded macOS host
        // (an endpoint-security agent vetting each exec).
        const WAIT: Duration = Duration::from_secs(60);

        fn sh(dir: &std::path::Path) -> SpawnSpec {
            let mut spec = SpawnSpec::shell(dir, size());
            spec.mode = SpawnMode::Shell(crate::shell::ShellSpec {
                program: Some("/bin/sh".into()),
                login: false,
            });
            spec.env.set("PS1", "$ ").set("ENV", "").set("HOME", dir);
            spec
        }

        fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
            let deadline = Instant::now() + WAIT;
            while !ready() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// `Terminal::spawn` under [`crate::pty::test_spawn_lock`].
        fn spawn(spec: SpawnSpec) -> (Terminal, Receiver<Event>) {
            let _spawn = crate::pty::test_spawn_lock();
            Terminal::spawn(spec).unwrap()
        }

        /// After teardown a handle answers "closed", and the reader thread ends (it drops
        /// its bus reference) — no thread or PTY descriptor is left behind. The screen
        /// model, which holds a writer sender to answer queries, is gone too.
        #[test]
        fn a_handle_after_teardown_is_closed_and_the_reader_ends() {
            let dir = tempfile::tempdir().unwrap();
            let (term, events) = spawn(sh(dir.path()));
            let handle = term.handle();
            assert_eq!(handle.id(), term.id());
            #[cfg(feature = "screen")]
            assert!(handle.screen().is_some(), "screen on by default");
            handle.write(b"echo up\n").unwrap();
            let exit = term.kill();
            assert_eq!(exit.status, None, "killed by a signal");
            #[cfg(feature = "screen")]
            assert!(handle.screen().is_none(), "the model was dropped");

            assert!(matches!(handle.write(b"x"), Err(Error::Closed)));
            assert!(matches!(handle.resize(size()), Err(Error::Closed)));
            assert_eq!(
                handle.foreground().owner,
                crate::pty::ForegroundOwner::Unknown
            );
            assert!(handle.subscribe().recv_timeout(WAIT).is_err());
            assert_eq!(handle.try_wait(), Some(exit));
            // The first receiver disconnects once drained.
            let deadline = Instant::now() + WAIT;
            loop {
                match events.recv_deadline(deadline) {
                    Ok(Event::Exit { .. }) => panic!("no Exit after teardown"),
                    Ok(_) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    Err(e) => panic!("receiver did not disconnect: {e}"),
                }
            }
            wait_for("the reader thread to end", || {
                Arc::strong_count(&handle.inner.bus) == 1
            });
        }

        /// The reader counts a chunk before the model processes it. With the host's
        /// receiver full, the chunk the reader is stuck publishing is counted (and on the
        /// screen) but has not reached the controller's receiver, which is served last: the
        /// count — not the screen — tells the controller it is behind.
        #[cfg(feature = "agent")]
        #[test]
        fn the_fed_count_runs_ahead_of_a_stalled_controller() {
            let dir = tempfile::tempdir().unwrap();
            let (term, events, watch) = {
                let _spawn = crate::pty::test_spawn_lock();
                Terminal::spawn_watched(sh(dir.path())).unwrap()
            };
            let handle = term.handle();
            // Far more chunks than the host's receiver holds.
            handle
                .write(b"yes 0123456789abcdef | head -c 50000000\n")
                .unwrap();
            wait_for("the host receiver to fill", || {
                events.len() == crate::pty::bus::CAPACITY
            });
            let mut fed = handle.output_chunks();
            wait_for("the reader to stall", || {
                std::thread::sleep(Duration::from_millis(200));
                let now = handle.output_chunks();
                std::mem::replace(&mut fed, now) == now
            });
            let counted = watch
                .try_iter()
                .filter(|e| matches!(e, Event::Output { .. }))
                .count() as u64;
            assert_eq!(fed, counted + 1, "the stuck chunk is fed, not delivered");
            drop(term);
        }

        /// Once on, a host write ending a line counts while the shell owns the terminal, not
        /// while a program does; the crate's own writes never count. The program in front
        /// is named by its group; the shell is not named.
        #[cfg(feature = "agent")]
        #[test]
        fn host_lines_to_the_shell_are_counted() {
            let dir = tempfile::tempdir().unwrap();
            let (term, _events) = spawn(sh(dir.path()));
            let handle = term.handle();
            let shell = || handle.foreground().owner == ForegroundOwner::Shell;
            wait_for("the shell in front", shell);
            // Job control: `cat` gets its own group.
            term.write(b"set -m\n").unwrap();
            assert_eq!(handle.shell_lines(), 0, "off");
            handle.count_shell_lines();
            term.write(b"echo partial").unwrap();
            assert_eq!(handle.shell_lines(), 0, "no line end");
            term.write(b" line\n").unwrap();
            handle.write(b"true\n").unwrap();
            assert_eq!(handle.shell_lines(), 1, "the crate's write does not count");
            assert_eq!(
                handle.foreground_program().1,
                None,
                "the shell is not named"
            );
            term.write(b"cat\r").unwrap();
            wait_for("cat in front", || !shell());
            let (fg, program) = handle.foreground_program();
            assert_eq!(fg.owner, ForegroundOwner::Agent);
            assert!(
                program.is_some_and(|p| Some(p.0) != term.pid()),
                "{program:?}"
            );
            term.write(b"to cat\n").unwrap();
            assert_eq!(handle.shell_lines(), 2, "a line to cat does not count");
            term.write(b"\x04").unwrap();
            wait_for("the shell in front", shell);
        }

        /// A second teardown (drop after `kill`) is a no-op that keeps the first answer.
        #[test]
        fn teardown_is_idempotent() {
            let dir = tempfile::tempdir().unwrap();
            let (term, _events) = spawn(sh(dir.path()));
            let inner = Arc::clone(&term.inner);
            let first = term.kill();
            assert_eq!(inner.teardown(), first);
        }
    }

    /// Run on a Windows host only (`cargo test -p agent-link --all-features --lib`); the
    /// gate cross-compiles them.
    #[cfg(windows)]
    mod windows {
        use super::*;
        use crate::proc::job;
        use crate::pty::ForegroundOwner;
        use std::time::{Duration, Instant};

        const WAIT: Duration = Duration::from_secs(5);

        fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
            let deadline = Instant::now() + WAIT;
            while !ready() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn output_until(events: &Receiver<Event>, needle: &str) {
            let deadline = Instant::now() + WAIT;
            let mut text = String::new();
            while !text.contains(needle) {
                match events.recv_deadline(deadline) {
                    Ok(Event::Output { bytes, .. }) => {
                        text.push_str(&String::from_utf8_lossy(&bytes));
                    }
                    other => panic!("no {needle:?} ({other:?}); output so far: {text:?}"),
                }
            }
        }

        /// Whether any of `pids` still runs.
        fn any_alive(pids: &[u32]) -> bool {
            let running = job::snapshot().expect("process snapshot");
            running.iter().any(|e| pids.contains(&e.pid))
        }

        /// Types `cmd /c ping -n 30 127.0.0.1` and returns the shell's tree once the ping
        /// grandchild runs: shell, inner `cmd.exe`, `PING.EXE`.
        fn start_ping(term: &Terminal) -> Vec<u32> {
            let shell = term.pid().expect("shell pid");
            term.write(b"cmd /c ping -n 30 127.0.0.1\r").unwrap();
            let mut tree = Vec::new();
            wait_for("ping under the shell", || {
                tree = job::tree(shell).unwrap_or_default();
                tree.len() >= 3
            });
            tree
        }

        #[test]
        fn the_job_kill_takes_a_grandchild_down() {
            let dir = tempfile::tempdir().unwrap();
            let (term, _events) = Terminal::spawn(SpawnSpec::shell(dir.path(), size())).unwrap();
            let job = term.inner.job.clone().expect("the child joined a job");
            let tree = start_ping(&term);
            let members = job.pids().unwrap();
            for pid in &tree {
                assert!(
                    members.contains(pid),
                    "{pid} is not in the job: {members:?}"
                );
            }
            term.kill();
            wait_for("the whole tree to end", || !any_alive(&tree));
        }

        /// A program started detached (its launcher exits at once, so its parent pid names a
        /// dead process) stays in the job but is not below the shell: the shell is still
        /// in front. Teardown kills it with the job — the documented cost of the job.
        #[test]
        fn a_detached_job_member_is_not_the_foreground() {
            let dir = tempfile::tempdir().unwrap();
            let (term, events) = Terminal::spawn(SpawnSpec::shell(dir.path(), size())).unwrap();
            let job = term.inner.job.clone().expect("the child joined a job");
            let shell = term.pid().expect("shell pid");
            term.write(b"echo ready^-8\r").unwrap();
            output_until(&events, "ready-8");
            // The inner cmd.exe starts ping without waiting for it and exits.
            term.write(b"cmd /d /c start \"\" /b ping -n 30 127.0.0.1 >nul\r")
                .unwrap();
            let mut detached = Vec::new();
            wait_for("a detached ping in the job, its launcher gone", || {
                // Snapshot first: a member it lists ran then, and is either below the
                // shell or detached; a process started after it is not judged.
                let entries = job::snapshot().unwrap_or_default();
                let tree = job::tree_in(&entries, shell);
                detached = job.pids().unwrap_or_default();
                detached.retain(|pid| !tree.contains(pid) && entries.iter().any(|e| e.pid == *pid));
                !detached.is_empty() && tree == [shell]
            });
            // The ping runs 30 s: an answer counting it would stay `Agent` past the wait.
            wait_for("the shell in front", || {
                term.foreground().owner == ForegroundOwner::Shell
            });
            term.kill();
            wait_for("the job kill to end the detached ping", || {
                !any_alive(&detached)
            });
        }

        /// A job that cannot be created (as when the host's own job forbids nesting) leaves
        /// a working session: snapshot foreground, `taskkill` teardown.
        #[test]
        fn spawn_works_when_the_job_cannot_be_created() {
            fn access_denied() -> std::io::Result<Job> {
                Err(std::io::Error::from_raw_os_error(5))
            }
            let dir = tempfile::tempdir().unwrap();
            let hooks = SpawnHooks {
                make_job: access_denied,
            };
            let (term, events) =
                Terminal::spawn_with(SpawnSpec::shell(dir.path(), size()), hooks).unwrap();
            assert!(term.inner.job.is_none());
            term.write(b"echo agent^-pty^-7\r").unwrap();
            output_until(&events, "agent-link-7");
            wait_for("the shell in front", || {
                term.foreground().owner == ForegroundOwner::Shell
            });
            let tree = start_ping(&term);
            wait_for("ping in front", || {
                term.foreground().owner == ForegroundOwner::Agent
            });
            term.kill();
            wait_for("the whole tree to end", || !any_alive(&tree));
        }
    }
}
