# `agent-link` User Manual

`agent-link` (`agent_link` in Rust) integrates external AI agent CLIs into Rust host
applications through pseudo-terminal (PTY) sessions and structured stdio transports. It has
no UI or host-framework dependency: your application owns rendering and UX while the crate
manages child lifecycles, bounded event streams, startup steering, verified message delivery
and protocol framing.

> **Version:** `0.1.1` · **Edition:** Rust 2024 · **MSRV:** 1.85 · **License:** MIT

Only **macOS** has verified native runtime results. Linux and Windows runtime support
remains unverified; see [platform evidence](../README.md#platforms). The built-in Cursor
profile is **work in progress**, with authenticated startup/composer validation pending.

---

## Choosing an Integration Layer

Pick the smallest layer that fits how you talk to the agent:

| Need | API | Feature | Guide |
| --- | --- | --- | --- |
| Raw shell or CLI on a PTY | `pty::Terminal` | always on | [PTY & Screen](pty-and-screen.md) |
| Headless terminal model & signals | `screen::Vt100Model`, `ScreenSnapshot` | `screen` (default) | [PTY & Screen](pty-and-screen.md#headless-screen-model-screen-feature) |
| Interactive agent CLI with startup & verified paste | `agent::AgentTerminal`, `ProfileRegistry` | `agent` (default) | [Agents & Delivery](agents-and-delivery.md) |
| Check whether CLIs are installed & signed in | `probe::probe_agents`, `probe_agents_detailed` | `probe` | [Permissions & Probes](permissions-and-probes.md) |
| Raw JSON-lines over stdio | `rpc::RpcChild` | `rpc` | [RPC & Native Adapters](rpc-and-native-adapters.md#generic-json-lines-rpc-rpc) |
| Bounded Codex app-server protocol | `codex::CodexAppServer` | `codex` | [RPC & Native Adapters](rpc-and-native-adapters.md#codex-app-server-codex) |
| Bounded ACP stable-v1 protocol | `acp::AcpClient` | `acp` | [RPC & Native Adapters](rpc-and-native-adapters.md#agent-client-protocol-acp) |
| Persistent Claude stream-json sessions | `claude::ClaudeSession` | `claude` | [RPC & Native Adapters](rpc-and-native-adapters.md#claude-stream-json-claude) |
| Versioned cross-adapter event projection | `portable::EventV1` | `codex`, `acp`, `claude`, or `transcript` | [RPC & Native Adapters](rpc-and-native-adapters.md#portable-event-projection-portable) |
| Opt-in bounded native approval callback pool | `approvals::Executor` | `approvals` | [RPC & Native Adapters](rpc-and-native-adapters.md#native-approval-callbacks-approvals) |
| Async streams, awaitable tickets & async teardown | `async_api::EventStream`, `kill_async` | `tokio` | [Async & Presentation](async-and-presentation.md#async-integration-tokio-feature) |
| Multi-viewer raw PTY fanout with slow-viewer drops | `observers::ObserverHub` | `observers` | [Async & Presentation](async-and-presentation.md#pty-observer-fanout-observers-feature) |
| Bounded transcript presentation projection | `transcript::Projector` | `transcript` | [Async & Presentation](async-and-presentation.md#transcript-projection-transcript-feature) |
| Durable SQLite worker supervision, Unix daemon, HTTP/SSE | Workspace sibling crates | separate packages | [Workspace Crates](workspace-crates.md) |

---

## Feature Flags

Until the first registry release, use a local checkout. Choose one dependency entry:

```toml
[dependencies]
# Default: PTY + headless screen (`vt100`, `regex`) + interactive agent profiles
agent-link = { path = "../agent-link" }

# Minimal core: PTY sessions, env, shell, identity, resolve, proc only
# agent-link = { path = "../agent-link", default-features = false }

# Common combinations
# agent-link = { path = "../agent-link", features = ["rpc", "probe", "tokio"] }
# agent-link = { path = "../agent-link", features = ["codex", "acp", "claude", "approvals"] }
```

| Feature | Default | Implies | Extra dependencies | Purpose |
| --- | --- | --- | --- | --- |
| *(none)* | — | — | `portable-pty`, `crossbeam-channel`, `log`, `thiserror`, `libc` (Unix) / `windows-sys` (Windows) | Core PTY, environment, shell, identity, path resolution, process helpers |
| `screen` | **yes** | — | `vt100`, `regex` | Headless terminal parser, normalized text, OSC/DEC signal tap |
| `agent` | **yes** | `screen` | — | `AgentTerminal`, built-in CLI profiles, startup gate, verified delivery |
| `rpc` | no | `agent` | — | `RpcChild` JSON-lines stdio transport, tracked writes, strict UTF-8 |
| `probe` | no | `agent` | — | CLI installation and three-way sign-in probes |
| `codex` | no | `rpc` | `serde_json` | Bounded Codex `app-server` v2 adapter |
| `acp` | no | `rpc` | `serde_json` | Bounded ACP stable-v1 adapter |
| `claude` | no | `rpc` | `serde_json` | Bounded persistent Claude `stream-json` adapter |
| `approvals` | no | — | — | Bounded worker pool for native approval callbacks (pair with `codex`/`acp`/`claude`) |
| `observers` | no | — | — | `ObserverHub` raw PTY fanout and `Terminal::attach_live_recovery*` |
| `transcript` | no | — | — | `Projector` bounded presentation view (also enables `portable` without `serde_json`) |
| `tokio` | no | — | `tokio` (`sync` + `rt` only) | `EventStream`, `Ticket::outcome`, `send_async`, `kill_async` |
| `test-support` | no | — | — | **Development only.** Builds `agent-link-fake-agent` for test suites. Do not enable in applications |

---

## Core Design Contracts

These contracts guide host integration:

1. **Bounded memory and backpressure.** PTY event channels hold 1,024 events (16 for receivers attached after `Exit`); RPC receivers default to 8. Leaving a receiver alive without draining it stalls output reading for that session. Drop unused receivers immediately.
2. **Queued input, synchronous teardown.** `write`, `send`, and `send_line` enqueue bounded work without waiting for child writes. Dropping or calling `kill()` tears down the child process group (Unix) or Job Object / process tree (Windows), with a ~2 s direct-child reap wait. This is not a deadline for every host call: file logging performs synchronous I/O under the recovery lock and can delay attachment, resize, and teardown. Drop handles outside your own locks, and use `kill_async()` in async code.
3. **Tickets are authoritative; events are best effort at shutdown.** Always retain and inspect `Ticket` / `RequestTicket` / `RpcWriteReceipt` outcomes. On shutdown, outstanding tickets settle immediately while final events are omitted if a receiver is full.
4. **YOLO / unattended permissions by default.** Built-in PTY and RPC profile launches pass unattended flags (and disable Codex's sandbox) unless you configure `Permissions::AgentDefault` or `Permissions::Custom`. Folder trust (`TrustPolicy`) defaults to *off* (user answers dialogs).
5. **Diagnostic privacy.** RPC payloads, command arguments, and stderr require `RpcSpawnSpec::trace = true` before transport logging. `EnvPolicy` and probe report debugging redact environment values and version output. Raw PTY output, replay logs, and native event payloads still contain child-produced content. Agent controller diagnostics have a separate [trace policy](permissions-and-probes.md#diagnostic-tracing--privacy).

---

## Wiki Chapters

1. [Getting Started](getting-started.md) — installation, quickstart recipes for PTY shells, interactive agents, and stdio RPC.
2. [PTY & Screen](pty-and-screen.md) — `Terminal`, `SpawnSpec`, `ScreenConfig`, `Vt100Model`, `EnvPolicy`, `TerminalIdentity`, foreground detection, and process helpers.
3. [Agents & Delivery](agents-and-delivery.md) — `AgentTerminal`, built-in profiles, startup dialogs, `SendOptions`, `Ticket`, `Revalidate`, `Timing`, and custom `AgentProfile`s.
4. [Permissions & Probes](permissions-and-probes.md) — `Permissions`, `TrustPolicy`, `AgentProfile::compatibility()`, `probe_agents_detailed`, and diagnostic tracing.
5. [RPC & Native Adapters](rpc-and-native-adapters.md) — `RpcChild`, `portable::EventV1`, `CodexAppServer`, `AcpClient`, `ClaudeSession`, and `approvals::Executor`.
6. [Async & Presentation](async-and-presentation.md) — `tokio` adapters, `attach_recovery`, `observers::ObserverHub`, and `transcript::Projector`.
7. [Workspace Crates](workspace-crates.md) — `agent-link-supervisor`, `agent-link-daemon`, and `agent-link-http`.
8. [Recipes & Troubleshooting](recipes-and-troubleshooting.md) — safe child replacement, fixture capture (`examples/capture.rs`), testing rules, and common failure modes.

## Compatibility Evidence

[Permission-default validation](permission-validation.md) records the versioned launch checks referenced by profile metadata. Native-adapter evidence and limitations are described in [RPC & Native Adapters](rpc-and-native-adapters.md#validation-evidence). These records describe measured operations, not blanket support for future CLI versions.
