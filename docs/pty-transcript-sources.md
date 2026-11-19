# Isolated PTY transcript sources

On macOS arm64 the optional `turns` feature can observe fresh turns from a direct interactive Codex, Pi or Cursor process. A `TranscriptReceiver` belongs to one terminal. Delivery tickets and the PTY event receiver keep their usual meanings.

| `TranscriptSource` | Returned observation | Evidence |
| --- | --- | --- |
| `Codex` | `CodexFinal(TurnReply)` from `task_complete.last_agent_message` after a matching `task_started` turn ID. Aborts and null finals produce no reply. | [Codex 0.154.0 TUI and rollout](../tests/fixtures/transcripts/codex-0.154.0/README.md) |
| `Pi` | `PiInferred(TurnReply)` from an assistant stop or length limit with text, followed by three seconds of quiet at a complete record, a new user prompt or a shell-execution entry. Intervening tools, errors and discontinuous parent IDs discard the candidate. | [Pi 1.5.2 PTY and session v3](../tests/fixtures/transcripts/pi-1.5.2/README.md) |
| `Cursor` | `CursorCompleted` for an explicit successful end, or `CursorNextPrompt` when another user prompt follows an answer whose end marker was absent. Neither outcome carries text. | [Cursor 2026.11.13-eacade1 packaged writer](../tests/fixtures/transcripts/cursor-2026.11.13-eacade1/README.md) |
| `CursorWithUnseparatedText` | `CursorUnseparatedText(TurnReply)` or `CursorUnseparatedTextAtNextPrompt(TurnReply)` with the assistant output after the last tool call. This output can include model reasoning mixed with the displayed answer. | [Recorded rewrite images](../tests/fixtures/transcripts/cursor-2026.11.13-eacade1/) |

## Admission and polling

1. Dedicate a local provider namespace to one new terminal. The host must control all writers, prevent independent processes from opening or importing sessions there, and exclude remote/shared backends and resume/fork workflows. The CLIs receive a canonical directory name and may resolve it again while running. Keep that name and its ancestors under host control and prevent renames or replacements for the full lease. Admission and observation compare the name to the pinned directory. Keep configuration and model access appropriate for this isolated run. Starting with an empty directory alone cannot exclude another same-account writer.
2. Create an `agent::turns::IsolatedTranscriptRoot` for that existing directory. Keep the lease alive. Each PTY needs its own directory even when several use the same working directory.
3. Call `AgentTerminal::spawn_with_transcript_turns(spec, agent, source, &root)` with the matching built-in profile and a direct `SpawnSpec`. Configure permissions explicitly in the direct argv. The lease supplies the provider root when the launch has no explicit setting. An explicit environment-policy or terminal-identity setting must name the leased directory. A removal, a conflicting root, pre-existing primary transcripts, and a competing controlled launch fail admission.
4. Call `term.attach_transcript_turns(TurnOptions::default())`. Poll `status()` until `TurnStatus::Active` to establish a verified EOF. A first prompt that creates the transcript can be part of this skipped history. Poll `try_recv()` for typed observations. A nonempty pull checks the foreground process, session path, file identity and continuity again. An empty pull returns immediately.
5. Revoke or drop the lease before relinquishing isolation. This withholds unread observations while leaving the terminal running. `TurnStatus::Gap(reason)` is sticky. After an attachment ends, a new attachment takes a fresh baseline once its predecessor's worker retires. A process that switched session ID requires a new controlled launch.

| Provider | Direct program and arguments | Leased root |
| --- | --- | --- |
| Codex | Absolute `codex` binary. Supported optional flags include `--no-alt-screen`, `--dangerously-bypass-approvals-and-sandbox`, and `--model MODEL`. The rollout must identify the 0.154.0 TUI and an interactive user thread. | `CODEX_HOME` |
| Pi | Absolute `node` followed by the absolute packaged `pi-coding-agent/dist/bundle/cli.js`. Supports `--session-id UUID`, `--model MODEL`, `--approve` and the measured `--no-tools`, `--no-extensions`, `--no-skills`, `--no-prompt-templates`, `--no-themes` switches. | `PI_CODING_AGENT_SESSION_DIR` |
| Cursor | The installation's absolute packaged `node` and sibling absolute `index.js`, optionally with `--use-system-ca` before the script. Optional CLI arguments include `--model MODEL`, `--trust` and `--force`. | `CURSOR_DATA_DIR` |

Each source pins the foreground process instance and file descriptor. It accepts a single primary file in the leased namespace. The Codex and Pi headers must also match the working directory and the UUID in the file name. Pi and Cursor specify the measured file format, which carries no CLI version. Hosts pin their installed version for equivalent behavior. Detection samples can miss a switch away and back between samples. Cursor's packaged writer was exercised independently of an authenticated end-to-end launch.

## Limits and continuity

Options share the existing turn queue limits, UTF-8 truncation and worker budget. At most four isolated terminal reservations can coexist in a process, including reservations retained by retiring workers. A directory walk examines at most 256 entries per resolution. The initial header is capped at 64 KiB. The Codex and Pi readers follow appends and report observed truncation, replacement or ambiguous bindings as gaps.

Cursor rewrites its primary file in place. The reader fingerprints each message row by position, allows a prior end marker to disappear, and admits only new suffix rows. Equal replies at two different positions remain two turns. Changes to historical message bytes or contradictory end markers produce a gap. A shorter prefix or unfinished write is held for up to three seconds, including when pulling queued output. Replacing the file inode ends the attachment. An indistinguishable reconstruction with identical message rows supplies no evidence of an additional turn.

For Cursor, `TurnOptions.records` bounds all rows in one image. Its maximum byte length is the smaller of `line_bytes` and 16 times `read_bytes` (256 rows and 1 MiB by default). Larger images report `Oversized`. Rechecking a queued Cursor observation can synchronously read and fingerprint one bounded image. Hosts can lower these limits. [Claude-specific setup and shared reader limits](agents-and-delivery.md#claude-pty-final-replies)
