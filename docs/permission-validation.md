# Permission-default validation

This is the public evidence record referenced by the built-in Codex and OpenCode
profiles. It ships with the crate so compatibility metadata can be inspected without
local developer notes. For configuration, see [permissions and probes](permissions-and-probes.md).

Recorded **2026-11-14 on macOS arm64**; this page preserves those results rather than
claiming a new validation run. These checks assess the YOLO/unattended launch
settings. They are separate from recorded composer-delivery evidence, native Windows
testing and authenticated model execution. No external model turns were submitted.

| Concern | Result | Scope |
| --- | --- | --- |
| Main `codex app-server` accepts and applies `-c` after the subcommand | Passed | Source at `b1573f102413e96a6481087922bf425231222dee`; live `codex-cli 0.169.0` config/read |
| Cursor `--force` preserves authenticated startup/composer detection | **WIP — unverified** | 2026.11.13-eacade1 accepts the flag; only signed-out login could be captured |
| OpenCode wildcard retains the external-directory workaround | Passed | Real OpenCode 1.18.25 ACP task/read execution with a local scripted model peer |

Release scope updated **2026-11-16**: Cursor support is work in progress. The
authenticated composer check below is follow-up validation before removing the WIP
label and does not block publication with that limitation documented. These historical
measurements are unchanged.

## Codex: app-server launch configuration

The profile launches `codex app-server -c 'approval_policy="never"' -c
'sandbox_mode="danger-full-access"'`. The evidence covers the main `codex` executable
with the `app-server` subcommand.

The actual source chain at the pinned revision is:

- [`MultitoolCli` and app-server dispatch](https://github.com/openai/codex/blob/b1573f102413e96a6481087922bf425231222dee/codex-rs/cli/src/main.rs): the main CLI parses config overrides and passes `root_config_overrides` to `run_main_with_transport_options`.
- [`config_override_args::try_parse_from`](https://github.com/openai/codex/blob/b1573f102413e96a6481087922bf425231222dee/codex-rs/cli/src/config_override_args.rs) and [`CliConfigOverrides`](https://github.com/openai/codex/blob/b1573f102413e96a6481087922bf425231222dee/codex-rs/utils/cli/src/config_override.rs): `-c` is global, and overrides are collected across subcommand boundaries in argument order.
- [App-server config loading](https://github.com/openai/codex/blob/b1573f102413e96a6481087922bf425231222dee/codex-rs/app-server/src/lib.rs): parses and loads the received overrides.

The ignored [live regression](../tests/rpc_vendor_live.rs) launches the exact profile
argv through `RpcChild`, initializes the protocol and calls `config/read`.
Its temporary config explicitly requests `on-request` and `read-only`:

| Launch | Reported approval policy | Reported sandbox mode |
| --- | --- | --- |
| `Permissions::AgentDefault` | `on-request` | `read-only` |
| Default unattended profile | `never` | `danger-full-access` |

The config file remains unchanged. No thread or turn is started. Reproduce explicitly:

```sh
cargo test --locked --all-features --test rpc_vendor_live -- --ignored --test-threads=1 --nocapture
```

`AGENT_LINK_LIVE_CODEX` can select an executable instead of the one on `PATH`.
This establishes startup config application, not the policy of a resumed thread or
a subsequent protocol request that explicitly overrides permissions.

## OpenCode: real subagent external-file reads

The [live probe](../scripts/check_opencode_permissions.py) uses the real **1.18.25**
CLI over ACP. It initializes a fresh session in a temporary Git workspace. A local
OpenAI-compatible SSE peer instructs the parent to call `task`, instructs that
subagent to `read` a marker file outside the workspace, and observes the actual
tool result returning through the subagent and parent.

No credentials or existing config are copied. On macOS the child also runs under a
sandbox that denies writes to the real home and outbound connections except localhost.
The peer generates fixed responses; it is not an external model service.

| `OPENCODE_PERMISSION` | Read result seen by subagent | Result returned to parent | ACP outcome |
| --- | --- | --- | --- |
| Unset (native control) | No | No | Still pending three seconds after the peer requested the read; no approval forwarded |
| `{"external_directory":"allow"}` | Yes | Yes | `end_turn`, no approval requests |
| `{"*":"allow"}` | Yes | Yes | `end_turn`, no approval requests |

The native observation is consistent with the known unforwarded subagent approval
bug; a finite deadline alone does not prove an infinite wait. The two positive cases
directly establish that the wildcard preserves the external-directory workaround.

This also matches the source: [config](https://github.com/anomalyco/opencode/blob/v1.18.25/packages/opencode/src/config/config.ts)
merges the env object; [agent construction](https://github.com/anomalyco/opencode/blob/v1.18.25/packages/opencode/src/agent/agent.ts)
places user rules after defaults, including the built-in external-directory `ask`;
[permission evaluation](https://github.com/anomalyco/opencode/blob/v1.18.25/packages/opencode/src/permission/index.ts)
selects the last matching rule. Explicit user rules can still restrict operations.

Reproduce against a separately installed/pinned 1.18.25 binary:

```sh
python3 scripts/check_opencode_permissions.py --opencode /path/to/opencode-1.18.25 --out /tmp/agent-link-permission-check
```

The probe refuses other versions. It does not establish OpenCode v2 compatibility,
all possible subagent tools, or behavior under arbitrary user-supplied restrictions.

## Cursor: work in progress

The temporary official **2026.11.13-eacade1** CLI accepts `--force`; its help describes
the same setting as `--yolo`. With a fresh config and no credentials it reaches
`Press any key to log in...`. The
[capture metadata](../tests/fixtures/cursor-2026.11.13-force-login.meta) records the
exact flag, version, platform and absence of credentials. The raw PTY stream,
final screen and event log accompany it. Replay tests keep the profile classification
at `Unknown` and the pending-input verdict at `None`, without composer evidence.

No authenticated Cursor installation or key was available. The existing 2026.09.23
trust fixture was not captured with `--force`. Neither fixture proves authenticated
YOLO startup/composer compatibility. Cursor remains available as a WIP profile.
Run the following follow-up check only when live validation is explicitly authorized:

```sh
cargo test --locked --all-features --test cursor_force_live -- --ignored --test-threads=1 --nocapture
```

Set `CURSOR_API_KEY` in the local environment and optionally `AGENT_LINK_LIVE_CURSOR`
to the executable path. The ignored check uses an isolated config, permits trust
steering only in its disposable workspace, requires a genuinely recognized ready
composer, and pastes a marker without Enter. Before removing the WIP label for
authenticated startup/composer support, record a passing run and its CLI version;
a login-only or skipped run does not count. Add a sanitized recording of that
successful run if profile markers need updating.

Even a passing startup/paste check would not establish verified submission. Cursor's
composer remains unmeasured for the library's delivery proof, so unreadable input
still yields `SubmitUnverified`. With the opt-in `UnreadableComposer::EnterOnce` it
yields `UnverifiedEnter`, which is not verified submission either.
