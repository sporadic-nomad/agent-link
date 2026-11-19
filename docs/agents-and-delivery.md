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

To select a model, set `agent.model = Some(ModelChoice::new("provider/model:tag"))`
before spawning a profile in a shell. Import `agent_link::agent::ModelChoice` and
chain `.with_effort("high")` to request an explicit effort argument. The CLI
interprets the model string itself. For example, Pi can infer its thinking level
from a `:high` suffix. [Pi 1.5.2 model resolver](https://github.com/earendil-works/pi/blob/b4afdddf44e059c67ccf5097e5317d913521b470/packages/coding-agent/src/core/model-resolver.ts)

With `model: None` (the default), the CLI gets the profile's ordinary launch
arguments. With an explicit choice, the order is permission and trust arguments,
model arguments, then tool arguments. Environment precedence is the same in both
cases. Explicit launch and delivery-triggered relaunch use the line prepared at spawn.
The resolved shell executable selects the quoting rules. On Unix, a shell named
`fish` uses fish quoting, which preserves literal backslashes and apostrophes.
Other Unix shell names use POSIX quoting. Windows uses `cmd.exe` quoting.

A choice on a bare shell or Direct child returns `Error::InvalidInput`. The same
error covers blank, control-bearing or flag-leading identifiers, words the selected
shell renderer cannot quote, an empty mapping, or a conflict with existing model
options. A standalone `--` in the base arguments is reserved as the end of options.
If the profile cannot map the model or the requested effort, spawn returns
`Error::Unsupported`. The mapped labels and their versioned sources are listed
under [PTY model and reasoning effort](#pty-model-and-reasoning-effort).

For an explicit choice, `Error::InputTooLarge` reports the byte count of the typed
launch, including quoting, clear-line keys and Enter, against
`SpawnSpec::input_limits.max_write_bytes`. If the write fits that budget, spawn
returns `Error::InvalidInput` when the quoted command before Enter exceeds the
shell line limit. Unix shells can read in canonical mode, so the check allows
4,095 UTF-8 bytes on Linux and Android, 1,023 on Apple platforms, or a conservative
254 on other POSIX targets. The cmd.exe limit is 8,191 UTF-16 code units.
[Linux canonical input](https://docs.kernel.org/driver-api/tty/n_tty.html) ·
[Darwin tty input limit](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/tty.h) ·
[POSIX terminal limits](https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/limits.h.html) ·
[cmd.exe command-line limit](https://learn.microsoft.com/en-us/troubleshoot/windows-client/shell-experience/command-line-string-limitation)

These checks run before spawning the shell. With tools, model validation precedes
tool configuration, and the final line must also fit. At launch time the write
still goes through normal PTY queue admission. Startup marker warnings for
selected launches omit screen rows, which can contain the echoed command.

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

## Codex, Pi and Cursor PTY turns

Controlled direct launches support provider-specific transcript observations with a host-enforced isolated namespace per terminal. Codex exposes explicit final text. Pi marks inferred replies, and Cursor distinguishes explicit completion, next-prompt inference and sensitive mixed text. [Setup, limits and recorded evidence](pty-transcript-sources.md)

## Claude PTY final replies

Enable the off-by-default `turns` feature to read completed replies from a directly launched Claude Code **2.1.296** on **macOS arm64**. Each receiver belongs to one `AgentTerminal`. A host that forwards replies can keep its PTY event receiver for interactive display and poll its turn receiver separately.

**Establish exclusive writer access first.** The host must control which processes can write inside the configuration root and ensure at most one writer per logical conversation, including sessions selected with interactive commands. For example, a host can control all launches in an isolated worker account and serialize admission to each session. A second same-account CLI can resume a live UUID and append replies that carry the original session's identifying fields. A randomly named directory, mode 0700, and an advisory lock cannot enforce exclusion on their own. [Recorded overlapping resume](../tests/fixtures/transcripts/claude-2.1.296/README.md)

1. Create an `agent::turns::ExclusiveClaudeRoot` for the existing isolated configuration directory and keep it alive while the guarantee holds. One lease can serve multiple terminals using distinct sessions.
2. Build a `SpawnSpec::direct` with the absolute path to the native executable named `claude` and a fresh, lowercase v4 UUID passed as `--session-id UUID`. Set `CLAUDE_CONFIG_DIR` to the leased directory, or make the effective `HOME/.claude` point there. Admission uses the child’s merged PTY environment, including custom terminal identity settings and any overrides or removals in the launch environment policy. Relative configuration overrides resolve from the canonical launch cwd. The child receives an explicit `CLAUDE_CONFIG_DIR` that names the directory by its macOS volume and file ID. Immediately before spawn, both the lease pathname and this ID-based path must resolve to the pinned directory. Use a local volume that supports lookup by file ID. The recorded project-name encoding supports ASCII cwd paths of at most 200 bytes. The launcher also accepts `--model MODEL`, `--tools ""`, `--strict-mcp-config`, and a `--settings` object containing only boolean `showTurnDuration`.
3. Call `AgentTerminal::spawn_with_claude_turns(spec, agent, &lease)` with the Claude profile, then `term.attach_claude_turns(TurnOptions::default())`. Ordinary spawns have no attachment. Poll `receiver.status()` for `TurnStatus::Active`, which includes the verified start offset. The first prompt can be part of skipped history when it creates the transcript.
4. Poll `receiver.try_recv()`. Each `TurnReply` contains the final assistant message's text blocks joined with blank lines, and a `truncated` flag. A typed human prompt, an assistant `end_turn`, and a `turn_duration` record delimit a reply. Repeated final text from distinct turns is delivered twice. Tool output, reasoning, partial or interrupted turns and API errors contribute no reply. A nonempty pull does bounded synchronous local filesystem and process I/O.
5. On `TurnStatus::Gap(reason)`, discard any local partial turn. A gap stays visible. After a verified `/clear` or other session change, a fresh attachment takes a new EOF baseline. This can be the terminal’s first attachment if a peer reader previously verified its initial writer record. `AttachError::Busy` means the previous worker is still retiring or the process-wide capacity is in use. Revoke or drop the lease **before** relaxing writer exclusion. All receivers under it withhold queued text. A host logger can revoke the lease during launch preparation or after spawn. Revocation waits for admitted process creation to finish. PTYs already started keep running.

Use a trusted installation of the recorded CLI. The reader checks the executable path and the version in the writer record. It also samples the direct child's kernel birth identity, terminal ownership and pinned transcript. It checks each candidate's bytes before publication and checks a 64-byte cursor window again on a queued pull. When the mode header is incomplete, the reader keeps the first transcript open across polls and checks that its name still identifies that file. Truncation checks start at first open and include lengths sampled while the header is incomplete. A shrink below an observed length yields `TurnGap::Truncated`, including during attachment. A switch away and back between samples or a rewrite entirely outside the checked bytes may escape detection. The host's isolation must cover writers for the whole lease lifetime. Ambiguous, missing, replaced, malformed or oversized evidence can end observation with a typed gap.

When two readers sample a peer on opposite sides of a session switch, the reader with the stale sample retries once. If the retry confirms the peer switched between UUIDs distinct from the reader’s session, the reader keeps its queued replies. Launch preparation reserves a UUID. Peer readers start counting that reservation as a possible writer at final admission, immediately before process creation. An observed overlap or an admitted writer whose identity is still uncertain can invalidate readers that share its transcript directory. The reader compares directory identities and treats ASCII names that differ only by case as possible aliases until the directories exist. A peer that closes after its UUID was sampled triggers the same bounded retry. Independent queues survive, and an observed overlap still invalidates the affected readers. Confirmed exit of the observed child closes its receiver even before PTY EOF. Its terminal can remain open for replay while other readers continue. The retained terminal still occupies a launch reservation.

`TurnOptions` defaults to a 64 KiB read and verification budget per pass, 256 records per batch, 1 MiB per line, 256 KiB per reply and 16 queued replies totaling at most 1 MiB. The parser tracks up to 256 message IDs of at most 256 bytes each per turn and 256 content blocks per message. Each writer record is capped at 64 KiB. A full queue ends observation with `TurnGap::Overflow`. Each terminal admits one reader, and the process admits four controlled launches and four active or retiring workers. The external integration test runs the full public API against the pinned native binary and a loopback model peer. [Public API acceptance test](../tests/claude_transcript_live.rs) · [Recording and reproduction command](../tests/fixtures/transcripts/claude-2.1.296/README.md)

---

## PTY questions

Enable the optional `questions` feature, choose one source for the running `AgentTerminal` and continue draining its ordinary PTY event receiver.

- `observe_questions()` uses the profile’s screen parser. Its first observed frame establishes a baseline. A newly drawn, complete Cursor shell approval includes the visible command, working directory and both action labels. Wrapped commands, shortened paths, partial redraws and menus that offer persistent permission changes stay at the terminal.
- `observe_transcript_questions(Arc<TurnReceiver>)` observes fresh Claude `AskUserQuestion` records in a verified direct PTY. Use `observe_provider_questions(&receiver)` with an isolated Codex PTY for Plan-mode `request_user_input`. Keep polling its receiver for final replies. The reader must belong to this terminal. A call must follow an attributable fresh prompt. The currently displayed question must agree with its recorded prompt and all visible option labels. An unobservable tool call can still block ordinary delivery through `question_dialog`.
- `observe_feed_questions(Arc<dyn QuestionFeed>)` reads the current, ordered requests from a host-authenticated transport pinned to this PTY and provider conversation. A feed supplies up to 64 pending requests and rechecks the exact request and its dispatch guard before a mutation. A failed or partial poll withholds new decisions. Reattach the feed or transcript source when the agent or conversation resets.
- Custom profiles can implement `question_on_screen`, `question_shown` and `answer_keys`. The default hooks withhold requests or return `Unsupported`. Callbacks run outside controller locks. A `KeyStep::when` predicate checks the exact selection, page or editor draft before its write.

`Event::Question` carries the terminal-local `QuestionId` and the complete ordered items. Keep the ID with the displayed form. Supply one `Answer::Choice`, `Choices` or `Text` per item to `term.answer_question(qid, answers)`. Indices start at zero. The synchronous call admits a job without waiting for child or network I/O. The repeatable `AnswerTicket` supports `try_result`, `wait(timeout)` and, with `tokio`, `result().await`. An `Ok(())` means the complete key sequence entered the PTY input queue or the bound server accepted the answer. Wait for the closing event to remove the form.

`Event::QuestionClosed` carries the same ID. `Host` means an accepted host answer preceded observed disappearance. `Desk` denotes a question that left its observed screen without a host answer. `Gone` covers loss of the source or foreground, observer stop, an uncertain mutation and a feed request withdrawn by an unknown actor. `stop_questions()` invalidates the open ID. Events at teardown are best effort. A callback can keep its detached thread until it returns. Disposal resolves outstanding tickets independently.

Answers share the delivery job and byte limits. Only one answer may be outstanding per terminal. A PTY answer sanitizes control characters, checks a quiet screen, verifies its foreground owner and checks the question again at each key. Literal text sent through a feed retains its original contents. Each of at most 64 key steps must fit the PTY write bound and its settle delay must be at most five seconds. If a partial write or network mutation could have been accepted, inspect the terminal after `Unconfirmed`. The library disables a second dispatch to that question ID.

A dialog can carry up to 16 items, 32 options per item, 8 KiB per field and 64 KiB overall. Provider IDs are limited to 128 bytes. The observer withholds oversized or ambiguous dialogs whole. Eight reserved event slots include queued and in-flight question publications. Screen disappearance must persist across samples for 300 ms. Screen changes, reattachment and launch changes fence callbacks. A proven foreground owner is required for both relay and mutation. Windows has no captured native question evidence. Debug formatting omits prompts, choices, provider IDs, command text, answer strings and key bytes.

Native recordings on macOS arm64 cover Claude Code 2.1.296 single-choice, multiple-choice, multiple-item review and a one-line text answer, and Codex 0.154.0 Plan-mode choices, multiple pages and one-line notes. Cursor 2026.11.13-eacade1’s packaged decision view and keyboard handler were exercised in a real PTY with instrumented callbacks. Its `y` selects “Run (once)” and `n` begins a request for alternative instructions. The fixture did not run the proposed command. [Question fixtures](../tests/fixtures/questions/README.md)

### OpenCode session-bound questions

For OpenCode 1.18.21, launch a TUI attached to a server that the host controls with `opencode attach URL --session ID --dir DIRECTORY`. Pass an already-connected TCP stream for that verified loopback server, the same absolute directory, session ID and Basic credentials to `agent::questions::opencode::OpenCodeQuestions::from_verified_stream`. Keep the feed in an `Arc` and pass a clone to `observe_feed_questions`. The host must control session changes from the keyboard and call `revoke()` before switching the TUI to another session or server.

The feed reads `GET /question` for that directory and filters by session. On an explicit answer it rechecks the full request and terminal authority and posts the selected labels or text to `/question/{id}/reply`. It uses one socket for its entire lifetime, with a three-second deadline and a 256 KiB response limit per exchange. An ambiguous POST closes the socket and resolves the answer as `Unconfirmed`. The native acceptance check observes the dialog, answers Blue and checks that the local model receives the tool result. [Recorded exchange and reproduction](../tests/fixtures/questions/opencode-1.18.21/README.md)

Use the native adapter’s approval callback for structured stdio requests. [Native approvals](rpc-and-native-adapters.md#native-approval-callbacks-approvals)

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

### Questions that take keyboard input

The delivery thread calls `AgentProfile::question_dialog` on the visible rows drawn
since launch. A match at the commit point stops the paste, including a paste-only send.
For a submit, it also checks while waiting for the pasted text to appear and before
each Enter. A match there stops the next Enter. The paste and an earlier Enter attempt
may already have reached the agent. After an Enter, delivery evaluates the usual
composer evidence to determine whether the submission succeeded.

After a post-paste match, delivery rechecks which program owns the terminal. If the
shell has taken it back, delivery sends Ctrl-C to clear the inherited draft and reports
`ShellOnly`. A different or unknown owner gives `ForegroundUnproven`. For an agent
question, the ticket and delivery event report `Aborted(DialogBlocking)` and the
question stays available for the user to answer. The startup gate and `blocking()`
stay as they were in this case. Inspect the session before sending again. Recognition
depends on the dialog layouts measured for the profile, and the screen can change
between a check and its write.
If a redraw invalidates the observation, delivery checks again. An unavailable check
at the end of the verification window gives `SubmitUnverified`.

| Built-in profile | Measured screen that takes keyboard input |
| --- | --- |
| Claude Code 2.1.296 | `AskUserQuestion` single and multiple questions, multi-select, and the final answer review. [Recorded menu](../tests/fixtures/claude-2.1.296-question-single.txt) · [Recorded review](../tests/fixtures/claude-2.1.296-question-multipage-review.txt) |
| Codex CLI 0.154.0 | Plan mode `request_user_input`, including multiple pages and the notes editor. Its ordinary ready header can remain visible above the active question. [Recorded Plan question](../tests/fixtures/codex-0.154.0-question-plan.txt) · [Recorded notes](../tests/fixtures/codex-0.154.0-question-notes-empty.txt) |
| OpenCode 1.18.21 | The `question` picker with numbered choices, a custom-answer row and selection controls. [Recorded picker](../tests/fixtures/opencode-1.18.21-question.txt) |
| Cursor 2026.11.13-eacade1 | Shell approval offering “Run (once)” and “Skip & tell the agent what to do instead”. [Recorded renderer](../tests/fixtures/cursor-2026.11.13-shell-approval-renderer.txt) |

The Claude, Codex and OpenCode recordings used isolated CLI homes and local model peers
on macOS arm64. Two Claude replies quote the menu text above a fresh composer.
[Short reply](../tests/fixtures/claude-2.1.296-prose-dialog-footer.txt) · [Scrolled reply](../tests/fixtures/claude-2.1.296-long-reply-select-hint.txt)

The Cursor recording used the official Darwin arm64 distribution's `decision-surface`
component, shell-decision option builder and Ink terminal renderer with `pwd` as
sample input. It measures recognition of that menu. Authenticated startup, composer
behavior and command execution require separate evidence. [Capture metadata](../tests/fixtures/cursor-2026.11.13-shell-approval-renderer.meta)

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
| `Aborted(reason)` | Delivery stopped. The reason describes which input may already have reached the terminal. |

### `AbortReason` Reference

| Variant (`as_str()`) | Cause |
| --- | --- |
| `NotConnected` (`"not_connected"`) | Session ended or internal monitor failed/overflowed. Immediate refusal if already disconnected. |
| `ShellOnly` (`"shell_only"`) | Shell stayed in front under `ShellFallback::Fail` / `strict`, multi-line/unclearable text, or shell returned to foreground after paste (controller sends Ctrl-C to clear shell line). |
| `ForegroundUnproven` (`"foreground_unproven"`) | Another program owns the terminal, or foreground confidence is rejected by `ForegroundPolicy` (e.g. Windows heuristic under `ExactOnly`). |
| `TrustPromptBlocking` (`"trust_prompt_blocking"`) | A startup/trust dialog was on screen where the message or verification would go. |
| `DialogBlocking` (`"dialog_blocking"`) | The profile recognized an active question before the paste or a subsequent Enter. Inspect the session before sending again. |
| `ResetRejected` (`"reset_rejected"`) | After an explicit reset's Enter, the profile recognized a fresh refusal from the agent. A queued follow-up in the same job is withheld. |
| `Revalidate` (`"revalidate"`) | `Revalidate::Sync` returned `false`/panicked, or `Revalidate::Handshake` received `false`/timed out. |
| `SubmitUnverified` (`"submit_unverified"`) | Delivery could not verify the pasted text or its submission, obtain a current dialog check, or distinguish fresh reset evidence. Enter may already have reached the agent. **Do not auto-resend.** |
| `Disposed` (`"disposed"`) | `AgentTerminal` was dropped before completion. |
| `WriteFailed` (`"write_failed"`) | Underlying PTY write failed. |
| `Unsupported` (`"unsupported"`) | `reset()` / `reset_and_send()` called without a profile or on a profile with `reset_command() == None`. |
| `EmptyText` (`"empty_text"`) | Nothing remained after stripping control characters (`\t` and `\n` kept; `\r` becomes `\n`). |
| `InputTooLarge` (`"input_too_large"`) | Raw text exceeded `AgentLimits::max_message_bytes` (refused immediately, **no `Event::Delivery`**). |
| `QueueFull` (`"queue_full"`) | `AgentLimits` byte or job budget exhausted (refused immediately, **no `Event::Delivery`**). |

> **Never auto-resend `SubmitUnverified`:** The agent may have already accepted the message and begun executing before redrawing its composer (or the profile may simply lack composer-read support, as with `opencode` and `cursor`). By default `Timing::submit_retry_max` is `0` (Enter is sent at most once). Raising `submit_retry_max` enables extra Enter attempts when text still appears in the composer, which can double-submit if the CLI redraws late.

---

## Conversation Resets (`reset` & `reset_and_send`)

- `reset()` submits the profile's reset command (`/clear` for Claude and Codex, `/new` for Pi, OpenCode and Cursor). Its default `ShellFallback::PasteInert` can leave a single-line command on the shell input line without Enter and return `PastedUnsubmitted`.
- `reset_and_send(text, opts)` reserves one queue slot and `text.len() + cmd.len()` bytes for one ticket. Both steps use `ShellFallback::Fail` and check for blocking questions. The reset uses `strict: false` and `Revalidate::None`. Only a delivered reset permits the follow-up, which uses `submit: true` and the caller's `strict` and `revalidate` settings.

During submit verification, both reset methods compare the screen with the rows
visible just before Enter. If the profile recognizes a fresh refusal, the ticket and
delivery event report `Aborted(ResetRejected)`, even if the composer cleared.
After verifying submission, both methods also check the response as fresh output
settles (`startup_quiet`, capped at `startup_max`). The compound job checks again
before pasting the follow-up, including after a host handshake.

The built-in Codex profile recognizes the busy-turn `/clear` diagnostic recorded
with Codex CLI 0.169.0. See the [120-column capture](../tests/fixtures/codex-0.169.0-clear-refused.txt)
and the [40-column capture](../tests/fixtures/codex-0.169.0-clear-refused-wrapped.txt).

If a redraw makes a refusal indistinguishable from one already on screen, or a
current observation remains unavailable, the ticket reports `SubmitUnverified`.
Either abort withholds the follow-up. Enter may already have reached the agent, so
inspect the session before retrying. A refusal can scroll away between observations.
`Delivered` confirms observed submission. Verifying that a new conversation exists
requires separate evidence.

Both reset methods use at most one Enter for the reset command and allow at least
`startup_max` for submit verification. An ordinary `send` that exactly matches the
configured reset command uses the same Enter limit and verification window. It still
reports ordinary message outcomes.

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

### PTY model and reasoning effort

| Built-in profile | Model argv | Optional effort argv | Accepted effort levels |
| --- | --- | --- | --- |
| Claude | `--model <id>` | `--effort <level>` | `low`, `medium`, `high`, `xhigh`, `max` |
| Codex | `-m <id>` | `-c model_reasoning_effort=<level>` | `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`, `persistent` |
| Pi | `--model <id>` | `--thinking <level>` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| OpenCode and Cursor | Explicit choices return `Error::Unsupported`. | — | — |

Claude Code 2.1.327 and Pi 1.5.2 list these flags and levels in `--help`.
Pi treats `--model=…` and `--thinking=…` as extension flags. Either can appear
in custom launch arguments alongside an explicit model and effort.
[Claude Code 2.1.327 package](https://registry.npmjs.org/@anthropic-ai/claude-code-darwin-arm64/2.1.327) ·
[Pi 1.5.2 CLI parser](https://github.com/earendil-works/pi/blob/b4afdddf44e059c67ccf5097e5317d913521b470/packages/coding-agent/src/cli/args.ts)

Codex CLI 0.170.0 defines the model switch and config override syntax in its argument
parser. The profile maps the named reasoning efforts from that version to fixed
strings for the `model_reasoning_effort` config key.
[Model flag](https://github.com/openai/codex/blob/29eb79e9866b3262a777fdf8195c1b9e63684c91/codex-rs/utils/cli/src/shared_options.rs#L22-L24) ·
[Config overrides](https://github.com/openai/codex/blob/29eb79e9866b3262a777fdf8195c1b9e63684c91/codex-rs/utils/cli/src/config_override.rs) ·
[Config schema](https://github.com/openai/codex/blob/29eb79e9866b3262a777fdf8195c1b9e63684c91/codex-rs/core/config.schema.json#L8809) ·
[Effort labels](https://github.com/openai/codex/blob/29eb79e9866b3262a777fdf8195c1b9e63684c91/codex-rs/protocol/src/openai_models.rs#L91-L103)

Synthetic launch tests check argv construction and quoting for POSIX shells, fish,
and `cmd.exe`. The CLI resolves the model and its available effort levels for the
account when launched. [Profile launch tests](../tests/profiles_launch.rs)

---

## Writing a Custom `AgentProfile`

Implement `agent::AgentProfile` (`Send + Sync`) to support another CLI or customize screen detection:

- **Required methods:**
  - `id(&self) -> &str`
  - `dialog(&self, text: &str) -> Option<Dialog>` — report startup prompts here. The startup gate can steer them using `key`. A `None` key leaves the prompt to the user. The method accepts screen text or raw rolling output, which you can normalize with `screen::normalize(text)`.
  - `input_ready(&self, screen: &str) -> bool` — recognize the ready composer; return `false` for dialogs or unreadable screens.
- **Important defaulted methods:**
  - `question_dialog(&self, screen: &str) -> bool` — recognize a question currently taking keyboard input. The default returns `false`. Claude, Codex, OpenCode and Cursor override it for the measured layouts above. Use `detect::dialog_at_bottom` with markers from a recorded version of the CLI. It skips blank rows and recognized rules or box edges below the footer. A redrawn empty composer such as `❯` closes the block.
  - `launchers(&self) -> &'static [&'static str]`
  - `launch(&self, perms, trust, cwd, shell) -> LaunchPlan`
  - `model_args(&self, choice: &ModelChoice, base_args: &[String]) -> Result<Vec<String>>` — return unquoted words for the entire selection. Check the base arguments for duplicate settings of the requested fields in the CLI's option syntax. Return `InvalidInput` for a conflict and `Unsupported` for an unmapped choice or effort. The default returns `Unsupported`.
  - `pending_input(&self, screen, sent) -> Option<bool>` — defaults to `detect::pending_input_default(screen, sent)` (`Some(true)` = still in composer, `Some(false)` = submitted/cleared, `None` = unreadable).
  - `reset_command(&self) -> Option<&'static str>`
  - `reset_rejected(&self, fresh_screen: &str) -> bool` — defaults to `false`. Match a complete refusal in its recorded CLI layout. The controller also runs the matcher against the pre-Enter rows to distinguish an old notice exposed by a redraw.
  - `signals(&self) -> SignalHints`
  - `login_check(&self) -> Option<LoginCheck>`
  - `rpc_launch_with_options(&self, options: &RpcLaunchOptions) -> Result<RpcLaunch>`
  - `compatibility(&self) -> Compatibility`

Use helpers in `agent::detect` (`has_all`, `has_any`, `from_last`, `selected_option`, `steer_two`, `pending_input_where`, `composer_holds`) and keep callbacks fast — they run outside the controller lock, and stale results (when screen/state revisions changed while running) are discarded automatically.

Runnable example: [`examples/custom_profile.rs`](../examples/custom_profile.rs) (`cargo run -p agent-link --example custom_profile`).
