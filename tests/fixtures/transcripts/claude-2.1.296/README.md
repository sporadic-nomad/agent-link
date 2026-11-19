# Claude Code 2.1.296 interactive PTY transcripts

Two native macOS arm64 processes ran the published `@anthropic-ai/claude-code-darwin-arm64@2.1.296` binary in separate 120 × 40 PTYs. Both used the same empty project, home and `CLAUDE_CONFIG_DIR`. A loopback Messages API peer returned the distinctive text shown in the screen excerpts. The CLI wrote the writer records and transcripts and rendered the replies. The environment contained a dummy API key, tools were disabled, and the macOS sandbox restricted outbound connections to the local peer.

The capture manifest pairs each direct launch command with its child PID, selected fields from `sessions/<pid>.json`, and visible reply lines from the terminal. It also records the largest input lines before projection. Executable, project and configuration paths, PIDs, session UUIDs and message IDs have consistent substitutions. Prompt text, reply text, block indices, stop reasons, record order, duration values and the relationship between IDs come from the recording. The JSONL files retain the mode, user, assistant and system rows. The projection omits attachments, history snapshots, tool configurations, socket details and unrelated metadata. [Capture manifest](capture.json)

| Recording | Observed behavior |
| --- | --- |
| [Terminal A](session-a.jsonl) | With the duration display enabled, both the 73 ms reply and the 66,033 ms reply ended with `turn_duration`. The long reply had two text fragments sharing a message ID and consecutive block indices. Esc interrupted a third prompt mid-stream. The partial assistant text was visible, followed by an interruption notice. The CLI wrote `isAbortedMidStream` and `interruptedMessageId` and omitted the duration record for that prompt. |
| [Terminal B](session-b.jsonl) | With the display disabled, the 57 ms and 66,028 ms replies both had duration records. Both terminals were running during the long turns. |
| [Terminal A after /clear](after-clear.jsonl) | The PID and writer's `startedAt` stayed the same. Its writer record and new transcript used a fresh `sessionId`. The assistant's separate `session_id` still held the launch ID. Local command history preceded the next typed prompt. |
| [A second CLI resumed a live session](overlap.jsonl) | In a separate run, a new direct `--resume <id>` process and the original process both produced complete replies in the same file. Both current-writer records named the same project and session. The original terminal's final screen omitted the other terminal's reply. The prompt, assistant and duration records provide no reliable per-writer discriminator in this case. |

The largest sampled writer record was 535 bytes, the largest raw JSONL line was 27,702 bytes, and the largest retained record before projection was 1,218 bytes. The longest joined final reply was 25 UTF-8 bytes.

## Scope of the public opt-in

The `turns` feature supports fresh direct launches of the recorded macOS arm64 CLI in a host-controlled configuration directory. The host supplies a fresh `--session-id` and an `ExclusiveClaudeRoot` when calling `AgentTerminal::spawn_with_claude_turns`. The reader pins the direct child's kernel birth identity, then checks its executable, foreground ownership, writer record and transcript. It samples the writer again before committing a batch and when pulling a queued reply. A fresh attachment starts at verified EOF and needs a new typed prompt. File creation after attachment can put the first prompt before that boundary. [Host guide](../../../../docs/agents-and-delivery.md)

The overlapping-resume recording establishes the limit of a process-local reservation. Another same-account CLI can resume the live UUID and write records indistinguishable from the original's. The host must control every writer that can reach the directory and keep one writer per conversation while the lease is active. Before letting an ordinary CLI write there or resume a live UUID, the host revokes or drops the lease. This invalidates its readers and withholds pending replies. Ordinary launches have no transcript attachment. A writer can also switch away and back between samples without leaving a distinguishable generation. Shell launches and other platforms lack the native binding evidence recorded here.

The offline `agent::profiles::turns::claude` tests replay these projections and use scripted PTYs for interleaving, late appends to a switched file, file replacement, limits and loss. The scripted cases include additional malformed, tool, error and repeated-text records.

## Reproduce the native acceptance check

On an Apple silicon Mac, run the following command from the repository root with Python 3 and the repository's Rust toolchain installed.

```sh
python3 scripts/check_claude_transcripts.py
```

The runner builds the ignored Rust test, fetches the official pinned macOS arm64 CLI into a temporary directory and checks its executable SHA-256 against the capture manifest. To use an existing copy of that binary, run `python3 scripts/check_claude_transcripts.py --binary /path/to/claude`. The test and its native CLI children run under `sandbox-exec` with fresh home, project and configuration directories. Both CLIs use a dummy API key, and the sandbox permits outbound connections only to the test's loopback Messages API peer. The controlled scenario assumes unrelated same-account processes leave the scratch directory alone.

The public API check on macOS 26.3.1 (a), arm64, on November 19, 2026 exercised both scenarios.

- Two controlled direct launches shared a cwd and configuration directory. Each attachment skipped a completed warm-up. The peer held one model response until the other terminal rendered its reply and its public receiver returned that text. The second pair reversed the order. The readers returned exactly two distinctive replies apiece, matching their respective screens. A duplicate launch ID and a resume through the controlled launcher were rejected. After `/clear`, A reported a session-change gap. Its fresh attachment skipped the new conversation's warm-up and returned only the following reply. B stayed active and committed another reply. Revoking the shared lease withheld that unread reply and produced `AuthorityRevoked` on both live readers. The PTY still accepted the following prompt and rendered its answer.
- Two ordinary direct launches selected the same UUID with `--session-id` and `--resume`. Both live PID records named that UUID. Each terminal rendered its own new reply and the shared JSONL contained both completed turns. Both terminals reported the reader unavailable.

The ignored integration test uses the public launch, attachment, status and revocation APIs. Regular Cargo tests replay the committed capture without fetching or starting Claude. [Native integration test](../../../claude_transcript_live.rs) · [Loopback runner](../../../../scripts/check_claude_transcripts.py)
