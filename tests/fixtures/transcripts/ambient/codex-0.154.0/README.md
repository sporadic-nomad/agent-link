# Codex 0.154.0 ambient capture

`turns.jsonl` is one interactive rollout of the Codex 0.154.0 TUI. Its `session_meta` records `"cli_version":"0.154.0"`, `"source":"cli"` and `"originator":"codex-tui"`. Its base instructions were shortened. See the [shared provenance](../README.md#provenance).

1. "Reply with exactly: first answer". The turn ends with `event_msg` `task_complete` and its `last_agent_message`.
2. "Write a long story about a lighthouse keeper, at least 1500 words." Esc interrupted the turn during reasoning. Codex wrote `event_msg` `turn_aborted`.

The `response_item`, `item_completed` and `token_count` records are kept for the parser to step over. Unit tests in `src/agent/profiles/ambient/codex/tests.rs` and the fake-agent PTY test `tests/ambient_sources.rs` replay this file.

## Questions

Two more rollouts of the Codex 0.154.0 TUI in Plan mode, captured on October 12, 2026, each one turn: `session_meta`, `task_started`, the prompt asking for the `request_user_input` tool, the `function_call`, its `function_call_output`, the final message and `task_complete`. Their `session_meta` records `"cli_version":"0.154.0"`. The files are the predecessor crate's question fixtures as it kept them (base instructions shortened); besides the shared replacements, nothing was changed here.

- `question-plan.jsonl`: one question (Red or Green).
- `question-multi.jsonl`: one call with two questions (a color and a shape).

Unit tests in `src/agent/profiles/ambient/codex/tests.rs` replay them through the parser's question hook.
