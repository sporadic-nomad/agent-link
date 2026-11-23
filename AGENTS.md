## Project context

`agent-link` (`agent_link` in Rust) integrates external AI agent CLIs into host
applications through PTY sessions and structured stdio. It manages subprocess
lifecycles, events, agent startup and message delivery. The RPC module transports
lines; applications implement vendor protocols on top. Optional `codex`, `acp` and
`claude` modules implement bounded owned-stdio subsets; private `protocol` helpers share
completion, request-admission and event-queue machinery without sharing wire semantics.
The public `portable` module exposes versioned native-event projections (currently with
`codex`, `acp` and `claude`); provider mappings and approval capabilities remain adapter-specific.
Profiles expose operation-scoped compatibility evidence; optional probe reports keep
runtime readiness and launch-context limitations separate from those declarations.
The library has no UI or host-framework dependency. Optional `tools` prepares per-run
consumer-owned CLI/MCP attachments and owns temporary configuration through child
teardown; consumers still implement tools, IPC and server lifecycle.

Optional `turns` observes fresh final replies for direct Claude Code 2.1.296 PTYs on
macOS arm64 with an explicit host-enforced exclusive writer lease. Its dedicated
receiver reports attribution and continuity loss independently of delivery tickets.
Codex, Pi and Cursor support separate direct, single-terminal namespaces. Codex
has explicit final text, Pi infers completion, and Cursor handles bounded in-place
rewrites with mixed model text exposed only by explicit opt-in. The separate
`turns::ambient` tier reads a CLI's own transcript for any PTY on Unix and Windows
without a lease; it labels its evidence but never claims writer exclusion or attribution.
The Claude, Codex, Pi and Cursor profiles supply best-effort ambient sources
(`agent::profiles::ambient`) for the user's own configuration directory, without a
version pin; Cursor's mixed text is opt-in there too.
Its feeds poll a CLI's own server instead: OpenCode's TUI server, launched with a loopback
port and per-spawn password only through `spawn_with_ambient_feed`, is read over the
private bounded loopback HTTP client (`agent::loopback`, shared with the OpenCode question
feed) after its server process is identified as the launched agent.
With `questions`, an ambient transcript attachment also supplies Claude and Codex
questions, relayed only while the screen shows the same question and answered with the
profile's keys (`observe_ambient_transcript_questions`), and `OpenCodeAmbientQuestions`
reads and answers questions for a host-specified root session on an OpenCode TUI server
launched that way, through the same owner-checked loopback client.

PTY recovery attachments expose process-local byte cursors, loss ranges and geometry;
optional display snapshots are fenced to output but are not parser checkpoints.

The workspace-only `agent-link-supervisor` crate owns durable coordination policy:
SQLite intent/receipt persistence, lifetime ownership locks, runtime birth/unload fences,
strict provider bindings and bounded leases. Its optional `codex` feature supplies a
native Runtime; hosts can supply their own runtime and daemon/IPC implementations.
It is not a core feature or dependency; ordinary core Drop semantics
remain unchanged. The separate unpublished `agent-link-daemon` hosts one Codex binding
on Unix with bounded authenticated loopback IPC, independent native dispatch and
finite leases; it never automatically clears runtime cleanup fences. Default Cargo
commands select `agent-link`; the shared static/full
gates and MSRV CI include all four packages. The separate unpublished
`agent-link-http` exposes read-only authenticated loopback HTTP/SSE presentation; it
does not share the daemon mutation IPC. Supervisor builds require a C compiler for SQLite.

- Rust 2024, minimum Rust version 1.85, MIT license. Check `Cargo.toml` for the current
  version and feature definitions.
- Default features: `screen` and `agent`. Optional integrations: `rpc`, `codex`, `acp`,
  `claude`, `probe`, `tools`, `tokio`, `turns`, `questions`, `transcript`, `observers` and `approvals`. `test-support`
  is development-only and enables `agent-link-fake-agent`.
- PTY/process operations use dedicated threads. Tokio provides optional adapters;
  keep the core usable without an async runtime.
- Target platforms are Unix and Windows; only macOS native runtime results are currently
  verified. Linux and Windows runtime support is unverified. Cross-compilation verifies
  compilation; native tests and live CLI checks provide separate compatibility evidence.
- Cursor's built-in profile is work in progress. Authenticated `--force` startup/composer
  validation is follow-up work before removing that label, not a publication gate.

For releases read [release preparation](CONTRIBUTING.md#release-preparation)
for packaging and publication work.

## Coding guidelines

- Keep changes focused and follow the existing module boundaries. Separate transport,
  process lifecycle, screen interpretation and vendor-specific profile behavior.
- Use `cargo fmt` and fix Clippy warnings. Keep imports, tests and implementations
  correctly gated by feature and platform; an all-features build alone is insufficient.
- Preserve Rust 1.85 compatibility. Check dependency MSRVs and enabled features before
  adding or upgrading dependencies; update `Cargo.lock` when dependency resolution changes.
- Use typed errors for expected failures. Validate input and resource limits before
  spawning processes or allocating queues. Avoid panics for host input or normal I/O errors.
  Document invariants around any necessary `unsafe` code and keep platform-specific
  operations in the existing platform helpers.
- Preserve the synchronous core API and optional Tokio boundary. Do not introduce a
  mandatory runtime, UI dependency or unconditional vendor SDK dependency.
- Treat public defaults and enum outcomes as integration contracts. Document intentional
  behavior changes and their migration implications in the same change.

### Runtime contracts to preserve

- **Concurrency:** follow documented lock ordering. Do not execute host callbacks,
  blocking I/O, event fanout, user-owned destructors or ticket wakeups while holding the
  controller state mutex. Apply callback results only if their state and screen revisions
  remain current. Keep normal host calls independent of child I/O waits.
- **Resource bounds:** retain byte and item budgets for active writes as well as queued
  work. Agent completions retain admission capacity until publication finishes. Rejected
  admissions must not launch an agent, write input or accumulate rejection events.
  Internal monitor overflow stops automation while preserving the raw terminal stream.
- **Shutdown:** full receivers and blocked profile/revalidation callbacks must not prevent
  child teardown or disposal of outstanding tickets. Tickets are authoritative; final
  shutdown events are best effort. Rust callbacks can retain their detached thread until
  they return. Preserve external-reaper detection and never signal a known-reaped PID;
  do not claim that a PID check and a subsequent signal are atomic.
- **Delivery:** unknown foreground ownership is unproven. Submission requires observed
  paste text followed by fresh, stable evidence from a readable composer. Noise, silence
  or an unknown verdict cannot confirm delivery. Preserve event order and the screen/output
  fences used to establish that evidence. The only exception is the explicit per-send
  `UnreadableComposer::EnterOnce` opt-in: a composer that stays unreadable can get one
  Enter after a quiet paste echo and the pre-Enter checks, and it reports
  `UnverifiedEnter`, never `Delivered`.
- **Defaults:** use YOLO/unattended permission settings for profile-based PTY and RPC
  launches. Restrictions require explicit `Permissions::AgentDefault` or `Custom`
  configuration; do not silently ignore unsupported RPC policy selection. Leave trust
  steering off by default. Enter retries require explicit opt-in and can duplicate a
  submission. An unverified result must not trigger an automatic resend.
- **Privacy:** RPC payloads, command arguments and stderr require explicit trace opt-in
  before logging. Keep ordinary diagnostics useful without adding sensitive content.
  Agent diagnostic tracing uses `AGENT_LINK_TRACE`; document any configuration changes.

## Test coverage guidelines

Cover observable behavior, failure paths and invariants. There is no enforced percentage
target; test counts alone do not establish coverage. Behavioral fixes should include a
regression that exercises the failure and checks its outcome and side effects. Avoid tests
that merely reproduce an implementation expression or unnecessarily expose private state.

| Change | Relevant coverage |
| --- | --- |
| PTY/process lifecycle or event ordering | Module unit tests; `tests/pty_roundtrip.rs`, `tests/pty_windows.rs` |
| Input admission or overload | Queue unit tests; `tests/input_limits.rs`, `tests/profile_isolation.rs` |
| Agent startup, ownership, delivery or reset | State/startup unit tests; `tests/agent_startup.rs`, `tests/agent_delivery.rs`, `tests/agent_delivery_owner.rs`, `tests/agent_strict.rs`, `tests/agent_reset.rs` |
| Isolated Codex, Pi and Cursor PTY turn observation | `agent::profiles::turns::isolated`, `tests/isolated_turns.rs`, `tests/compatibility.rs` |
| PTY question observation and answers | `agent::questions`, `tests/pty_questions.rs`, `tests/isolated_turns.rs`, `tests/fixtures/questions/`, optional native `scripts/check_opencode_questions.py` |
| Claude PTY turn observation | `agent::turns`, `agent::profiles::turns::claude`, `tests/claude_turns.rs`, `tests/compatibility.rs` |
| Best-effort ambient PTY turn observation | `agent::turns::ambient`, `tests/ambient_turns.rs` |
| Built-in ambient sources (lookups, parsers, fixtures) | `agent::profiles::ambient`, `tests/ambient_sources.rs`, `tests/compatibility.rs`, `tests/fixtures/transcripts/ambient/` |
| Ambient feeds, OpenCode server feed, loopback HTTP or server-owner check | `agent::turns::ambient`, `agent::loopback`, `agent::profiles::ambient::opencode`, `tests/ambient_feed.rs` (stub server in `tests/common/http_stub.rs`, `opencode_stub.rs`), `agent::questions` |
| Best-effort ambient questions (transcript hook, screen agreement, OpenCode TUI question feed) | `agent::turns::ambient::question`, `agent::profiles::ambient` (question hooks, `tests/fixtures/transcripts/ambient/*/question-*.jsonl`), `agent::questions::opencode::ambient`, `tests/ambient_questions.rs`, `tests/pty_questions.rs` |
| RPC framing, errors or privacy | `src/rpc/tests.rs`, framing/input unit tests, `tests/rpc_roundtrip.rs` |
| Native adapter lifecycle or protocol | `src/codex/tests.rs`, `tests/codex_adapter.rs`, `src/claude/tests.rs`; synthetic pinned wire projections in `tests/fixtures/codex-app-server/` |
| CLI profiles or launch behavior | `tests/profiles_fixtures.rs`, `tests/profiles_launch.rs`, startup replay tests |
| Application tool attachment/configuration | `src/tools/tests.rs`, `tests/tool_attachments.rs`; affected native adapter tests for MCP admission |
| Async adapters or cancellation | `tests/tokio_api.rs` and async module tests |
| README and runnable examples | `tests/readme.rs`, `tests/example_behaviors.rs` and the compiled examples |
| HTTP/SSE presentation | `cargo test --locked -p agent-link-http --all-features -- --test-threads=1`; offline bounded socket fixtures |
| Local daemon/IPC | `cargo test --locked -p agent-link-daemon --all-features -- --test-threads=1`; offline bounded IPC/process fixtures |
| Durable supervision, ownership or persistence | `cargo test --locked -p agent-link-supervisor --all-features -- --test-threads=1`; isolated ledger and crash/launcher fixtures |

- Cover relevant empty, oversized and saturated inputs; EOF/disconnection; failed writes;
  cancellation; and cleanup after errors. For concurrency changes, exercise blocked
  consumers/callbacks, in-flight work and stale results after state changes.
- Prefer deterministic state-machine tests with controlled time for decisions. Use real
  subprocess/PTY integration tests when the behavior depends on I/O or process ownership.
  Reuse the shared helpers and scripted agent before adding new harnesses.
- Use channels, barriers and deadline-based waits to reproduce races. Avoid arbitrary
  sleeps as synchronization. Release blocked callbacks and clean up children even when
  an assertion fails; give test waits finite deadlines and useful diagnostics.
- Run PTY tests serially (`--test-threads=1`) and use the existing spawn helpers/locks.
  Concurrent `openpty` calls can fail on macOS. Use isolated temporary homes and working
  directories; ordinary tests must not depend on credentials or change the user's CLI state.
- Preserve raw `.bin` fixture bytes and capture metadata. Base new profile recognition
  on measured CLI output, include ambiguous/negative cases, and sanitize captured secrets.
  Live tests are ignored because they can make API calls and change CLI state; run them
  only when the task explicitly includes live validation.

### Validation commands

Run commands from the repository root. For routine changes, run only the unit tests,
integration test targets and examples relevant to the changed behavior, using the
coverage table above. Use a module or test-name filter to narrow unit tests. For example,
for a delivery change:

```sh
cargo test --locked --all-features --lib agent::delivery::tests -- --test-threads=1
cargo test --locked --all-features --test agent_delivery -- --test-threads=1
```

Select additional related targets when a change crosses module boundaries. Do not run
the full unit or integration suite for a routine handoff. Before handing off code,
dependency or feature changes, also run the shared static checks:

```sh
python3 scripts/check.py --skip-tests
```

This checks formatting, the supported feature combinations, all-target Clippy with
warnings denied, default-feature and `turns`-only core rustdoc and all-feature workspace rustdoc. Reserve `python3 scripts/check.py` (without `--skip-tests`)
for release preparation or an explicit request for a full run; it also runs the full
non-live test suite. CI continues to run the full gate.

For dependency/MSRV changes, also run the Rust 1.85 checks listed in CI.
For platform changes, run affected native tests or CI jobs when available; cross-target
checks do not establish runtime support. State which checks ran and which remain unverified,
including when the full suite was deferred to release validation.

For prose-only changes, check paths, links, commands and affected examples. If executable
content or a behavior contract changed, run the related checks and tests.

## Documentation updates

- `docs/` is the tracked, packaged public user manual for host developers and agents
  integrating this library. Public compatibility evidence belongs there or in tracked
  test fixtures. Keep public links usable from a clean checkout.
- `internal-docs/` is an optional, Git-ignored local technical knowledge base for
  project development. Preserve it locally; never make builds, tests, public evidence,
  or release procedures depend on its presence. Use `internal-docs/README.md` as its
  local index when available. Current contracts belong in source/rustdoc and the manual;
  historical research must retain its date, scope and limitations.

- Update public rustdoc with API changes: purpose, defaults, errors, blocking behavior,
  ownership/lifetimes, feature requirements and platform limitations as applicable.
- Update `README.md` and the relevant public guide in `docs/` when host-visible behavior, setup, limits,
  permissions, events or delivery guarantees change. Add externally visible changes to
  the current unreleased entry in `CHANGELOG.md`.
- Keep README Rust snippets identical to their compiled examples or async module docs;
  `tests/readme.rs` enforces this. Update the source example and its quoted excerpt together.
- Keep `Cargo.toml`, CI, `scripts/check.py`, `CONTRIBUTING.md` and its release instructions
  consistent when features, MSRV, validation or packaging requirements change. Update
  this guide when architecture or development conventions change.
- Distinguish recorded fixtures, cross-compilation, native test results and live CLI
  validation. Keep compatibility statements tied to evidence; preserve documented
  limitations until corresponding behavior is implemented and verified.

## Workspace and handoff

Inspect `git status` before editing. Preserve unrelated edits and local/ignored files;
stage only the intended changes. When subagent work is used, give each implementation
agent a separate Git worktree and default `target` directory, review its diff and
validation before merging, and resolve integration issues on the combined result.

Report what changed, the relevant checks and any remaining limitations. Follow the
release guide for archive inspection and dry runs; keep actual publication separate
from local validation.

The unpublished `agent-link-http` workspace crate owns bounded authenticated loopback
read-only HTTP/SSE presentation; it is not a core dependency or remote mutation service.
Its native socket tests run serially (`cargo test --locked -p agent-link-http -- --test-threads=1`).
