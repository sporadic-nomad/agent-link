# agent-link-supervisor

Application-hosted durable worker coordination. This sibling crate is unpublished;
it does not add dependencies, a runtime, or different Drop behavior to `agent-link`.

The application provides the worker process and authenticated IPC, or uses the
separate unpublished [local daemon](../daemon/README.md). Supply a native
`Runtime` or enable the optional `codex` feature for the built-in owned-stdio Codex
bridge, with exact receipt correlation and finite cleanup observation.
The supervisor supplies exclusive ownership, synchronized operation intent, permanent
ID/digest deduplication, strict provider bindings, runtime reuse fences, exact-turn
cancellation records, bounded attachment leases and a separate stop capability.

Use it by workspace/path dependency:

```toml
agent-link-supervisor = { path = "../agent-link/supervisor" }
```

The crate uses bundled SQLite (a C compiler is needed), SHA-256 and filesystem locks.
There is no async runtime, vendor SDK or dependency on the core crate. Default Cargo
commands at the repository root select only `agent-link`; build/test this package with
`cargo test --locked -p agent-link-supervisor --all-features -- --test-threads=1`.

The normal lifecycle is:

1. Durably provision a private directory on a local filesystem, then open its store
   with an exact existing provider `Binding`.
2. Inspect its runtime fence. Authenticate and `adopt` a surviving runtime if possible;
   otherwise positively prove old teardown and call `confirm_stopped` before `resume`.
3. Keep the returned runtime inside the worker. Retain a `StopHandle` on an independent
   control path and drive bounded leases through periodic `tick` calls.
4. Submit exact bytes with a stable operation ID. Repeated IDs replay receipts without
   calling the provider. Unknown outcomes block new submits until exact reconciliation.
5. Persist native evidence with `reconcile` before exposing terminal data to viewers.
6. Request stop, await exact native cleanup/unsubscribe completion, then confirm stopped.

`Runtime` is a trusted host adapter: strict resume, exact-turn interruption, bounded
native I/O and concurrent stop fencing must be implemented there. `confirm_stopped`,
`adopt` and `reconcile` consume trusted evidence; they do not infer it from PID liveness
or silence. Stop and Drop do not clear the durable runtime fence. This makes incomplete
cleanup explicit after a crash, even if the child probably exited.

Operation records are never evicted. Full stores refuse new IDs; reserved cancellation
capacity and direct stop remain separate. Prompts are hashed rather than persisted;
opaque terminal result bytes may contain sensitive data. Keep stores private.

The [supervisor guide](../docs/workspace-crates.md#agent-link-supervisor-supervisor) documents budgets, sync/locking assumptions,
evidence boundaries and validation limitations. This crate provides the coordination
layer; daemon/client IPC lives in the separate host crate. It does
not promise exactly-once provider execution or certify power-loss durability.
