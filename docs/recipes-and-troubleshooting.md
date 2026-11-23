# Recipes, Fixture Capture & Troubleshooting

Practical patterns, tooling commands, and answers to common operational issues when building on `agent-link`.

**Prev:** [Workspace Crates](workspace-crates.md) · **Home:** [User Manual](README.md)

---

## Recipe: Validate a Replacement with `rpc::preflight`

When restarting or upgrading an `RpcChild`, validate the replacement `RpcSpawnSpec` **before** dropping the old child. Keep ownership in the caller so a preflight error leaves that child running:

```rust
use agent_link::rpc::{preflight, RpcChild, RpcSpawnSpec};

fn replace_child(
    current: &mut Option<RpcChild>,
    new_spec: RpcSpawnSpec,
) -> agent_link::Result<()> {
    preflight(&new_spec)?; // Failure leaves `current` untouched
    drop(current.take()); // Synchronous teardown; keep this outside host locks
    let (child, events) = RpcChild::spawn(new_spec)?;
    // Spawn reader/forwarder for `events`...
    drop(events);
    *current = Some(child);
    Ok(())
}
```

Preflight does not reserve resources or guarantee that spawn or agent startup will succeed. After the old child is torn down, a spawn failure leaves `current` as `None`. On Unix, the spawned login shell resolves the launcher: an uninstalled agent can pass both preflight and spawn, then emit `RpcEvent::Exit`. Inspect that event's stderr tail and use `RpcChild::try_wait` to observe the shell's exit status. Windows preflight also resolves the launcher on the child's effective PATH. Plan for startup failures after replacement on either platform.

---

## Recipe: Recording & Sanitizing PTY Fixtures (`examples/capture.rs`)

`examples/capture.rs` (`--features screen`) records real CLI sessions into `.bin` (raw byte stream), `.events.jsonl` (OSC/sync/frame signals), `.txt` (final screen), and `.meta` (capture metadata) files for deterministic testing.

```sh
cargo run -p agent-link --example capture --features screen -- \
  --cwd /path/to/workspace \
  --shell-line "codex" \
  --size 40x120 \
  --duration 15000 \
  --script "3000:hello{enter};5000:{wait-text:hello}" \
  --out tests/fixtures/codex-0.154.0-demo \
  --sanitize
```

### Key `capture` Options

| Flag | Description |
| --- | --- |
| `--cwd <dir>` | Required working directory. |
| `--cmd <prog> [--arg <a>]...` | Direct spawn mode (mutually exclusive with `--shell-line`). |
| `--shell-line "<line>"` | Spawns `$SHELL -l` / `%COMSPEC%`, waits `--shell-settle` (default `1500` ms), types `<line>` + Enter. |
| `--identity <id>` | `honest` (default), `vscode` (with optional `--term-version`), or `custom:K=V,K=V`. |
| `--size <rows>x<cols>` | Default `40x120` (bounds `1..=1000`). |
| `--duration <ms>` | Default `20000` ms (unlimited in `--interactive`, exit via `Ctrl-]`). |
| `--script "<ms>:<payload>;..."` | Timed inputs. Supports `\r \n \t \e \xHH \u{H...}` and tokens `{enter}`, `{up}`, `{down}`, `{esc}`, `{ctrl-c}`, `{paste:<text>}`, `{wait-text:<needle>}` (30 s wait). |
| `--no-answer-queries` | Disable automatic DA1/DSR/CPR/DECRQM 2026 replies (enabled by default). |
| `--out <prefix>` | Atomically writes `.bin`, `.events.jsonl`, `.txt`, `.meta`. |
| `--sanitize` | Scrubs home path (`/home/u`), home dirname, and username (`u`), replays `.txt`, and warns on remaining matches. Requires `--out` and `<agent>-<version>-<scene>` naming (or `--agent`, `--agent-version`, `--scene`). |
| `--interactive` | Unix-only raw local TTY forwarding (`Ctrl-]` stops). |

---

## Testing Conventions

- **Run PTY tests serially:** Pass `-- --test-threads=1` to `cargo test`. Concurrent `openpty` calls on macOS can fail with `Unknown error: -6`.
- **Fake agent binary (`test-support`):** Integration tests use the scripted stand-in binary `agent-link-fake-agent` (enabled by `test-support`). Keep `test-support` disabled in production builds.
- **Live tests are `#[ignore]`d:** Tests that invoke real installed CLIs make real API calls and can write CLI trust/config files. Do not run `--ignored` tests unless you intend to hit real accounts in an isolated environment.
- **Shared validation script (from repo root):**
  - Routine checks (fmt, feature-matrix Clippy with `-D warnings`, workspace rustdoc): `python3 scripts/check.py --skip-tests`
  - Full non-live suite: `python3 scripts/check.py`

---

## Troubleshooting

### Session freezes / stops producing output
- **Likely cause:** An unconsumed `Receiver<Event>` (or `EventStream` / `Receiver<RpcEvent>`) reached capacity (1,024 for PTY, 8 default for RPC).
- **Fix:** Ensure every receiver returned by `spawn`, `subscribe`, `attach`, or `attach_recovery` is continuously drained in its own thread/task, or explicitly `drop()`ped if unused. Never `.await` `send_async()` / `Ticket::outcome()` on the same task that drains `EventStream`.

### `Error::Spawn("openpty: ... Unknown error: -6")` on macOS
- **Likely cause:** Multiple threads called `Terminal::spawn` or `AgentTerminal::spawn` simultaneously.
- **Fix:** Serialize `spawn` calls behind a `Mutex` or retry on `Error::Spawn`.

### `AbortReason::ForegroundUnproven` on Windows
- **Likely cause:** `AgentSpec::foreground_policy` defaults to `ForegroundPolicy::ExactOnly`, while Windows foreground detection returns `Confidence::Heuristic`.
- **Fix:** Only use `SendOptions::strict: true` on Windows if you explicitly opt into `ForegroundPolicy::AcceptHeuristic` and accept its heuristic tradeoff.

### `AbortReason::SubmitUnverified`
- **Likely cause:** Either the profile cannot read its composer (`opencode`, `cursor`), the CLI cleared/redrew asynchronously without showing the expected transition within `submit_verify_window` (700 ms), or paste was never echoed.
- **Fix:** **Do not automatically resend** (`SubmitUnverified` often occurs after the message was already accepted). Use `codex`/`claude`/`pi` or a custom measured `AgentProfile::pending_input` implementation, or switch to structured stdio (`codex`, `acp`, `claude`, `rpc`) for protocol-level request/turn confirmation.
- **Unreadable composer only:** if unverified submission is acceptable, set `SendOptions::unreadable_composer` to `UnreadableComposer::EnterOnce` (and use `reset_with` for resets). The Enter is written once without evidence and the outcome becomes `UnverifiedEnter`. A paste whose output never goes quiet still gets no Enter.

### `AbortReason::UnverifiedEnter`
- **Cause:** The send opted into `UnreadableComposer::EnterOnce` and the profile could not read its composer. Exactly one Enter was written after the paste's output went quiet; nothing confirmed what it did.
- **Fix:** Treat the message as sent but unconfirmed. **Do not automatically resend**, because a resend can submit twice. Check the agent's reply (for example a transcript source or the screen) to confirm.

### Native adapter closes with `Failure::ConsumerLagged` or `Overloaded`
- **Likely cause:** The host stopped draining `events.recv_until(...)` long enough for `events` / `event_bytes` to saturate, or reverse requests exceeded `control_capacity` / `approvals`.
- **Fix:** Drain the adapter's event receiver continuously on a dedicated loop/thread (offload heavy approval callbacks to `approvals::Executor`), or raise `CodexLimits` / `AcpLimits` / `claude::Limits` appropriately.

### `supervisor` / `daemon` refuses to start with `RecoveryRequired`
- **Likely cause:** A previous worker runtime was born and left a durable runtime fence (fences are preserved across crashes and controlled shutdowns because escaped descendant processes cannot be disproved by direct-child reaping alone).
- **Fix:** Establish positive external evidence that the fenced `RuntimeId`'s provider work and descendants are gone, then clear the fence explicitly via `Supervisor::confirm_stopped(id)` or `agent_link_daemon::recover(...)`. Never delete `owner.lock` or the SQLite store while an active owner may exist.
