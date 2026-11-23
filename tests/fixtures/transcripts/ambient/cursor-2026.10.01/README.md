# Cursor Agent 2026.10.01 ambient capture

Cursor Agent 2026.10.01 rewrites a conversation's transcript in place at every turn's first write. Earlier `turn_ended` records are dropped and the messages are kept. These files are copies of one conversation's file, taken after each turn. See the [shared provenance](../README.md#provenance).

| File | After |
| --- | --- |
| `turns.jsonl` | "Reply with exactly: first answer" |
| `turns.rewritten.jsonl` | "Reply with exactly: second answer"; the first turn's `turn_ended` is gone |
| `turns.aborted.jsonl` | A third turn interrupted with Esc; Cursor kept none of its messages and wrote `"status":"aborted"` over the previous end |
| `turns.tool.jsonl` | "Run `ls` in the shell, then reply with exactly: after tool"; the prompt was trimmed because Cursor had put the aborted prompt back in front of it |

The account's model wrote no reasoning in these turns. In the controlled tier's [packaged writer images](../../cursor-2026.11.13-eacade1/README.md), the text block joins the answer and the reasoning. Unit tests in `src/agent/profiles/ambient/cursor/tests.rs` and the fake-agent PTY test `tests/ambient_sources.rs` replay these files. The PTY test rewrites one file through these images in order.
