# agent-link-daemon

Unpublished Unix local service for **one existing Codex conversation per private
worker directory**. It is a separate product layer; core dependencies, runtime
requirements, default Cargo selection and ordinary Drop behavior are unchanged.
The crate compiles on Windows but refuses hosting/client endpoint discovery there:
a Windows ACL provisioning and ownership policy has not been implemented.

Run as a foreground service owned by your service manager (or a separately launched
process). The client does not spawn, kill or recreate workers. Viewer/launcher exit
leaves the worker running until its finite detached expiry. No live provider calls
are needed for tests; using `serve` with your installed Codex does use your account.

```sh
# Provision this owner-only directory on a local filesystem and durably sync its
# creation and ancestors according to the supervisor filesystem contract first.
mkdir -m 700 /private/path/my-worker
cargo run --locked -p agent-link-daemon -- serve /private/path/my-worker EXISTING_FULL_THREAD_ID
# In another terminal:
printf '%s\n' '{"op":"status"}' '{"op":"submit","id":"request-001","payload":"Reply briefly"}' | cargo run --locked -p agent-link-daemon -- client /private/path/my-worker
```

Service managers must preserve the worker directory and must not automatically
remove `owner.lock`, delete the database, clear runtime fences or retry with a new
conversation. Restart after any previously born runtime intentionally fails closed.
Even controlled shutdown preserves the fence: direct-child/dispatcher cleanup cannot
prove escaped descendants have stopped. An operator or containing host must establish
that exact incarnation's external containment/teardown evidence before using the
supervisor's explicit recovery API. No IPC command clears or overrides a fence.
The service never automatically resends an operation after timeout or disconnect.

## Local protocol and Rust client

`Client::connect(root)` reads `endpoint.json` from the owner-only directory. Both
listeners bind exactly IPv4 `127.0.0.1` on OS-assigned ports. Every request carries a
random 256-bit bearer secret; the Rust client injects it. The secret is stored only
in the private endpoint file, never printed, put in argv or exposed through Debug.
The file is removed on orderly shutdown; a crash can leave a stale endpoint file.
Authentication is possession of this file, not OS user authentication or TLS. Other
processes running as the same user, filesystem ACLs, privileged users, inherited
credentials and service-manager environment remain deployment trust boundaries.
The host checks Unix owner UID, no symlink and mode 0700 or stricter; operators must
also exclude permissive extended ACLs and protect directory ancestors. No arbitrary
remote executable, shell, environment, working-directory or recovery command exists.

Requests/responses are JSON objects prefixed by a four-byte unsigned big-endian byte
length. Fields below omit `token`, which raw clients must add on **every** request.
One request is in flight per connection. There is no output/transcript stream; HTTP,
SSE and PTY observer presentation belong to a different layer.

| Request | Response/meaning |
| --- | --- |
| `{"op":"status"}` | Durable worker ID, fenced generation and native busy flag |
| `{"op":"receipt","id":"x"}` | Persisted receipt or null |
| `{"op":"attach"}` | One lease bound to this authenticated connection |
| `{"op":"renew"}` | Renew that same live lease |
| `{"op":"detach"}` | Release this connection's lease |
| `{"op":"submit","id":"x","payload":"exact UTF-8 text"}` | Commit unknown intent and dispatch at most once |
| `{"op":"interrupt","id":"cancel-x","target":"x"}` | Commit cancellation for x's exact accepted native turn |
| `{"op":"stop"}` | Request this runtime's independent stop path |

Rust `Client::request` sends stop through a separate reserved control listener; raw
clients use `endpoint.json`'s `control` address. That listener accepts only stop.
All other operations use `address`. Submit/interrupt initially return a committed
`unknown` receipt because native execution runs independently. Query until exact
accepted/terminal evidence has been persisted. Replaying the same ID and bytes
returns its receipt; different bytes return conflict. An unknown receipt never
licenses a new ID or resend. Results are bounded native JSON bytes represented as
an integer array; they may contain private output. Receipt digests and runtime IDs
identify exact evidence, not a provider exactly-once guarantee.

Leases default to 30 seconds; renew more frequently than that and send a request at
least once per second to keep the idle connection alive. Connections idle for two
seconds are closed. Disconnect releases its lease when admitted, otherwise its TTL
expires. Last detach/expiry starts the five-minute default detached deadline; active
native work does not postpone it. Store persistence and filesystem I/O remain
synchronous: a stuck filesystem can stall owner-loop policy progress. Native calls
cannot stall that loop; already accepted stop calls bypass it and ordinary queues.

## Bounds and evidence

Frames are limited to 2 MiB before allocation; native payloads to the supervisor's
1 MiB default. There are at most 16 ordinary and two reserved control client threads,
16 queued requests, one queued/active native dispatch and two acceptance evidence
items. Each client has at most one active frame/write/reply. Reads and writes each
have a two-second total deadline, including trickled frames. Full sockets/queues are
closed or refused rather than accumulating work. Ordinary client saturation cannot
consume reserved stop admissions. Saturating both reserved slots delays another
stop admission by at most the socket deadline, subject to OS scheduling. Stop is
at-most-once and does not await a normal native response. Default native requests
have a 30-second deadline. The owner polls every 10 ms, continuously persists exact
acceptance and terminal evidence even without viewers, and services monotonic ticks.
Native and ledger budgets are independently enforced. Native evidence overflow
stops conservatively; it does not infer a result. No unbounded durable work queue is
provided. A refused busy request has not been accepted unless its stable ID already
exists; query that ID to distinguish uncertainty after a client timeout.

Tests use the shared credential-free Codex peer in separate processes, covering
launcher/viewer disconnect, same-ID replay/conflict and native start counts, exact
terminal persistence, unauthenticated/oversized/full clients, reserved stop and
lease expiry during blocked native submit, and crash/reopen fences. These are
synthetic macOS process tests, not authenticated Codex or process-tree containment
validation. Runtime support on other Unix systems requires native CI evidence.

## Explicit recovery procedure

1. Stop the service manager's restart loop. Acquire the worker through
   `Supervisor::open` with its original binding and limits (an active owner refuses
   the lock). Read `fenced_runtime()` and retain **all 16 worker ID bytes and the
   generation**. Do not remove files or infer identity from a PID/thread ID.
2. Establish positive external evidence that this exact birth's provider work,
   subscriptions, callbacks and descendants are gone. For example, a containing
   host can audit its exact birth-bound platform sandbox/container after verified
   teardown. This service does not establish such containment itself. If the host
   cannot obtain that evidence, leave the fence closed and retain the store for
   inspection; do not restart a replacement automatically.
3. Drop the inspection handle, then call `agent_link_daemon::recover(root, binding,
   original_limits, expected_full_runtime_id, proof_callback)`. The public function's
   compiled rustdoc example shows this local API procedure. Its trusted callback
   must return true only for the audited exact ID; stale IDs and false proof are
   rejected without clearing the fence. The function reacquires the exclusive lock
   and rechecks the exact persisted birth before invoking proof and confirming.
4. Restart `serve` with the same existing conversation binding. A new runtime birth
   does **not** settle unknown operations. Inspect/reconcile them using exact native
   evidence; never resend under a new ID merely because cleanup was confirmed.

This is a host/operator evidence boundary, not an automatic PID liveness probe or a
claim that bearer-authenticated clients can authorize recovery. The local recovery
API deliberately cannot settle a receipt or delete deduplication history.
