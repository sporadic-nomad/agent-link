# Ambient transcript captures

Captured transcripts that the best-effort ambient sources (`agent::profiles::ambient`) replay. One directory per CLI and recorded version. Each README records the scenario and what was changed. The ambient sources pin no CLI version. These captures show only that the recorded versions' records parse as documented. [Ambient tier](../../../../docs/pty-ambient-turns.md)

| Directory | CLI version | Scenario |
| --- | --- | --- |
| [claude-2.1.296](claude-2.1.296/README.md) | Claude Code 2.1.296 | Three turns: a plain answer, an answer after a tool call, and thinking plus a tool call; three `AskUserQuestion` sessions |
| [codex-0.154.0](codex-0.154.0/README.md) | Codex 0.154.0 | A plain answer and a turn interrupted with Esc; two Plan-mode `request_user_input` rollouts |
| [pi-0.85.1](pi-0.85.1/README.md) | pi 0.85.1 | A plain answer, an answer after a tool call, an interrupted essay, and a failed request |
| [cursor-2026.10.01](cursor-2026.10.01/README.md) | Cursor Agent 2026.10.01 | The conversation file after each of four turns, rewritten in place |

The same sources also replay the controlled tier's captures, which have their own provenance: [Claude 2.1.296](../claude-2.1.296/README.md), [Codex 0.154.0](../codex-0.154.0/README.md), [Pi 1.5.2](../pi-1.5.2/README.md) and [Cursor 2026.11.13-eacade1](../cursor-2026.11.13-eacade1/README.md).

## Provenance

The interactive TUI of each CLI ran in a scratch project, signed in to the CLI's own account. Each capture was taken on October 11, 2026 (the question captures on October 12), by a predecessor implementation of this observer, from the CLI's own transcript file. Only the transcript was kept, without a paired terminal recording. The capture platform was not recorded.

The captures keep the lines the parsers read and a few they must step over. Other lines were removed. Before publication the following values were replaced:

- the project path, with `/fixture/project`;
- request and response identifiers, with `req_fixture_N` and `resp_fixture_N`;
- the time zone in Cursor's prompt wrapper, with `UTC`.

Encrypted reasoning and signatures had already been shortened to placeholders. Session, turn, message and tool-call IDs, model names, timestamps, prompts and replies come from the recordings.
