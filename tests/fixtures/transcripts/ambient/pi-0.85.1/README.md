# pi 0.85.1 ambient capture

pi 0.85.1 writes session format version 3 and no end-of-turn record. See the [shared provenance](../README.md#provenance).

`turns.jsonl` is one interactive session:

1. "Reply with exactly: first answer". The turn has an assistant message with `stopReason: "stop"`.
2. "Run `ls`, then reply with exactly: after tool". The turn has a `toolUse` message, its `toolResult` and a final `stop` message.
3. "Write a 3000-word essay about the history of the sea, with headings." Esc interrupted it, and pi wrote `stopReason: "aborted"`. The partial text and the reasoning signature were shortened.

`error.jsonl` is one turn against a dummy API key. The provider answered 401, and pi wrote `stopReason: "error"` with no content.

The timestamps are the recorded ones. The quiet-window tests use them to place the transcript's pauses. Unit tests in `src/agent/profiles/ambient/pi/tests.rs` and the fake-agent PTY test `tests/ambient_sources.rs` replay these files.
