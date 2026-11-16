//! Integrate external AI agent CLIs through pseudo-terminal or structured stdio
//! transports. Manage subprocess lifecycles and consume their events. Interactive
//! sessions add screen observation, startup handling and verified message delivery;
//! headless sessions provide bounded line transport or an optional native Codex adapter.
//! No UI or host-framework dependency: applications own rendering.
//!
//! Version 0.1.0. Before 1.0, minor releases may introduce breaking API changes.
//!
//! # Start here
//!
//! | To… | Use | Feature |
//! |---|---|---|
//! | run a shell or program on a PTY | [`Terminal`] (module [`pty`]) | always |
//! | run an agent CLI with startup handling and verified delivery | `agent::AgentTerminal` (module `agent`) | `agent` |
//! | run an agent CLI headless over stdio (JSON lines) | `rpc::RpcChild` (module `rpc`) | `rpc` |
//! | use native Codex threads, turns and approvals over owned stdio | `codex::CodexAppServer` | `codex` |
//! | tell whether an agent CLI is installed and signed in | `probe::probe_agents` | `probe` |
//! | consume any of these from async code | module `async_api` | `tokio` |
//!
//! Each module documents its purpose and contracts, most with an example. Runnable
//! programs live in `examples/`: `bare_shell` (a login-shell round trip), `agent_send`
//! (a real turn on an installed agent CLI), `rpc_child` (`cat` as a JSON-lines agent;
//! feature `rpc`), `codex_app_server` (a real native turn; feature `codex`) and
//! `capture` (records a PTY session as a test fixture).
//!
//! ```text
//! cargo run -p agent-link --example bare_shell
//! ```
//!
//! # Platforms
//!
//! Only macOS has verified native runtime results. Linux and Windows (ConPTY, Job Objects;
//! heuristic foreground answers) are implementation targets with native CI configured,
//! but their runtime support remains unverified. Cross-compilation is compilation evidence
//! only. On macOS, concurrent `openpty` calls occasionally fail — serialise concurrent
//! spawns or retry that error. MSRV: 1.85.
//!
//! # Features
//!
//! | Feature | Default | Adds |
//! |---|---|---|
//! | `screen` | yes | headless terminal model (`vt100`), screen-text normalisation (`regex`) and signal tap (title, progress, sync frames, query replies) |
//! | `agent` | yes | built-in agent profiles and delivery controller (implies `screen`) |
//! | `rpc` | no | stdio line transport for JSON-over-stdio agents (implies `agent`) |
//! | `acp` | no | bounded ACP stable-v1 adapter (implies `rpc`; optional `serde_json`) |
//! | `claude` | no | persistent bounded Claude stream-json adapter (implies `rpc`; optional `serde_json`) |
//! | `codex` | no | bounded Codex app-server pilot (implies `rpc`; optional `serde_json`) |
//! | `probe` | no | CLI installed / signed-in checks, `probe_agents` (implies `agent`) |
//! | `tokio` | no | async adapters: event streams, awaitable tickets, `send_async`, teardown on Tokio's blocking pool (`sync` and `rt` only) |
//!
//! PTY sessions, environment and login-shell helpers, launcher resolution and process-tree
//! helpers are always enabled.
//!
//! # PTY sessions
//!
//! [`Terminal::spawn`] starts a shell or program on a PTY from a [`SpawnSpec`] and returns
//! the session plus an [`Event`] receiver. Write, resize, query [`Foreground`] ownership,
//! [`attach`](Terminal::attach) late with a [`Replay`] of the session log, and drop the
//! [`Terminal`] to kill the child and its foreground job. [`SessionRegistry`] keeps one
//! terminal per key. See [`pty`].
//!
//! # Screen model
//!
//! With `screen`, a session feeds every output chunk to a headless terminal (`vt100`)
//! before publishing it: `Terminal::screen` returns visible rows, cursor and modes, while
//! the session emits window-title, progress and synchronized-output frame events. The model
//! answers terminal queries unless `answer_queries` is disabled because the host renderer
//! answers them. See `screen`.
//!
//! # Agent profiles
//!
//! With `agent`, `agent::AgentProfile` defines per-CLI screen detection (startup dialogs
//! and next keystrokes, ready composer, or unsubmitted input), shell launch commands
//! (`LaunchPlan` for approval posture and trust policy), reset command, `PATH` launchers,
//! sign-in check and stdio launch. `agent::ProfileRegistry::builtin` includes `claude`,
//! `opencode`, `pi`, `codex` and `cursor`. Cursor support is work in progress:
//! authenticated unattended startup/composer behavior remains unverified.
//!
//! `agent::AgentTerminal` pairs a session with an agent controller: it types the launch
//! line, steers startup dialogs within bounded budgets, and emits readiness (`AgentState`
//! with a `blocking` overlay flag) and activity events. `send` delivers one message at a
//! time: pasted when ready, submitted with a screen-verified Enter, and reported via
//! `Ticket` and event. Unknown foreground ownership is refused; strict mode re-checks
//! ownership at commit and after verification, and hosts can hook right before paste.
//! Unreadable or unchanged composers never prove submission; Enter retries are opt-in and
//! may double-submit. `reset` and `reset_and_send` start a fresh conversation. Defaults
//! use YOLO/unattended permissions and leave trust dialogs to the host. Configure
//! `Permissions::AgentDefault` or `Permissions::Custom` explicitly for restrictions.
//! See `agent`.
//!
//! # RPC transport
//!
//! With `rpc`, `rpc::RpcChild` runs an agent CLI headless over JSON-lines stdio (no PTY):
//! spawned via login shell `exec` in its own session on Unix (or resolved on the child's
//! `PATH` on Windows), emitting one event per stdout line, an exit event with a bounded
//! stderr tail, a line writer, and process-group / process-tree kill on drop. Frame and
//! queue limits reject excess input; output-framing or pipe failures end publication with a
//! typed transport error, after which the child should be dropped or killed. Payload
//! logging requires explicit trace opt-in. `rpc::preflight` validates a spec before spawn
//! so replacements can be checked before tearing down an existing child. See `rpc`.
//!
//! # Native Codex adapter
//!
//! With `codex`, `codex::CodexAppServer` owns one stdio child and one native thread.
//! Requests have correlated tickets and total deadlines; essential approvals have
//! origin-bound reply tokens. Events pair versioned `portable::EventV1` observations
//! with optional native fields, retained by default. Full host event queues
//! terminate observation explicitly, with failure readable independently of events.
//! Profile launches preserve unattended defaults. See `codex` for limits and scope.
//!
//! # Install and sign-in probes
//!
//! With `probe`, `probe::probe_agents` checks per profile whether the CLI is installed
//! (`--version` exits 0) and signed in (`Some(false)` only on a verified signed-out marker;
//! `None` is never treated as signed out). It runs one thread per profile with bounded
//! timeouts and bounded capture, applying the complete environment policy. Detailed
//! reports (`probe::probe_agents_detailed`) retain version output, command outcomes and
//! context caveats, with direct mode and an explicit cwd available. Unix login-shell
//! startup files may override the supplied context. Profile compatibility metadata
//! separates declarations and operation-scoped evidence. See `probe`.
//!
//! # Async
//!
//! With `tokio`, async hosts can drive the API without blocking runtime threads:
//! `EventStream` wraps a receiver with `async fn next` (no `futures` dependency),
//! `Terminal::events_stream` subscribes a stream, `Ticket::outcome` and
//! `AgentTerminal::send_async` await delivery, and `kill_async` runs teardown on Tokio's
//! blocking pool. See `async_api`.
//!
//! # Threading model
//!
//! - Session I/O runs on dedicated threads; session calls never block on child I/O.
//!   One-shot CLI helpers block up to their timeout — keep them off UI threads.
//! - Each PTY session spawns reader and writer threads; agent sessions add monitor and job
//!   threads; RPC children spawn stderr, reader and writer threads. `probe_agents` spawns
//!   and joins one thread per profile. With `tokio`, each `EventStream` adds a forwarding
//!   thread (exiting on drop or after draining a disconnected receiver; a full unread
//!   stream keeps it waiting); `Ticket::outcome` adds none.
//! - Events use bounded channels: drain or drop every receiver, or a full channel will
//!   stall the session.
//! - Teardown closes publication and resolves agent tickets independently of event
//!   consumers (final delivery events are best-effort; tickets are authoritative). It kills
//!   the child's process group and foreground job (Windows: Job Object including detached
//!   descendants, or process tree; RPC child: process group / process tree), then reaps the
//!   child within ~2 s; worker threads are detached, never joined. Hosts that reap children
//!   independently must coordinate with teardown, as PID ownership checks and signals are
//!   not atomic.
//! - Handles are `Send + Sync`; the only process-wide mutable state is a monotonic
//!   session-id counter.

#![warn(missing_docs)]

// Unit tests share the integration fixture loader, which imports `agent_link`.
#[cfg(test)]
extern crate self as agent_link;

// Module map (`[feature]` marks gated directories/files; everything else is always on):
//
//   error.rs                 Error, Result
//   env.rs                   EnvPolicy, EnvTarget, prepend_path
//   shell.rs                 login_shell, shell_base, login_shell_argv, ShellSpec
//   identity.rs              TerminalIdentity, KnownTerminal
//   resolve.rs               PATH / PATHEXT launcher resolution
//   proc/mod.rs              kill_tree, kill_process_tree, no_console_window, system32_program,
//                            output_with_timeout, status_with_timeout
//   proc/job.rs              Windows Job Object, Toolhelp process snapshot
//   pty/mod.rs               SessionId, PtySize, SpawnMode, SpawnSpec, ScreenConfig,
//                            SessionLogConfig, Event, Foreground, ExitInfo, InputLimits
//   pty/session.rs           Terminal: spawn, reader/writer threads, teardown
//   pty/child.rs             child exit check (PID reserved), tree kill, bounded reap
//   pty/bus.rs               EventBus (seq-fenced publish), subscribe, forward_to
//   pty/foreground.rs        Unix tcgetpgrp classification; Windows heuristic
//   pty/log.rs               SessionLog (Off | Memory | File), Replay, attach
//   pty/model.rs             session screen-model slot (no-op without `screen`)
//   pty/registry.rs          SessionRegistry<K>
//   screen/mod.rs  [screen]  ScreenModel, ScreenSnapshot, TapSignal, PromptMark
//   screen/text.rs           normalize, ScreenText
//   screen/vt100_model.rs    Vt100Model (catch_unwind + rebuild)
//   screen/tap.rs            title, OSC 9;4, OSC 133, DEC 2026, query responder
//   agent/mod.rs   [agent]   re-exports
//   agent/terminal.rs        AgentTerminal, AgentSpec, AgentState, AgentLimits
//   agent/state/             controller state + transitions (single lock, no I/O under lock)
//   agent/monitor.rs         monitor thread: bus events, startup gate, activity, timers
//   agent/controller.rs      job thread: FIFO queue, message job, reset_and_send
//   agent/delivery.rs        SendOptions, Ticket, TicketId, DeliveryOutcome, AbortReason
//   agent/startup/           startup gate, dialog budgets, rolling UTF-8 output buffer
//   agent/settle.rs          lull, paste-landed and submit-outcome decisions
//   agent/verify.rs          verified Enter
//   agent/proof.rs           strict foreground proofs, commit-point check and handshake
//   agent/leftover.rs        pre-launch screen baseline and rows drawn since
//   agent/activity.rs        Activity, ActivityTracker
//   agent/timing.rs          Timing defaults
//   agent/permissions.rs     Permissions, PreTrust, TrustPolicy, ForegroundPolicy
//   agent/launch.rs          LaunchPlan, ShellKind, one-shot login-shell command lines
//   agent/profile.rs         AgentProfile, Dialog, DialogKind, TrustKey, ScreenState,
//                            SignalHints, Unlock, LoginCheck, RpcLaunch, RpcLaunchOptions
//   agent/detect.rs          shared screen-detection helpers
//   agent/profiles/          ProfileRegistry + builtin profiles (claude, codex, pi,
//                            opencode, cursor)
//   rpc/mod.rs     [rpc]     RpcChild, RpcSpawnSpec, RpcLimits, RpcEvent, preflight,
//                            forward_rpc, frame_line
//   rpc/command.rs           child command construction and pre-spawn validation
//   rpc/child.rs             child exit check (PID reserved), tree kill, bounded reap
//   rpc/pump.rs              stderr / reader / writer threads, event publisher
//   rpc/stderr_ring.rs       bounded stderr tail
//   codex/         [codex]   bounded owned-stdio app-server pilot, native state and approvals
//   portable.rs    [codex]   versioned host event projections, origin and usage types
//   protocol/      [codex]   shared completion, pending-admission and event-queue primitives
//   probe/mod.rs   [probe]   ProbeResult, probe_agents
//   probe/run.rs             version/login runners, probe env, logging
//   probe/login.rs           classify_login (three-way verdict)
//   async_api.rs   [tokio]   EventStream, Terminal::events_stream, Ticket::outcome,
//                            AgentTerminal::send_async, kill_async

#[cfg(feature = "agent")]
pub mod agent;
#[cfg(feature = "approvals")]
pub mod approvals;
#[cfg(feature = "tokio")]
pub mod async_api;
#[cfg(feature = "codex")]
pub mod codex;
pub mod env;
pub mod error;
pub mod identity;
#[cfg(feature = "observers")]
pub mod observers;
#[cfg(any(
    feature = "codex",
    feature = "acp",
    feature = "claude",
    feature = "transcript"
))]
pub mod portable;
#[cfg(feature = "probe")]
pub mod probe;
pub mod proc;
#[cfg(any(feature = "codex", feature = "acp", feature = "claude"))]
mod protocol;
pub mod pty;
pub mod resolve;
#[cfg(feature = "rpc")]
pub mod rpc;
#[cfg(feature = "screen")]
pub mod screen;
pub mod shell;
#[cfg(feature = "transcript")]
pub mod transcript;

#[cfg(feature = "tokio")]
pub use async_api::EventStream;
pub use error::{Error, Result};
pub use identity::{KnownTerminal, TerminalIdentity};
pub use pty::{
    Confidence, Event, ExitInfo, Foreground, ForegroundOwner, InputLimits, ProgressState, PtySize,
    RecoverySnapshot, Replay, ReplayCursor, ReplayError, ReplayGap, ReplayGapReason, ScreenConfig,
    SessionId, SessionLogConfig, SessionRegistry, SpawnMode, SpawnSpec, Terminal, forward_to,
};

#[cfg(feature = "acp")]
pub mod acp;
#[cfg(feature = "claude")]
pub mod claude;
