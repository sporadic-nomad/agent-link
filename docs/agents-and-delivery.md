# Interactive Agents (`AgentTerminal`), Profiles & Verified Delivery

This chapter covers driving interactive agent CLIs in a PTY using the `agent` feature (enabled by default).

**Prev:** [PTY & Screen](pty-and-screen.md) · **Next:** [Permissions & Probes](permissions-and-probes.md)

---

## `AgentTerminal` Overview

`AgentTerminal` wraps a `Terminal` (forcing `ScreenConfig::enabled = true`) and adds two internal threads (`agent-link-monitor-<id>` and `agent-link-jobs-<id>`):

- **Startup gate:** launches the agent in the shell (or monitors a `SpawnMode::Direct` child immediately), steers recognized startup dialogs (when opted in), emits `Event::AgentState`, and sets `blocking()` while your host UI should mask interactive input.
- **Activity tracker:** follows OSC 0/2 Braille title spinners and OSC 9;4 progress reports and emits `Event::Activity` (`Working`, `Idle`, `Unknown`). Activity is informational only and never gates delivery.
- **Serialized delivery queue:** delivers `send()`, `reset()`, and `reset_and_send()` calls one at a time in FIFO order with bracketed paste and screen-verified Enter submission.

### Spawning and Launching

```rust
use agent_link::agent::{AgentSpec, AgentTerminal, ProfileRegistry};
use agent_link::{PtySize, SpawnSpec};

let profile = ProfileRegistry::builtin().get("codex").unwrap();
let spec = SpawnSpec::shell(std::env::current_dir()?, PtySize { rows: 40, cols: 120 });
let agent = AgentSpec::new(profile); // Defaults: Permissions::Unattended, TrustPolicy::default()

let (term, events) = AgentTerminal::spawn(spec, agent)?;
term.launch()?; // Types the launch command into the shell and arms the startup gate
```

If `AgentSpec::profile` is `None` (`AgentSpec::default()`), `AgentTerminal` runs a bare shell with no launch, no startup gate, and no agent/activity events (`state()` remains `AgentState::ShellOnly` until `Exited`).

For `SpawnMode::Direct` with a profile, the child itself *is* the agent: `spawn()` arms the startup gate immediately and `launch()` is a no-op. Note that `AgentSpec::permissions` does **not** rewrite host-supplied `Direct` arguments; build those arguments from `profile.launch(...)` if you want profile policy applied.

---

## Agent State, `blocking`, and `Activity`

Read current state at any time without blocking on I/O:
- `term.state() -> AgentState`:
  - `ShellOnly`: no agent running yet, no profile, or the agent exited back to the shell.
  - `Starting`: launched; waiting for startup output to settle.
  - `TrustPrompt`: a startup dialog is on screen (being steered, or waiting for user input). Never transitions to `Ready` while visible.
  - `Ready`: startup gate settled, through composer recognition or its timed fallback; delivery still checks readiness and ownership before input.
  - `Exited`: PTY session ended.
- `term.blocking() -> bool`: `true` during startup settle and automated dialog steering; lifted once startup settles (including if a `TrustPrompt` is left for the user to answer), on monitor overflow/failure, or on teardown.
- `term.activity() -> Activity`: `Working`, `Idle`, or `Unknown`.
- `term.status() -> (AgentState, bool, Activity)`: reads all three atomically under one lock.

Changes arrive as `Event::AgentState { id, state, blocking }` and `Event::Activity { id, state }` on all receivers (`AgentTerminal` registers its monitor receiver last, so host receivers always receive the triggering PTY output before the controller's state transition).

`status()` does not synchronize with event receivers. Events received after a snapshot
may describe older transitions that were queued or awaiting publication. To display
current status, subscribe before the initial snapshot, then use state and activity
events as prompts to read `status()` again instead of applying their potentially older
values over the snapshot.

---

## Verified Message Delivery (`send`, `SendOptions`, `Ticket`)

### `SendOptions`

```rust
pub struct SendOptions {
    pub submit: bool,              // default: false (paste only)
    pub strict: bool,              // default: false
    pub on_shell: ShellFallback,   // default: ShellFallback::PasteInert
    pub revalidate: Revalidate,    // default: Revalidate::None
}
```

- **`submit: false` (default):** pastes sanitized text into the composer without pressing Enter.
- **`submit: true`:** pastes text, waits for the pasted text to appear on screen, writes Enter (`\r`), and verifies on screen that the composer transitioned out of holding the pasted text.
- **`strict: true`:** requires `ForegroundPolicy`-trusted proof that the crate-launched agent owns the terminal both at the commit point and after submit verification; forbids `ShellFallback::PasteInert`.
- **`on_shell`:** what happens if the shell stays in front after one automatic relaunch attempt:
  - `ShellFallback::PasteInert` *(default)*: places a **single-line** message onto the shell's input line *without* pressing Enter (`DeliveryOutcome::PastedUnsubmitted`). Falls back to `AbortReason::ShellOnly` for multi-line messages or `cmd.exe` with a profile.
  - `ShellFallback::Fail`: pastes nothing and aborts with `AbortReason::ShellOnly`.
- **`revalidate`:** commit-point check executed right after the pre-paste output lull (and before strict proofs/paste):
  - `Revalidate::None`
  - `Revalidate::Sync(Box<dyn Fn() -> bool + Send + Sync>)`: runs on the job thread; must not block on the terminal/agent; returning `false` or panicking aborts with `AbortReason::Revalidate`.
  - `Revalidate::Handshake { timeout }`: emits `Event::CommitPoint { id, ticket }` and waits up to `timeout` for `term.confirm(ticket_id, proceed)`.

Profile callbacks run outside the controller lock. Delivery discards their screen
verdicts if output or the screen revision changes during evaluation, then observes
again. Time spent in a delayed callback cannot make obsolete composer evidence
stable enough to verify submission.

### `Ticket` and `DeliveryOutcome`

`send()`, `reset()`, and `reset_and_send()` return a `Ticket` immediately:
- `ticket.id() -> TicketId` (1-based monotonic per `AgentTerminal`).
- `ticket.wait(timeout) -> Option<DeliveryOutcome>` (blocks up to `timeout`).
- `ticket.try_outcome() -> Option<DeliveryOutcome>` (non-blocking poll).
- *(With feature `tokio`)*: `ticket.outcome().await -> DeliveryOutcome`.

Possible `DeliveryOutcome` values:

| Outcome | Meaning |
| --- | --- |
| `Delivered` | Pasted, and (with `submit: true`) a composer transition was verified on screen. Terminal evidence, not a provider protocol ACK. |
| `PastedUnsubmitted` | Shell remained in front after one relaunch; single-line text placed inertly on the shell input line without Enter. |
| `Aborted(reason)` | Not delivered (see `AbortReason` below). |

### `AbortReason` Reference

| Variant (`as_str()`) | Cause |
| --- | --- |
| `NotConnected` (`"not_connected"`) | Session ended or internal monitor failed/overflowed. Immediate refusal if already disconnected. |
| `ShellOnly` (`"shell_only"`) | Shell stayed in front under `ShellFallback::Fail` / `strict`, multi-line/unclearable text, or shell returned to foreground after paste (controller sends Ctrl-C to clear shell line). |
| `ForegroundUnproven` (`"foreground_unproven"`) | Another program owns the terminal, or foreground confidence is rejected by `ForegroundPolicy` (e.g. Windows heuristic under `ExactOnly`). |
| `TrustPromptBlocking` (`"trust_prompt_blocking"`) | A startup/trust dialog was on screen where the message or verification would go. |
| `Revalidate` (`"revalidate"`) | `Revalidate::Sync` returned `false`/panicked, or `Revalidate::Handshake` received `false`/timed out. |
| `SubmitUnverified` (`"submit_unverified"`) | Paste was not seen, composer appeared unchanged after Enter, or profile cannot read composer (`pending_input` returned `None`). **Do NOT auto-resend.** |
| `Disposed` (`"disposed"`) | `AgentTerminal` was dropped before completion. |
| `WriteFailed` (`"write_failed"`) | Underlying PTY write failed. |
| `Unsupported` (`"unsupported"`) | `reset()` / `reset_and_send()` called without a profile or on a profile with `reset_command() == None`. |
| `EmptyText` (`"empty_text"`) | Nothing remained after stripping control characters (`\t` and `\n` kept; `\r` becomes `\n`). |
| `InputTooLarge` (`"input_too_large"`) | Raw text exceeded `AgentLimits::max_message_bytes` (refused immediately, **no `Event::Delivery`**). |
| `QueueFull` (`"queue_full"`) | `AgentLimits` byte or job budget exhausted (refused immediately, **no `Event::Delivery`**). |

> **Never auto-resend `SubmitUnverified`:** The agent may have already accepted the message and begun executing before redrawing its composer (or the profile may simply lack composer-read support, as with `opencode` and `cursor`). By default `Timing::submit_retry_max` is `0` (Enter is sent at most once). Raising `submit_retry_max` enables extra Enter attempts when text still appears in the composer, which can double-submit if the CLI redraws late.

---

## Conversation Resets (`reset` & `reset_and_send`)

- `term.reset() -> Ticket`: sends the profile's `reset_command()` (`"/clear"` for `claude` and `codex`; `"/new"` for `pi`, `opencode`, `cursor`) with `submit: true` and `on_shell: ShellFallback::Fail`.
- `term.reset_and_send(text, opts) -> Ticket`: executes a reset followed by `text` as **one** atomic delivery ticket:
  1. Reserves `text.len() + cmd.len()` bytes (using 1 job slot).
  2. Sends `reset_command` (`strict: false`, `on_shell: Fail`, `revalidate: None`). If reset fails, `text` is not sent.
  3. Waits for fresh output after reset's Enter and settles until ready or `startup_quiet` (capped at `startup_max`).
  4. Sends `text` with `submit: true`, `on_shell: Fail`, and your `opts.strict` / `opts.revalidate`.

Reset commands are never retried automatically.

---

## Admission Limits (`AgentLimits`) & Monitor Overflow

`AgentSpec::limits` (`AgentLimits`) defaults to:
- `max_message_bytes`: **256 KiB** (`262_144` bytes, measured on raw text before sanitization).
- `max_pending_bytes`: **4 MiB** (`4_194_304` bytes across queued, running, and unpublished-completion jobs).
- `max_pending_jobs`: **128** jobs (including unpublished completions).

Capacity is released only after `Event::Delivery` finishes publishing, so draining your event receiver keeps admission flowing. Rejected calls (`InputTooLarge`, `QueueFull`, `NotConnected`) resolve their `Ticket` immediately without launching an agent or emitting `Event::Delivery`.

Separately, the internal controller monitor queue retains at most **1,024 events** (max **8 KiB** payload each). If the monitor overflows, automation permanently disables for that session (`NotConnected`), `blocking` lifts, and raw PTY events continue normally.

---

## Timing Configuration (`Timing`)

All `Duration` fields in `AgentSpec::timing` are clamped to `24 hours` at spawn. Key defaults:

| Field | Default | Role |
| --- | --- | --- |
| `startup_quiet` / `startup_max` | `450 ms` / `3 s` | Fallback settle after launch/dialog keys |
| `trust_watch` / `ready_dialog_watch` | `12 s` / `1 s` | Startup dialog watch window; post-ready late-dialog window |
| `dialog_settle` / `approve_cooldown` | `300 ms` / `350 ms` | Delay before first dialog key; min spacing between keys |
| `approve_attempts` / `nav_presses` | `2` / `4` | Max Enters / arrow keys per dialog |
| `startup_buffer_chars` | `16,000` | Rolling raw output window scanned for dialogs |
| `pre_paste_quiet` / `pre_paste_max` | `120 ms` / `2 s` | Wait for quiet output before pasting |
| `paste_echo_quiet` / `paste_echo_max` | `250 ms` / `2 s` | Wait after paste before pressing Enter |
| `submit_verify_quiet` / `submit_verify_window` | `250 ms` / `700 ms` | Submit verification quiet & per-Enter window |
| `submit_retry_max` | `0` | Extra Enter retries (opt-in; disabled by default) |
| `idle_debounce` / `progress_stale` | `600 ms` / `2.5 s` | `Activity::Idle` debounce; OSC 9;4 stale expiry |
| `probe_timeout` / `revalidate_timeout` | `8 s` / `10 s` | Probe command deadline; suggested handshake timeout |

---

## Built-in Profiles & Registry

`ProfileRegistry::builtin()` registers five built-in profiles in order: `claude`, `opencode`, `pi`, `codex`, `cursor`.

| ID | Launchers | PTY Unattended Flags / Env | Pre-trust (`FlagsOnly`) | Reset | Composer Verification |
| --- | --- | --- | --- | --- | --- |
| `claude` | `claude` | `--permission-mode bypassPermissions` (if `accepts_bypass_warning()`) | None (steers Trust + BypassWarning; Bypass persists in `~/.claude/settings.json`) | `/clear` | Measured (`ClaudeProfile::accepting_bypass_warning(false)` omits flag) |
| `codex` | `codex` | `--dangerously-bypass-approvals-and-sandbox` (plus `-c check_for_update_on_startup=false` always) | `-c projects={"<cwd>"={trust_level="trusted"}}` (non-persistent; skipped under `cmd.exe`) | `/clear` | Measured (also steers FullAccess & UpdatePrompt; vetoes `model: loading`) |
| `pi` | `pi` | *(none needed)* | `--approve` (non-persistent; dialog steering uses `PiTrust::Persist` by default or `PiTrust::SessionOnly`) | `/new` | Measured (reads between `────` rules) |
| `opencode` | `opencode` | `OPENCODE_PERMISSION={"*":"allow"}` env default | None (no startup dialog) | `/new` | **Unreadable** (`SubmitUnverified` on submit) |
| `cursor` (**WIP**) | `cursor-agent`, `agent` | `--force` (on `agent`) | `--trust` (**persists** `~/.cursor/projects/<slug>/.workspace-trusted`) | `/new` | **Unreadable** (`SubmitUnverified` on submit) |

Cursor's authenticated `--force` startup/composer behavior remains unverified. Its
profile is available for experimentation; the recorded trust and signed-out login
screens do not establish authenticated composer compatibility. See the
[WIP validation scope](permission-validation.md#cursor-work-in-progress).

Replace a built-in or register your own profile using `ProfileRegistry::register(Arc::new(...))`:

```rust
use std::sync::Arc;
use agent_link::agent::profiles::{ClaudeProfile, PiProfile, PiTrust};
use agent_link::agent::ProfileRegistry;

let mut registry = ProfileRegistry::builtin();
registry.register(Arc::new(ClaudeProfile::accepting_bypass_warning(false)));
registry.register(Arc::new(PiProfile::with_trust(PiTrust::SessionOnly)));
```

---

## Writing a Custom `AgentProfile`

Implement `agent::AgentProfile` (`Send + Sync`) to support another CLI or customize screen detection:

- **Required methods:**
  - `id(&self) -> &str`
  - `dialog(&self, text: &str) -> Option<Dialog>` — return `Some(Dialog { kind, key })` when a dialog is visible (`key: None` pauses for the user). Accepts either normalized screen text or raw rolling output; normalize with `screen::normalize(text)`.
  - `input_ready(&self, screen: &str) -> bool` — recognize the ready composer; return `false` for dialogs or unreadable screens.
- **Important defaulted methods:**
  - `launchers(&self) -> &'static [&'static str]`
  - `launch(&self, perms, trust, cwd, shell) -> LaunchPlan`
  - `pending_input(&self, screen, sent) -> Option<bool>` — defaults to `detect::pending_input_default(screen, sent)` (`Some(true)` = still in composer, `Some(false)` = submitted/cleared, `None` = unreadable).
  - `reset_command(&self) -> Option<&'static str>`
  - `signals(&self) -> SignalHints`
  - `login_check(&self) -> Option<LoginCheck>`
  - `rpc_launch_with_options(&self, options: &RpcLaunchOptions) -> Result<RpcLaunch>`
  - `compatibility(&self) -> Compatibility`

Use helpers in `agent::detect` (`has_all`, `has_any`, `from_last`, `selected_option`, `steer_two`, `pending_input_where`, `composer_holds`) and keep callbacks fast — they run outside the controller lock, and stale results (when screen/state revisions changed while running) are discarded automatically.

Runnable example: [`examples/custom_profile.rs`](../examples/custom_profile.rs) (`cargo run -p agent-link --example custom_profile`).
