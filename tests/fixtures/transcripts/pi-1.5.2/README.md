# Pi 1.5.2 interactive session

On November 19, 2026, the native macOS arm64 Node process ran Pi 1.5.2 in an isolated PTY with a fresh session directory, dummy provider credentials, and a loopback OpenAI-compatible completions peer. The packaged `pi-coding-agent/dist/bundle/cli.js` SHA-256 was `e79626f2dd6f94aa45d30f3fa63cd84319a6eefcd150b353cfaf274366926774`. The launch selected a controlled model and session UUID and disabled tools, extensions, skills, prompt templates and themes.

Two successive prompts produced the displayed strings `pi-proposal-reply-1` and `pi-proposal-reply-2`. The corresponding assistant records contained those strings and `stopReason: "stop"`. The file contained no explicit turn-complete record between them.

`turns.jsonl` retains the observed entry types, IDs, parents, user/assistant text and stop reasons. The cwd is `/fixture/project`. The system body, account, endpoint, timing, model and usage metadata are omitted. The quiet interval and handling of the next prompt are library inferences. The public regression exercises both outcomes. [Public regression](../../../isolated_turns.rs)
