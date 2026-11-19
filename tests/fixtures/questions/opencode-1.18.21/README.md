# OpenCode 1.18.21 question exchange

On November 19, 2026, the native macOS arm64 `opencode-darwin-arm64@1.18.21` executable ran a local server and an attached 120 × 40 TUI in an isolated project and home. The binary SHA-256 was `8c783005340f8dfc5e7d168478dd0dd2bd1faead531cb34270de2a9689d9f135`. A loopback model peer requested a sample color from the question tool. The run used dummy credentials and restricted outbound connections to loopback.

`GET /question?directory=…` returned one request for the selected session. The fixture preserves the request shape, prompt, choices, order and flags. Session, request, message and call IDs have consistent sample values. OpenCode omitted `custom`, whose default permits a text answer. The attached TUI showed Blue, Amber and “Type your own answer”. [Recorded request](list.json) · [Terminal recording](../../opencode-1.18.21-question.txt)

The Rust question feed fetched the list, fetched it again before dispatch, and posted `{"answers":[["Blue"]]}` to the request's `/reply` route. The server returned JSON `true`, the subsequent list was empty, and the local model peer received the tool result. The scoped host contract requires one verified server connection and a known PTY, workspace and session. [Instance question API](https://github.com/anomalyco/opencode/blob/v1.18.21/packages/opencode/src/server/routes/instance/httpapi/groups/question.ts)

With the pinned executable available on macOS, reproduce the public PTY acceptance from the repository root.

```sh
python3 scripts/check_opencode_questions.py --binary /path/to/opencode
```
