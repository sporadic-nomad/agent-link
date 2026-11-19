# agent-link

Integrate external AI agent CLIs (Claude, Codex, Pi and OpenCode, with Cursor support
in progress) into Rust applications through PTY or structured stdio transports.

For interactive sessions, spawn an agent in the user's login shell or directly, watch
its screen through a headless terminal model, steer startup dialogs, paste messages and
verify submission. For headless sessions, use the bounded JSON-lines stdio transport
and implement the agent's protocol on top, or enable an optional ACP, Claude stream-json or
Codex app-server adapter for native requests, turns and approvals. The library manages process lifecycles,
streams events, and checks whether supported agents are installed and signed in.

The crate has no UI and no host-framework dependency: a desktop app, a TUI or a test
harness renders the byte stream itself and drives the agent through a synchronous,
thread-backed API. Async adapters for Tokio sit behind a feature.

**Version: 0.2.0.** A standalone library for desktop apps, TUIs, services and
test harnesses. Before 1.0, minor releases may introduce breaking API changes. See the
[user manual](docs/README.md) for usage guides, or
[release preparation](CONTRIBUTING.md#release-preparation) for publication requirements.

**Permission default: YOLO/unattended.** Profile-based PTY and RPC launches allow
tools without approval where the CLI supports it. Codex launches also disable its
sandbox. Configure `Permissions::AgentDefault` to use the CLI's policy, or
`Permissions::Custom` to supply restrictions. See
[permission configuration](docs/permissions-and-probes.md#permission-defaults-permissions).

## Contents

| Module | Feature | What it does |
| --- | --- | --- |
| `pty` (`Terminal`) | always | Runs a shell or a program on a PTY (openpty; ConPTY on Windows). Output arrives as events on bounded channels; write, resize, ask who owns the foreground (the shell or a program it started), attach late with a replay. Dropping it kills the child's process group and foreground job (Windows: its Job Object). |
| `env`, `shell`, `identity`, `resolve`, `proc` | always | The child's environment policy, login-shell argv, the `TERM` identity a child sees, `PATH`/`PATHEXT` launcher lookup, process-tree kill and timeout runners. |
| `screen` | `screen` (default) | A headless terminal (`vt100`) fed every chunk before any receiver gets it: rows, cursor, modes; title, OSC 9;4 progress and sync-frame events. Answers terminal queries unless your renderer does. |
| `agent` (`AgentTerminal`) | `agent` (default) | Built-in profiles for five CLIs. Types the launch line, answers startup dialogs, reports readiness and activity, delivers messages one at a time with a verified Enter; strict delivery, a commit-point check, reset. |
| `agent::turns` | `turns` | Controlled direct PTYs on macOS arm64. Claude and Codex can yield explicit final replies, Pi yields inferred replies, and Cursor reports completion with separately opted-in mixed text. Each reader requires host-enforced writer isolation. [Claude setup](docs/agents-and-delivery.md#claude-pty-final-replies) · [Codex, Pi and Cursor setup](docs/pty-transcript-sources.md). |
| `tools` (`AgentTools`) | `tools` | Attaches consumer-owned CLI commands and MCP connections to one run, with child PATH/environment, provider configuration and temporary-file ownership. [Guide](docs/application-tools.md). |
| `rpc` (`RpcChild`) | `rpc` | An agent CLI headless over stdio: one event per stdout line, a stderr tail, process-group teardown. |
| `acp` (`AcpClient`) | `acp` | Optional bounded stable-v1 ACP adapter: strict session restore, text prompts, cooperative cancel and origin-bound permissions. [Guide](docs/rpc-and-native-adapters.md#agent-client-protocol-acp). |
| `claude` (`ClaudeSession`) | `claude` | Optional persistent Claude stream-json sessions: initialize, bounded user/control writes, results, explicit resume binding and single-use approval tokens. [Guide](docs/rpc-and-native-adapters.md#claude-stream-json-claude). |
| `codex` (`CodexAppServer`) | `codex` | Optional owned-stdio Codex pilot: one managed root thread per connection, correlated requests, native turn events, bounded approvals and explicit overload. [Guide](docs/rpc-and-native-adapters.md#codex-app-server-codex). |
| `probe` (`probe_agents`, `probe_agents_detailed`) | `probe` | Installed and three-way sign-in checks; detailed reports retain bounded version output, launcher identity, outcomes and context caveats. [Guide](docs/permissions-and-probes.md#cli-installation--sign-in-probes-probe-feature). |
| `portable` (`EventV1`) | `codex`, `acp`, `claude`, or `transcript` | Versioned provider/session/turn/item observations (`portable::EventV1`); retains native method and origin without inferring missing fields. |
| `approvals` (`Executor`) | `approvals` | Opt-in bounded native callback executor; explicit delegation and provider-specific token decisions. [Guide](docs/rpc-and-native-adapters.md#native-approval-callbacks-approvals). |
| `transcript` (`Projector`) | `transcript` | Bounded presentation projection with native/raw provenance and explicit gaps; never delivery or completion authority. [Guide](docs/async-and-presentation.md#transcript-projection-transcript-feature). |
| `observers` (`ObserverHub`) | `observers` | Bounded raw PTY observer fanout, replay/live fences and explicit slow-viewer disconnects. [Guide](docs/async-and-presentation.md#pty-observer-fanout-observers-feature). |
| `async_api` (`EventStream`) | `tokio` | Event streams, awaitable tickets, `send_async`, `kill_async`. Uses Tokio's `sync` and `rt` only. |

The `tools` facade validates MCP HTTP hosts, ports and IPv6 syntax before launch. It also
rejects Pi MCP environment/header templates and CLI directories that need PATH escaping,
including semicolons on Windows. See its
[validation rules](docs/application-tools.md#ownership-validation-and-limits).

`Terminal::attach_recovery` adds session-bound byte cursors, explicit replay gaps,
current geometry and an optional display snapshot at the replay/live boundary.
It preserves receiver backpressure; the display is not a parser checkpoint. With
`observers`, `Terminal::attach_live_recovery` and
`Terminal::attach_live_recovery_metadata` attach directly at the current fence without
copying retained replay (and without cloning screen text in the metadata variant).
See [replay recovery](docs/async-and-presentation.md#session-logging--replay-recovery)
for cursor scope and file-log lifetimes.

The workspace also contains three separate, unpublished crates (`agent-link-supervisor`,
`agent-link-daemon`, `agent-link-http`) that leave core dependencies, default features and
Drop behavior unchanged (see *Optional presentation and supervision layers* below).
Default Cargo commands select `agent-link`; shared static/full and MSRV gates include all
four workspace packages.

Add the crate from crates.io:

```toml
[dependencies]
agent-link = "0.2" # screen + agent
# agent-link = { version = "0.2", default-features = false }
# agent-link = { version = "0.2", features = ["rpc", "probe", "tokio"] }
# Optional native/presentation features: "acp", "claude", "codex", "approvals", "observers", "transcript"
# Add "tools" for application CLI/MCP attachments.
```

For development, use `{ path = "../agent-link" }` or
`{ git = "https://github.com/sporadic-nomad/agent-link" }` instead of the version.

## Examples

The three examples quoted below live in [`examples/`](examples/);
`tests/readme.rs` checks their excerpts. Additional examples in [`examples/`](examples/)
include:
- Offline/local: [`custom_profile`](examples/custom_profile.rs) (default features),
  [`application_tools`](examples/application_tools.rs) (`--features tools`; prepares CLI/MCP attachments without starting an agent),
  [`probe_agents`](examples/probe_agents.rs) (`--features probe`),
  [`tokio_shell`](examples/tokio_shell.rs) (`--no-default-features --features tokio`),
  [`pty_recovery_observers`](examples/pty_recovery_observers.rs) (`--features "screen,observers,transcript"`),
  and [`capture`](examples/capture.rs) (fixture recorder; `--features screen`).
- Real CLI turns: [`codex_app_server`](examples/codex_app_server.rs) (`--features codex`),
  [`acp_prompt`](examples/acp_prompt.rs) (`--features acp`), and
  [`claude_session`](examples/claude_session.rs) (`--features claude`).

### A bare shell

```text
cargo run -p agent-link --example bare_shell
```

Spawns your login shell in the current directory, types `echo hel''lo` (the shell prints
`hello`), prints the raw output
until the prompt is back, prints the screen the headless model drew, and tears the session
down ([`examples/bare_shell.rs`](examples/bare_shell.rs)):

```rust
let cwd = std::env::current_dir()?;
let size = PtySize { rows: 24, cols: 80 };
let (term, events) = Terminal::spawn(SpawnSpec::shell(cwd, size))?;
let pid = term.pid().unwrap_or_default();
println!("session {}: pid {pid}", term.id());
// The quote (cmd.exe: the caret) splits the word, so the shell's echo of the typed line
// never reads `hello`, only the command's output does. cmd.exe ends a line with `\r`.
let line: &[u8] = if cfg!(windows) {
    b"echo hel^lo\r"
} else {
    b"echo hel''lo\n"
};
term.write(line)?; // queued; the session's writer thread types it

// Print the output until the command's own output arrived…
let deadline = Instant::now() + OUTPUT_WAIT;
let mut seen = String::new();
while !seen.contains("hello") {
    match events.recv_deadline(deadline) {
        Ok(Event::Output { bytes, .. }) => {
            let text = String::from_utf8_lossy(&bytes);
            println!("output: {text:?}");
            seen.push_str(&text);
        }
        Ok(Event::Exit { .. }) => return Err("the shell exited first".into()),
        Ok(_) => {} // title and frame events from the screen model
        Err(_) => return Err(format!("no `hello` within {OUTPUT_WAIT:?}").into()),
    }
}
```

The crate knows no prompt format, so the example then waits for 500 ms of quiet, prints
`term.screen()` and calls `term.kill()`. On macOS with zsh (prompt escapes shortened):

```text
session 1: pid 13685
output: "echo hel''lo\r\n"
output: "\u{1b}[1m\u{1b}[7m%\u{1b}[27m…"
output: "\r\u{1b}[0m\u{1b}[27m\u{1b}[24m\u{1b}[Juser@host openflow % \u{1b}[K\u{1b}[?2004he\u{8}echo"
output: " hel''lo\u{1b}[?2004l\r\r\n"
output: "hello\r\n"
…
screen:
echo hel''lo
user@host openflow % echo hel''lo
hello
user@host openflow %
torn down: ExitInfo { status: None }
```

The first line is the terminal echoing the command typed before the shell was ready. On
Windows the example types `echo hel^lo` into `cmd.exe`.

### A message to an agent

```text
cargo run -p agent-link --example agent_send -- claude "Reply with the single word: pong"
```

The CLI must be installed and signed in: this is a **real turn on your account**. The agent
starts in the current directory with **YOLO/unattended permissions by default**. This
example explicitly opts into folder trust, which some CLIs save. Library defaults do
not add pre-trust flags or answer dialogs automatically.
To configure restrictions, set `agent.permissions` to `Permissions::AgentDefault` for
the CLI's own policy, or `Permissions::Custom` for replacement arguments and environment
values. See [permission configuration](docs/permissions-and-probes.md#permission-defaults-permissions).

For a Claude, Codex, or Pi shell launch, set `agent.model` to
`Some(ModelChoice::new("model-id").with_effort("high"))`. Omit `with_effort` to pass
the model string without a separate effort argument. The CLI interprets that string.
Pi accepts `--model=…` and `--thinking=…` extension flags alongside this selection.
The default is `None`. Spawn rejects unsupported choices, choices on a bare shell or
Direct child, and selected commands that exceed the shell line or PTY write limit.
Shell launches use fish quoting when the resolved Unix shell is named `fish`.
Each relaunch uses the same arguments. See [PTY model selection and supported levels](docs/agents-and-delivery.md#pty-model-and-reasoning-effort).

Any built-in id is accepted (`claude`, `opencode`, `pi`, `codex`, `cursor`), but verified
submission requires a readable composer (see *Agent CLIs*). Launch, then wait
until the controller reports `Ready` ([`examples/agent_send.rs`](examples/agent_send.rs)):

```rust
let mut agent = AgentSpec::new(profile);
// YOLO/unattended permissions are the default. Explicitly trust this working folder
// so startup can complete without an interactive renderer.
agent.trust = TrustPolicy::unattended();
let cwd = std::env::current_dir()?;
let (term, events) = AgentTerminal::spawn(SpawnSpec::shell(cwd, SIZE), agent)?;

term.launch()?; // types the profile's launch line into the shell
let deadline = Instant::now() + READY_WAIT;
loop {
    let state = match events.recv_deadline(deadline) {
        Ok(Event::AgentState { state, .. }) => state,
        Ok(_) => continue, // output, titles, activity
        Err(_) => return Err(failure(&term, &format!("not ready within {READY_WAIT:?}"))),
    };
    println!("state: {state:?}");
    match state {
        AgentState::Ready => break,
        AgentState::Exited => return Err(failure(&term, "the session ended")),
        _ => {} // starting, a startup dialog being answered
    }
}
drop(events); // not read from here on: an undrained receiver would stall the session
```

Then send. Keep the ticket: it is authoritative. Admitted deliveries normally also
publish `Event::Delivery`; immediate admission rejections have no delivery event, and
shutdown notifications are best effort:

```rust
let opts = SendOptions {
    submit: true, // paste, then an Enter verified on screen
    ..SendOptions::default()
};
let ticket = term.send(message, opts);
let Some(outcome) = ticket.wait(DELIVERY_WAIT) else {
    let what = format!("no outcome within {DELIVERY_WAIT:?}");
    return Err(failure(&term, &what));
};
```

`Delivered` reports a queued paste and, with `submit`, a verified composer transition
after Enter. Screen evidence is revalidated after profile callbacks; stale results
cannot confirm submission. It is terminal evidence, not a protocol-level acknowledgement. `PastedUnsubmitted`: the shell, not the agent, was in front after one
relaunch, so a one-line text was left on the shell's input line without Enter. `Aborted`
names why nothing (or nothing more) was typed: `ShellOnly`, `ForegroundUnproven` (another
program owns the terminal), `TrustPromptBlocking`, `DialogBlocking`, `SubmitUnverified`, … The example exits
0 only on `Delivered`.

### A JSON-lines child

Built-in profile RPC launches default to YOLO/unattended permissions. Use
`profile.rpc_launch_with_options(&options)` with `RpcLaunchOptions::permissions` set to
`Permissions::AgentDefault` or `Permissions::Custom` to configure restrictions.
Unsupported presets return `Error::Unsupported`; the host implements any required
protocol approval replies. See [launch policy](docs/permissions-and-probes.md#permission-defaults-permissions).

```text
cargo run -p agent-link --example rpc_child --features rpc
```

`cat` stands in for an agent CLI in its stdio mode (`claude --print` with stream-json,
`codex app-server`, `pi --mode rpc`, `opencode acp`). On unix it runs through your login
shell in a session of its own, as an agent would; on Windows it needs a `cat` on `PATH`
([`examples/rpc_child.rs`](examples/rpc_child.rs)):

```rust
// A raw command; built-in profile RPC launches add YOLO/unattended settings by default.
let launch = RpcLaunch::new("cat", &[]);
let mut spec = RpcSpawnSpec::new(launch, std::env::current_dir()?);
spec.log_tag = "example".into(); // log lines read `[example:cat] …` (`log` facade)
spec.log_key = "cat".into();
// Validate the spec before replacing an existing child. Only Windows checks launcher
// PATH resolution here; Unix resolves it in the spawned login shell.
// Success does not guarantee that spawning or agent startup will succeed.
preflight(&spec)?;
let (child, events) = RpcChild::spawn(spec)?;
```

Each stdout line is one `RpcEvent::Line`; an `Exit` with the stderr tail follows only when
stdout ends naturally without a terminal transport error, never after a kill.
`StdinError` closes input while output continues; `Error` replaces `Exit`. Codex/ACP
also refuse new requests and approval replies once input closure is observed, while
keeping native responses and existing deadlines active. Poll
`child.try_wait()` for the exit code separately:

```rust
let (done_tx, done_rx) = mpsc::channel();
forward_rpc(events, move |event| match event {
    RpcEvent::Line { seq, line, .. } => {
        println!("line {seq}: {line}");
        if line == REQUEST {
            let _ = done_tx.send(Ok(()));
        }
    }
    RpcEvent::Error { error, .. } => {
        let _ = done_tx.send(Err(format!("RPC transport failed: {error}")));
    }
    RpcEvent::Exit { stderr_tail, .. } => {
        let _ = done_tx.send(Err(format!("the child exited; stderr: {stderr_tail}")));
    }
    _ => {}
})?;

child.send_line(REQUEST)?; // the writer thread appends `\n` and flushes
```

Output:

```text
child 1: pid 13941
line 0: {"id":1,"method":"ping"}
round trip: ok
torn down: exit code None
```

### Async

With feature `tokio`, an async host consumes the same sessions without blocking a runtime
thread (from the `async_api` module docs). This timeout example also requires your
application's Tokio dependency to enable `time` and a runtime with time enabled:

```rust
use std::time::Duration;
use agent_link::{Event, EventStream, PtySize, SpawnSpec, Terminal};

let spec = SpawnSpec::shell(std::env::temp_dir(), PtySize { rows: 24, cols: 80 });
let (term, events) = Terminal::spawn(spec)?;
let result = tokio::time::timeout(Duration::from_secs(30), async {
    let mut events = EventStream::new(events)?; // from the session's first byte on
    // Split the echoed word so only command output can match `hello`.
    let line: &[u8] = if cfg!(windows) {
        b"echo hel^lo\r"
    } else {
        b"echo hel''lo\n"
    };
    term.write(line)?;

    let mut seen = Vec::new();
    while let Some(event) = events.next().await {
        match event {
            Event::Output { bytes, .. } => {
                seen.extend_from_slice(&bytes);
                if seen.windows(5).any(|word| word == b"hello") {
                    return Ok::<(), Box<dyn std::error::Error>>(());
                }
                // Retain only the suffix needed to match across chunk boundaries.
                seen.drain(..seen.len().saturating_sub(4));
            }
            Event::Exit { .. } => return Err("shell exited before output".into()),
            _ => {}
        }
    }
    Err("event stream ended before output".into())
}).await;

// Tear down off the runtime thread on success, error, and timeout.
term.kill_async().await;
result??;
```

## What you can rely on

- **Teardown.** Dropping a terminal kills its process group and foreground job
  (Windows: its Job Object or process tree), then waits up to about two seconds to reap
  the direct child. Agent shutdown resolves outstanding tickets even when an event
  receiver is full. Final shutdown events are best effort; tickets are authoritative.
  Use `kill_async` in async applications. Hosts that reap children independently must
  coordinate with teardown: ownership checks and PID-based signals are not atomic.
- **Events.** PTY receivers hold 1,024 events; RPC receivers default to 8, configurable
  through `RpcLimits`. Drain or drop each receiver to keep normal operation moving.
  PTY `Exit` leaves receivers connected until teardown; RPC terminal `Exit` or `Error`
  disconnects after buffered events drain. Session IDs distinguish replacement sessions
  from earlier events.
  The internal agent monitor is also bounded; overflow stops automation with
  `NotConnected` while raw terminal events remain available.
- **Delivery.** Accepted messages run one at a time. Unknown foreground ownership is
  refused, including heuristic Windows answers under the default `ExactOnly` policy.
  A Windows host can explicitly choose `AcceptHeuristic`. Submission requires seeing
  the pasted message, followed by fresh, stable evidence that it left a readable
  composer. Ordinary Enter retries are disabled by default. Opting in requires
  pending text and can submit twice when a CLI redraws late.
  A profile that recognizes an open question stops even a paste-only send with
  `DialogBlocking`. A match after the paste stops the next Enter, including an opted-in
  retry. If the shell now owns the terminal, delivery sends Ctrl-C to clear its draft
  and reports `ShellOnly`. After `DialogBlocking` or `SubmitUnverified`, inspect the
  session before sending again because input may already have reached the agent. The
  screen and foreground can change between the checks and the PTY write.
- **Conversation resets.** `reset()` and `reset_and_send()` submit the reset command
  at most once. If the profile recognizes a fresh refusal, the ticket reports
  `Aborted(ResetRejected)` (`reset_rejected`) and the follow-up is withheld.
  Ambiguous or unavailable reset evidence gives `SubmitUnverified`, even if Enter
  already reached the agent. Inspect the session before retrying. A delivered reset
  confirms observed submission. A host that needs proof of a new conversation must
  obtain it separately.
- **Input limits.** `InputLimits`, `AgentLimits` and `RpcLimits` bound pending input,
  including active writes and agent completions awaiting publication. Oversized or
  saturated input is refused without waiting on the child. Agent admission rejections
  resolve their tickets immediately and publish no delivery event. See the defaults
  and overload handling in [the user manual](docs/agents-and-delivery.md#admission-limits-agentlimits--monitor-overflow).
- **RPC limits and privacy.** `RpcLimits` bounds line sizes and queued input, including
  the active write. Oversized input and saturation return `InputTooLarge` and
  `QueueFull`. Default stdin/stdout frames allow 8 MiB, pending stdin allows 32 MiB,
  and eight output events retain at most 64 MiB raw (lossy UTF-8 can expand up to 3×).
  Oversized stdout and read failures emit terminal `RpcEvent::Error`; stdin I/O
  failures emit nonterminal `StdinError` and keep output draining. Oversized stderr
  lines become a bounded marker and draining resumes at the next line.
  `send_line_tracked(line, deadline)` adds bounded queue/write/flush receipts
  and starts its deadline worker lazily;
  uncertain writes and unknown future receipt outcomes must not be automatically
  resent. `RpcUtf8Mode::Strict` rejects
  invalid stdout UTF-8. `close_stdin()` drains accepted input before closing stdin
  while output continues. These are transport observations, not provider receipts.
  Drop or kill the handle after a terminal transport error. Payloads and arguments
  require explicit `trace: true` for logging; `trace_stderr: true` opts into stderr alone.
  Both default to false. Exit events still carry a bounded stderr tail.
  See [migration notes](CHANGELOG.md#011--2026-11-17) for new defaults and full struct literals.
- **Identity.** A child sees `TERM=xterm-256color`, `COLORTERM=truecolor` and none of the
  host terminal's well-known variables (`TERM_PROGRAM`, `TMUX`, `COLUMNS`, … — exact names;
  other vendor-prefixed ones such as `KITTY_*` pass through unless your policy removes
  them), unless the host asks for another identity; the host's environment policy always
  wins.
- **Native protocol adapters (`codex`, `acp`, `claude`) and `approvals`.**
  - `codex` ([guide](docs/rpc-and-native-adapters.md#codex-app-server-codex)): manages one root thread per connection over owned
    strict-UTF-8 stdio. Resume requires turn history (`excludeTurns` omitted or `false`)
    and treats replayed approvals as active-turn evidence; non-root/subagent notifications
    share the event stream (route by native thread ID) while non-root approval requests
    return `-32601`. Terminal results/notifications invalidate pending approvals; expired
    replies return `StaleApproval` and close with `ApprovalExpired`. Terminal state and
    observed usage commit before publication without displacing the active turn's sample.
  - `acp` ([guide](docs/rpc-and-native-adapters.md#agent-client-protocol-acp)): stable-v1 JSON-RPC subset with strict `session/load` /
    `session/resume` (no new-session fallback), cooperative `session/cancel` and
    single-use permission tokens (`Permission::request()`).
  - `claude` ([guide](docs/rpc-and-native-adapters.md#claude-stream-json-claude)): persistent custom stream-json control sessions
    where turn results never close stdin and strict `--resume=ID` refuses user input until
    matching provider identity is observed.
  - All three adapters emit `portable::EventV1` alongside optional native envelopes
    (retained by default; configurable via `spawn_with_options`), expose immutable native
    details and deadlines (`Approval::request()` in Codex; `request()` and `deadline()` in
    ACP and Claude), and preserve preceding native frames on transport failure within
    deadlines and event budgets. With `approvals` ([guide](docs/rpc-and-native-adapters.md#native-approval-callbacks-approvals)),
    `approvals::Executor` runs opt-in host callbacks on a fixed bounded worker pool without
    auto-allowing panics, invalid choices, timeouts or delegation.
- **Process helpers.** `proc::output_with_timeout` retains child ownership while
  collecting output, so timeout cleanup cannot signal a PID its reader workers released.
  Known externally reaped Unix children are never signalled; hosts must still coordinate
  their own reapers because a PID check and a later signal are not atomic.
  Both this helper and Windows `proc::status_with_timeout` reject a timeout that
  cannot form a deadline with `io::ErrorKind::InvalidInput` before launching a child.
  Their timeout budgets start before the synchronous spawn call.
  See [process helpers](docs/pty-and-screen.md#path-resolution--process-helpers-resolve-shell-proc).
- **Threads.** Calls never wait on the child. Each PTY session spawns reader and writer
  threads (an agent session two more); `RpcChild` spawns stderr, reader and writer threads
  (plus a lazy deadline worker on `send_line_tracked`), and native adapters add one
  dispatcher thread. `observers::ObserverHub` runs one worker thread, `approvals::Executor`
  runs a fixed worker pool and deadline monitor, and each `EventStream` (`tokio`) adds one
  forwarding thread. Teardown detaches session and adapter threads, never joins them.

## Agent CLIs

| Agent | Recorded profile versions | Startup handled |
| --- | --- | --- |
| claude | 2.1.293 and 2.1.296, live smoke 2.1.295 | folder-trust dialog, bypass-permissions warning |
| codex | 0.154.0 (dialog wording since 0.142.5) | pre-trust flag; trust, full-access and update prompts |
| pi | 0.85.1 | pre-trust flag; project-trust dialog |
| opencode | 1.18.21 | no startup dialog |
| cursor (**WIP**) | 2026.09.23 (trust); 2026.11.13 (`--force` login and isolated approval renderer) | pre-trust flag (saves a trust record); workspace-trust dialog; authenticated startup/composer unverified |

These versions describe the supplied screen evidence. Separate
[recorded permission checks](docs/permission-validation.md) cover Codex `0.169.0`
`config/read` and OpenCode `1.18.25` subagent external-directory reads. Cursor support is
**work in progress**: authenticated `--force` startup/composer behavior remains unverified.
The profile is available for experimentation; its live check is follow-up work before
removing the WIP label. Claude, Codex and Pi include composer fixtures.
OpenCode and Cursor composer layouts are not sufficiently measured for
verified submits; unreadable layouts return
`SubmitUnverified`. Use a measured custom profile or a structured RPC integration when
verified delivery is required.

Profiles read screen text, so a reworded CLI release can break detection; the controller
then logs a `startup markers not recognised` report. Replace a built-in with
`ProfileRegistry::register`, or write your own `AgentProfile` with the helpers in
`agent::detect`.

The built-in delivery guard recognizes Claude `AskUserQuestion`, Codex Plan mode,
OpenCode's question picker and Cursor's shell approval menu on their recorded layouts.
If one has the keyboard, a normal send ends with `Aborted(DialogBlocking)`. Pi and
custom profiles inherit `false` unless they implement `AgentProfile::question_dialog`.
[Recorded layouts and capture scope](docs/agents-and-delivery.md#questions-that-take-keyboard-input)

Compatibility declarations are available through `AgentProfile::compatibility()`;
they distinguish supported, unsupported and unknown operations and identify their
evidence. With `probe`, `probe_agents_detailed` applies the full `EnvPolicy` and can
check a direct executable in an explicit working directory. Login-shell mode remains
the default and reports that startup files may override the context. Explicit Unix
shell paths are used as given, including custom paths named `sh`; a nonexistent shell
fails to spawn. If the host working directory cannot be read, detailed readiness is
`Untested` with `WorkingDirectoryUnavailable`; the legacy `installed: false` is
inconclusive. A successful version probe does not certify a native protocol or a future CLI release. See
[compatibility and probe diagnostics](docs/permissions-and-probes.md#profile-compatibility-metadata-compatibility).

## Platforms

- **macOS**: developed and tested here; the whole suite runs against real shells.
  `openpty` called from several threads of one process at the same moment now and then
  fails (`Spawn("openpty: … Unknown error: -6")`): serialise concurrent `Terminal::spawn`
  calls, or retry that error.
- **Linux — unverified**: uses the Unix implementation; native tests are configured in CI.
- **Windows — unverified** (ConPTY, Job Objects): native tests are configured in CI;
  foreground answers are heuristic. ConPTY can retain its output pipe after child
  completion, so `Event::Exit` may not arrive while the session is alive. Drain events
  while polling `Terminal::try_wait`, then explicitly drop or kill the session when it
  is no longer needed. Process completion does not mean all output has been drained.

**Only macOS has verified native runtime results for this release.** Linux and Windows
remain implementation targets with unverified runtime support. Successful native test
results are required before extending the verified-platform claim; cross-compilation
alone does not establish runtime support.

Minimum Rust version: 1.85.

## Documentation

- **User Manual Wiki:** [docs/README.md](docs/README.md) — [Getting Started](docs/getting-started.md),
  [PTY & Screen](docs/pty-and-screen.md), [Agents & Delivery](docs/agents-and-delivery.md),
  [Permissions & Probes](docs/permissions-and-probes.md),
  [RPC & Native Adapters](docs/rpc-and-native-adapters.md),
  [Async & Presentation](docs/async-and-presentation.md),
  [Workspace Crates](docs/workspace-crates.md), and
  [Recipes & Troubleshooting](docs/recipes-and-troubleshooting.md).
- **API docs:** `cargo doc -p agent-link --all-features --open`. Each module starts with an
  overview.
- **Compatibility evidence:** [permission validation](docs/permission-validation.md).
- **Contributing and releases:** [CONTRIBUTING.md](CONTRIBUTING.md).
- **Changes:** [CHANGELOG.md](CHANGELOG.md).

## Development

- Routine changes (from the repository root): run related tests and
  `python3 scripts/check.py --skip-tests` for formatting, feature checks, Clippy and
  rustdoc with warnings denied across all four workspace crates (`agent-link-supervisor`
  bundles SQLite, so workspace builds require a C compiler).
- Release gate: `python3 scripts/check.py` also runs the full non-live test suite.
- CI is configured to run the gate on Linux, macOS and Windows, and to check the declared
  MSRV separately. Only macOS native results have been verified.
- See [CONTRIBUTING.md](CONTRIBUTING.md) for targeted test commands.
- The tests drive real shells and a scripted stand-in agent, the test-support binary
  `agent-link-fake-agent` (enabled only by the development feature `test-support`).
  Live smokes against the real CLIs are
  `#[ignore]`d: they make real API calls.
- `examples/capture.rs` records a CLI's PTY session (raw stream, terminal signals, final
  screen) as a test fixture.

## Optional presentation and supervision layers

The optional `observers` feature provides bounded transport-neutral PTY fanout through
`observers::ObserverHub`. It continuously drains one recovery receiver (or attaches live
with `Terminal::attach_live_recovery` / `attach_live_recovery_metadata`) and disconnects
slow viewers with an independently readable reason. See [observer contracts](docs/async-and-presentation.md#pty-observer-fanout-observers-feature)
for byte/item budgets, process-local cursors, explicit gaps, and the event whitelist.

The optional `transcript` feature provides a [bounded presentation projection](docs/async-and-presentation.md#transcript-projection-transcript-feature)
(`transcript::Projector`) with local IDs/revisions, source provenance and explicit gaps;
screen text never establishes delivery, readiness or completion.

The separate unpublished `agent-link-supervisor` workspace crate ([guide](docs/workspace-crates.md#agent-link-supervisor-supervisor))
adds durable SQLite operation receipts, exclusive worker ownership, strict
resume/re-adoption fences, attachment leases and independent stop control for
application-hosted workers. Its optional `codex` feature supplies a bounded owned-stdio
runtime with strict resume, exact terminal receipt correlation and observable direct-child
cleanup.

The separate unpublished [local daemon](docs/workspace-crates.md#agent-link-daemon-daemon) (`agent-link-daemon`) hosts one
durable Codex worker over authenticated bounded loopback IPC on Unix (Windows compilation
alone is not runtime evidence). It preserves crash and cleanup fences until explicit
evidenced recovery.

The separate unpublished `agent-link-http` workspace crate provides authenticated,
read-only loopback HTTP snapshots and bounded raw SSE observers, with explicit
resync/provenance and optional host-projected transcript JSON. See [HTTP presentation](docs/workspace-crates.md#agent-link-http-http).

## License

Licensed under the [MIT License](LICENSE).
