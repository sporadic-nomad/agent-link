# Interactive question evidence

These captures were made on macOS arm64 on 2026-11-19. The Claude Code 2.1.296 and Codex 0.154.0 CLIs ran inside fresh temporary homes with network access limited to an isolated loopback model fixture. The model returned one native question tool call after the prompt “Survey fixture”. The provider generated the actual menus, editor, review screen and result records.

The four files sharing a capture stem retain PTY bytes, geometry and version metadata, input/output timing and the terminal’s final screen with trailing cell padding removed. Claude’s multiple-item capture chose “Amber” and “Square”, then submitted the review. Codex’s note capture selected “None of the above”, entered “Green” and submitted. The JSONL projections retain the original tool call, call identifier and result. They omit installation, launch and session metadata, so replay through an attribution test must supply that test’s verified recording identity.

`screens.json` contains the question rows and all following visible nonblank rows from individual captures. Frames cover Claude’s first and second pages, both checkbox states, the review and text editor, Codex’s ordinary, last-page and note states, and Cursor’s complete and wrapped command previews. The default delivery-guard fixture families retain their own raw captures alongside this directory.

For the Cursor 2026.11.13-eacade1 samples, the original packaged decision renderer and Ink input handler ran inside a real PTY with a local example command and inert action callbacks. `cursor-key-intents.jsonl` records their outcomes for `y` and `n`. The latter started the alternative-instruction flow. This exercise used no account, executed no command and made no live service call.

Replay the projected screens and record pairs with `cargo test --locked --no-default-features --features questions --lib agent::questions:: -- --test-threads=1`. The real foreground, byte ordering and teardown regressions run with `cargo test --locked --no-default-features --features questions,test-support --test pty_questions -- --test-threads=1`.
