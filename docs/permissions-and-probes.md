# Permissions, Trust, Compatibility & CLI Probes

This chapter covers permission postures (`Permissions`), folder-trust steering (`TrustPolicy`), foreground trust (`ForegroundPolicy`), static profile compatibility metadata, and runtime CLI probes (`probe` feature).

**Prev:** [Agents & Delivery](agents-and-delivery.md) · **Next:** [RPC & Native Adapters](rpc-and-native-adapters.md)

---

## Permission Defaults (`Permissions`)

**By default, `agent-link` launches profile-based PTY and RPC sessions in unattended ("YOLO") mode.** Restrictions require explicit configuration.

Set `AgentSpec::permissions` (for PTY shell launches) or `RpcLaunchOptions::permissions` (for `AgentProfile::rpc_launch_with_options`):

| Variant | Behavior |
| --- | --- |
| `Permissions::Unattended` *(default)* | Adds the profile's unattended flags/env defaults (allowing tools without prompts where supported, and disabling Codex's sandbox). |
| `Permissions::AgentDefault` | Adds no permission flags or env defaults; the CLI's own user/project configuration applies. |
| `Permissions::Custom { args, env }` | Replaces unattended flags/env with your exact arguments and environment variables (host `EnvPolicy::remove` still wins). |

```rust
use agent_link::agent::{AgentSpec, Permissions, ProfileRegistry, RpcLaunchOptions};

let profile = ProfileRegistry::builtin().get("claude").unwrap();

// PTY: use Claude's own configured permission policy
let mut agent = AgentSpec::new(profile.clone());
agent.permissions = Permissions::AgentDefault;

// RPC: supply explicit custom flags
let launch = profile.rpc_launch_with_options(
    &RpcLaunchOptions::default().with_permissions(Permissions::Custom {
        args: vec!["--permission-mode".into(), "default".into()],
        env: vec![],
    }),
)?;
```

> **Note:** `RpcLaunchOptions` is `#[non_exhaustive]`; build from `RpcLaunchOptions::default()` using `.with_resume(bool)`, `.with_probe(bool)`, and `.with_permissions(Permissions)`. Calling the older `profile.rpc_launch(resume, probe)` uses `Permissions::Unattended`.

### Per-Profile Permission Mapping

| Profile | `Permissions::Unattended` (PTY) | `Permissions::Unattended` (RPC) |
| --- | --- | --- |
| `claude` | `--permission-mode bypassPermissions` (if `accepts_bypass_warning()`) | `--permission-mode bypassPermissions` |
| `codex` | `--dangerously-bypass-approvals-and-sandbox` | `-c approval_policy="never" -c sandbox_mode="danger-full-access"` |
| `pi` | *(none; native tools run without approval)* | *(none)* |
| `opencode` | `OPENCODE_PERMISSION={"*":"allow"}` env default | `OPENCODE_PERMISSION={"*":"allow"}` env default |
| `cursor` | `--force` (explicit denies still apply) | `Error::Unsupported` |

For OpenCode, a narrow compatibility workaround (`profiles::opencode::OPENCODE_EXTERNAL_DIRECTORY_WORKAROUND` = `r#"{"external_directory":"allow"}"#`) is available for hosts that want to pre-allow external-directory reads in `task` subagents (which OpenCode 1.18.21–1.18.25 otherwise does not forward over ACP) without granting `{"*":"allow"}`. Pass it via `Permissions::Custom` env or `EnvPolicy::set`.

The [permission validation record](permission-validation.md) gives the measured versions and scope. Cursor support is **work in progress**: authenticated `--force` startup/composer behavior remains unverified, and its signed-out recording is not submission evidence. The authenticated check is follow-up validation before removing that label.

### Environment Precedence in `LaunchPlan` / `RpcLaunch`

1. `env_defaults` apply **only** if your `EnvPolicy` does not already set that key.
2. Explicit `env` (e.g. from `Permissions::Custom`) overrides `EnvPolicy::set`.
3. `EnvPolicy::remove` wins over both.
4. On Unix login-shell launches, shell startup files (`.zshrc`, `.bashrc`, etc.) execute afterward and can override environment variables again.

---

## Trust Policy & Foreground Policy

### Folder Trust (`TrustPolicy`)

Folder trust is separate from `Permissions` and defaults to **off** (`TrustPolicy::default()` = `PreTrust::Off, steer_dialogs: false`), leaving workspace trust dialogs for the user.

Call `TrustPolicy::unattended()` (`PreTrust::FlagsOnly, steer_dialogs: true`) for automated/headless flows. Be aware of persistence side effects:
- **Cursor** `--trust` saves `~/.cursor/projects/<cwd-slug>/.workspace-trusted`.
- **Claude** accepting the bypass warning saves `"skipDangerousModePermissionPrompt": true` in `~/.claude/settings.json` (and answering its trust dialog saves trust in `~/.claude.json`).
- **Pi** answering its default `PiTrust::Persist` dialog saves `~/.pi/agent/trust.json`; use `PiProfile::with_trust(PiTrust::SessionOnly)` or `--approve` (`PreTrust::FlagsOnly`) to avoid writing `trust.json`.
- **Codex** `-c projects=...` pre-trust flag writes nothing to disk (not used under `cmd.exe`).

### Foreground Trust (`ForegroundPolicy`)

`AgentSpec::foreground_policy` governs strict delivery (`SendOptions::strict`) and relaunch checks:
- `ForegroundPolicy::ExactOnly` *(default)*: accepts only `Confidence::Exact` (Unix `tcgetpgrp`). Refuses automatic launch/strict delivery on Windows.
- `ForegroundPolicy::AcceptHeuristic`: accepts `Confidence::Heuristic` (Windows Toolhelp process snapshot).

---

## Profile Compatibility Metadata (`compatibility`)

Without spawning anything, `profile.compatibility()` returns `agent::compatibility::Compatibility`:
- `revision: Option<&'static str>` — crate version (not CLI version).
- `pty: Support`, `rpc_launch: Support`, `login_check: Support` (`Supported`, `Unsupported`, `Unknown`).
- `evidence: &'static [Evidence]` — operation-scoped provenance records naming `operation`, `kind` (`Declaration`, `Negotiated`, `SyntheticFixture`, `CapturedTerminal`, `CapturedTranscript`, `LiveCheck`), optional `cli_version` / `platform`, and `source`.

Claude has separate `CapturedTranscript` records for binding the writer and parsing final replies. Both cover direct PTY launches of Claude Code 2.1.296 on macOS arm64. The host must control the configuration directory and enforce one writer per observed session. The capture shows a second CLI resuming an active UUID and appending indistinguishable turns. [Paired recordings](../tests/fixtures/transcripts/claude-2.1.296/README.md)

---

## CLI Installation & Sign-In Probes (`probe` feature)

Enable the `probe` feature (implies `agent`) to inspect whether agent CLIs are installed and signed in. Probes block up to their timeout and run one thread per profile — call them on a worker thread, never on a UI thread.

### Quick Summary (`probe_agents`) vs Detailed Reports (`probe_agents_detailed`)

Runnable example: [`examples/probe_agents.rs`](../examples/probe_agents.rs) (`cargo run -p agent-link --example probe_agents --features probe`).

```rust
use agent_link::agent::{profiles, Timing};
use agent_link::env::EnvPolicy;
use agent_link::probe::{probe_agents_detailed, ProbeMode, ProbeOptions};

let mut env = EnvPolicy::default();
env.remove("SECRET_TOKEN");

let mut options = ProbeOptions::default();
options.timeout = Timing::default().probe_timeout; // default 8s
options.mode = ProbeMode::LoginShell;              // or ProbeMode::Direct

let reports = probe_agents_detailed(&profiles::builtin(), &env, &options);
for (id, report) in &reports {
    println!(
        "{id}: installed={} signed_in={:?} readiness={:?}",
        report.result.installed, report.result.signed_in, report.readiness
    );
}
```

### Three-Way Sign-In Semantics (`ProbeResult`)

`ProbeResult { installed: bool, signed_in: Option<bool> }`:
- `installed`: `true` if a configured launcher answered `--version` with exit code `0`. A legacy `false` is inconclusive when the probe could not run. If the host working directory cannot be read, detailed reports retain `RuntimeReadiness::Untested` and `ContextDiagnostic::WorkingDirectoryUnavailable`; no commands run. Supply an explicit absolute `ProbeOptions::cwd` to avoid depending on that directory.
- `signed_in`:
  - `Some(true)`: the profile's `login_check` command exited `0`.
  - `Some(false)`: the login check exited non-zero **and** its output matched `signed_out_marker`.
  - `None`: unknown ("assume fine" — e.g. no `login_check`, not installed, timed out, truncated/non-UTF-8 output, or unrecognized message). Never treat `None` as signed out.

### Context Modes, Bounds, and Caveats

- **`ProbeMode::LoginShell` (default):** on Unix runs through `<shell> -l -i -c` (or `<shell> -c` for `sh`); startup files may alter `PATH`, `cwd`, or env (`ContextDiagnostic::ShellMayOverrideContext`), and aliases/functions have no resolved executable path. On Windows, resolves directly on `PATH`/`PATHEXT`.
- **`ProbeMode::Direct`:** resolves launchers directly using `env.effective_path()` in `cwd` and populates `ProbeObservation::executable` (reused for the login check).
- **`EnvPolicy`:** the full policy (`inherit: false`, `set`, `remove`, `path_prepend`) is applied to both `--version` and login-check commands.
- **Bounds:** each stream retains at most **8 KiB** while draining excess output. Truncated or invalid UTF-8 login output forces `signed_in` to `None`. Timeout kills the process group/tree and reaps up to ~2 s.
- **Privacy:** `ProbeReport`'s `Debug` impl redacts `version_output`, and `[probe-agents]` logs never include version output, login output, or environment values.

---

## Diagnostic Tracing & Privacy

- **PTY Agent Controller:** logs `[agent-link:<id>]` trace lines at `info` when `AgentSpec::trace` is `Some(true)`, or when `None` and `AGENT_LINK_TRACE=1` or `cfg!(debug_assertions)`.
- **RPC & Native Adapters:** `RpcSpawnSpec::trace` and `trace_stderr` default to `false`. `trace: true` logs payloads, stderr, and command arguments (truncated to 400 chars). `trace_stderr: true` logs stderr alone; it does not enable argument, stdin or stdout logging. Stderr may contain secrets with either opt-in; leave both disabled unless needed for diagnosis.
