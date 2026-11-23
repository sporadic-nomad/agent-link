# Best-effort ambient PTY turns

The optional `turns` feature has two tiers. The controlled tier ([Claude](agents-and-delivery.md#claude-pty-final-replies), [Codex, Pi and Cursor](pty-transcript-sources.md)) verifies who wrote a transcript and requires a direct launch on macOS arm64, a host-enforced exclusive configuration root and a pinned CLI version. The **ambient** tier in `agent::turns::ambient` reads an agent CLI's own JSON-lines transcript for any `AgentTerminal`: shell or direct spawn, the user's own configuration directory and sign-in, whatever CLI version is installed, on Unix and Windows.

> **No writer exclusion, no attribution guarantee.** Another process of the same user — a second session of the CLI, a resumed or forked conversation, an editor extension, any program — can write a transcript the source locates, or append to it, and its records are reported like the agent's own. An `AmbientTurn` is "what the CLI's own transcript says ended a turn". Use it for convenience relaying, such as a desktop host that relays agent replies to chat channels. Never use it for authorisation decisions, billing or as proof that this terminal's agent produced the text.

| | Controlled (`TurnReceiver`, `TranscriptReceiver`) | Ambient (`AmbientReceiver`) |
| --- | --- | --- |
| Launch | Direct only, admitted argv | Any `AgentTerminal` |
| Platforms | macOS arm64 | Unix and Windows (plain file reads) |
| Configuration root | Host-enforced exclusive lease | The user's own |
| CLI version | Pinned and recorded | Whatever is installed |
| Writer attribution | Verified at every nonempty pull | None |
| Workers | Four per process | `ambient_worker_limit()`, default 64 |
| Sources | Built-in Claude, Codex, Pi, Cursor | Built-in Claude, Codex, Pi, Cursor ([below](#built-in-sources)), or a host `AmbientSource`; a profile's `ambient_feed()` (built-in: [OpenCode](#opencode)) or a host `AmbientFeed` |

## Attaching

```rust
use agent_link::agent::turns::ambient::{AmbientOptions, AmbientRecvError};

// The terminal's profile's source (Claude, Codex, Pi, Cursor), or a host's own with
// `term.attach_ambient_turns_with(source, options)`.
let replies = term.attach_ambient_turns(AmbientOptions::default())?;
loop {
    // Blocks until a turn, the close or the timeout: no polling timer.
    match replies.recv_timeout(Duration::from_secs(60)) {
        // Option<TurnReply>: bounded, Debug-redacted.
        Ok(turn) => relay(turn.evidence, turn.reply),
        // Still waiting or active.
        Err(AmbientRecvError::Timeout) => {}
        // Closed (`AmbientRecvError::Closed(reason)`) and drained: reattach or stop.
        Err(_closed) => break,
    }
}
```

`attach_ambient_turns` uses the profile's `AgentProfile::ambient_turns` source, else its `AgentProfile::ambient_feed` ([feeds](#feeds)), and returns `AttachError::NoAmbientSource` when it has neither; `attach_ambient_turns_with` takes a host-supplied `Arc<dyn AmbientSource>`, `attach_ambient_feed_with` a host-supplied `Arc<dyn AmbientFeed>`. Both validate `AmbientOptions` (`InvalidLimits`), refuse attachment once the PTY records child exit, the session ends, the terminal is dropped or its monitor fails (`NoLaunch`), and start one worker thread named `agent-link-ambient-<id>` (`Busy` beyond the process-wide budget, `Spawn` if the thread cannot start). They perform no file I/O on the caller. Several attachments to one terminal are independent.

The receiver follows upstream's pull model: `status()` and `try_recv()` never perform I/O or block, and nothing goes through the PTY event stream or the controller's monitor queue. `stop()` and drop discard queued turns and wake the worker and every waiter without waiting for the worker.

### Waiting instead of polling

`AmbientReceiver` has the same waits as the controlled receivers ([Waiting on a turn receiver](agents-and-delivery.md#waiting-on-a-turn-receiver)), so a host with many terminals needs no timer loop per receiver:

- `recv_timeout(timeout)` returns `Ok` with the oldest queued turn, `Err(AmbientRecvError::Closed(reason))` once observation closed and no turn is left, or `Err(AmbientRecvError::Timeout)` while it is still waiting or active. `Waiting(Unavailable(_))` is not final: the worker retries, and the wait goes on. After a close other than `stop()` (an `Overflow`, the terminal's end), the turns queued before it come first. A zero timeout makes one attempt.
- `wait_status_change(seen, timeout)` returns the current status once it differs from `seen` (another waiting reason or other `Active` fields count) or is `Closed`, and `None` on timeout.
- With `tokio`, `recv().await` and `status_change(seen).await` are the async equivalents without a deadline; wrap them in a timeout of your own. They occupy no thread, spawn nothing, hold no lock across an await, and are cancel-safe: a dropped future has removed nothing.

The controlled tier's `TurnRecvError` is not reused: ambient observation has no gap, a close carries an `AmbientClosed` reason, and queued turns survive it.

A turn's publication, every status the worker publishes that differs from the last one (including a change of waiting reason or of a feed's state), any close (the overflow included), `stop()` or drop from any thread, and terminal teardown wake every waiting thread and task at once. Teardown ends the worker's wait at once; it drains the transcript or feed, then closes. Everything the worker learns by sampling is noticed at its next wake (`AmbientOptions::poll`, default 500 ms) and wakes waiters then: launches, relaunches and departures, child exit, monitor loss, a new transcript path, newly written lines, an ended quiet window, a feed's events and state. While the worker is inside a slow file read, source lookup or feed poll (up to two seconds), these wait for that call to return; `stop()` does not. `status()` samples nothing itself, so a wait that reaches its timeout reports what the worker last published. Any number of threads and tasks may wait on one receiver; each turn reaches one of them.

## Lifecycle and "from now"

An attachment follows the terminal, not one agent process: attach once per terminal.

- While no agent launched by the terminal runs — before `launch()`, or after the agent returned to the shell — status is `Waiting(NoAgent)` and nothing is read. An agent the user starts by hand in the shell is not one the terminal launched. A direct spawn's child is the agent, with or without a profile.
- Every launch and relaunch (the controller's lifecycle revision) and every observed departure starts a new generation. The transcript followed so far is read a last time, and a candidate final record there ends its turn with `InferredAtTranscriptEnd`. Positions are then dropped. The old reader leaves files created at or after relaunch to the new reader. Without a file creation time, the old reader drains only a file it previously identified. Files created at or after the new launch normally start at byte zero. Older files start at their size when first located or when their directory was first listed. If the worker runs out of space to remember positions, the [offset limit](#limits-and-threads) governs where reading resumes. Resuming a suspended launch establishes fresh starting positions too, so already-read replies and content written while observation was paused are not replayed.
- A suspended launch (for example, Ctrl-Z followed by `fg`) resumes observation when the same identified process group positively regains the foreground. Unknown ownership does not resume it; no controller lifecycle change is required.
- A departure is noticed when the foreground, as the terminal's `ForegroundPolicy` trusts it, shows the shell or another program instead of the launch's identified program. If that program was never identified (Windows under the default `ExactOnly` policy, or after the host submitted a line to the shell), it is noticed only at the controller's next launch or delivery, and reading continues meanwhile.
- The worker calls `AmbientSource::locate` on every wake (`AmbientOptions::poll`, default 500 ms) with the current `program` and `launched_at`. Direct launches capture this time immediately before process creation. A different path is followed from then on; the reader drains the old path in bounded passes through the byte length observed at departure. Later appends remain eligible if the path is followed again. At the captured length, a drain requires the same file identity, modification time and change time. If the file grows, the reader checks up to 64 saved bytes at the original EOF. A rewrite combined with growth can escape detection if it preserves the bytes checked for continuity.
- When the terminal's child exits or the terminal is dropped, the worker first reconciles the latest recorded launch, then reads the current transcript through its observed EOF. A replacement that starts and ends between polls gets its own launch context and freshness time for the final lookup. The worker then looks up the departing launch once more when its launch time is known and no replacement launch has been observed. A newly selected path drains under the usual freshness rules. This can recover a Pi reply written to a new session between the last poll and exit. Success gives `Closed(TerminalEnded)`. A previously observed transcript disappearing before the final read, a read failure, an exhausted lookup byte budget, a file identity mismatch on reopen, or a rewrite detected during that drain gives `Closed(IncompleteDrain)`. Turns queued before any close other than `stop()` stay available to `try_recv()` and the [waits](#waiting-instead-of-polling).

For an older transcript, a line still being written at the starting position is new once it ends. A file located before it exists is read from its start once it appears. A file that shrinks, changes its first line or changes its last bytes read was rewritten or replaced: it is read again from its start to locate the last handled line or its predecessor. If only the predecessor matches in place, the reader treats a later matching marker and bounded reply as already reported until it observes an intervening turn boundary. This can withhold a genuinely new completion when the transcript gives it identical evidence. If neither line can be placed or the rewrite interrupted the initial baseline scan, the worker discards the replay parser and reads from the rewritten file’s end with a fresh parser. If the file shrinks below an unfinished baseline, the observed shorter EOF becomes the new baseline immediately. Appends beyond it can report fresh turns while the reader is still scanning history. When a new file shares the beginning of a conversation with another transcript, the worker skips that shared prefix. A source can identify a per-session header with `AmbientSource::is_session_header` (built in for Pi and Codex). After the first differing conversation record, even an identical reply and end marker count as fresh. If an earlier transcript exceeded the digest capacity or baseline scan window, the worker starts an unknown transcript at its EOF at discovery, including zero for an empty file. This can skip genuine replies already present at discovery. File creation times decide whether a file is new: on a file system without them (some Linux file systems) a transcript created after the launch is read from its size when located, and a turn that ended before the first lookup is missed.

## Evidence

| `AmbientEvidence` | Meaning |
| --- | --- |
| `ExplicitEnd` | The source recognised an explicit end-of-turn record. |
| `InferredFromNextRecord` | A later record, typically the next prompt, showed that the record before it ended the turn. |
| `InferredAfterQuiet` | The transcript stayed unchanged for `AmbientOptions::quiet` (default 3 s) after a record the parser holds as a final reply. A later record can still contradict it. |
| `InferredAtTranscriptEnd` | The agent stopped writing the transcript — moved to another one, returned to the shell, relaunched or exited — after such a record. |

None of these attributes the turn to the terminal's agent. `AmbientTurn::reply` is `None` when the turn ended without text or with blank text.

Quiet and departure inference require the last observed line to end with a newline.
Pi checks every content block for a tool call before inferring an end. With text enabled, Cursor discards narration through the last tool call, including a call past its 256-block retention limit. An unreadable user or assistant message, or an end marker without a readable status, discards pending Cursor text and clears the evidence for inferring completion at the next prompt.

## Status

| `AmbientStatus` | Meaning |
| --- | --- |
| `Waiting(Starting)` | The worker has not finished its first look. |
| `Waiting(NoAgent)` | No agent launched by the terminal runs. |
| `Waiting(Unavailable(reason))` | `AmbientUnavailable`: `AgentUnknown`, `NoConfigRoot`, `NotFound`, `ReadBudgetExceeded`, `Ambiguous`, `Unsupported` from the source; `MissingDirectory` when the located file's directory does not exist; `Io(kind)` for a failed lookup or read; `Feed(AmbientFeedUnavailable)` for a [feed](#feeds). Retried on the next wake, except a feed's reason that is final for the launch. |
| `Active { unrecognised, oversized_lines }` | Reading the located transcript, which may not exist yet. `unrecognised`: a record of a kind the source knows lacked expected fields since the last turn end, so the installed CLI's format may have changed. `oversized_lines`: lines skipped for exceeding `line_bytes`. |
| `Closed(reason)` | `Stopped`, `TerminalEnded`, `Monitor` (the controller lost its PTY monitor), `Overflow`, `IncompleteDrain` (a failed final transcript read or a feed with pages outstanding at its final deadline), `WorkerPanic`. Final. |

## Limits and threads

`AmbientOptions::limits` is a `TurnOptions` with the controlled tier's ceilings: each pass reads at most `read_bytes` new bytes and hands at most `records` lines to the parser; passes that hit a budget continue without waiting, up to 64 per wake. A line longer than `line_bytes` is never buffered; it is hashed for continuity, skipped and counted. A skipped line discards the current parser and its pending reply and question. A reply keeps at most `reply_bytes`, cut at a UTF-8 boundary with `TurnReply::truncated` set. When the first character exceeds that budget, the reply has empty text and retains the truncation flag, including for Cursor’s opt-in text. The reader compacts each reply string before staging it. Staged and queued turns share the same `queue_items` and `queue_bytes` limits. This includes tentative turns held while locating a rewrite anchor. A turn that does not fit closes the attachment with `Closed(Overflow)`, as the controlled reader ends with `TurnGap::Overflow`; the turns queued before it stay available. The worker exits after overflow even if the host keeps the receiver to drain it. `poll` accepts 10 ms to 60 s, `quiet` 10 ms to 1 h.

Each worker reads regular files and keeps active read state for up to four transcripts. Each has a partial-line buffer under `line_bytes` plus `read_bytes`. To establish starting positions, the worker examines up to 4,096 directory entries in total per launch, including entries it skips. It keeps up to 4,096 offsets for inactive paths. For both listed and evicted files it saves up to 64 bytes before the offset. A listed offset can fall inside a partial line. An evicted offset follows the last complete line consumed. If the saved bytes differ on reopening, the worker resumes at the current EOF. If that table fills and an evicted transcript's offset cannot be saved, the worker starts any existing file with an unknown position at its current end for the rest of the launch. This can miss intervening turns but avoids reporting old turns again. A file first located while missing still starts at byte zero when it appears.

The worker keys at most the last 4 MiB of a file's existing content and remembers up to 8,192 conversation prefix digests to recognise copied history. At most `ambient_worker_limit()` workers run per process (default `DEFAULT_AMBIENT_WORKERS` = 64, settable from 1 to `MAX_AMBIENT_WORKERS` = 256 with `set_ambient_worker_limit`), independently of the controlled tier's four. A stopped worker inside a slow read or source callback keeps its slot until that call returns, but publishes nothing more. Terminal teardown never waits for a worker. A panic in a source or parser closes the attachment with `WorkerPanic`.

## Built-in sources

The Claude, Codex, Pi and Cursor profiles return a source from `AgentProfile::ambient_turns`; OpenCode's does not (its turns come from its server, not a file). Each source reads the user's own configuration directory with whatever CLI version is installed. It pins no version: it skips unknown record kinds and fields, and reports `Unrecognised` only for a record of a kind it knows that lacks the fields it reads. A lookup reads the configuration overrides from the agent's environment through `AmbientContext::var` and resolves a relative root against the agent's cwd. It builds every path from that root and from names it listed or validated, never from a path a file records. Symbolic links inside the root are followed, as the CLI follows them. Each lookup walks at most 4,096 directory entries and has a 4 MiB budget for header and session reads, including buffered read-ahead. A read started with budget remaining may finish up to its per-file cap. If another candidate still requires a read, the source returns `ReadBudgetExceeded`. Claude restarts its fallback scan on each wake, so this status can persist until its PID file becomes usable or the scan fits. A source caches decoded headers for up to 1,024 files. Codex and Pi validate the opened transcript on each pass, including on departure. An unchanged file version reuses its decoded header. A replacement or modification requires a fresh decode. More entries than that make the lookup `Ambiguous`. The project is the agent's cwd, canonical (Windows: without `\\?\`), as all four CLIs record it.

| | Claude | Codex | Pi | Cursor |
| --- | --- | --- | --- | --- |
| Configuration root | `CLAUDE_CONFIG_DIR`, else `~/.claude` | `CODEX_HOME`, else `~/.codex` | `PI_CODING_AGENT_SESSION_DIR` (every project's sessions), else `PI_CODING_AGENT_DIR` or `~/.pi/agent`, then `sessions/--<project>--` | `CURSOR_DATA_DIR`, else `~/.cursor` |
| Transcript | `projects/<slug>/<sessionId>.jsonl`, the session the agent's `sessions/<pid>.json` names; `/clear` names a new one. Without a usable pid file (a wrapper leads the job, or the program is not known yet): the live interactive session of the project started since the launch, latest started | `sessions/YYYY/MM/DD/rollout-*.jsonl` of today and yesterday plus the rollout currently being followed. The newest modified since the launch whose `session_meta` names the project | The `*.jsonl` written last since the launch whose `session` header names the project | `projects/<slug>/agent-transcripts/<id>/<id>.jsonl` created since the launch, written last |
| Not the terminal's agent | A pid file started before the launch (a reused pid); `entrypoint` other than `cli` (print and SDK sessions); a dead process; `isSidechain` lines | `source` other than `cli` (`codex exec`, `codex app-server`, subagents' objects); `thread_source: "subagent"` | Not told apart: pi's RPC mode writes the same sessions | Subagents' `<id>/subagents/` files. A conversation from before the launch is never followed; while it is written after the followed one, the lookup reports `Ambiguous`. Print mode writes the same transcripts |
| End of turn | `system` `turn_duration`; a `user` record with `interruptedMessageId` (Esc) ends the open turn with no reply | `event_msg` `task_complete`; `turn_aborted` with no reply | None written. The final assistant message (`stop` or `length`, no tool call) ends the turn at the next prompt or `!` command, or after `quiet`. A tool result read before inference discards the candidate. `error` and `aborted` end it with no reply | `turn_ended` with `status: "success"`; a prompt after an answer whose `turn_ended` a rewrite dropped. Aborted and failed turns are not reported, as in the controlled tier |
| Reply | The latest assistant message supplies its text blocks, joined by a blank line, only if it stopped with `end_turn`. A newer non-meta assistant record with missing or non-array content discards the earlier reply. Thinking, tool calls, API error and aborted messages never count | `last_agent_message` (`null`: none) | The final message's text blocks joined by a blank line; never thinking | **None by default** (completion only). `CursorProfile::ambient_turns_with_unseparated_text()` opts in to the text after the turn's last tool call, as written: Cursor joins reasoning into that text without a marker |
| `AmbientEvidence` | `ExplicitEnd` | `ExplicitEnd` | `InferredFromNextRecord`, `InferredAfterQuiet`, `InferredAtTranscriptEnd` | `ExplicitEnd`, or `InferredFromNextRecord` (status unknown) |
| `Unrecognised` | `assistant` without a `message.content` array; `system` without `subtype` | `event_msg` without a payload type; `task_complete` without `last_agent_message` | `message` without `role`; an assistant one without a `content` array or `stopReason` | `turn_ended` without `status`; `user` or `assistant` without a `message.content` array |
| Recorded fixtures | 2.1.296, [ambient capture](../tests/fixtures/transcripts/ambient/claude-2.1.296/README.md) and the [controlled capture](../tests/fixtures/transcripts/claude-2.1.296/README.md) | 0.154.0, [ambient capture](../tests/fixtures/transcripts/ambient/codex-0.154.0/README.md) and the [controlled capture](../tests/fixtures/transcripts/codex-0.154.0/README.md) | 0.85.1 [ambient capture](../tests/fixtures/transcripts/ambient/pi-0.85.1/README.md); 1.5.2 [controlled capture](../tests/fixtures/transcripts/pi-1.5.2/README.md) | 2026.10.01 [ambient capture](../tests/fixtures/transcripts/ambient/cursor-2026.10.01/README.md); 2026.11.13-eacade1 [writer images](../tests/fixtures/transcripts/cursor-2026.11.13-eacade1/README.md) |

Pass Cursor's opt-in source to `attach_ambient_turns_with`:

```rust
use agent_link::agent::profiles::CursorProfile;

let source = CursorProfile.ambient_turns_with_unseparated_text();
let replies = term.attach_ambient_turns_with(source, AmbientOptions::default())?;
// Each reply can mix Cursor's reasoning with its displayed answer.
```

Codex and Pi follow symlinked transcript files and use the target’s modification time for discovery. They verify that the recorded working directory resolves to the same filesystem directory as the project, both during lookup and before reading the opened transcript. Claude uses the same identity check for its project-based fallback.

Limits of the built-in sources:

- Another session of the same CLI in the same project can be located instead of the terminal's: Codex's newest rollout, Pi's session written last, Cursor's conversation written last, or a Claude session found by the fallback. A Claude agent with its own pid file is located by its pid. A resumed or forked conversation, or another program, can append to the file followed (see the warning above).
- Claude's long project directories (over 200 characters, cut and hashed by Claude) are matched by their first 200 characters. Codex's date directories are listed as the UTC dates from two days back to one ahead, which covers the local today and yesterday in every zone. A rollout already selected by this attachment remains eligible after its start directory ages out. It competes with newly discovered rollouts by modification time and undergoes the same header checks.
- Cursor rewrites its file in place. The reader matches the previous end marker or its preceding line at its original byte position. If inserted metadata separates the predecessor from the old marker, the reader compares the marker and its bounded reply before publishing the rewritten suffix. Metadata between a partial reply and the next prompt preserves the partial reply’s original position, so a previously observed abort still suppresses it. An intervening observed turn boundary allows a later identical reply to count. If a rewrite also moves those lines, the reader falls back to their content.
- File creation times decide which Cursor conversations are new. On a file system without them, every conversation written since the launch counts as new.

## Writing a source

An `AmbientSource` (`Send + Sync + 'static`) implements `locate(&AmbientContext) -> Result<PathBuf, AmbientUnavailable>` and `parser() -> Box<dyn AmbientParser>`. Keep `locate` cheap and bounded: it runs on every wake. Read the agent's variables with `AmbientContext::var` and `home()`, never this process's environment. `agent::turns::ambient::paths` has `config_root`, `anchored`, `project_path`, `same_dir`, `created` and `written`.

An `AmbientParser` receives each complete, nonblank line (newline removed, ASCII whitespace trimmed, at most `line_bytes`, not validated as UTF-8 or JSON) in `record(line, reply_bytes)`, and `quiet(reply_bytes)` while the file stays quiet or when it cannot grow any more. It returns `AmbientRecord::Ended(reply)` for an explicit end record, `EndedBefore(reply)` when this line shows that the previous one ended the turn, or `Unrecognised` for a known record kind without its expected fields. Return `SuppressedEnd` for an unsuccessful end whose position must survive a rewrite. Cursor uses it to keep an aborted or failed turn from resurfacing at the next prompt. A metadata record can return `AmbientRecord::Unchanged` to preserve the originating line for quiet and next-prompt inference. Retain at most one bounded reply candidate; longer text is cut by the observer. A new parser is created for each file and after each rewrite, when lines are fed again from the start; the observer drops anything already reported.

With the `questions` feature, fresh records call `question(line)` immediately after `record`. Copied and replayed records call `reset_question_context()` to clear prompt evidence. A fresh question line can return `AmbientQuestion::Asked(PendingQuestion)` for a question tool's call written before its result (a nonempty provider id, within the question bounds), `Answered(id)` for its result, `Withdrawn` for a prompt, the turn's end or a record that cannot be read, `None` for nothing. The observer retains a compact copy of a valid question’s ID, items and option labels. The default reads no questions.

## Questions

With the `questions` feature, `AgentTerminal::observe_ambient_transcript_questions(&receiver)` relays what a transcript attachment's parser says the agent asks (the built-in Claude and Codex sources do), but only while the terminal's screen shows the same question, and answers it with the profile's keys. The worker keeps what the transcript it follows shows asked: only a file's own new lines count; copied history, a rewritten file read again and a line over `line_bytes` withdraw the question, and so does a new launch. A read that stopped on a partial line, or a transcript that cannot be read now, decides nothing. Stopping or dropping the receiver ends the question source's authority. See [Best-effort ambient questions](agents-and-delivery.md#best-effort-ambient-questions) for the rules, the weaker evidence and OpenCode's question feed.

## Evidence for this tier

The framework is covered offline: unit tests for the bounded reader, rewrite placement, copied history, quiet and next-record inference, queue and budget contracts, blocking and awaited waits, and the `ambient_turns` integration target, which drives the fake agent on real PTYs (direct and, on Unix, from the test shell) with a synthetic source and temporary transcripts.

The built-in sources are covered by unit tests in `agent::profiles::ambient` and by the `ambient_sources` integration target. The unit tests replay the recorded fixtures listed above line by line, with variations that each change one record, and run the lookups against temporary configuration roots with file times set explicitly. The integration target runs the fake agent as the direct child of each built-in profile's terminal and writes the recorded transcripts under a temporary root. Each profile's `compatibility()` names these captures and versions; it claims no live check of this tier.

These results are native macOS only. Linux and Windows compile in CI-style checks, but their runtime behaviour is unverified. That includes Windows foreground and departure handling, the Windows process-liveness check behind Claude's fallback lookup, and Windows paths of the four CLIs. No real CLI has been observed through this tier, and no user configuration directory has been read by its tests.

## Feeds

A CLI that writes no transcript to tail but serves its turns has an `AmbientFeed` instead of an `AmbientSource`. The same receiver, status, evidence, limits, worker budget, stop semantics and lifecycle apply.

- `AmbientFeed::open(&AmbientContext, Option<&AmbientEndpoint>) -> Box<dyn AmbientFeedReader>` is called at the first wake of each agent launch (generation). The reader is dropped when that launch ends.
- `AmbientFeedReader::poll(&AmbientContext, &AmbientFeedBudget) -> AmbientFeedPoll` is called on every wake while the launch's agent runs, and with `budget.last` when the launch ends. Normal polls each have a two-second deadline. Final polls continue while `more` is set and share one two-second deadline. If pages are still outstanding at expiry, or the feed returns `Ended` with `more` on a final poll, the receiver closes with `IncompleteDrain` after publishing completed turns. It reads what arrived and never waits for more. The budget supplies `line_bytes`, `reply_bytes`, and an `events` cap for this poll. The worker reduces the cap from `records` to the number of maximum-sized replies that fit, with a minimum of one candidate to check for overflow.
- `AmbientFeedPoll { state, events, oversized, more }`:
  - `AmbientFeedState::Ready` gives `Active`.
  - `Unavailable(reason)` gives `Waiting(Unavailable(reason))` and is polled again on the next wake.
  - `Ended(reason)` gives the same status, but the reader is dropped and not polled again until the next launch.
  - `AmbientFeedEvent::Ended { key, ended, reply }` is reported with `ExplicitEnd`: at most once per key (the newest 4,096 keys, kept as digests under random keys), never when `ended` is before the generation started, the reply cut to `reply_bytes`. A key longer than 256 bytes counts as unrecognised.
  - `AmbientFeedEvent::Unrecognised` sets `Active { unrecognised: true }` until a turn ends cleanly.
  - `oversized` adds to `Active.oversized_lines`.
  - `more` makes the worker poll again without waiting for its next wake.
- `stop()` never waits for a poll in progress. Nothing the poll returns afterwards is published, and the worker keeps its slot until the poll returns.

A feed that reads the agent's own server needs the agent launched to serve one. `AgentTerminal::spawn_with_ambient_feed(spec, agent)` calls the profile's `AgentProfile::ambient_feed_launch(&mut LaunchPlan)`. The hook adds arguments and environment to the launch and returns an `AmbientEndpoint`: a loopback `SocketAddr` with Basic credentials. `AmbientEndpoint::new` refuses any address that is not loopback, and its `Debug` never shows the password. The terminal keeps the endpoint and hands it to every reader it opens. Ordinary `spawn` is unchanged: nothing is served without this opt-in, and a feed opened without an endpoint ends with `Feed(NotServed)`.

`spawn_with_ambient_feed` returns `Error::InvalidInput` without a profile, for a direct argv that is not UTF-8, or when `spec.env` removes a variable the hook sets. It returns `Error::Unsupported` when the profile serves no feed, and `Error::Io` when no loopback port or no OS randomness is available. It cannot be combined with `spawn_with_tools` or the controlled tier's spawns.

| `AmbientFeedUnavailable` | Meaning | Retried |
| --- | --- | --- |
| `NotServed` | The terminal was not spawned with `spawn_with_ambient_feed`. | No |
| `NotListening` | Nothing accepts connections at the endpoint (yet). | Every wake until the server first answered, then with backoff |
| `OwnerUnverified` | The process serving the endpoint could not be shown to be the launched agent (the connection was not accepted in time, or by a process this one cannot inspect). Nothing was sent. | With backoff |
| `OwnerMismatch` | A process that is not the launched agent serves the endpoint. Nothing was sent. | No (until the next launch) |
| `Unprotected` | The agent's server answered a request without credentials. | No (until the next launch) |
| `CredentialsRefused` | The agent's server refused the launch's credentials. | No (until the next launch) |
| `Status(code)` | An unexpected HTTP status. | With backoff |
| `Protocol` | An answer that could not be read in time or within its bound, or has an unknown shape. | With backoff |

`AmbientUnavailable::AgentUnknown` means that the launched agent's program is not identified yet. This happens, for example, before a shell launch's program owns the terminal. Nothing is connected until the program is identified. A platform that cannot identify a connection's server process ends the feed with `AmbientUnavailable::Unsupported`.

## OpenCode

OpenCode keeps its sessions in a database, not in a transcript file, and its TUI serves them over HTTP when launched with a port. `OpencodeProfile::ambient_feed()` reads that server. **Compatibility is declared, not measured:** the flags, routes, event names and message shapes follow OpenCode 1.18.21 as the predecessor crate used them (checked there by hand, with no automated live test). This crate tests the feed only against a synthetic OpenCode-like server. OpenCode 2.x is not covered.

### Setup

```rust
use agent_link::agent::profiles::OpencodeProfile;
use agent_link::agent::turns::ambient::AmbientOptions;
use agent_link::agent::{AgentSpec, AgentTerminal};

let (term, events) =
    AgentTerminal::spawn_with_ambient_feed(spec, AgentSpec::new(Arc::new(OpencodeProfile)))?;
term.launch()?; // a shell spawn; a direct spawn's child is the agent
let replies = term.attach_ambient_turns(AmbientOptions::default())?;
```

The launch gets `--port <a 127.0.0.1 port free a moment before> --hostname 127.0.0.1`, `OPENCODE_SERVER_USERNAME=opencode`, and `OPENCODE_SERVER_PASSWORD` set to 16 bytes of OS randomness as 32 hex digits. These values replace any value the plan or the host's policy set for those keys. A plan that enables `--mini`, already sets `--port` or `--hostname`, or contains the end-of-options marker `--` fails with `InvalidInput` before spawn. [OpenCode 1.18.21 mini-mode check](https://github.com/anomalyco/opencode/blob/v1.18.21/packages/opencode/src/cli/cmd/tui.ts#L152-L160).

- **Direct spawn**: the arguments are appended to the host's argv, and only the agent and its children get the password. Each spawn has its own port and password.
- **Shell spawn**: the arguments join the typed launch line (the port is visible on screen, the password is not). The variables join the shell's spawn environment, so **every process started from that shell inherits the password**, and every launch typed in that terminal uses the same port and password. Rotating the password per launch would require typing it, which would echo it to the screen, the output stream and the shell history. Spawn a new terminal for a new port and password.

### What is read

1. **Whose server?** The feed connects to the endpoint. Before it writes anything on the connection, it identifies the local process that holds the connection's server end. That process must be the launched agent: a process in its process group on Unix (the direct child leads its own session; a shell launch's agent is its foreground group), or the program or a process below it on Windows.
   - **macOS**: `libproc`.
   - **Linux**: `/proc/net/tcp{,6}` and `/proc/<pid>/fd`.
   - **Windows** checks the TCP owner again with process handles open. For a descendant,
     every Toolhelp parent on the path to the agent must have a strictly earlier creation
     time than its child. A missing process, inaccessible handle or ambiguous timestamp
     gives `OwnerUnverified`; the connection receives no request.

   The check waits up to 500 ms for the server to accept the connection. If another process holds the server end, the result is `OwnerMismatch`, which is final. If no holder is seen, the result is `OwnerUnverified`, which is retried. On macOS and Linux the owner scan checks its deadline while visiting processes and descriptors. macOS rechecks the socket holder’s process group after the descriptor match. Linux also checks while reading the socket table. Expiry leaves ownership unverified. Every new connection is checked.
2. **Protected?** On a verified connection, `GET /session` without credentials must be refused with `401`. The probe closes its socket after the response head. Any `2xx` gives the final `Unprotected` status regardless of the response body. The authenticated request uses a fresh, owner-checked connection.
3. **Credentials.** `GET /session` with Basic auth must answer `200` with a list; the sessions' `parentID`s are learnt. An authenticated `401` or `403` on this or a later request closes that connection at the response head and ends the feed with `CredentialsRefused`. Then `GET /event`, on its own verified connection, opens the server-sent event stream and the status becomes `Active`.
4. **Events.** Each `data:` line holds one JSON `{type, properties}`.
   - `session.idle`, or `session.status` with `status.type` `idle`, queues its session. At most 64 sessions are queued. Evicting the oldest also drops its retry count.
   - `session.created` and `session.updated` teach a session's `parentID`; `session.deleted` forgets it.
   - A session id must be at most 128 bytes of letters, digits, `_` and `-`, because it goes into a request path.
   - When the stream ends, it is opened again after a backoff: 250 ms, doubling up to 8 s, reset by a stream that stayed open 10 s. OpenCode does not replay what it sent while the stream was down.
5. **Turn end.** A queued session that is a subagent's (the `task` tool's sessions) gives no turn. A session that neither the list nor an event named is looked up with `GET /session/{id}` first. The response must name the requested session before the reader caches its parent classification. The reader retries a mismatched lookup. The reader then fetches `GET /session/{id}/message`.
   - The reader visits turns in session order and saves the last handled message ID for each session. Multiple turns between polls can arrive together. If the event budget fills, the reader continues on the next poll. For each user prompt the last assistant message other than a compaction summary needs a completion timestamp or a nonempty finish reason. A finished message uses `time.completed` when available and falls back to `time.created`. A later assistant message with a different `parentID` closes the preceding prompt; a queued user prompt alone does not prove completion. The trailing assistant must reference the latest user prompt. A `tool-calls` or `unknown` finish, or any tool part needing model follow-up, prevents completion even with `finish: "stop"`. Provider-executed tools and cleanup-marked interrupted tool errors do not require follow-up. [OpenCode completion rule](https://github.com/anomalyco/opencode/blob/v1.18.21/packages/opencode/src/session/prompt.ts#L1044-L1059). An automatic compaction record discards the preceding assistant step. Neither the compaction summary nor its synthetic continuation starts another turn. An overflow replay can copy the original prompt without a synthetic flag. For the trailing prompt, the reader checks `GET /session/status` and then fetches the latest message with `?limit=1`. It publishes the candidate only if the session was idle and the last message ID matches across the check. [OpenCode 1.18.21 compaction](https://github.com/anomalyco/opencode/blob/v1.18.21/packages/opencode/src/session/compaction.ts#L475-L595) · [Session status](https://github.com/anomalyco/opencode/blob/v1.18.21/packages/opencode/src/session/status.ts) · [Message pagination](https://github.com/anomalyco/opencode/blob/v1.18.21/packages/opencode/src/session/message-v2.ts#L425-L466).
   - Only a message that `finish`ed `stop` has a reply: its `text` parts that are neither `synthetic` nor `ignored`, blank ones left out, joined by a blank line, cut at `reply_bytes`.
   - Any other finish (tool calls, an abort, an error, `length`) gives a turn without a reply. An earlier stopped message never stands in.
   - If the status still says busy or the message ID changes, the reader fetches the messages again on a later poll. It uses the same bounded retries as a failed fetch. After three unsuccessful attempts on a pending turn, a live reader reports it as unrecognised.
6. **Shape check.** A `data:` line that is not JSON, an event without a `type`, a known turn event without its fields, and a message list of another shape are `Unrecognised`. Reading goes on.

With the `questions` feature, `OpenCodeAmbientQuestions` lists and answers pending questions for a host-specified root session on the same server, using the same endpoint, credentials and owner check. [OpenCode questions from the TUI server](agents-and-delivery.md#opencode-questions-from-the-tui-server)

### Security model and residual risk

- **The password goes only to the launched agent's process.** It is sent in the `Authorization` header and only on a connection whose server end the owner check found in that process. A port squatter never receives a request, whether it bound the port between the release and OpenCode's bind, or listens beside it. The bind-then-release race cannot be prevented, because OpenCode cannot inherit a listening socket. A verified mismatch ends that launch's feed with `OwnerMismatch`. An inconclusive owner check gives `OwnerUnverified` and the feed retries.
- **Only loopback.** The endpoint is a loopback socket address. Nothing resolves a name, reads a proxy setting or follows a redirect.
- **The password is in the environment.** In a shell spawn it is in the shell's environment and in every process the shell starts. In a direct spawn it is in OpenCode's environment and its children's, including the commands its tools run. A process of the same user can usually read another process's environment (`ps eww`, `/proc/<pid>/environ`) and can then drive the OpenCode server as the user. The owner check protects the password in transit to the server. It cannot keep it from processes that already run as the user.
- **The password is never logged.** No `Debug` impl shows it: `AmbientEndpoint`, `EnvPolicy`, `LaunchPlan`, `AmbientContext` and the readers all redact it. Controller traces do not include environment values. Tests check the trace and log output of a full session.
- **Process identity limits.** On Unix, a descendant of the agent that leaves its process group with `setsid` or `setpgid` does not count as the agent, so its server would read `OwnerMismatch`. A process of another user cannot be inspected: one that holds the server end reads `OwnerUnverified` and is never sent anything. Windows pins the supplied program PID during each owner check. For a shell launch, the earlier foreground observation supplies that PID. On Windows under the default `ForegroundPolicy::ExactOnly`, a shell launch's program is not identified, so the feed waits with `AgentUnknown`; use a direct spawn or `AcceptHeuristic`.
- **What the server says is best effort.** Another OpenCode client of the same server, such as `opencode attach` to the same port with the password read from the environment, can add turns that are reported like the agent's own. Replies carry no attribution guarantee.

### Limits

- One poll has a two-second budget. After departure, OpenCode uses only connections verified during the launch. If a pending lookup needs a new connection or exhausts its three attempts, the receiver closes with `IncompleteDrain`.
- One exchange (connect, owner check and request) takes at most 1 s. A connection waits at most 500 ms to be accepted.
- An event-stream drain takes at most 500 ms, `records` lines and 4 MiB, and ends after 20 ms without data.
- Event lines longer than `line_bytes` are skipped and counted in `oversized_lines`.
- Bodies are capped at 64 KiB for a session lookup, 4 MiB for the session list and status map and 8 MiB for a message list. A longer message list skips its turn and counts it in `oversized_lines`. Within those caps, the parser processes sessions, statuses and message parts one at a time and retains bounded reply text.
- Response heads are capped at 16 KiB.
- The reader remembers at most 4,096 sessions, 64 queued idle sessions and 3 attempts per reply.
- For each queued session the reader makes one message-list request per page of results, which returns the whole session, tool outputs included. A trailing candidate requires a status lookup and, when idle, one latest-message lookup. Each unknown session also requires a session lookup.

### OpenCode evidence

| What | Kind | Where |
| --- | --- | --- |
| Flags, routes, event and message shapes of OpenCode 1.18.21 | Declaration | This page; `OpencodeProfile::compatibility()` |
| Client rules: handshake, owner check, from-now, once per turn, truncation, backoff, bounds, a port squatter, an unprotected server, refused credentials, slow, stalled, oversized and endless answers | Synthetic fixture | `agent::loopback` and `agent::profiles::ambient::opencode` unit tests; `tests/ambient_feed.rs`, where the fake agent serves the synthetic API from the launched process |
| Owner check against real sockets | Native test, macOS arm64 only | `agent::loopback::owner` unit tests |

Synthetic tests cover Linux `/proc` rows and Windows parent chains with reused PIDs, missing links and equal creation times. Linux and Windows owner checks are cross-compiled, but their runtime behaviour is **unverified**. No OpenCode was launched to produce this evidence. There is no ignored live test for the feed yet: a live smoke would need the pinned OpenCode binary and a local model peer, like `scripts/check_opencode_questions.py`.
