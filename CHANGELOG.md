# Changelog

Notable changes to `agent-link`, in the [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
format. Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html); before
1.0 a minor version may break the API.

## [0.1.0] — unreleased

The first version. Nothing is published yet; changes until the first release go into this
entry.

### Changed during publication hardening

- Added the public GitHub repository URL to package metadata and installation
  examples. The crate remains unreleased on crates.io.

- Clarified that agent status snapshots do not fence queued state/activity events;
  hosts displaying current status should refresh the snapshot on those events.
  RPC preflight documentation now distinguishes Windows launcher lookup from Unix
  shell-time resolution and does not promise successful replacement startup.

- Made the async shell quickstart and example match output across chunk boundaries
  with bounded retained bytes and a finite deadline, use platform-specific commands,
  and perform asynchronous teardown on success and failure.

- Discard stale delivery-profile callback results when output or screen revisions
  change during evaluation. Delayed callbacks cannot turn an obsolete empty composer
  into verified submission or use stale paste/dialog evidence to permit Enter.

- Fixed the Unix and Windows `proc::output_with_timeout` PID-reuse race by retaining
  child ownership in the caller through the timeout/signalling decision. Pipe readers
  cannot reap the child, and known externally reaped Unix children are never signalled.
  Reader startup failures now return I/O errors with child cleanup. The timeout budget
  starts before spawning; unrepresentable timeouts return `InvalidInput` before launch.

- Marked Cursor support as work in progress, with authenticated `--force`
  startup/composer checks tracked as follow-up validation. Scoped verified native
  runtime support to macOS; Linux and Windows remain unverified implementation targets
  with native CI configured. Updated the release checklist to match this scope.

- Windows `proc::status_with_timeout` now rejects unrepresentable deadlines with
  `io::ErrorKind::InvalidInput` before spawning, instead of panicking after launch.
  The timeout budget now starts before the synchronous spawn call.
- Corrected examples to pair transcript screens with their recovery fences, drain
  ACP text through the matching prompt response, and require a successful Claude
  turn result before reporting success. Added offline example regressions; no new
  live compatibility claim. Timeout and disconnection before a result remain failures.

- Track and package the public user manual and permission validation evidence, keeping
  built-in compatibility artifact paths usable in clean checkouts and crate archives.
  Local `internal-docs` engineering notes are now ignored and excluded from packages;
  public documentation and release instructions no longer depend on them. Corrected
  manual recipes and added documentation boundary/link checks to the static gate.

- Added unpublished `agent-link-http` workspace presentation: authenticated loopback
  geometry/fence snapshots, bounded raw SSE and explicit reconnect/resync, optional
  bounded host-projected JSON, finite client/header/frame/write limits and no remote
  mutation or delivery inference. Added no-display atomic recovery metadata attachment
  so observer startup and geometry snapshots do not clone host screen strings.

- Added optional dependency-free `transcript` presentation projection with bounded active
  text, local message IDs/revisions, native origins, fenced heuristic screen views,
  explicit truncation/eviction/gaps and no inferred delivery or lifecycle outcomes.

- Added unpublished `agent-link-daemon` local Unix service/client above the supervisor,
  with authenticated bounded loopback IPC, independent native dispatch, exact durable
  receipt reconciliation, finite leases and reserved stop admissions. Runtime cleanup
  fences remain intact after crash and controlled shutdown; core defaults are unchanged.

- Approval workers now contain panic-payload destructor panics by closing the pool
  and settling tickets, rather than silently losing a fixed worker. Secondary panic
  payloads are exceptionally retained with a finite admitted-request count bound.

- Added opt-in `approvals` fixed callback workers for Codex, ACP and Claude. Queued
  and active requests share finite item/payload-byte budgets; callback deadlines,
  cancellation and shutdown settle authoritative tickets independently of hung Rust
  callbacks, whose workers and capacity stay charged. Delegation preserves the host
  token; panic/invalid decisions never auto-allow. Native reply tickets remain distinct
  from callback outcomes and tool execution. ACP original request metadata is now
  read through `Permission::request()` instead of a replaceable public field.


- Added optional `observers` transport-neutral bounded PTY fanout, with bounded history,
  per-viewer and aggregate byte/item budgets, finite viewers, atomic replay/live fences,
  explicit unavailable-history ranges, and independent slow-viewer disconnect reasons.
  Added `Terminal::attach_live_recovery` under that feature to avoid replay copying at
  startup. No network service, authentication, runtime or parser checkpoint is included.


- Added optional `acp` stable-v1 owned-stdio adapter: bounded requests/events, strict
  load/resume without fallback, text prompts, cooperative cancellation, single-use
  permission capabilities and conservative portable projections. Offline controlled
  and real-pipe tests provide subset evidence; no live provider claim or SDK/runtime
  dependency. Existing profile launch permission defaults remain unchanged.

- Added optional `claude` native stream-json sessions with persistent initialization,
  correlated interrupt controls, bounded raw/portable events, result snapshots, exact
  provider resume binding and revocable single-use approval tokens. Results never
  close stdin or imply idle/background-task completion. Offline synthetic evidence
  only; strict resume waits for provider identity and may be unavailable before input.


- Added the separate, unpublished `agent-link-supervisor` workspace crate: synchronized
  SQLite operation intent and retained SHA-256 ID/digest receipts; exclusive lifetime
  ownership locks and durable birth identities; strict session binding, explicit
  re-adoption and cleanup fences; exact-turn interrupt records with separate admission;
  finite persistence/lease budgets and stop independent of ordinary work. Applications
  provide worker hosting, IPC and the native runtime/evidence implementation. Unknown
  operations are never resent or evicted. Core dependencies, features and Drop behavior
  remain unchanged. Added offline crash/launcher tests and workspace static/MSRV gates;
  the coordination crate itself is not a daemon and makes no live compatibility claim.
  The separate local Codex daemon is described in its own entry above.
- Added optional supervisor `codex` owned-stdio runtime with strict session resume,
  exact ID/digest/runtime terminal correlation, bounded continuous event drain and
  independent stop. Added actual owned-child reap and dispatcher completion evidence;
  external reaping/errors remain unknown and process-tree quiescence is not claimed.

- Fixed a recovery-fence regression where blocked controller-event fanout could hide
  already-read PTY output from delivery checks. The reader now announces each chunk
  before waiting for publication; rendering and log commit remain atomic for recovery.
  Old screen observations are invalid while announced output is pending.

- Added opt-in `Terminal::attach_recovery` with session-bound byte cursors, typed
  cross-session/future-cursor refusals, retained ranges, explicit loss metadata, current
  geometry and a display snapshot pinned to the replay/live boundary. Session IDs are
  process-local generations; displays are not parser checkpoints. Added `Replay::FileRange`
  for bounded file segments; failed logs now retain their path lock until teardown so
  another session cannot erase a recoverable prefix. Existing raw events, `attach` and
  receiver backpressure remain unchanged. Remote observer adapters remain deferred.

- Fixed `login_shell_argv` to preserve the selected `sh` program while using `-c`.
  Probes and RPC launches now honor custom `sh` paths instead of substituting `/bin/sh`;
  missing overrides fail to spawn. Probe reports name the program actually attempted.
  Callers that relied on the old substitution should explicitly select `/bin/sh`.

- Fixed Windows probe resolution to skip empty PATH segments consistently with RPC,
  avoiding implicit workspace-local launcher selection. Shared Windows suffix expansion
  now examines only the filename, so extensionless paths under dotted parent directories
  (including `..`) resolve correctly.

- Added operation-scoped `AgentProfile::compatibility()` metadata and Codex protocol
  provenance, separating declarations, synthetic fixtures, captured terminals and live
  checks. Added `probe_agents_detailed` with explicit cwd/direct mode, executable paths,
  bounded version output and typed command/readiness diagnostics. Existing `ProbeResult`
  remains unchanged. Both probe APIs now apply the full environment policy, including
  config-directory values and `inherit: false`; callers relying on the previous ignored
  values must adjust their policy. Capture drains excess output after 8 KiB per stream;
  truncated or invalid UTF-8 login output remains unknown. Login-shell startup files may
  override context and Windows PATHEXT resolution still uses the host's setting. No
  version allowlist or new live compatibility claim was added.

- Fixed Codex usage retention across interleaved turns: an observed active turn's
  sample survives late other-turn updates, and reobserving the same latest terminal
  outcome preserves its matching usage. Retention stays bounded to the latest root,
  active-turn and latest-terminal samples, without a per-turn history.

- Added the versioned `portable::EventV1` host projection to Codex events, preserving
  provider/session/turn/item/request identity and transport generation/order. Text deltas,
  replacements and explicit phases stay separate from tool lifecycle, usage and turn
  completion; unknown messages retain their native method/envelope. `CodexOptions` and
  `spawn_with_options` allow native event/usage envelopes to be omitted, retaining them
  by default. Event and approval byte budgets now include extracted origin/projection
  strings and attached capability data, so tight budgets can reach overload earlier.
  `Snapshot::last_usage`, `TurnOutcome::context` and matching `TurnOutcome::usage` commit
  before publication. Approval tokens expose immutable origin, native details, deadlines
  and supported offered choices without installing host callbacks. Pre-release callers
  must handle `CodexEvent::native` as `Option<Arc<Value>>` and new snapshot/outcome fields;
  `CodexEvent` is now non-exhaustive. Payload logging still requires explicit trace opt-in.
  Synthetic fixtures and subprocess tests cover this projection; no new live CLI claim.

- Added optional `codex` owned-stdio app-server pilot with a shared bounded lifecycle:
  generation-scoped request tickets, absolute deadlines, one native thread per
  connection, start/resume, text turns, explicit steering/interruption and command/file
  approvals. Native events/results retain identities and unknown fields. Host event
  saturation terminates explicitly; sticky failure and tickets remain readable. Control
  writes have reserved admission. Default launch policy remains unattended. The only
  added optional runtime dependency is `serde_json`; Rust 1.85 remains supported.
  ACP/Claude, sockets, reconnect and durable recovery remain outside this pilot.
  Synthetic protocol and real-pipe tests do not claim live Codex interoperability.
  Replayed command/file approvals preserve active-turn identity even when resume omits
  an `inProgress` turn: starts remain blocked, exact-turn controls remain available
  after resume, and resolving an approval does not falsely mark the turn idle.

- Fixed Codex close racing response or shutdown publication: tickets remain available
  for synchronous settlement, preserving observed native results/errors and keeping
  wakeups outside the controller lock. Terminal turn results now invalidate pending
  approvals just like notifications, preventing stale replies and spurious expiry.

- Codex resume now refuses `excludeTurns: true` and non-boolean values before admission,
  so metadata-only responses cannot bypass `turn_start`'s busy guard. Callers must omit
  `excludeTurns` or set it to `false`.
  Explicit activity without a turn ID now remains busy after resume completion,
  including before approval replay; exact-turn controls wait for an observed ID.

- Delayed Codex resume responses with empty or omitted turn history no longer restore
  busy after replay has identified and completed the active turn. Unrelated completions
  still cannot clear unidentified activity. Oversized or excessively nested request
  JSON is structurally rejected before controller locking, keeping its destruction
  independent of shutdown and ticket settlement.

- Expired Codex approval tokens now consistently return `StaleApproval`, even when
  `respond` detects expiry before maintenance. Terminal `ApprovalExpired` behavior
  is preserved; valid decisions are no longer mislabeled `InvalidDecision`.

- Codex now forwards subagent and other non-root notifications through its bounded
  event stream instead of terminating the connection. Only correlated start results
  or explicit resume admission select the root; child events cannot alter its snapshot
  or approvals. Hosts must route native events by thread ID. Non-root approval requests
  receive -32601 without a host token; child-thread control remains unsupported.

- Codex write-receipt failures and sends refused with `Closed` now stop dispatch and
  drain preceding native frames through the ordered transport terminal event, retaining
  on-time responses and terminal-turn evidence. Absolute deadlines and event limits
  remain enforced. Receipt failures use `Eof`, `Transport` or deadline outcomes;
  `WriteFailed` covers other transport admission failures.

- Added RPC tracked writes with bounded receipts and absolute queue/write/flush
  deadlines, optional strict stdout UTF-8 validation, and ordered stdin half-close.
  Partial or failed writes invalidate input; receipts settle independently of blocked
  pipes and event consumers. `send_line()` and lossy decoding remain the defaults.
  `RpcSpawnSpec` struct literals must add `stdout_utf8` (constructors are unchanged).
  A deadline worker starts lazily on the first tracked admission; startup failure
  returns `Error::Spawn` without accepting input. Late stdin failures after stdout
  EOF preserve the pending `Exit` and its stderr tail.
  `RpcUtf8Mode` is non-exhaustive; unknown write outcomes must be treated like
  `PossiblyWritten`, without automatic resend. Closed input consistently returns
  `Closed` before validation errors. Flush completion is timed before lock contention,
  deadline monitoring avoids writer-only wakeups, and strict UTF-8 decoding validates
  each frame once.

- Renamed the standalone crate from `agent-pty` to `agent-link` before its first release
  to reflect both PTY and structured stdio support. Rust imports now use `agent_link`;
  tracing uses `AGENT_LINK_TRACE`, and the test helper is `agent-link-fake-agent`.

- Reorganized the source archive into a standalone Cargo project with an MIT license,
  explicit package contents, contributor/release guides and a native platform CI matrix.
  Fixture executables now require the development-only `test-support` feature.
- Unified profile launch permissions: PTY and RPC default to YOLO/unattended mode.
  `Permissions::AgentDefault` explicitly preserves CLI policy; `Custom` supplies
  replacement flags and environment values. Pre-release integrations that relied on
  the previous PTY default must select `AgentDefault` explicitly.
- Added `RpcLaunchOptions` and fallible `AgentProfile::rpc_launch_with_options`;
  unsupported profiles refuse policy selection instead of silently ignoring it.
  `rpc_launch(resume, probe)` remains the unattended convenience call. Existing custom
  profiles must implement the new method to accept explicit options. The options struct
  is `#[non_exhaustive]`; build it with `default()` and the `with_resume`, `with_probe`
  and `with_permissions` methods.
- Codex RPC now sets `approval_policy="never"` and `sandbox_mode="danger-full-access"`.
  Claude's exported `CLAUDE_RPC_FLAGS` now contains transport flags only; the profile
  applies permission flags separately. OpenCode's unattended env default is now
  `{"*":"allow"}`, with the former external-directory workaround available explicitly as
  `OPENCODE_EXTERNAL_DIRECTORY_WORKAROUND`. Cursor unattended launches use `--force`.
- Added explicit `RpcLaunch::env` overrides with the same precedence as PTY launch
  environments. Hosts constructing `RpcLaunch` with struct literals must add `env` (or
  use `RpcLaunch::new`). Environment values stay redacted from `Debug` output.
- Verified the main `codex app-server -c ...` path with isolated live `config/read`
  checks on 0.169.0, and OpenCode's wildcard default with real subagent external-file
  reads on 1.18.25 using a local scripted model peer. Added reproducible live checks
  and a Cursor 2026.11.13 `--force` login fixture. Cursor remains work in progress:
  authenticated composer validation is follow-up work before removing that label,
  not a publication gate. No verified-submission support is added.
- Folder pre-trust and automatic dialog steering require explicit opt-in. Unknown
  foreground ownership refuses delivery instead of treating an armed launch as proof
  of ownership.
- Submitted delivery now requires observed paste text followed by a readable composer
  transition after Enter. Background output, silence and unknown composer states cannot
  confirm submission. Automatic Enter retries are disabled by default; explicit retry
  configuration can cause duplicate submission when a CLI redraws late.
- Teardown resolves admitted tickets even when an event consumer or a host callback is
  blocked. Final delivery events during teardown are best effort; tickets are authoritative.
- Moved custom profile classification outside the controller lock, with screen revision
  checks before accepting results. A bounded internal monitor disables automation on
  overflow while preserving raw terminal output and authoritative ticket outcomes.
- PTY teardown recognizes an externally reaped child and avoids signaling that known
  stale PID. Hosts must still coordinate external reapers with session teardown.
- Added configurable PTY input and agent-job admission limits. Active writes and
  unpublished completions retain capacity. Rejected agent admissions resolve tickets
  immediately without queuing events or starting the agent; monitor wakes are coalesced.
- Added bounded RPC framing, stdin admission and output queues, with typed transport
  errors. RPC payloads, command arguments and stderr enter logs only with `trace: true`.

### Added

Modules (feature in brackets; `screen` and `agent` are on by default):

- `pty`: `Terminal` runs a shell (`SpawnMode::Shell`, the user's login shell by default) or a
  program (`SpawnMode::Direct`) on a PTY from a `SpawnSpec` — `write` (non-blocking, in call
  order), `resize`, `interrupt`, `foreground` (`Foreground`: an `owner` —
  `ForegroundOwner::{Shell, Agent, Unknown}` — and a `confidence` — `Confidence::Exact` on
  unix, `Heuristic` on Windows), `subscribe`, `attach` (a `Replay` of the session log, no
  gap and no overlap), `try_wait` (`ExitInfo`), `kill`; with `screen` also `screen` and
  `repaint_stream`. `Event`, `ProgressState`, `SessionId`, `PtySize`, `ScreenConfig`,
  `SessionLogConfig` (`Off`, `Memory`, `File`), `SessionRegistry`, `forward_to`.
- `env`: `EnvPolicy` (`set`, `remove`, `path_prepend`, `inherit`; `effective_path`, `apply`),
  `EnvTarget` for `std::process::Command` and portable-pty's `CommandBuilder`, `prepend_path`.
- `shell`: `login_shell`, `shell_base`, `login_shell_argv`, `ShellSpec`.
- `identity`: `TerminalIdentity` (`Honest`, `Impersonate(KnownTerminal)`, `Custom`).
- `resolve`: `find_on_path`, `find_in_path_os`, `DEFAULT_PATHEXT`, `LAUNCHABLE_EXTS`.
- `proc`: `kill_tree`, `system32_program`, `output_with_timeout`; on Windows also
  `kill_process_tree`, `no_console_window`, `status_with_timeout`.
- `error`: `Error`, `Result`.
- `screen` [`screen`]: `ScreenModel`, `Vt100Model` (a `vt100` panic is contained: the model
  restarts blank and reports it), `ScreenSnapshot`, `TapSignal`, `PromptMark`, `normalize`,
  `ScreenText`.
- `agent` [`agent`]: `AgentTerminal` (`spawn`, `launch`, `send`, `reset`, `reset_and_send`,
  `confirm`, `interrupt`, `state`, `blocking`, `activity`, `status`, `terminal`),
  `AgentSpec`, `AgentState`, `Activity`, `Timing`; delivery: `SendOptions`, `ShellFallback`,
  `Revalidate`, `Ticket`, `TicketId`, `DeliveryOutcome`, `AbortReason`; profiles:
  `AgentProfile`, `Dialog`, `DialogKind`, `TrustKey`, `ScreenState`, `SignalHints`, `Unlock`,
  `LoginCheck`, `RpcLaunch`, `ProfileRegistry`, `profiles` (claude, codex, pi, opencode,
  cursor), `detect` (helpers for a host's own profile); launch: `Permissions`, `PreTrust`,
  `TrustPolicy`, `ForegroundPolicy`, `LaunchPlan`, `ShellKind`.
- `rpc` [`rpc`]: `RpcChild` (`spawn`, `id`, `pid`, `send_line`, `try_wait`, `kill`),
  `RpcSpawnSpec`, `RpcEvent`, `preflight`, `forward_rpc`, `frame_line`.
- `probe` [`probe`]: `probe_agents`, `ProbeResult`, `classify_login`.
- `async_api` [`tokio`; Tokio's `sync` and `rt` only]: `EventStream<E = Event>` (`new`,
  `next`, `poll_next`; re-exported at the root), `Terminal::events_stream`,
  `Ticket::outcome`, `AgentTerminal::send_async`, `kill_async` on `Terminal`,
  `AgentTerminal` and `RpcChild`.
- Examples: `bare_shell`, `agent_send` and `rpc_child` (quoted in the README), `capture`
  (records a CLI's PTY session as a test fixture).

Behavior contracts and integration guidance are documented in the README and
[user manual](docs/README.md). In particular, screen verification is
terminal evidence rather than a protocol acknowledgement; applications requiring
correlated requests and completion should use a vendor's structured protocol.

### Known limitations

- Windows is cross-compiled and linted only, never run on a Windows host; its foreground
  answers are heuristic.
- macOS: `openpty` called from several threads at the same moment can fail
  (`openpty: … Unknown error: -6`); serialise concurrent spawns or retry.
- Profiles match screen text: a reworded CLI release needs a profile update. OpenCode and
  Cursor pending-composer layouts remain unmeasured and may return `SubmitUnverified`.
  Recorded fixture versions are not a claim of fresh live CLI validation.
