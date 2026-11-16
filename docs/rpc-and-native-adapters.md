# Headless Stdio (`rpc`) & Native Protocol Adapters

This chapter covers structured stdio integrations: the generic line transport (`rpc`), versioned portable events (`portable`), the three bounded native adapters (`codex`, `acp`, `claude`), and opt-in approval callbacks (`approvals`).

**Prev:** [Permissions & Probes](permissions-and-probes.md) · **Next:** [Async & Presentation](async-and-presentation.md)

---

## Generic JSON-Lines RPC (`rpc`)

Enable `rpc` (implies `agent`) to run any headless CLI over line-delimited stdio without a PTY.

### Spawning and `preflight`

```rust
use agent_link::agent::{ProfileRegistry, RpcLaunchOptions};
use agent_link::rpc::{preflight, RpcChild, RpcEvent, RpcSpawnSpec, RpcUtf8Mode};

let launch = ProfileRegistry::builtin()
    .get("pi")
    .unwrap()
    .rpc_launch_with_options(&RpcLaunchOptions::default())?;

let mut spec = RpcSpawnSpec::new(launch, std::env::current_dir()?);
spec.stdout_utf8 = RpcUtf8Mode::Strict; // Reject invalid stdout UTF-8

preflight(&spec)?; // Validates limits, cwd, and platform-specific command construction
let (child, events) = RpcChild::spawn(spec)?;
```

On Unix, `preflight` checks shell arguments and quoting; the spawned login shell
resolves the agent executable. On Windows, `preflight` also resolves the launcher
on the child's effective PATH before spawning.
Preflight does not reserve resources or guarantee spawn or agent startup. In particular,
an absent Unix launcher can pass both preflight and spawn, then emit `RpcEvent::Exit`.
Inspect its stderr tail and use `RpcChild::try_wait` to observe the shell's exit status.

- **Unix:** spawns via login shell (`<shell> -l -i -c 'exec ...'`, or `<shell> -c` for `sh`) in a new session (`setsid`).
- **Windows:** resolves launcher directly on `env.effective_path()` with `CREATE_NO_WINDOW`.
- **Threads per child:** `stderr`, `reader`, `writer`, plus an on-demand `deadlines` worker started on the first `send_line_tracked` call. All threads are detached on teardown.

### `RpcLimits` Defaults

| Field | Default | Meaning |
| --- | --- | --- |
| `max_stdout_line_bytes` | `1 MiB` | Max raw stdout frame including delimiter. |
| `max_stderr_line_bytes` | `64 KiB` | Max raw stderr frame including delimiter. |
| `max_stdin_line_bytes` | `1 MiB` | Max stdin frame including appended `\n`. |
| `max_pending_stdin_bytes` | `4 MiB` | Queued + active write bytes. |
| `max_pending_stdin_lines` | `1,024` | Queued + active lines (also caps pending receipts). |
| `event_capacity` | `64` | Bounded `Receiver<RpcEvent>` capacity. |

### Writing, Tracked Receipts & Half-Close

- `child.send_line(line)` — queues a line (`\n` appended). Returns `Error::InvalidInput` (if `line` contains `\n`), `Error::InputTooLarge`, `Error::QueueFull`, or `Error::Closed`.
- `child.send_line_tracked(line, deadline: Instant) -> Result<RpcWriteReceipt>`:
  - Enforces an absolute deadline covering queueing, writing, and flushing.
  - Inspect via `receipt.status()`, `receipt.wait()`, or `receipt.wait_until(deadline)`. Dropping a receipt does **not** cancel the write.
  - `RpcWriteOutcome`:
    - `Written`: entire line + `\n` written and flushed before deadline.
    - `NotWritten(reason)`: writer never started this line (safe to know nothing was sent).
    - `PossiblyWritten(reason)` (or `_`): writing began; zero, some, or all bytes may have reached the child. **Never automatically resend.**
- `child.close_stdin()` — rejects new sends immediately, finishes accepted lines in FIFO order, then drops stdin while stdout/stderr continue draining.
- `child.reap_observation().status()` — non-blocking direct-child reap evidence (`Pending`, `Reaped`, `Unknown`).
- `child.kill()` / `Drop` — tears down process group/tree and reaps bounded at ~2 s.

### `RpcEvent` & Transport Errors

- `RpcEvent::Line { id, seq, line }` — one decoded stdout line (trailing `\n` then `\r` stripped).
- `RpcEvent::Error { id, error: RpcTransportError }` — terminal error (`LineTooLong`, `InvalidUtf8`, `WriteDeadlineExceeded`, `Io`). No `Exit` follows; drop or kill the child.
- `RpcEvent::Exit { id, stderr_tail }` — natural stdout EOF without teardown; `stderr_tail` retains up to 32 lines / 4 KiB.

---

## Portable Event Projection (`portable`)

Enabled automatically whenever `codex`, `acp`, `claude`, or `transcript` is enabled.

`portable::EventV1` (`EventV1::VERSION == 1`) provides a conservative cross-provider observation alongside optional raw native JSON:
- `context: EventContext`: `provider` (`Provider::Codex | Acp | Claude`), `generation: SessionId` (local, never a resume token), `seq: u64` (transport line sequence), optional `session_id`, `turn_id`, `item_id`, `request_id: Option<RequestId>` (`String` vs `Number`). Missing IDs stay `None`.
- `native_method: String`
- `kind: EventKind`:
  - `SessionStarted`, `TurnStarted`, `TurnCompleted { status: CompletionStatus }` (`Completed`, `Interrupted`, `Failed`, `Unknown`)
  - `Text { text, update: TextUpdate, phase: TextPhase }` — `Delta` appends; `Replace` replaces accumulated item text; `Commentary` / `Final` / `Unknown`
  - `Tool { tool: ToolKind, state: ItemState }` (`Command` | `FileChange`, `Started` | `Completed`)
  - `Usage(Usage)` — `total: TokenUsage`, `last: TokenUsage`, `model_context_window: Option<u64>` (do **not** sum cumulative updates)
  - `ApprovalRequested`, `ApprovalResolved`, `Other`

All three native adapters default to `NativeEvents::Retain` and accept `NativeEvents::Omit` via `spawn_with_options` to discard raw event envelopes while keeping snapshots, outcomes, and approval details.

---

## Codex App-Server (`codex`)

Enable `codex` (implies `rpc`, adds `serde_json`). Manages **one root thread per connection** over owned strict-UTF-8 stdio. Runnable example: [`examples/codex_app_server.rs`](../examples/codex_app_server.rs).

### Lifecycle & Methods (`CodexAppServer`)

1. `CodexAppServer::spawn(spec, CodexLimits::default())` or `spawn_with_options(spec, CodexOptions)` -> `(app, events)`.
2. `app.initialize(name, version, deadline)` -> `RequestTicket` (automatically queues `initialized` notification on success).
3. `app.thread_start(params, deadline)` or `app.thread_resume(thread_id, params, deadline)`.
   - `thread_resume` requires turn history: omit `excludeTurns` or set it to `false` (passing `true` or non-boolean fails with `InvalidInput`).
4. `app.turn_start(text, deadline)`, `app.turn_steer(expected_turn_id, text, deadline)`, `app.turn_interrupt(expected_turn_id, deadline)`.
5. Poll `events.recv_until(deadline)` for `CodexEvent { generation, seq, portable, native, approval }`.
6. Check `app.snapshot()` (`initialized`, `server`, `thread_id`, `thread`, `active_turn`, `last_turn`, `last_usage`, `failure`) and `app.failure()`.

### Key Contracts

- **Turn guards & replayed approvals:** `turn_start` fails with `CodexError::Busy` while `active_turn` is present or an active resume awaits turn identification. Replayed approvals count as active-turn evidence. `turn_steer` and `turn_interrupt` require the observed active turn ID (`CodexError::TurnMismatch` otherwise).
- **Subagents:** notifications for non-root threads share `events` without modifying `snapshot()`; route by `event.portable.context.session_id`. Non-root approval requests are rejected with `-32601` (`approval: None`).
- **Approvals:** `Approval::request()` exposes immutable `ApprovalRequest` (`context`, `id`, `method`, `params`, `deadline`, and `.decisions()`). Call `approval.respond(decision)` (`"accept"`, `"acceptForSession"`, `"decline"`, `"cancel"`, or offered amendment objects).
  - Terminal results/notifications invalidate pending approvals for that turn.
  - Answering an expired token returns `Err(CodexError::StaleApproval)` **and** stops the connection with `Failure::ApprovalExpired`.
  - Answering an unoffered decision returns `Err(CodexError::InvalidDecision)` **and** stops the connection with `Failure::Protocol`.
- **Budgets (`CodexLimits`):** `max_frame_bytes` 1 MiB, `pending_requests` 32 / 4 MiB, `control_capacity` 8 / 2 MiB, `approvals` 16 / 2 MiB, `events` 64 / 4 MiB, `approval_timeout` 300 s, `control_timeout` 10 s. Event queue saturation stops the connection with `Failure::ConsumerLagged`.

---

## Agent Client Protocol (`acp`)

Enable `acp` (implies `rpc`, adds `serde_json`). Implements a bounded **stable-v1** JSON-RPC 2.0 subset (`protocolVersion: 1`, `clientCapabilities: {}` — no delegated fs/terminal methods; unsupported reverse requests receive `-32601`). Runnable example: [`examples/acp_prompt.rs`](../examples/acp_prompt.rs).

### Lifecycle & Methods (`AcpClient`)

1. `AcpClient::spawn(spec, AcpLimits::default())` or `spawn_with_options(spec, AcpOptions)` -> `(client, events)`.
2. `client.initialize(name, version, deadline)` -> negotiates `protocolVersion: 1`.
3. One session per connection (all require an **absolute** `cwd`):
   - `client.session_new(cwd, deadline)`
   - `client.session_load(session_id, cwd, deadline)` (requires `agentCapabilities.loadSession == true`)
   - `client.session_resume(session_id, cwd, deadline)` (requires `agentCapabilities.sessionCapabilities.resume` object; never falls back to `session/new`)
4. `client.prompt(text, deadline)` (one outstanding prompt at a time; completes only on native `session/prompt` response, not updates).
5. `client.cancel(deadline)` — cooperative `session/cancel` notification that atomically revokes pending permissions with `cancelled` outcomes; the prompt stays active until its own response arrives.
6. `Permission` tokens in `AcpEvent::permission`: read `perm.request()`, `perm.request_id()`, `perm.origin()`, `perm.deadline()`, then call `perm.select(option_id)` or `perm.cancel()`. Unlike Codex, passing an invalid `option_id` returns `Err(AcpError::InvalidDecision)` without consuming the token or closing the connection.

A completed prompt ticket can coexist with unread text events. When displaying the
answer, continue draining through that prompt's matching response event, then check the
ticket outcome. Keep a finite deadline and report stream failure or timeout.

---

## Claude Stream-JSON (`claude`)

Enable `claude` (implies `rpc`, adds `serde_json`). Implements Claude's persistent custom control protocol (not JSON-RPC). Runnable example: [`examples/claude_session.rs`](../examples/claude_session.rs).

### Launch & Lifecycle (`ClaudeSession`)

1. Build launch flags with `claude::launch(&Permissions::Unattended, stdio_approvals)`:
   - Adds `--output-format stream-json --input-format stream-json --verbose`, unattended `--permission-mode bypassPermissions`, and optionally `--permission-prompt-tool stdio`. (Note: bypass mode or CLI rules may still shadow tool ask prompts.)
2. `ClaudeSession::spawn(spec, Limits::default(), resume)` or `spawn_with_options(spec, Options, resume)` -> `(session, events)`:
   - Rejects conflicting CLI resume flags (`--resume`, `--continue`, `-r`, `-c`, etc.) in `spec.launch.args`.
   - When `resume: Some(id)` is passed, appends `--resume=<id>` and blocks `send`/`interrupt` until `initialize` succeeds and matching `session_id` is observed (fail-closed; some CLI versions emit `session_id` only after first input, making pre-input strict resume unavailable).
3. `session.initialize(deadline)`, `session.send(text, uuid, deadline)`, `session.interrupt(deadline)`.
   - Turn `result` envelopes never close stdin (`Snapshot::last_result` commits before event publication).
   - `Limits` defaults: `frame_bytes` 1 MiB, `pending` 16 / 4 MiB, `control_pending` 4 / 2 MiB, `approvals` 8 / 1 MiB, `events` 64 / 4 MiB, `tasks` 16 / 1 MiB, **`approval_timeout` 60 s**.
   - `Approval::respond(updated_input, denial)`: supply either a JSON object `Some(&obj)` for `updated_input` OR `Some(message)` for `denial` (`None, None` sends a host denial).

`Outcome::Written` confirms the input write. To report a successful turn, consume
its native result and require a successful terminal status. Timeout or disconnection
before that result, and failed or unknown result statuses, must not be reported as
success or trigger an automatic resend. A later EOF does not erase an already
observed successful result.

---

## Validation Evidence

The native adapters have pinned synthetic protocol and offline pipe coverage. These fixtures establish the implemented subset; they do not establish authenticated model execution, universal provider interoperability, or Windows native runtime behavior.

| Adapter | Pinned evidence | Scope |
| --- | --- | --- |
| Codex | [Provenance](../tests/fixtures/codex-app-server/provenance.json) and [v2 projections](../tests/fixtures/codex-app-server/v2.json), revision `b1573f102413e96a6481087922bf425231222dee` | Authored protocol projections and fake-peer lifecycle tests; no complete upstream schema or live-turn claim. |
| ACP | [Stable-v1 projections](../tests/fixtures/acp/stable-v1.json), schema crate 1.17.0 and Rust SDK 5.2.0 | Supported methods, routing, permissions, and failure paths against an offline peer. |
| Claude | [Fixture provenance](../tests/fixtures/claude-stream-json/README.md), Python SDK 0.2.182 / bundled CLI pin 2.1.325 | Synthetic control envelopes and persistent fake-peer sessions; pre-input resume identity ordering remains unverified against a live CLI. |

Profile screen captures and the [permission launch checks](permission-validation.md) provide separate operation-scoped evidence. A successful launch or probe does not certify native turn handling.

---

## Native Approval Callbacks (`approvals`)

Enable `approvals` alongside `codex`, `acp`, or `claude` to run opt-in approval callbacks on a fixed bounded thread pool without blocking protocol dispatchers.

```rust
use std::time::{Duration, Instant};
use agent_link::approvals::{CodexDecision, Executor, Limits};

let executor = Executor::new(Limits::default())?; // 2 workers, 32 requests, 1 MiB payload budget
// Inside your event loop, when `event.approval` is `Some(token)`:
let deadline = Instant::now() + Duration::from_secs(30);
let ticket = executor.codex(token, deadline, |req| {
    // Inspect req.method, req.params, req.decisions()
    CodexDecision::Decision(serde_json::json!("decline"))
})?;
```

- **Submit methods:** `executor.codex(...)` (`CodexDecision::Decision | Cancel | Delegate`), `executor.acp(...)` (`AcpDecision::Select | Cancel | Delegate`), `executor.claude(...)` (`ClaudeDecision::Allow | Deny | Delegate`).
- **Delegation:** `Delegate` sends no reply and settles `Outcome::Delegated`. Keep a clone of the native token before calling `executor.<provider>(...)` if your callback might delegate.
- **Safety:** Callback panics and invalid decisions fall back to conservative cancel/deny (never auto-allow). Effective deadline is `min(host_deadline, capability_deadline)` including queue wait. Hung Rust callbacks retain their worker and admission capacity until they return; `ticket.cancel()` and `executor.close()` settle tickets independently without waiting on hung callbacks.
