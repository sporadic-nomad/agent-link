# Changelog

Notable changes to `agent-link`, in the [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
format. Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html); before
1.0 a minor version may break the API.

## [Unreleased]

## [0.3.1] — 2026-11-23

### Added

- `turns` adds a best-effort ("ambient") observation tier, `agent::turns::ambient`, for
  any `AgentTerminal`: shell or direct spawn, Unix and Windows, no lease, isolated root,
  version pin or launch budget. It provides **no writer exclusion and no attribution
  guarantee**; replies are what the CLI's own transcript says ended a turn.
  `AgentTerminal::attach_ambient_turns` uses the new `AgentProfile::ambient_turns` hook
  (default `None` for custom profiles) and
  `attach_ambient_turns_with` takes a host `AmbientSource`/`AmbientParser`. The pull
  `AmbientReceiver` reports `AmbientEvidence` per turn (explicit end, next record, quiet
  window, transcript end) and typed `AmbientStatus` waiting, active and closed states.
  Reading starts from now and survives rewrites, replacement and session switches. Byte,
  record, reply and queue bounds reuse `TurnOptions`; a full queue closes with
  `Overflow`. One worker per attachment, at most `ambient_worker_limit()` (default 64)
  per process, independent of the controlled tier. `AttachError::NoAmbientSource` is new.
  [Guide](docs/pty-ambient-turns.md)
- Built-in best-effort ambient sources: `ClaudeProfile`, `CodexProfile`, `PiProfile` and
  `CursorProfile` now return one from `AgentProfile::ambient_turns`, so
  `AgentTerminal::attach_ambient_turns` works for their terminals on the user's own
  configuration directory (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
  `PI_CODING_AGENT_SESSION_DIR`/`PI_CODING_AGENT_DIR`, `CURSOR_DATA_DIR`, else the home
  defaults) with no version pin. Claude and Codex end turns explicitly; Pi's ends are
  inferred (next prompt or quiet). Cursor reports successful ends **without text** by
  default, like the controlled tier; the new
  `CursorProfile::ambient_turns_with_unseparated_text` opts in to its text, which can mix
  reasoning with the answer. Lookups skip print, SDK, `exec`, app-server and subagent
  sessions, stay under the resolved root and are bounded per wake. Each profile's
  `compatibility()` declares the recorded fixtures it was tested against (Claude 2.1.296,
  Codex 0.154.0, Pi 0.85.1 and 1.5.2, Cursor 2026.10.01 and 2026.11.13-eacade1); no
  live check. [Guide](docs/pty-ambient-turns.md#built-in-sources)
- The ambient tier can poll a feed instead of tailing a file: `AmbientFeed` /
  `AmbientFeedReader` (one reader per agent launch, a two-second budget per poll),
  `AgentProfile::ambient_feed` (used by `attach_ambient_turns` when a profile has no
  transcript source), `AgentTerminal::attach_ambient_feed_with`, and
  `AmbientUnavailable::Feed(AmbientFeedUnavailable)` with typed reasons. The receiver,
  evidence, limits, worker budget and lifecycle are unchanged.
- OpenCode best-effort replies from its TUI's server. The new opt-in
  `AgentTerminal::spawn_with_ambient_feed` calls the new
  `AgentProfile::ambient_feed_launch` hook. OpenCode's adds `--port <free loopback port>
  --hostname 127.0.0.1` and a fresh 32-hex-digit `OPENCODE_SERVER_PASSWORD` from OS
  randomness. Ordinary spawns are unchanged. In a shell spawn every process the shell
  starts inherits the password.
  - The feed sends the password only on a connection whose server end it found held by
    the launched agent's process (`libproc` on macOS, `/proc` on Linux,
    `GetExtendedTcpTable` on Windows). A port squatter is sent nothing; an unprotected
    or refusing server ends the launch's feed.
  - Shapes follow OpenCode 1.18.21 as declared, tested only against a synthetic server;
    OpenCode 2.x is not covered.
  - `turns` now depends on `httparse`, `base64` and `getrandom` 0.4; Windows builds
    enable `windows-sys`'s `Win32_NetworkManagement_IpHelper`.
  - The OpenCode question client's HTTP code moved, unchanged, to a module both
    features share.
  [Guide](docs/pty-ambient-turns.md#opencode)
- `questions` gains best-effort question sources for the ambient tier, for a desktop host
  that runs the CLIs in the user's own shell and configuration on any platform.
  - `AgentTerminal::observe_ambient_transcript_questions(&AmbientReceiver)` relays the
    questions a transcript attachment shows (Claude `AskUserQuestion`, Codex Plan-mode
    `request_user_input`) through the existing question events, book, limits and answer
    keys, only while the terminal's screen shows the same question. A call counts only
    after a prompt the attachment read; copied history, rewrites, oversized lines, the
    receiver's stop or drop and a new launch withdraw it. Evidence is weaker than the
    controlled tier's: no writer exclusion, attribution or version pin; the on-screen
    dialog is what makes a relayed question this terminal's.
  - The new defaulted `AmbientParser::question` hook returns `AmbientQuestion::{Asked,
    Answered, Withdrawn}`; the built-in Claude and Codex sources implement it with the
    controlled tier's record parser, which now compiles on every platform.
  - `agent::questions::opencode::OpenCodeAmbientQuestions` is a `QuestionFeed` for an
    OpenCode TUI launched with `spawn_with_ambient_feed`: it lists and answers its own
    server's pending question requests with the ambient feed's endpoint, credentials and
    server-owner check (before every new connection, the answer's included). Pass the displayed root
    session ID to `OpenCodeAmbientQuestions::new(&term, session_id)` and stop observation
    before switching sessions. Both list and answer revalidation filter by that ID.
  - Tested offline on macOS arm64 against recorded transcripts, the fake agent's question
    dialog and a synthetic OpenCode-like server; no real CLI was observed, and Linux and
    Windows runtime behavior is unverified.
  [Guide](docs/agents-and-delivery.md#best-effort-ambient-questions)
- `turns`: `TurnReceiver` and `TranscriptReceiver` gain `recv_timeout` and
  `wait_status_change`, and with `tokio` the cancel-safe `recv` and `status_change`, so
  hosts no longer poll on a timer. `TurnRecvError` distinguishes a timeout from the gap
  or closure that ended observation. Publication, gaps, `stop()` and terminal teardown wake
  waiters at once. Lease revocation, child exit, launch changes and monitor loss are
  sampled by the worker's next 50 ms pass. `try_recv`, `status` and their checks are
  unchanged. [Guide](docs/agents-and-delivery.md#waiting-on-a-turn-receiver)
- `turns`: the best-effort `AmbientReceiver` gains the same waits, `recv_timeout` and
  `wait_status_change`, and with `tokio` the cancel-safe `recv` and `status_change`, so a
  host with many terminals needs no timer loop per receiver. The new
  `ambient::AmbientRecvError` has `Timeout` and `Closed(AmbientClosed)`; the controlled
  tier's `TurnRecvError` is not reused because ambient observation has no gap and keeps
  the turns queued before a close (other than `stop()`), which a receive returns before
  `Closed`. Publication, a published status change (a waiting reason or a feed's state
  included), a close, `stop()` and terminal teardown wake waiters at once; what the worker
  samples (launches, departures, child exit, monitor loss, new lines) at its next wake.
  `try_recv`, `status`, `stop`, the bounds and the worker are unchanged.
  [Guide](docs/pty-ambient-turns.md#waiting-instead-of-polling)
- Opt-in reset turn gate: a nonzero `Timing::reset_turn_wait` holds `reset()` and the
  reset step of `reset_and_send()` while the agent's activity is `Working`, bounded in
  total by that value, before the reset command is typed. A turn that outlasts the
  bound ends with the new `AbortReason::AgentWorking` (`agent_working`) and nothing
  typed. While waiting, the job reads the foreground every 250 ms. A departed agent
  is relaunched, another foreground program gives `ForegroundUnproven` and a recognized
  question gives `DialogBlocking`. Teardown and session end stop the wait at once.
  Activity is heuristic (a Braille title spinner or OSC 9;4 progress; among the built-in
  profiles only Codex has a recorded spinner): the gate makes a reset typed into a
  running turn less likely without proving the turn ended, and `reset_rejected` checks
  still apply. Tested offline with the scripted fake agent only.
  [Guide](docs/agents-and-delivery.md#waiting-for-a-running-turn-opt-in)
- `SendOptions::unreadable_composer` and `agent::UnreadableComposer` add an explicit,
  per-send opt-in for profiles that cannot read their composer. The built-in OpenCode
  and Cursor profiles are examples. The default `Refuse` keeps the current
  behavior: no Enter and `SubmitUnverified`. With `EnterOnce`, the profile must find no
  readable composer during the whole paste check. Fresh output after the paste must then
  go quiet. Delivery rechecks composer readability alongside questions and startup dialogs.
  An ordinary send that now finds a readable composer on the agent's screen ends with
  `SubmitUnverified` and withholds Enter. If the composer is still unreadable and the
  foreground check passes, delivery writes exactly one Enter without composer evidence.
  It never retries that Enter, even with `submit_retry_max`. The delivery ends
  with the new `AbortReason::UnverifiedEnter`
  (`unverified_enter`), never `Delivered`. A readable composer keeps the verified path.
  Without quiet output after the paste, the delivery still ends `SubmitUnverified` with
  no Enter. Do not resend `UnverifiedEnter` automatically, because the message may
  already have been submitted.
  [Guide](docs/agents-and-delivery.md#unreadable-composers-opt-in)
- `AgentTerminal::reset_with(UnreadableComposer)` applies the opt-in to `reset()`.
  `reset_and_send` applies the caller's `unreadable_composer` to both halves. After
  an unverified reset Enter, refusal checks still run. The follow-up is pasted only if
  fresh output settles after that Enter. Otherwise it is withheld with
  `SubmitUnverified`. A compound job with an unverified reset never reports
  `Delivered`. Its follow-up can still reach the old conversation.
- The opt-in has offline fake-agent coverage only. No live OpenCode or Cursor run
  establishes that the unverified Enter submits a message.

### Changed

- The built-in OpenCode profile maps an explicit PTY `ModelChoice` without an effort
  to `-m <id>` instead of returning `Error::Unsupported`. The TUI command has no effort
  option, so a choice with an effort is still refused with `Error::Unsupported`, never
  launched without it. Base arguments that already select a model return
  `Error::InvalidInput`. The flag is declared from a prior opencode 1.18.21 help
  listing that is not recorded in this repository; its compatibility record is a
  `Declaration`, and opencode 2.x is not covered. Hosts that relied on the refusal to
  hide a model picker for OpenCode now get a launch with the selected model.
- `Timing` has the new field `reset_turn_wait` (default `Duration::ZERO`, off: resets
  behave as in 0.3.0). Exhaustive `Timing` literals must add `reset_turn_wait:
  Duration::ZERO`; struct update syntax (`..Timing::default()`) inherits it.
- `SendOptions` has a new public field. Exhaustive `SendOptions` literals must add
  `unreadable_composer: UnreadableComposer::Refuse` to keep the previous behavior.
  `SendOptions::default()` and struct update syntax inherit that default.
  `AbortReason` is `#[non_exhaustive]`, so the new `UnverifiedEnter` variant does not
  break matches. Hosts that map reasons to their own states should still handle it
  explicitly. `DeliveryOutcome` is unchanged.

### Fixed

- Ambient OpenCode completions use assistant `parentID` and tool follow-up state;
  queued prompts and tool steps with `finish: "stop"` no longer publish narration as a
  final reply. Provider-executed tools and interrupted cleanup errors retain their
  completion exceptions.
- Ambient observation resumes from fresh positions when a suspended shell agent's
  identified process group regains the foreground, without requiring a relaunch.
- Terminal closure reconciles the latest launch before the final transcript lookup,
  recovering eligible replies from replacements that started and ended between polls,
  including when the terminal was dropped.

- An ambient transcript that shrinks during its initial scan uses the observed shorter EOF as its baseline, so later appends can report turns before the file regains its original size. OpenCode processes session lists, status maps and message parts incrementally within the existing response caps.
- Ambient transcript readers compare conversation prefixes across files. Independent sessions keep their replies when a later reply and end marker match. Sources can identify a replaceable session header with `AmbientSource::is_session_header`.
- A departing shell launch leaves transcripts created by its replacement to the new launch. Ambient attachment returns `NoLaunch` as soon as the PTY records child exit, even while the controller is waiting for EOF.
- The reset turn gate discards foreground evidence collected before a relaunch. OpenCode ambient feed launches that enable `--mini` return `InvalidInput` before spawning.
- Final ambient transcript reads check the captured file identity and revision. A replacement or a detected same-length rewrite after EOF capture reports `IncompleteDrain`. Repeated notifications during an empty turn-receiver pull now respect the original timeout.
- Cursor discards pending reply text and completion evidence after an unreadable turn record. OpenCode retries a session lookup when its response names a different session.

- OpenCode auto-compaction keeps intermediate tool steps, synthetic continuations and overflow replays within the original prompt.
- OpenCode retries a trailing reply when an idle notification precedes the status update and checks the latest message ID before publishing.
- Ambient transcript readers discard pending reply text when a rewrite loses its anchor. Codex and Pi reuse the decoded header across passes over the same file version.
- An expired reset turn wait skips the foreground and question poll and returns `AgentWorking` if the agent is still working.

- On macOS, OpenCode checks the accepted socket holder’s process group after matching its descriptor.
- Ambient readers distinguish repeated Cursor replies across history-preserving rewrites, follow Codex rollouts beyond the date discovery window, and drain a session selected between the final poll and exit.
- Ambient Claude question relay requires nonblank user prompt text. Ambient queue admission compacts reply strings from custom parsers and feeds before charging their retained capacity. Ambient question observers also discard spare capacity in validated question strings and vectors.

- Ambient observers withhold quiet and departure inference while a transcript line is incomplete. Claude drops the preceding reply when a newer non-meta assistant record lacks a content array. After overflow, the worker releases its slot without waiting for the next poll, and queued turns remain available.
- Cursor remembers observed unsuccessful ends across transcript rewrites, including when metadata replaces the end marker before the next prompt. Ambient readers capture an empty file’s baseline at discovery and report `IncompleteDrain` if an observed file disappears before departure completes.
- OpenCode reports `IncompleteDrain` when final fetch retries run out. Its question observer stops on a redirect, 404, or 410 from the initial probe, and Windows server-owner checks honor the deadline during socket and process enumeration.

- A relaunch after Enter now gives `SubmitUnverified` while an ordinary send is waiting for its receipt.
- Ambient transcript eviction retains the bytes preceding the consumed offset. A changed suffix on reopen moves the baseline to EOF. On exit, a final lookup can recover a first transcript created between polls.
- OpenCode final polls reuse verified connections and report `IncompleteDrain` when a pending request requires a new one. Completed turns remain available.
- Cursor’s opt-in ambient text retains an empty, truncated reply when the byte limit cuts off its first character.

- Ambient project lookups use filesystem directory identity. New transcript scans start at EOF when the bounded history cannot establish freshness.
- OpenCode verifies the trailing prompt against a current idle state and enforces the SSE record budget within each socket read. Final ambient feed pages share one deadline and report `IncompleteDrain` when pages remain.

- PTY sends and resets bind each pending Enter to the agent lifecycle recorded with the paste.
  A host relaunch before Enter withholds that key, including retries and the `EnterOnce` opt-in.
  A relaunch between a reset follow-up's final check and its paste also withholds the follow-up.
- Sends and standalone resets repeat their pre-paste checks if a host relaunch races
  the paste. After a paste, shell cleanup checks the launch before queueing Ctrl-C
  and before marking the agent gone.
- OpenCode's Windows socket-owner check verifies each parent using process handles and creation
  times. An unverifiable parent link leaves the connection unauthenticated.
- Pi keeps inferred replies tied to their originating record across metadata and forks. Pi and Cursor now account for tool calls beyond the retained content blocks.
- Codex and Pi discover symlinked transcripts by the target’s modification time and recheck transcript identity after replacement. Switching to a listed transcript preserves turns appended across a partial line.
- OpenCode returns completed turns in session order across polls, rejects feed launch plans containing `--` before spawn, and observes the owner-check deadline during macOS and Linux scans.
- The `turns`-only rustdoc build resolves the ambient receiver documentation with warnings denied.
- A readable composer in the final `EnterOnce` check aborts a reset with `SubmitUnverified` and withholds its follow-up.
- OpenCode requires completion evidence before publishing a message ID. On Linux, an inconclusive server-owner scan remains retryable.
- Ambient transcript readers drain through the observed EOF on departure and report `IncompleteDrain` on failure. Replayed prompts cannot authorize Claude or Codex questions, and an oversized line discards the pending reply candidate.
- Direct launches timestamp transcript lookups before child creation so early Claude session records stay eligible.
- The default-feature profile documentation builds with broken links denied.
- `UnreadableComposer::EnterOnce` checks the paste quiet interval on the final screen revision for both sends and resets.
- OpenCode authentication probes classify the response head and close the socket. An unauthenticated success or an authenticated 401 or 403 ends the turn feed for that launch, even when the response body stalls or exceeds the size limit.
- A transcript rewrite during the initial baseline scan now resumes from the rewritten EOF. Ambient readers enforce the attachment’s item and byte limits from reply staging through dequeue.
- Controlled turn receivers release the pull lock before waking async consumers on dequeue or gap detection.
- Opt-in resets recheck activity under the terminal writer lock. A turn that resumes while the reset waits for another writer uses the reset’s remaining wait time.
- Ambient transcript readers reject special files. The per-launch directory scan counts every entry, and saved offsets stay bounded when transcripts are evicted.
- The OpenCode turn feed drops retry counts with evicted idle sessions. Its ambient question feed uses one three-second deadline to read the pending list and look up the bound session during polling or answer revalidation.
- Transcript rewrites can insert metadata before a reported end marker without publishing the completion twice.
- Claude, Codex and Pi distinguish an incomplete lookup with `AmbientUnavailable::ReadBudgetExceeded`. Codex and Pi charge buffered header read-ahead against the lookup budget.

## [0.3.0] — 2026-11-19

### Added

- `turns` adds isolated Codex, Pi and Cursor PTY transcript receivers on macOS arm64.
  Codex emits explicit finals and Pi labels inferred replies. Cursor handles in-place
  history rewrites, distinguishes inferred from explicit completion, and makes
  mixed assistant/reasoning text an explicit opt-in.
- Optional `questions` exposes interactive PTY question and close events, version-scoped
  Claude, Codex and Cursor observation, session-bound OpenCode question lists and replies, explicitly requested
  answer tickets and redraw, foreground and lifecycle checks at dispatch.
- The optional `turns` feature exposes final replies from a controlled, direct Claude
  Code 2.1.296 PTY on macOS arm64. Hosts enforce one writer per session and pass a
  revocable `ExclusiveClaudeRoot`. Attachments start at verified EOF, return bounded
  final text, and report gaps on loss of attribution or continuity. Admission checks
  the merged PTY environment and revalidates the leased directory immediately before
  spawn. The child addresses it by macOS volume and file ID. Host loggers can revoke
  the lease during launch preparation or after spawn. Peer readers can verify the
  initial session before a terminal’s first attachment and retry stale samples
  across independent session switches and peer closures. Reservations affect peer
  readers once process creation can begin. Peer checks account for directory aliases.
  Confirmed exit closes the child's receiver even before PTY EOF. Truncation checks
  start at first open, and a partial mode header keeps the same transcript pinned
  across polls.
  The Claude profile records writer-binding and final-text evidence separately.
  [Guide](docs/agents-and-delivery.md#claude-pty-final-replies)

- `AgentSpec::model` and `agent::ModelChoice` select a model and optional reasoning
  effort for profile-built PTY shell launches. Claude, Codex and Pi map the selection
  once for the lifetime of the shell. Custom profiles can implement
  `AgentProfile::model_args`. Spawn rejects an unsupported choice or a quoted
  command that exceeds the shell's line limit or the configured PTY write budget.
  Pi accepts equals-form extension flags alongside the built-in model and thinking options.
- The default `agent` feature checks `AgentProfile::question_dialog` before pasting,
  while waiting for the paste to appear, and before each Enter. A matching question
  stops delivery with `AbortReason::DialogBlocking` (`dialog_blocking`). Inspect the
  session before resending because a paste or an earlier Enter may have reached it.
  After a post-paste match, a shell that regained the terminal gets Ctrl-C and the
  outcome is `ShellOnly`. Other or unknown foreground owners give `ForegroundUnproven`.
  Profiles can match recorded layouts with `agent::detect::dialog_at_bottom`, which
  treats a bare composer prompt below the footer as the end of the question.
  The Claude, Codex, OpenCode and Cursor profiles now recognize their recorded
  question layouts by default. This includes Claude's answer review, Codex Plan
  selections and notes, OpenCode's question picker and Cursor's shell approval menu.
  The Cursor recording exercises its packaged renderer in isolation. Pi and custom
  profiles retain the default `false` predicate.
- `AgentProfile::reset_rejected` lets a PTY profile recognize a refusal after an
  explicit reset. The built-in Codex profile recognizes the busy-turn `/clear`
  diagnostic recorded with Codex CLI 0.169.0. A recognized refusal now gives
  `Aborted(ResetRejected)` (`reset_rejected`) even if the composer cleared.
  Both `reset()` and `reset_and_send()` check the response as fresh output settles.
  Ambiguous or unavailable reset evidence yields `SubmitUnverified`. Either reset
  outcome withholds the follow-up. Inspect the session before retrying an unverified
  reset because Enter may already have reached the CLI.

### Changed

- When upgrading from 0.2, exhaustive `AgentSpec` literals must add
  `model: None` to keep the previous launch behavior. `AgentSpec::new`,
  `AgentSpec::default` and struct update syntax inherit that default.

### Fixed

- Enabling `questions` alongside a native stdio adapter preserves literal JSON
  extension keys in provider events.
- Profile-built PTY launches use fish quoting when the resolved Unix shell is named
  `fish`, preserving literal backslashes and apostrophes in model IDs and other arguments.

## [0.2.0] — 2026-11-17

### Fixed

- MCP HTTP URLs now validate parsed host, port and IPv6 syntax before configuration
  rendering or child spawn. Malformed endpoints return `InvalidInput`; existing userinfo,
  whitespace and fragment restrictions remain. Parsing preserves valid URL text and does
  not resolve DNS or connect. The optional `tools` feature now includes the `url` parser.
- Pi MCP attachment validation also rejects bare `$NAME`, `$$` and `$!` forms in
  environment and header values, preventing native interpolation or escape decoding
  from changing literal values. Ordinary dollar signs remain supported.
- CLI attachments reject directories requiring PATH escaping, including semicolons on
  Windows: successful `join_paths` encoding is insufficient when EnvPolicy prepends the
  original directory verbatim. Existing generic EnvPolicy behavior is unchanged.

### Added

- Optional `tools` facade with `AgentTools`, `CliTool` and `McpServer`: application-owned
  commands on child PATH, explicit CLI discovery instructions, and per-run stdio/HTTP
  MCP configuration without global installation or configuration edits. Supports Codex,
  Claude, Pi's `registerMcpServer` extension API and ACP; OpenCode PTY and Cursor MCP
  attachments are explicitly unsupported. `AgentTerminal::spawn_with_tools` and prepared
  `RpcLaunch` values retain temporary configuration through process teardown.
- ACP new/load/resume includes attached MCP definitions; unadvertised HTTP capability
  is rejected before session admission. Existing unattached sessions keep empty MCP lists.
- Bounded validation, private Unix configuration permissions, redacted descriptor Debug,
  and explicit additive/exclusive policy (exclusive currently Claude only). The host still
  owns tool/server implementation, endpoint lifecycle and authorization. Offline macOS
  tests cover launch, configuration and cleanup; no new live vendor validation is claimed.

### Changed

- Enabling `tools` adds `tools: Option<ToolAttachment>` to `RpcLaunch` and `LaunchPlan`.
  Constructors default to `None`; full `RpcLaunch` struct literals need this field. Preserve it
  when forwarding a prepared launch. Existing defaults and permission policies are unchanged.

## [0.1.1] — 2026-11-17

### Changed

- Codex/ACP now refuse new requests and approval replies after observed input closure,
  without consuming approval tokens or admitting undeliverable controls. Native output
  continues draining with existing deadlines; input closure alone is not terminal.
- RPC stdin I/O failures now emit nonterminal `RpcEvent::StdinError { id, kind }`:
  pending receipts settle and further input is refused, while output continues toward
  `Exit`. Handle `StdinError` to observe input failures. Terminal `Error` still replaces
  `Exit`; exit codes remain available separately through `try_wait()`.
- Oversized stderr lines now produce a bounded drop marker and resume at the next
  line instead of stopping RPC. Oversized stdout and genuine read failures remain terminal.
- RPC stdin/stdout frame defaults increase from 1 to 8 MiB, pending stdin from 4 to
  32 MiB, and event capacity decreases from 64 to 8, preserving the 64 MiB raw queued
  stdout allowance (lossy UTF-8 can expand up to 3×, plus the reader frame). Codex and
  Claude frame defaults increase to 8 MiB, with pending/control/event byte budgets
  of 32 MiB and approval bytes of 16 MiB; item counts and ACP frame limits are unchanged.
  Hosts needing earlier bounds should set limits explicitly and review companion budgets.
- Added `RpcSpawnSpec::trace_stderr` (default `false`) for stderr-only logging. Full
  struct literals must add this field; prefer constructors and field assignment.
  Full `trace` still enables payload, argument and stderr logging; either stderr opt-in
  may expose sensitive diagnostics.
- Clarified async bridge capacity and disconnection contracts: PTY `Exit` does not end
  the stream; RPC terminal `Exit` or `Error` does. An unbounded source remains unbounded.
- An unavailable host working directory yields detailed probe readiness `Untested`
  with `WorkingDirectoryUnavailable`; the legacy false installation result is inconclusive.
  These changes have offline regression coverage, not new live vendor validation.

## [0.1.0] — 2026-11-16

The first release.

### Changed during publication hardening

- Documented the Windows ConPTY lifetime limitation: child completion can leave the
  output pipe open, so hosts must drain events and poll `Terminal::try_wait()` instead
  of relying only on `Event::Exit`, then explicitly tear down the session. Windows
  integration tests now check output, process status and cleanup separately; the
  natural-output-EOF event contract is unchanged.

- Corrected native test fixtures for Linux Bash startup and Windows ACP paths and
  PTY events, and synchronized delayed receipt displays and shell interrupt handling,
  preserving the delivery and process-lifecycle assertions.

- Fixed newer Rust CI builds for the HTTP crate with a scoped deprecation allowance,
  preserving Rust 1.85 support and runtime behavior.

- Added the public GitHub repository URL to package metadata and installation
  examples.

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

- Windows runtime support remains unverified; native CI is configured and foreground
  answers are heuristic. ConPTY can retain output after child completion; see the
  [process-completion guidance](docs/pty-and-screen.md#events-and-receiver-backpressure).
- macOS: `openpty` called from several threads at the same moment can fail
  (`openpty: … Unknown error: -6`); serialise concurrent spawns or retry.
- Profiles match screen text: a reworded CLI release needs a profile update. OpenCode and
  Cursor pending-composer layouts remain unmeasured and may return `SubmitUnverified`.
  Recorded fixture versions are not a claim of fresh live CLI validation.
