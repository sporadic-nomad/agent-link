# Claude Code 2.1.296 ambient capture

`turns.jsonl` is one interactive session of Claude Code 2.1.296. Its records carry `"version":"2.1.296"` and `"entrypoint":"cli"`. See the [shared provenance](../README.md#provenance).

1. "Reply with exactly: first answer". The turn has a text-only `end_turn` message and a `system` `turn_duration` record. It also has a `system` `stop_hook_summary` record, taken from an earlier capture of the same version.
2. "Run `ls`, then reply with exactly: after tool". The turn has a `tool_use` message, its `tool_result`, a text-only `end_turn` message and `turn_duration`.
3. A turn with thinking, a tool call and its result. Its final text-only message was removed from the capture, so the turn ends with no reply.

`attachment` records with model and token reminders are kept for the parser to step over. The `system`/`turn_duration` record is the explicit end. Unit tests in `src/agent/profiles/ambient/claude/tests.rs` replay this file and variations that each change one record.

## Questions

Three more sessions of Claude Code 2.1.296, captured on October 12, 2026 in `bypassPermissions` mode with the Sonnet model, each one turn: a prompt asking for the `AskUserQuestion` tool, the assistant's `tool_use` call, the `tool_result` with the user's answers, a text-only `end_turn` message and `turn_duration`. Their records carry `"version":"2.1.296"`. The predecessor crate observed that this version writes the call while its dialog is open only in such sessions; otherwise the call arrives with its result.

- `question-single.jsonl`: one single-choice question (Red or Green).
- `question-multiselect.jsonl`: one multiple-choice question (Red, Green, Blue).
- `question-multi.jsonl`: one call with two questions, a multiple-choice one and a single-choice one answered with typed text.

The files are the predecessor crate's question fixtures as it kept them: a `permission-mode` record and the turn's records. Besides the shared replacements, nothing was changed here. Unit tests in `src/agent/profiles/ambient/claude/tests.rs` replay them through the parser's question hook.
