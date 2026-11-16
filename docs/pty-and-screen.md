# PTY Sessions, Headless Screen & Process Control

This chapter covers the always-on PTY foundation (`pty`, `env`, `shell`, `identity`, `resolve`, `proc`, `error`) and the default `screen` feature.

**Prev:** [Getting Started](getting-started.md) · **Next:** [Agents & Delivery](agents-and-delivery.md)

---

## Spawning a PTY (`SpawnSpec` & `Terminal`)

A PTY session runs a child process attached to a pseudo-terminal (`openpty` on Unix, ConPTY on Windows) with dedicated reader and writer threads.

### `SpawnMode` and `ShellSpec`

Choose between spawning an interactive shell or executing a program directly:

- `SpawnSpec::shell(cwd, size)` creates `SpawnMode::Shell(ShellSpec { program: None, login: true })`:
  - **Unix:** uses `$SHELL` (or the first existing of `/bin/zsh`, `/bin/bash`, `/bin/sh`). Passes `"-l"` **only** when `login: true` and the shell basename is `"zsh"` or `"bash"`.
  - **Windows:** uses `%COMSPEC%` (or `"cmd.exe"`), passing no flags.
  - Note: `ShellSpec::default()` has `login: false`, whereas `SpawnSpec::shell(...)` sets `login: true`.
- `SpawnSpec::direct(program, args, cwd, size)` creates `SpawnMode::Direct { program, args }`:
  - Runs `program` directly without a shell. If `program` has no directory component, it is resolved on the child's effective `PATH`.

### `PtySize` and `SpawnSpec` Validation

`Terminal::spawn(spec)` validates everything before starting a process:
- `spec.cwd` must exist and be a directory (`Error::InvalidInput`).
- `spec.size` (`rows`, `cols`) must have both dimensions `>= 1` (`Error::InvalidInput`).
- `spec.input_limits` must be valid (`Error::InvalidInput`).
- `spec.log` (if `SessionLogConfig::File`) must be an absolute path (`Error::InvalidInput`).
- For `SpawnMode::Direct`, `program` must be non-empty and resolvable on `env.effective_path()` (`Error::NotFound`).

> **macOS `openpty` concurrency:** Calling `openpty` from multiple threads in the same process at the exact same instant on macOS occasionally fails with `Error::Spawn("openpty: … Unknown error: -6")`. Serialize concurrent `Terminal::spawn` calls with a mutex or retry that error.

### Core `Terminal` Operations

| Method | Behavior |
| --- | --- |
| `term.id() -> SessionId` | Process-wide monotonic ID starting at `1`, never reused (even if spawn fails, its minted ID is skipped). |
| `term.pid() -> Option<u32>` | OS PID of the direct child. |
| `term.write(bytes) -> Result<()>` | Non-blocking enqueue into `InputLimits`. Delivers bytes in call order. Returns `Error::InputTooLarge`, `Error::QueueFull`, or `Error::Closed`. |
| `term.resize(size) -> Result<()>` | Validates `>= 1x1`, then atomically resizes the PTY and the headless screen model. |
| `term.interrupt() -> Result<()>` | Enqueues `b"\x03"` (Ctrl-C) to trigger `SIGINT` for the foreground job. |
| `term.foreground() -> Foreground` | Reports `{ owner: ForegroundOwner, confidence: Confidence }`. |
| `term.subscribe() -> Receiver<Event>` | Attaches a new live receiver from this point forward (or queued `Exit` if already exited). |
| `term.attach() -> (Replay, Receiver<Event>)` | Atomically snapshots retained log + attaches live receiver with zero gap and zero overlap. |
| `term.attach_recovery(from)` | Recovery attachment with byte cursors, explicit gaps, geometry, and optional display snapshot ([Async & Presentation](async-and-presentation.md#session-logging--replay-recovery)). |
| `term.try_wait() -> Option<ExitInfo>` | Non-blocking sticky exit status. On Unix uses `waitid(WNOHANG \| WNOWAIT)` so the PID remains reserved until teardown. |
| `term.kill() -> ExitInfo` | Tears down immediately and returns `ExitInfo`. Same as `Drop`, except it returns `ExitInfo`. |

### `SessionRegistry<K>`

`SessionRegistry<K>` maps host keys (`K: Eq + Hash`) to `Terminal` instances (one per key):
- `insert(key, term) -> Option<Terminal>` returns any displaced `Terminal`. **Drop the returned `Terminal` outside your own locks** because dropping tears down the old session (~2 s reap bound).
- `get(&key)`, `remove(&key)`, `drain()`, `len()`, `is_empty()`.

---

## Events and Receiver Backpressure

Every PTY session emits `Event` values to all registered receivers (`spawn`, `subscribe`, `attach`, `attach_recovery`):

| Variant | Feature | Meaning |
| --- | --- | --- |
| `Output { id, seq, bytes }` | always | Raw PTY bytes (up to 8 KiB per chunk). `seq` starts at `0` and increments by `1`. |
| `Exit { id, status }` | always | Child closed stdout / reached natural EOF. Currently `status` is `None` (use `try_wait()` for the exit code). Never sent after `kill()`/`Drop`. Sticky for late receivers. |
| `Title { id, title }` | `screen` | Window title updated via OSC 0 or OSC 2. |
| `Progress { id, state }` | `screen` | OSC 9;4 progress state (`Clear`, `Normal(Option<u8>)`, `Error(Option<u8>)`, `Indeterminate`, `Paused(Option<u8>)`). |
| `Frame { id }` | `screen` | DEC 2026 synchronized-output frame ended (`CSI ? 2026 l` or `ESC c` while in frame). |
| `ScreenReset { id, reason }` | `screen` | `vt100` parser panicked internally and was rebuilt blank (contained safely). |
| `AgentState`, `Activity`, `Delivery`, `CommitPoint` | `agent` | Emitted by `AgentTerminal`'s controller ([Agents & Delivery](agents-and-delivery.md)). |

Use `forward_to(events, |ev| { ... })` to process a `Receiver<Event>` on a dedicated `"agent-link-forward"` thread.

### Backpressure Rules

- Each subscriber channel is bounded to **1,024 events** (or **16 events** if attached after `Exit`).
- If **any** receiver fills up, the PTY reader thread blocks until space opens or that receiver is dropped.
- **Always drain or drop every receiver.** Teardown cancels blocked fanout immediately so a stalled consumer never blocks `kill()` or `Drop`.

---

## Teardown and Foreground Detection

### Teardown Semantics

Dropping a `Terminal` or calling `term.kill()`:
1. Stops event publication and unblocks any reader waiting on a full receiver. Synchronous file-log writes can still delay access to the recovery lock; see [file logging](async-and-presentation.md#session-logging--replay-recovery).
2. Kills the child and its descendants:
   - **Unix:** sends `SIGKILL` to the child's process group and foreground process group (`tcgetpgrp`), unless an external reaper already reaped the PID (`ECHILD`).
   - **Windows:** closes the session's Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) or falls back to `taskkill /T /F` if job assignment failed. **Caveat:** every descendant inherits the Job Object on Windows, including detached GUI applications launched from the shell (e.g., an editor or browser); closing the session terminates them too.
3. Reaps the direct child bounded at ~2 s, then detaches a background reaper thread if still pending. Worker threads are detached, never joined.

> **External child reapers:** If your host runs a global `waitpid(-1, ...)` reaper, coordinate with `Terminal`. An ownership check and a subsequent `kill` are not atomic if another thread reaps and lets the OS reuse the PID.

### Foreground Detection

`term.foreground()` returns `Foreground { owner: ForegroundOwner, confidence: Confidence }`:
- `ForegroundOwner`: `Shell`, `Agent`, or `Unknown`.
- `Confidence`:
  - `Exact` (Unix kernel `tcgetpgrp` inspection).
  - `Heuristic` (Windows Toolhelp process snapshot / Job Object inspection, or when closed/unsupported).

---

## Input Admission Bounds (`InputLimits`)

`SpawnSpec::input_limits` bounds queued and in-flight writes (including screen query replies):

| Field | Default | Meaning |
| --- | --- | --- |
| `max_write_bytes` | `1 MiB` | Largest single `write()` slice. |
| `max_pending_bytes` | `4 MiB` | Total bytes queued plus the chunk currently being written/flushed. |
| `max_pending_chunks` | `1,024` | Total chunks queued plus the active chunk. |

Validation requires all three to be `> 0` and `max_pending_bytes >= max_write_bytes`. Excess calls return `Error::InputTooLarge` or `Error::QueueFull` immediately without blocking.

---

## Headless Screen Model (`screen` feature)

When `screen` is enabled (default) and `SpawnSpec::screen.enabled` is `true`, every output chunk is fed into the screen model **before** any subscriber receives `Event::Output`. Therefore, `term.screen()` can be slightly ahead of the event you are currently reading from your receiver.

### `ScreenConfig`

| Field | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Enables `Vt100Model` tracking (always forced `true` by `AgentTerminal`). |
| `scrollback_lines` | `200` | Scrollback buffer lines retained by `vt100`. |
| `answer_queries` | `true` | Automatically replies to DA1, DSR, CPR, and DECRQM 2026 queries via non-blocking `try_send` sharing `InputLimits`. **Set to `false` if your host UI renderer (e.g. xterm.js) already answers terminal queries**, otherwise replies are doubled. |

### Reading the Screen

- `term.screen() -> Option<ScreenSnapshot>`:
  - `rows: Vec<String>` — visible rows top-to-bottom, trailing spaces trimmed.
  - `cursor: (u16, u16)` — 0-based `(row, col)` (note: after printing in the last column, `col == width` due to pending wrap).
  - `alternate_screen: bool`, `bracketed_paste: bool`, `title: String`, `size: PtySize`.
  - `generation: u64` — increments on every `process()` call and parser reset.
  - `text() -> String` — joins `rows` with `"\n"`.
  - `row_hash() -> u64` — process-local hash of `rows`.
- `term.repaint_stream() -> Option<Vec<u8>>`: emits minimal ANSI state bytes (`vt100::Parser::state_formatted`) to repaint a newly attached UI view.
- `screen::normalize(raw) -> String`: strips OSC, 7/8-bit CSI, two-byte escapes, and C0/DEL controls, collapses whitespace runs to single spaces, trims, and lowercases.
- `screen::ScreenText`: wraps lines and provides `bottom_rows(n)` (slice up to `n` rows ending at the last non-blank row).
- Custom screen models can implement `screen::ScreenModel`. Note that `Vt100Model` enforces `cols >= 2` internally (preventing `vt100` panics on 1-column wide chars) while still reporting requested `PtySize` in `snapshot().size`.

---

## Environment, Identity, Resolution & Process Helpers

### `EnvPolicy` (`agent_link::env`)

Controls child environment variables and `PATH`:
- `inherit: bool` (default `true`): `false` clears the inherited environment first.
- `set: Vec<(OsString, OsString)>` (or `.set(k, v)`): applied in order; later duplicates win.
- `remove: Vec<OsString>` (or `.remove(k)`): applied **after** `set`, so `remove` always wins.
- `path_prepend: Vec<PathBuf>`: prepended to `PATH` using `:` (Unix) or `;` (Windows).
- `effective_path() -> OsString`: computes the exact `PATH` the child will receive.
- `Debug` prints `set_keys` only — never values — to avoid leaking credentials.

### `TerminalIdentity` (`agent_link::identity`)

Agent CLIs change rendering behavior (color depth, synchronized output) based on host terminal env vars. `SpawnSpec::identity` sets an explicit terminal identity **after** `clear_env` (so it survives `inherit: false`) and **before** `EnvPolicy` (so host `EnvPolicy` overrides it):

- `TerminalIdentity::Honest` *(default)*: sets `TERM=xterm-256color`, `COLORTERM=truecolor`, and strips 24 exact-named host variables (`TERM_PROGRAM`, `TERM_PROGRAM_VERSION`, `TERM_SESSION_ID`, `TERM_FEATURES`, `TERMINAL_EMULATOR`, `TERMINFO`, `COLORFGBG`, `WT_SESSION`, `WT_PROFILE_ID`, `KITTY_WINDOW_ID`, `VTE_VERSION`, `LC_TERMINAL`, `LC_TERMINAL_VERSION`, `ITERM_SESSION_ID`, `ITERM_PROFILE`, `TMUX`, `TMUX_PANE`, `STY`, `WINDOW`, `ZELLIJ`, `ZELLIJ_SESSION_NAME`, `ZELLIJ_PANE_ID`, `COLUMNS`, `LINES`). Other vendor-prefixed variables (e.g. `KITTY_*`) pass through unless removed in `EnvPolicy`.
- `TerminalIdentity::Impersonate(KnownTerminal)`: sets `Honest` plus identity variables for `VsCode`, `WindowsTerminal`, `WezTerm`, or `Ghostty` (only use when your renderer actually implements that terminal's behavior — e.g., Claude emits DEC 2026 sync frames only under `VsCode`).
- `TerminalIdentity::Custom { vars }`: sets exact `(name, value)` pairs and strips nothing.

### Path Resolution & Process Helpers (`resolve`, `shell`, `proc`)

- `resolve::find_on_path(bins)` / `resolve::find_in_path_os(path, bins)`:
  - Searches `bin` -> `dir` -> extension.
  - On Unix, checks regular files with any execute bit (`0o111`).
  - On Windows, intersects `PATHEXT` with `LAUNCHABLE_EXTS` (`.com`, `.exe`, `.bat`, `.cmd`), ignoring `.ps1`/`.vbs` and never resolving bare extensionless npm shell shims.
- `proc`: `kill_tree(pid)`, `output_with_timeout(cmd, timeout)` (runs `setsid` on Unix so interactive login shells don't stop on `SIGTTIN`/`SIGTTOU`), `system32_program(stem)`, plus Windows-only `no_console_window(&mut cmd)`, `kill_process_tree(pid)`, and `status_with_timeout(cmd, timeout)`.

`output_with_timeout` keeps the child owned by the caller while separate workers drain
stdout and stderr. Only the caller decides whether to return a completed result or
perform timeout cleanup; a reader cannot reap the child before that decision. A timeout
still cleans up descendants in the owned Unix process group when they hold an output
pipe open after the direct child exits. Known externally reaped Unix children are never
signalled. Hosts with their own reaper must coordinate with this call: an ownership
check and a later PID-based signal are not atomic.

`output_with_timeout` and Windows `status_with_timeout` validate the deadline before
spawning. A timeout that cannot be represented by `Instant` returns
`io::ErrorKind::InvalidInput` and does not launch the command. The timeout budget starts
before the synchronous spawn call; the operating system's spawn operation itself cannot
be interrupted by this deadline. Output capture cleanup allows up to two additional
seconds for reaping, plus a Windows `taskkill` round trip. Escaped descendants may retain
detached output readers until closing their pipes.

---

## Error Handling (`error::Error`)

`agent_link::Error` is `#[non_exhaustive]`:
- `Spawn(String)` — PTY or process spawn failed (if a child had already started, it is already killed and reaped).
- `Closed` — session/transport closed.
- `InvalidInput(String)` — pre-spawn or pre-write validation failed.
- `InputTooLarge { size, limit }` — payload exceeded single-item budget; nothing queued.
- `QueueFull` — pending bytes or chunks exhausted; retry after progress.
- `NotFound(String)` — launcher not found on child's `PATH`.
- `Unsupported(String)` — operation unsupported by profile or platform.
- `Io { context, source }` — I/O failure (`Display` already includes `source`).
