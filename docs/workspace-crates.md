# Workspace Companion Crates (`supervisor`, `daemon`, `http`)

Alongside the core `agent-link` crate, the repository workspace includes three **unpublished** companion crates (`publish = false`):
- `agent-link-supervisor` (`supervisor/`)
- `agent-link-daemon` (`daemon/`)
- `agent-link-http` (`http/`)

They are excluded from the published `agent-link` package on crates.io and do not add dependencies (such as SQLite or HTTP parsers), async runtimes, or modified `Drop` behavior to `agent-link`. Default Cargo commands at the repository root select `agent-link` only; pass `-p <crate>` to build or test them.

**Prev:** [Async & Presentation](async-and-presentation.md) · **Next:** [Recipes & Troubleshooting](recipes-and-troubleshooting.md)

---

## `agent-link-supervisor` (`supervisor/`)

Application-hosted durable coordination for workers that must track intent and receipts across client disconnects or process restarts. Uses bundled SQLite (**requires a C compiler**), SHA-256 prompt digests, and `fs2` filesystem locks.

```toml
[dependencies]
agent-link-supervisor = { path = "../agent-link/supervisor", features = ["codex"] }
```

### Core Model & Lifecycle

1. **Provision & open:** provision an owner-only directory on a local filesystem and call `Supervisor::open(path, Binding { provider, session }, Limits::default())`. Acquires an exclusive `owner.lock` and opens the SQLite store.
2. **Runtime fence & recovery:** inspect `supervisor.fenced_runtime() -> Option<RuntimeId>`.
   - `resume(now, factory)` refuses with `Error::RecoveryRequired` if a prior birth fence is still active.
   - `adopt(id, now, factory)` attaches to a surviving runtime matching the fenced `RuntimeId`.
   - `confirm_stopped(id)` clears the fence only after your host positively proves the old runtime and any descendants have stopped (never inferred from silence or PID liveness alone).
3. **Leases & stop control:** keep a cloned `StopHandle` (`supervisor.stop_handle()`) on an independent control path, call `attach` / `renew` / `detach` for client leases, and call `tick(now)` periodically. Once all leases expire and `detached_ttl` elapses, `tick` triggers `stop.request()`.
4. **Durable submit & interrupt:**
   - `submit(id, payload)` persists `Outcome::SubmittedUnknown` before calling `Runtime::submit`. Replaying the same `(id, payload)` returns the stored receipt without calling the provider; reusing `id` with different bytes returns `Error::Conflict`. Unresolved prior turns block new submits with `Error::Busy`.
   - `interrupt(id, target)` targets a previously `Accepted` submit in the current incarnation.
   - `reconcile(binding, evidence)` advances `SubmittedUnknown -> Accepted -> Terminal` with exact native evidence before exposing terminal bytes.
5. **Drop & fences:** dropping `Supervisor` triggers `StopHandle::request()`, **leaving the durable runtime fence intact** so incomplete cleanup remains explicit after crashes or restarts.

### Defaults (`Limits`) & Optional `codex` Feature

- `Limits::default()`: `operations: 1,024`, `controls: 64`, `payload_bytes: 1 MiB`, `result_bytes: 16 KiB`, `database_bytes: 32 MiB`, `leases: 8`, `lease_ttl: 30 s`, `detached_ttl: 300 s`. (Persistence budgets are immutable once created.)
- Feature `codex` provides `codex::CodexRuntime` (`CodexRuntime::spawn`, `resume`, `poll_evidence`, `cleanup_complete`, `wait_cleanup_until`, `confirm_stopped`), bridging `CodexAppServer` with `request_timeout: 30 s` and `evidence_capacity: 64`.

Test command:
```sh
cargo test --locked -p agent-link-supervisor --all-features -- --test-threads=1
```

---

## `agent-link-daemon` (`daemon/`)

A local **Unix-only** foreground service and Rust client (`agent-link-daemon`) built on `agent-link-supervisor` for hosting one existing Codex thread per private `0700` directory. (Compiles on Windows, but refuses hosting/discovery there.)

### Running the Daemon & CLI Client

```sh
# 1. Create an owner-only directory on a local filesystem
mkdir -m 700 /private/path/my-worker

# 2. Start the foreground daemon for an existing Codex thread ID
cargo run --locked -p agent-link-daemon -- serve /private/path/my-worker EXISTING_FULL_THREAD_ID

# 3. Send JSON requests from another terminal
printf '%s\n' \
  '{"op":"status"}' \
  '{"op":"submit","id":"req-001","payload":"Reply briefly"}' \
  | cargo run --locked -p agent-link-daemon -- client /private/path/my-worker
```

### Protocol, Security & Bounds

- **Endpoint discovery & auth:** `serve` verifies directory ownership (`uid == geteuid()`, not symlink, mode `0700` or stricter), binds two `127.0.0.1:0` TCP listeners (ordinary ops + reserved `control` listener for `stop`), and writes `endpoint.json` (`0600`) containing `address`, `control`, and a random 256-bit hex `token`. `Client::connect(root)` injects `"token"` automatically.
- **Framing & operations:** 4-byte big-endian `u32` length-prefixed JSON (`FRAME_BYTES = 2 MiB`). Ops: `status`, `receipt`, `attach`, `renew`, `detach`, `submit`, `interrupt`, `stop`.
- **Timeouts:** leases default to `30 s`; idle client connections close after `2 s` (keepalive/renew must be `< 2 s`); detached expiry is `5 minutes` (even if native work is active).
- **Fences & explicit recovery:** restart after *any* previously born runtime fails closed until an operator establishes external containment/teardown evidence and calls `agent_link_daemon::recover(root, binding, limits, expected_runtime_id, proof_callback)`.

Test command:
```sh
cargo test --locked -p agent-link-daemon --all-features -- --test-threads=1
```

---

## `agent-link-http` (`http/`)

A bounded, authenticated, **read-only** loopback HTTP/1.1 + SSE presentation server (`agent-link-http`) wrapping an existing `Arc<Terminal>` via `observers::ObserverHub`. Provides no spawn, input, resize, approval, or mutation endpoints.

### Binding & Running the Example

```sh
# Generate a random 32..=256 printable ASCII bearer secret
export AGENT_LINK_HTTP_TOKEN="$(openssl rand -hex 24)"
cargo run -p agent-link-http --example observe
```

In Rust:
```rust,ignore
use std::sync::Arc;
use agent_link_http::{Config, Server};

let mut config = Config::new(secret_token); // defaults to 127.0.0.1:0
let server = Server::bind(Arc::clone(&terminal), config)?;
println!("Listening on http://{}", server.local_addr());
```

### Routes & Limits

All requests require `Authorization: Bearer <token>` (constant-time verified) and receive `Cache-Control: no-store`, `Connection: close`, `X-Content-Type-Options: nosniff`:
- **`GET /snapshot`:** returns JSON with an incarnation-scoped `cursor` (`"<incarnation>:<session>:<offset>"`), `session`, `offset`, `next_output_seq`, `geometry` (`rows`, `cols`), `parser_checkpoint: false`, `resize_history: false`, `display: null`, plus optional `projection` installed via `server.publish_projection(revision, cursor, json_bytes)`.
- **`GET /events`:** `text/event-stream` SSE feed. Accepts optional `Last-Event-ID: <cursor>` (returns `409 Conflict` for foreign/future cursors). Emits `snapshot` (on fresh connect), `output` (raw byte array + cursor ID), `exit` (natural EOF), and `resync` (before closing on history gap, slow viewer, or frame limit). Closes after `stream_lifetime` (default `60 s`); clients must reconnect explicitly.
- **Default `Config` limits:** `clients: 16`, `header_bytes: 8,192`, `frame_bytes: 256 KiB`, `projection_bytes: 64 KiB`, `active_bytes: 1 MiB`, `scratch_bytes: 8 MiB`, `io_timeout: 2 s`, `stream_lifetime: 60 s`.

Test command:
```sh
cargo test --locked -p agent-link-http --all-features -- --test-threads=1
```
