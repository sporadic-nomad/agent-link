# Async (`tokio`), Replay Recovery, Observers & Transcripts

This chapter covers asynchronous Tokio integration (`tokio`), session logging and replay recovery (`pty`), bounded raw PTY observer fanout (`observers`), and bounded transcript projection (`transcript`).

**Prev:** [RPC & Native Adapters](rpc-and-native-adapters.md) · **Next:** [Workspace Crates](workspace-crates.md)

---

## Async Integration (`tokio` feature)

Enabling `tokio` adds async wrappers using only Tokio's `sync` and `rt` features (no `futures` crate dependency). Session I/O continues to run on dedicated background threads. Runnable example: [`examples/tokio_shell.rs`](../examples/tokio_shell.rs).

### `EventStream`, `send_async`, `Ticket::outcome`, and `kill_async`

The timeout below requires your application's Tokio dependency to enable `time` in addition to its runtime features, and a runtime with time enabled.

```rust
use std::time::Duration;
use agent_link::{Event, EventStream, PtySize, SpawnSpec, Terminal};

async fn run() -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
}
```

Available async APIs:
- `EventStream::new(rx)` / `term.events_stream()`: bridges a bounded receiver into a matching-capacity `tokio::sync::mpsc` channel using one forwarding thread (`"agent-link-stream"` / `"agent-link-stream-<id>"`). Exposes cancel-safe `next().await` and `poll_next(cx)` (implements `Unpin` so hosts can adapt to `futures::Stream`). Dropping `EventStream` stops its forwarding thread immediately.
  PTY capacities are 1,024 events (16 after `Exit`); RPC defaults to 8 configurable events. An unbounded source gets a 1,024-event Tokio buffer but remains unbounded; a rendezvous source gets one slot. Streams end only after source disconnection and draining: PTY `Exit` leaves the source connected until teardown, while RPC terminal `Exit` or `Error` disconnects it. RPC `StdinError` leaves output draining.
- `ticket.outcome().await -> DeliveryOutcome` (`agent` feature): zero-thread wakeup via `Notify`; cancel-safe.
- `agent_term.send_async(text, opts)`: queues the delivery synchronously on call, then awaits `ticket.outcome()`. (Does not support `Revalidate::Handshake` — use `send()` + `confirm()` + `ticket.outcome()` instead.)
- `kill_async(self)`: available on `Terminal`, `AgentTerminal`, and `RpcChild` (`rpc` feature). Spawns teardown immediately on `tokio::task::spawn_blocking` (does not wait for first `.await`).

> **Deadlock warning:** Never `.await` `send_async` or `Ticket::outcome()` inside the **only** task draining that terminal's `EventStream` — if the stream fills up, the controller stalls and cannot complete the delivery! Also avoid synchronously dropping `Terminal`, `AgentTerminal`, or `RpcChild` on async runtime threads; use `kill_async().await` or `spawn_blocking`.

---

## Session Logging & Replay Recovery

Configure output recording via `SpawnSpec::log` (`SessionLogConfig`):

| Variant | Behavior |
| --- | --- |
| `SessionLogConfig::Off` | No output recorded (`Replay::None`). |
| `SessionLogConfig::Memory { cap_bytes }` *(default: 32 MiB)* | Ring of merged output blocks up to `cap_bytes` (`> 0`). Oldest blocks are evicted when full (at least the newest chunk is kept). |
| `SessionLogConfig::File { path }` | Absolute path only (`Error::InvalidInput` if relative). Unbounded raw output, including any secrets the child prints. Opened before spawn; truncated once child starts. On Unix refuses symlinks (`O_NOFOLLOW`) and non-regular files, enforces `0o600`, takes `flock(LOCK_EX \| LOCK_NB)`. On Windows opens with `FILE_SHARE_READ` only. Write errors permanently stop recording (`Replay::FileFailed`) while retaining the path lock until teardown. |

File recording performs synchronous writes under the recovery lock. Use a local disk: a stalled filesystem can delay output, attachment, resize, and teardown despite bounded child-reap and socket deadlines. Keep the path unchanged while replay readers may still open it.

### `attach()` vs `attach_recovery(cursor)`

- `term.attach() -> (Replay, Receiver<Event>)`: atomically snapshots the retained log and registers a live receiver with zero gap and zero overlap. Call `replay.into_bytes()` to read `Bytes`, `File`, or `FileRange`.
- `term.attach_recovery(from: Option<ReplayCursor>) -> Result<(RecoverySnapshot, Receiver<Event>), ReplayError>`:
  - Pass `None` to request from byte offset `0`, or `Some(ReplayCursor { session, offset })` from a previous snapshot.
  - Cross-session cursors (`ReplayError::SessionMismatch`) and offsets past the boundary (`FutureCursor`) are refused without registering a receiver. Torn-down sessions return `ReplayError::Closed` (natural EOF still succeeds).
  - Returns `RecoverySnapshot`:
    - `requested`, `boundary` (first live byte offset), `retained_start`, `replay_range: Range<u64>`
    - `replay: Replay` (`Replay::FileRange { path, offset, len }` for bounded file slices)
    - `gaps: Vec<ReplayGap>` (`ReplayGapReason::Evicted | RecordingDisabled | FileFailed`)
    - `next_output_seq: u64` (sequence of the first live `Event::Output`)
    - `size: PtySize` (geometry at the boundary)
    - `display: Option<ScreenSnapshot>` *(with `screen` feature)*

> **Display snapshots are not parser checkpoints:** `RecoverySnapshot::display` gives point-in-time visible rows, cursor, and basic modes, omitting cell styling, hidden buffers, and partial escape/UTF-8 fragments. Moreover, `replay` *precedes* `display` — never feed `replay` into a renderer after drawing `display`, or old output will duplicate.

---

## PTY Observer Fanout (`observers` feature)

Enable `observers` to fan out raw PTY output to multiple viewers through `observers::ObserverHub` without one slow viewer stalling the core session or other viewers. Runnable example: [`examples/pty_recovery_observers.rs`](../examples/pty_recovery_observers.rs).

```rust
use std::time::Duration;
use agent_link::observers::{Item, Limits, ObserverHub};

let hub = ObserverHub::new(&term, Limits::default())?;
let viewer = hub.attach(None)?; // or Some(cursor)

if let Some(gap) = &viewer.gap {
    eprintln!("Missing history bytes {:?}; resync renderer", gap);
}
loop {
    match viewer.recv_timeout(Duration::from_secs(1)) {
        Some(Item::Output { bytes, .. }) => {
            // Feed these raw bytes to your renderer.
            let _ = bytes;
        }
        Some(Item::Exit { .. }) => break,
        None => {
            if let Some(reason) = viewer.reason() {
                eprintln!("Disconnected: {reason:?}");
                break;
            }
            // A timeout alone means the session is quiet; keep watching.
        }
    }
}
```

### Defaults & Contracts (`ObserverHub`)

- **Core attachment:** `ObserverHub::new` calls `term.attach_live_recovery_metadata()` (which attaches at the live fence without copying core logs or cloning `ScreenSnapshot` strings) and spawns `"agent-link-observers"` to continuously drain that core receiver.
- **`observers::Limits` defaults:** `viewers: 16`, `history_bytes: 1 MiB`, `history_items: 1,024`, `viewer_bytes: 1 MiB`, `viewer_items: 1,024`, `aggregate_bytes: 8 MiB`, `aggregate_items: 8,192` (all 7 must be `> 0`).
- **Whitelist:** forwards only `Item::Output` and natural `Item::Exit` (no agent controller events or diagnostics).
- **Explicit slow-viewer drops:** if a viewer exceeds `viewer_bytes`, `viewer_items`, `aggregate_bytes`, or `aggregate_items`, the hub disconnects that viewer with `Disconnect::SlowViewer` (readable via `viewer.reason()` even after disconnection).
- **`hub.shutdown()` / `Drop`:** stops fanout and joins the worker thread without tearing down `Terminal`.

*(Note: `observers` also enables `Terminal::attach_live_recovery()` and `Terminal::attach_live_recovery_metadata()` directly on `Terminal`.)*

---

## Transcript Projection (`transcript` feature)

Enable `transcript` for `transcript::Projector` — a dependency-free, synchronous bounded presentation buffer combining screen text and `portable::EventV1` native text observations.

```rust
use agent_link::transcript::{Limits, Projector};

let mut proj = Projector::new(Limits::default())?; // 128 messages, 1 MiB text + origin IDs
// proj.screen(text, RawSpan { generation, start, end, first_seq, last_seq })?;
// proj.native(&event_v1)?;
// proj.begin_segment(); // Call on scroll, resize, alt-screen transition, or interruption
// proj.record_gap();    // Call on dropped output / reconnect gap
let snap = proj.snapshot();
```

### Key Rules

- **Screen provenance:** pair `RecoverySnapshot::display` with the byte boundary and
  output sequence fence from the same recovery snapshot. An independently read
  `term.screen()` can already contain later output. Record recovery gaps explicitly
  before projecting the fenced view.
- **Never a delivery or completion authority:** `Confidence` (`HeuristicScreen`, `NativeItem`, `ProjectionOnly`) describes interpretation strength only. Screen text may contain chrome, wrapped lines, or quoted prompts.
- **Merging:** `native()` projects `EventKind::Text` only. It merges deltas/replacements only when `item_id` is `Some` and `(provider, generation, session_id, turn_id, item_id)` match; otherwise it creates distinct `ProjectionOnly` messages. Out-of-order/duplicate native `seq` for a retained item returns `Error::StaleNative`.
- **Truncation & eviction:** `Update` returns exact `evicted` local message IDs (oldest first) and `gap: true` when a native origin exceeds `Limits::bytes`. Oversized text is truncated on valid UTF-8 boundaries (`Message::truncated = true`; subsequent deltas cannot repair lost prefixes).
