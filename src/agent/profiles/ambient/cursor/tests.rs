use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;
use crate::agent::profiles::ambient::testing::{
    Seen, at, canonical, ctx, dirs, ended, feed, fixture, line, start, write_at,
};

// --- turns ------------------------------------------------------------------------------

/// The opt-in source: unseparated text.
fn texts() -> CursorAmbient {
    CursorAmbient { text: true }
}

/// The profile's source: completion only.
fn completion() -> CursorAmbient {
    CursorAmbient { text: false }
}

fn before(text: &str) -> Seen {
    Seen::Before(Some(text.into()))
}

fn user() -> String {
    line(json!({"role": "user", "message": {"content": [{"type": "text", "text": "prompt"}]}}))
}

fn assistant(content: Value) -> String {
    line(json!({"role": "assistant", "message": {"content": content}}))
}

fn text(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn tool() -> Value {
    json!({"type": "tool_use", "name": "Shell", "input": {"command": "ls"}})
}

fn turn_ended(status: &str) -> String {
    line(json!({"type": "turn_ended", "status": status}))
}

const CAPTURE: &str = "ambient/cursor-2026.10.01";

/// The captured file after each turn (2026.10.01). cursor rewrites it at every turn's first
/// write, dropping the earlier `turn_ended` lines: read from its start, the earlier turns
/// end at the prompts after them, the last with its `turn_ended`. The text after the
/// turn's last tool call is the reply, exactly as written.
#[test]
fn the_captured_rewrites_end_each_turn_once_read_from_the_start() {
    let read = |name: &str| feed(&texts(), &fixture(&format!("{CAPTURE}/{name}")));
    assert_eq!(read("turns.jsonl"), [ended("first answer")]);
    assert_eq!(
        read("turns.rewritten.jsonl"),
        [before("first answer"), ended("second answer")]
    );
    assert_eq!(
        read("turns.tool.jsonl"),
        [
            before("first answer"),
            before("second answer"),
            ended("after tool")
        ]
    );
    // A turn interrupted with Esc: cursor kept none of its messages and wrote its
    // `turn_ended` over the previous one. Not a successful end: not reported.
    assert_eq!(read("turns.aborted.jsonl"), [before("first answer")]);
}

/// The profile's source reports the same ends with no text.
#[test]
fn completion_only_reports_the_same_ends_without_text() {
    let read = |name: &str| feed(&completion(), &fixture(&format!("{CAPTURE}/{name}")));
    assert_eq!(read("turns.jsonl"), [Seen::Ended(None)]);
    assert_eq!(
        read("turns.tool.jsonl"),
        [Seen::Before(None), Seen::Before(None), Seen::Ended(None)]
    );
}

/// The controlled tier's images of the packaged writer (2026.11.13-eacade1): its text
/// joins the answer and the reasoning in one block, which only the opt-in reports.
#[test]
fn the_packaged_writer_images_mix_reasoning_into_the_text() {
    let lines = fixture("cursor-2026.11.13-eacade1/snapshot-2.jsonl");
    assert_eq!(
        feed(&texts(), &lines),
        [
            before("Historical reply.\n\nFixture reasoning."),
            before("Same visible reply.\n\nFixture reasoning."),
            ended("Same visible reply.\n\nFixture reasoning."),
        ]
    );
    assert_eq!(
        feed(&completion(), &lines),
        [Seen::Before(None), Seen::Before(None), Seen::Ended(None)]
    );
}

/// cursor's reasoning joined into its text block is reported as written by the opt-in.
#[test]
fn reasoning_joined_into_the_reply_is_reported_as_is_when_opted_in() {
    let joined = "first answer\n\nThe user wants exactly two words back.";
    let lines = [
        user(),
        assistant(json!([text(joined)])),
        turn_ended("success"),
    ];
    assert_eq!(feed(&texts(), &lines), [ended(joined)]);
    assert_eq!(feed(&completion(), &lines), [Seen::Ended(None)]);
}

/// The text blocks after the turn's last tool call, joined by a blank line; narration
/// before it never counts, and a turn without text after it has no reply.
#[test]
fn the_text_after_the_last_tool_call_is_the_reply() {
    let lines = [
        user(),
        assistant(json!([text("narration"), tool()])),
        assistant(json!([text("one")])),
        assistant(json!([text("two")])),
        turn_ended("success"),
    ];
    assert_eq!(feed(&texts(), &lines), [ended("one\n\ntwo")]);
    let lines = [
        user(),
        assistant(json!([text("narration"), tool()])),
        turn_ended("success"),
    ];
    assert_eq!(feed(&texts(), &lines), [Seen::Ended(None)]);
}

#[test]
fn a_tool_use_after_the_block_limit_discards_earlier_narration() {
    let mut blocks = vec![text("narration"); super::super::json::MAX_ITEMS];
    blocks.push(tool());
    for answer in [None, Some("after tool")] {
        if let Some(answer) = answer {
            blocks.push(text(answer));
        }
        let lines = [assistant(blocks.clone().into()), turn_ended("success")];
        assert_eq!(
            feed(&texts(), &lines),
            [Seen::Ended(answer.map(String::from))]
        );
    }
}

/// A tool result some versions write in a `user` envelope: what came before it was
/// narration, and it is no prompt.
#[test]
fn a_tool_result_in_a_user_line_is_no_prompt() {
    let result = line(json!({"role": "user", "message": {"content": [
        {"type": "tool_result", "content": "notes.txt"}
    ]}}));
    let lines = [
        user(),
        assistant(json!([text("narration")])),
        result,
        assistant(json!([text("after")])),
        turn_ended("success"),
    ];
    assert_eq!(feed(&texts(), &lines), [ended("after")]);
}

/// A prompt starts the turn afresh; it ends an earlier turn left open (its `turn_ended`
/// dropped), and only such a turn.
#[test]
fn a_prompt_ends_only_a_turn_left_open() {
    let lines = [
        assistant(json!([text("stale")])),
        user(),
        turn_ended("success"),
    ];
    assert_eq!(feed(&texts(), &lines), [before("stale"), Seen::Ended(None)]);
    let lines = [
        user(),
        assistant(json!([text("one")])),
        turn_ended("success"),
        user(),
    ];
    assert_eq!(feed(&texts(), &lines), [ended("one")]);
    let lines = [user(), assistant(json!([text("n"), tool()])), user()];
    assert_eq!(feed(&texts(), &lines), [Seen::Before(None)]);
    assert_eq!(feed(&texts(), &[user(), user()]), []);
}

/// Turns that ended otherwise than successfully are not reported, as in the controlled
/// tier; the parser starts over after them.
#[test]
fn a_turn_that_did_not_succeed_is_not_reported() {
    for status in ["aborted", "error", "something-new"] {
        let lines = [
            user(),
            assistant(json!([text("partial")])),
            turn_ended(status),
            user(),
        ];
        assert_eq!(feed(&texts(), &lines), [], "{status}");
    }
}

#[test]
fn a_known_record_without_its_fields_is_unrecognised() {
    let lines = [
        line(json!({"type": "turn_ended"})),
        line(json!({"role": "assistant", "message": {}})),
        line(json!({"role": "assistant"})),
        line(json!({"role": "user", "message": {"content": "text"}})),
        user(),
        assistant(json!([text("answer")])),
        turn_ended("success"),
    ];
    assert_eq!(
        feed(&texts(), &lines),
        [
            Seen::Unrecognised,
            Seen::Unrecognised,
            Seen::Unrecognised,
            Seen::Unrecognised,
            ended("answer")
        ]
    );
}

#[test]
fn unreadable_turn_records_discard_pending_text_and_inference() {
    for malformed in [
        json!({"role": "user", "message": {"content": "unreadable"}}),
        json!({"role": "assistant"}),
        json!({"type": "turn_ended"}),
    ] {
        let mut lines = vec![assistant(json!([text("old")])), line(malformed)];
        let mut next_prompt = lines.clone();
        next_prompt.push(user());
        assert_eq!(feed(&completion(), &next_prompt), [Seen::Unrecognised]);

        lines.extend([assistant(json!([text("new")])), turn_ended("success")]);
        assert_eq!(feed(&texts(), &lines), [Seen::Unrecognised, ended("new")]);
    }
}

#[test]
fn other_lines_end_nothing() {
    let lines = [
        line(json!({"type": "metadata", "metadata": {"overview": "o"}})),
        line(json!({"role": "system", "message": {"content": []}})),
        line(json!({"type": "other", "status": "success"})),
        line(
            json!({"role": "assistant", "newField": 1, "message": {"content": [
                {"type": "thinking", "text": "hidden"}, 7
            ]}}),
        ),
    ];
    assert_eq!(feed(&texts(), &lines), []);
}

/// The text keeps at most the budget, cut at a UTF-8 boundary; completion only keeps none.
#[test]
fn the_text_is_bounded_and_kept_only_when_opted_in() {
    let mut parser = texts().parser();
    let _ = parser.record(user().as_bytes(), 3);
    let _ = parser.record(assistant(json!([text("ééé")])).as_bytes(), 3);
    let Some(AmbientRecord::Ended(Some(reply))) =
        parser.record(turn_ended("success").as_bytes(), 3)
    else {
        panic!("an explicit end");
    };
    assert_eq!(reply.text, "é");
    assert!(reply.truncated);
    for end in [turn_ended("success"), user()] {
        parser.record(assistant(json!([text("🙂")])).as_bytes(), 1);
        let reply = match parser.record(end.as_bytes(), 1) {
            Some(AmbientRecord::Ended(Some(reply)) | AmbientRecord::EndedBefore(Some(reply))) => {
                reply
            }
            other => panic!("expected the truncated reply, got {other:?}"),
        };
        assert!(reply.text.is_empty() && reply.truncated);
    }
    let mut parser = CursorParser {
        text: false,
        after_tool: TurnReply::default(),
        answered: false,
    };
    let _ = parser.record(assistant(json!([text("kept nowhere")])).as_bytes(), 64);
    assert!(parser.after_tool.text.is_empty());
}

// --- lookup -----------------------------------------------------------------------------

/// Creation times the tests set: few file systems let a test set one.
#[derive(Default)]
struct Born(HashMap<PathBuf, SystemTime>);

impl Born {
    fn locate(&self, ctx: &AmbientContext) -> Result<PathBuf, AmbientUnavailable> {
        transcript(ctx, |path, _| self.0.get(path).copied())
    }

    /// The transcript of conversation `id` under `dir`, as cursor writes it, created at
    /// `start() + born` and last written at `start() + written`.
    fn transcript(&mut self, dir: &Path, id: &str, born: i64, written: i64) -> PathBuf {
        let path = dir.join(id).join(format!("{id}.jsonl"));
        write_at(&path, &user(), at(written));
        self.0.insert(path.clone(), at(born));
        path
    }
}

/// cursor's directory for `project`, by its rule.
fn transcripts(root: &Path, project: &Path) -> PathBuf {
    root.join("projects")
        .join(cursor_slug(&canonical(project)))
        .join("agent-transcripts")
}

fn launched(project: &Path, home: &Path, vars: &[(&str, &Path)]) -> AmbientContext {
    let mut ctx = ctx(project, home, vars);
    ctx.launched_at = Some(start());
    ctx
}

#[test]
fn the_slug_dashes_collapses_and_trims() {
    assert_eq!(
        cursor_slug(Path::new("/Users/me/my proj")),
        "Users-me-my-proj"
    );
    assert_eq!(
        cursor_slug(Path::new(r"C:\Users\me\proj")),
        "C-Users-me-proj"
    );
    // As 2026.10.01 named a captured project's directory.
    assert_eq!(
        cursor_slug(Path::new("/private/tmp/of-capture-p1Rj")),
        "private-tmp-of-capture-p1Rj"
    );
    assert_eq!(cursor_slug(Path::new("/a//b--c/")), "a-b-c");
    assert_eq!(cursor_slug(Path::new("/")), "");
}

#[test]
fn the_root_is_its_data_dir_else_the_homes_cursor() {
    let home = || Some(PathBuf::from("/home/me"));
    let os = |text: &str| Some(std::ffi::OsString::from(text));
    assert_eq!(cursor_root(os("/d"), home()), Some(PathBuf::from("/d")));
    for blank in ["", "  "] {
        assert_eq!(
            cursor_root(os(blank), home()),
            Some(PathBuf::from("/home/me/.cursor"))
        );
    }
    assert_eq!(cursor_root(None, None), None);
    // Path building only (Windows is not run here).
    let root = cursor_root(None, Some(PathBuf::from(r"C:\Users\me"))).unwrap();
    assert_eq!(
        root.to_str().unwrap().replace('/', "\\"),
        r"C:\Users\me\.cursor"
    );
}

/// The newest conversation created since the launch, the one written last among them:
/// a new one (`/new`) and `/resume` back to an earlier one are followed; one from before
/// the launch never.
#[test]
fn the_conversation_written_last_since_the_launch_is_located() {
    let (_tmp, project, home) = dirs();
    let dir = transcripts(&home.join(".cursor"), &project);
    let ctx = launched(&project, &home, &[]);
    let mut born = Born::default();
    born.transcript(&dir, "before", -20, -10);
    assert_eq!(born.locate(&ctx), Err(AmbientUnavailable::NotFound));
    let a = born.transcript(&dir, "a", 5, 5);
    assert_eq!(born.locate(&ctx), Ok(a.clone()));
    let b = born.transcript(&dir, "b", 10, 10);
    assert_eq!(born.locate(&ctx), Ok(b.clone()));
    // cursor rewrites the file in place at a turn's first write.
    write_at(&a, &user(), at(20));
    assert_eq!(born.locate(&ctx), Ok(a));
    write_at(&b, &user(), at(30));
    assert_eq!(born.locate(&ctx), Ok(b));
}

/// `/resume` of a conversation from before the launch: it is not followed, so the lookup
/// declines rather than keep the one it followed reading as healthy. Written to again,
/// that one is followed again.
#[test]
fn a_resumed_conversation_from_before_the_launch_is_declined() {
    let (_tmp, project, home) = dirs();
    let dir = transcripts(&home.join(".cursor"), &project);
    let ctx = launched(&project, &home, &[]);
    let mut born = Born::default();
    let older = born.transcript(&dir, "older", -60, -60);
    let new = born.transcript(&dir, "new", 5, 5);
    assert_eq!(born.locate(&ctx), Ok(new.clone()));
    write_at(&older, &user(), at(10));
    assert_eq!(born.locate(&ctx), Err(AmbientUnavailable::Ambiguous));
    write_at(&new, &user(), at(20));
    assert_eq!(born.locate(&ctx), Ok(new));
    // With no conversation of its own yet.
    let (_tmp, project, home) = dirs();
    let dir = transcripts(&home.join(".cursor"), &project);
    let mut born = Born::default();
    born.transcript(&dir, "older", -60, 10);
    assert_eq!(
        born.locate(&launched(&project, &home, &[])),
        Err(AmbientUnavailable::Ambiguous)
    );
}

#[test]
fn the_root_is_cursor_data_dir_when_set() {
    let (tmp, project, home) = dirs();
    let data = tmp.path().join("data");
    let ctx = launched(&project, &home, &[("CURSOR_DATA_DIR", &data)]);
    let mut born = Born::default();
    let ours = born.transcript(&transcripts(&data, &project), "ours", 5, 5);
    born.transcript(&transcripts(&home.join(".cursor"), &project), "home", 9, 9);
    assert_eq!(born.locate(&ctx), Ok(ours));
}

/// A conversation directory without its `<id>.jsonl`, a subagent's transcript
/// (`<id>/subagents/…`) and loose files are passed over.
#[test]
fn only_conversation_transcripts_are_located() {
    let (_tmp, project, home) = dirs();
    let dir = transcripts(&home.join(".cursor"), &project);
    let ctx = launched(&project, &home, &[]);
    let mut born = Born::default();
    let ours = born.transcript(&dir, "ours", 5, 5);
    std::fs::create_dir_all(dir.join("empty")).unwrap();
    for (path, secs) in [
        ("other/notes.jsonl", 9),
        ("ours/subagents/sub.jsonl", 9),
        ("loose.jsonl", 9),
    ] {
        write_at(&dir.join(path), "{}\n", at(secs));
        born.0.insert(dir.join(path), at(secs));
    }
    assert_eq!(born.locate(&ctx), Ok(ours));
}

#[test]
fn without_a_transcript_a_launch_or_a_root_nothing_is_located() {
    let (_tmp, project, home) = dirs();
    let ctx = launched(&project, &home, &[]);
    let born = Born::default();
    assert_eq!(born.locate(&ctx), Err(AmbientUnavailable::NotFound));
    std::fs::create_dir_all(transcripts(&home.join(".cursor"), &project)).unwrap();
    assert_eq!(born.locate(&ctx), Err(AmbientUnavailable::NotFound));
    let mut unlaunched = ctx.clone();
    unlaunched.launched_at = None;
    assert_eq!(
        born.locate(&unlaunched),
        Err(AmbientUnavailable::AgentUnknown)
    );
    let env = crate::env::EnvPolicy {
        inherit: false,
        ..crate::env::EnvPolicy::default()
    };
    assert_eq!(
        born.locate(&AmbientContext::new(&project, env)),
        Err(AmbientUnavailable::NoConfigRoot)
    );
}

/// A relative `CURSOR_DATA_DIR` is taken against the agent's cwd; the path is absolute.
#[test]
fn a_relative_data_dir_is_taken_against_the_agents_cwd() {
    let (_tmp, project, home) = dirs();
    let ctx = launched(
        &project,
        &home,
        &[("CURSOR_DATA_DIR", Path::new("rel-data"))],
    );
    let mut born = Born::default();
    let ours = born.transcript(
        &transcripts(&project.join("rel-data"), &project),
        "ours",
        5,
        5,
    );
    let path = born.locate(&ctx).unwrap();
    assert!(path.is_absolute(), "{}", path.display());
    assert_eq!(path, ours);
}

/// cursor names the project directory after its resolved cwd: a project reached through a
/// symlink is found.
#[cfg(unix)]
#[test]
fn the_project_is_the_canonical_path() {
    let (tmp, project, home) = dirs();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&project, &link).unwrap();
    let mut born = Born::default();
    let ours = born.transcript(&transcripts(&home.join(".cursor"), &project), "ours", 5, 5);
    assert_eq!(born.locate(&launched(&link, &home, &[])), Ok(ours));
}

/// The profile's source reads the file system's own creation times: a transcript created
/// now is created since a launch a moment ago.
#[test]
fn the_source_reads_real_creation_times() {
    let (_tmp, project, home) = dirs();
    let mut ctx = ctx(&project, &home, &[]);
    ctx.launched_at = Some(SystemTime::now() - std::time::Duration::from_secs(60));
    let dir = transcripts(&home.join(".cursor"), &project);
    let path = dir.join("now").join("now.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, user()).unwrap();
    assert_eq!(completion().locate(&ctx), Ok(path));
}
