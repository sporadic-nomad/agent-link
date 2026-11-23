use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;
use crate::agent::profiles::ambient::testing::{
    Seen, at, canonical, ctx, dirs, feed, feed_quiet, fixture, line, stamp, start, write_at,
};

// --- turns ------------------------------------------------------------------------------

fn pi() -> PiAmbient {
    PiAmbient::default()
}

/// The quiet window the observer uses by default.
const QUIET_MS: i64 = 3_000;

/// Milliseconds since the epoch of a UTC timestamp as pi writes it
/// (`2026-10-11T20:05:24.050Z`).
fn iso_ms(value: &Value) -> i64 {
    let text = value.as_str().expect("a timestamp");
    let num = |range: std::ops::Range<usize>| -> i64 { text[range].parse().expect("a number") };
    let (year, month, day) = (num(0..4), num(5..7), num(8..10));
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + num(11..13) * 3_600 + num(14..16) * 60 + num(17..19);
    secs * 1_000 + num(20..23)
}

fn value(line: &str) -> Value {
    serde_json::from_str(line).unwrap()
}

/// The lines fed as the observer would: the transcript is quiet after a line when the
/// next one was written at least the quiet window later (by the captured times).
fn feed_timed(lines: &[String]) -> Vec<Seen> {
    feed_quiet(&pi(), lines, |i| {
        let next = value(&lines[i + 1])["timestamp"].clone();
        iso_ms(&next) - iso_ms(&value(&lines[i])["timestamp"]) >= QUIET_MS
    })
}

fn message(role: &str, extra: Value) -> String {
    let mut message = json!({"role": role});
    message
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    line(json!({"type": "message", "id": "0000abcd", "message": message}))
}

fn assistant(stop: &str, content: Value) -> String {
    message("assistant", json!({"content": content, "stopReason": stop}))
}

fn quiet(text: &str) -> Seen {
    Seen::Quiet(Some(text.into()))
}

fn before(text: &str) -> Seen {
    Seen::Before(Some(text.into()))
}

/// The captured session (0.85.1), quiet where the capture paused: each final message is
/// reported once the file stayed quiet; a turn interrupted with Esc (`aborted`) ends with
/// no reply.
#[test]
fn the_captured_session_reports_each_final_message_once_quiet() {
    assert_eq!(
        feed_timed(&fixture("ambient/pi-0.85.1/turns.jsonl")),
        [
            quiet("first answer"),
            quiet("after tool"),
            Seen::Quiet(None)
        ]
    );
    assert_eq!(
        feed_timed(&fixture("ambient/pi-0.85.1/error.jsonl")),
        [Seen::Quiet(None)],
        "a failed request (401) ends its turn without a reply"
    );
}

/// The next prompt ends the turn before it, even within the quiet window.
#[test]
fn the_next_prompt_ends_the_turn_before_it() {
    assert_eq!(
        feed(&pi(), &fixture("ambient/pi-0.85.1/turns.jsonl")),
        [
            before("first answer"),
            before("after tool"),
            Seen::Quiet(None)
        ]
    );
    // The controlled tier's capture (1.5.2): two prompts, each answered with `stop`.
    assert_eq!(
        feed(&pi(), &fixture("pi-1.5.2/turns.jsonl")),
        [before("pi-proposal-reply-1"), quiet("pi-proposal-reply-2")]
    );
}

#[test]
fn text_blocks_are_joined_and_thinking_never_counts() {
    let mut parser = pi().parser();
    let line = assistant(
        "stop",
        json!([
            {"type": "thinking", "thinking": "Two words.", "text": "Two words."},
            {"type": "text", "text": "one"}, {"type": "text", "text": "  "},
            {"type": "text", "text": "two"}
        ]),
    );
    assert!(parser.record(line.as_bytes(), 1024).is_none());
    let Some(AmbientRecord::Ended(Some(reply))) = parser.quiet(1024) else {
        panic!("an end once quiet");
    };
    assert_eq!(reply.text, "one\n\ntwo");
    assert!(parser.quiet(1024).is_none(), "reported once");
}

#[test]
fn quiet_without_a_final_message_ends_nothing() {
    let lines = [
        message("user", json!({"content": [{"type": "text", "text": "hi"}]})),
        assistant(
            "toolUse",
            json!([{"type": "toolCall", "id": "c", "name": "bash"}]),
        ),
    ];
    assert_eq!(feed(&pi(), &lines), []);
    assert_eq!(feed(&pi(), &[]), []);
}

/// A `stop` message with a tool call is not final; a tool result after a final message
/// (pi went on) takes it back; `length` is final.
#[test]
fn only_a_final_message_without_tool_calls_or_later_messages_ends_a_turn() {
    let text = json!([{"type": "text", "text": "early"}]);
    let with_call = json!([{"type": "text", "text": "early"}, {"type": "toolCall", "id": "c"}]);
    assert_eq!(feed(&pi(), &[assistant("stop", with_call)]), []);
    let lines = [
        assistant("stop", text.clone()),
        message("toolResult", json!({"content": [], "toolCallId": "c"})),
    ];
    assert_eq!(feed(&pi(), &lines), []);
    assert_eq!(feed(&pi(), &[assistant("length", text)]), [quiet("early")]);
}

/// Entries that are no message (a model change, compaction, an extension's) and unknown
/// fields leave the final message final.
#[test]
fn other_entries_keep_the_final_message() {
    let lines = [
        assistant(
            "stop",
            json!([{"type": "text", "text": "kept", "newField": 1}]),
        ),
        line(json!({"type": "model_change", "provider": "p", "modelId": "m"})),
        line(json!({"type": "compaction", "summary": "s"})),
        line(json!({"type": "custom", "customType": "x"})),
        line(json!({"type": "label", "message": "not a message entry"})),
        line(json!({"type": "something_new"})),
    ];
    assert_eq!(feed(&pi(), &lines), [quiet("kept")]);
}

/// A `!` command ends the turn before it too; an error or abort ends with no reply.
#[test]
fn a_shell_command_or_a_failed_message_ends_the_turn() {
    let lines = [
        assistant("stop", json!([{"type": "text", "text": "answer"}])),
        message("bashExecution", json!({"command": "ls"})),
        assistant("error", json!([])),
        message(
            "user",
            json!({"content": [{"type": "text", "text": "again"}]}),
        ),
    ];
    assert_eq!(feed(&pi(), &lines), [before("answer"), Seen::Before(None)]);
}

#[test]
fn a_known_record_without_its_fields_is_unrecognised() {
    let lines = [
        message("assistant", json!({"content": []})),
        message("assistant", json!({"stopReason": "stop"})),
        message(
            "assistant",
            json!({"content": "text", "stopReason": "stop"}),
        ),
        line(json!({"type": "message", "id": "x"})),
        line(json!({"type": "message", "message": {"role": 1}})),
        assistant("stop", json!([{"type": "text", "text": "answer"}])),
    ];
    assert_eq!(
        feed(&pi(), &lines),
        [
            Seen::Unrecognised,
            Seen::Unrecognised,
            Seen::Unrecognised,
            Seen::Unrecognised,
            Seen::Unrecognised,
            quiet("answer")
        ]
    );
}

// --- lookup -----------------------------------------------------------------------------

fn os(text: &str) -> Option<OsString> {
    Some(OsString::from(text))
}

/// pi's directory for `project`, by its rule.
fn pi_dir(project: &Path) -> String {
    let cwd = canonical(project).to_str().unwrap().to_string();
    let cwd = cwd.strip_prefix(['/', '\\']).unwrap_or(&cwd);
    format!("--{}--", cwd.replace(['/', '\\', ':'], "-"))
}

/// A session file `name` in `dir` as pi writes one — its header (`cwd`), then a message —
/// last written at `start() + secs`.
fn session(dir: &Path, name: &str, cwd: &Path, secs: i64) -> PathBuf {
    let path = dir.join(name);
    let header = json!({
        "type": "session", "version": 3, "id": name, "timestamp": "2026-10-11T20:05:23.116Z",
        "cwd": cwd,
    });
    let message = r#"{"type":"message","id":"00bebc20","parentId":null,"message":{"role":"user","content":[]}}"#;
    write_at(&path, &format!("{header}\n{message}\n"), at(secs));
    path
}

fn launched(project: &Path, home: &Path, vars: &[(&str, &Path)]) -> AmbientContext {
    let mut ctx = ctx(project, home, vars);
    ctx.launched_at = Some(start());
    ctx
}

fn default_dir(home: &Path, project: &Path) -> PathBuf {
    home.join(".pi/agent/sessions").join(pi_dir(project))
}

#[test]
fn the_directory_name_drops_the_leading_separator_and_dashes_the_rest() {
    assert_eq!(
        pi_dir_name(Path::new("/Users/me/proj")),
        "--Users-me-proj--"
    );
    assert_eq!(
        pi_dir_name(Path::new(r"C:\Users\me\proj")),
        "--C--Users-me-proj--"
    );
}

#[test]
fn the_sessions_directory_default_and_overrides() {
    let project = Path::new("/Users/me/proj");
    let home = || Some(PathBuf::from("/home/me"));
    let default = Some(PathBuf::from(
        "/home/me/.pi/agent/sessions/--Users-me-proj--",
    ));
    assert_eq!(sessions_dir(None, None, home(), project), default);
    assert_eq!(
        sessions_dir(None, os("/a"), home(), project),
        Some(PathBuf::from("/a/sessions/--Users-me-proj--"))
    );
    // The session directory wins, and holds every project's sessions directly.
    assert_eq!(
        sessions_dir(os("/s"), os("/a"), home(), project),
        Some(PathBuf::from("/s"))
    );
    assert_eq!(sessions_dir(os(""), os(""), home(), project), default);
    assert_eq!(
        sessions_dir(os("~/s"), None, home(), project),
        Some(PathBuf::from("/home/me/s"))
    );
    assert_eq!(
        sessions_dir(None, os("~/a"), home(), project),
        Some(PathBuf::from("/home/me/a/sessions/--Users-me-proj--"))
    );
    assert_eq!(
        sessions_dir(os("~"), None, home(), project),
        Some(PathBuf::from("/home/me"))
    );
    assert_eq!(sessions_dir(None, None, None, project), None);
}

/// Path building only (Windows is not run here): a Windows home and project.
#[test]
fn the_sessions_directory_of_a_windows_home() {
    let dir = sessions_dir(
        None,
        None,
        Some(PathBuf::from(r"C:\Users\me")),
        Path::new(r"C:\Users\me\proj"),
    )
    .unwrap();
    assert_eq!(
        dir.to_str().unwrap().replace('/', "\\"),
        r"C:\Users\me\.pi\agent\sessions\--C--Users-me-proj--"
    );
}

/// The session written last since the launch: a newer one (`/new`) is followed, `/resume`
/// back to an earlier one too, one not written since the launch never.
#[test]
fn the_session_written_last_since_the_launch_is_located() {
    let (_tmp, project, home) = dirs();
    let dir = default_dir(&home, &project);
    let cwd = canonical(&project);
    let ctx = launched(&project, &home, &[]);
    let before = session(&dir, "before.jsonl", &cwd, -10);
    assert_eq!(pi().locate(&ctx), Err(AmbientUnavailable::NotFound));
    let a = session(&dir, "a.jsonl", &cwd, 5);
    assert_eq!(pi().locate(&ctx), Ok(a.clone()));
    let b = session(&dir, "b.jsonl", &cwd, 10);
    assert_eq!(pi().locate(&ctx), Ok(b.clone()));
    stamp(&a, at(20));
    assert_eq!(pi().locate(&ctx), Ok(a));
    stamp(&b, at(30));
    assert_eq!(pi().locate(&ctx), Ok(b));
    // `/resume` of a session from before the launch: written since, it is followed (the
    // observer reads it from its size when it first listed the directory).
    stamp(&before, at(40));
    assert_eq!(pi().locate(&ctx), Ok(before));
}

#[cfg(unix)]
#[test]
fn a_symlinked_session_becomes_eligible_when_its_target_is_written() {
    let (tmp, project, home) = dirs();
    let ctx = launched(&project, &home, &[]);
    let target = session(tmp.path(), "session-data", &project, -10);
    let dir = default_dir(&home, &project);
    std::fs::create_dir_all(&dir).unwrap();
    let link = dir.join("linked.jsonl");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_eq!(pi().locate(&ctx), Err(AmbientUnavailable::NotFound));
    stamp(&target, at(5));
    assert_eq!(pi().locate(&ctx), Ok(link));
}

/// `PI_CODING_AGENT_SESSION_DIR` holds every project's sessions in one directory: they are
/// told apart by the header's `cwd`. It wins over `PI_CODING_AGENT_DIR`.
#[test]
fn the_session_dir_holds_every_projects_sessions_directly() {
    let (tmp, project, home) = dirs();
    let (s, a) = (tmp.path().join("s"), tmp.path().join("a"));
    let ctx = launched(
        &project,
        &home,
        &[
            ("PI_CODING_AGENT_SESSION_DIR", &s),
            ("PI_CODING_AGENT_DIR", &a),
        ],
    );
    let ours = session(&s, "ours.jsonl", &canonical(&project), 5);
    session(&s, "other.jsonl", Path::new("/elsewhere/project"), 10);
    session(
        &a.join("sessions").join(pi_dir(&project)),
        "agent-dir.jsonl",
        &canonical(&project),
        20,
    );
    assert_eq!(pi().locate(&ctx), Ok(ours));
}

#[test]
fn the_agent_dir_holds_the_sessions_directory() {
    let (tmp, project, home) = dirs();
    let a = tmp.path().join("a");
    let ctx = launched(&project, &home, &[("PI_CODING_AGENT_DIR", &a)]);
    let ours = session(
        &a.join("sessions").join(pi_dir(&project)),
        "ours.jsonl",
        &canonical(&project),
        5,
    );
    session(
        &default_dir(&home, &project),
        "home.jsonl",
        &canonical(&project),
        10,
    );
    assert_eq!(pi().locate(&ctx), Ok(ours));
}

/// pi takes a relative directory against its cwd: so is it here, and the path stays
/// absolute.
#[test]
fn relative_overrides_are_taken_against_the_agents_cwd() {
    let (_tmp, project, home) = dirs();
    let vars = [("PI_CODING_AGENT_SESSION_DIR", Path::new("rel-sessions"))];
    let ours = session(
        &project.join("rel-sessions"),
        "ours.jsonl",
        &canonical(&project),
        5,
    );
    let path = pi().locate(&launched(&project, &home, &vars)).unwrap();
    assert!(path.is_absolute(), "{}", path.display());
    assert_eq!(path, ours);
    let vars = [("PI_CODING_AGENT_DIR", Path::new("rel-agent"))];
    let ours = session(
        &project.join("rel-agent/sessions").join(pi_dir(&project)),
        "ours.jsonl",
        &canonical(&project),
        5,
    );
    assert_eq!(pi().locate(&launched(&project, &home, &vars)), Ok(ours));
}

#[test]
fn without_a_session_a_launch_or_a_root_nothing_is_located() {
    let (_tmp, project, home) = dirs();
    let ctx = launched(&project, &home, &[]);
    assert_eq!(pi().locate(&ctx), Err(AmbientUnavailable::NotFound));
    std::fs::create_dir_all(default_dir(&home, &project)).unwrap();
    assert_eq!(pi().locate(&ctx), Err(AmbientUnavailable::NotFound));
    session(
        &default_dir(&home, &project),
        "s.jsonl",
        &canonical(&project),
        5,
    );
    let mut unlaunched = ctx.clone();
    unlaunched.launched_at = None;
    assert_eq!(
        pi().locate(&unlaunched),
        Err(AmbientUnavailable::AgentUnknown)
    );
    let env = crate::env::EnvPolicy {
        inherit: false,
        ..crate::env::EnvPolicy::default()
    };
    assert_eq!(
        pi().locate(&AmbientContext::new(&project, env)),
        Err(AmbientUnavailable::NoConfigRoot)
    );
}

/// A file whose first line is no complete session header (being written, or no pi
/// session) is passed over, and so are other files.
#[test]
fn files_it_cannot_tell_are_passed_over() {
    let (_tmp, project, home) = dirs();
    let dir = default_dir(&home, &project);
    let ours = session(&dir, "ours.jsonl", &canonical(&project), 5);
    write_at(
        &dir.join("partial.jsonl"),
        "{\"type\":\"session\",\"cwd\":",
        at(9),
    );
    write_at(
        &dir.join("foreign.jsonl"),
        "{\"type\":\"message\"}\n",
        at(9),
    );
    write_at(&dir.join("notes.txt"), "{}\n", at(9));
    std::fs::create_dir_all(dir.join("dir.jsonl")).unwrap();
    assert_eq!(pi().locate(&launched(&project, &home, &[])), Ok(ours));
}

/// pi names the directory after its resolved cwd and records that cwd: a project reached
/// through a symlink is found.
#[cfg(unix)]
#[test]
fn the_project_is_the_canonical_path() {
    let (tmp, project, home) = dirs();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&project, &link).unwrap();
    let ours = session(
        &default_dir(&home, &project),
        "ours.jsonl",
        &canonical(&project),
        5,
    );
    assert_eq!(pi().locate(&launched(&link, &home, &[])), Ok(ours));
}

#[test]
fn replacing_a_cached_session_rechecks_its_project() {
    let (_tmp, project, home) = dirs();
    let ctx = launched(&project, &home, &[]);
    let dir = default_dir(&home, &project);
    let ours = session(&dir, "ours.jsonl", &canonical(&project), 5);
    let source = pi();
    assert_eq!(source.locate(&ctx), Ok(ours.clone()));
    #[cfg(unix)]
    let other = PathBuf::from(format!("{}\\", canonical(&project).display()));
    #[cfg(windows)]
    let other = project.with_file_name("other-project");
    std::fs::create_dir(&other).unwrap();
    let replacement = session(&dir, "other.jsonl", &other, 5);
    std::fs::rename(replacement, &ours).unwrap();
    assert!(
        !source
            .accepts_file(&ctx, &std::fs::File::open(&ours).unwrap())
            .unwrap()
    );
    assert_eq!(source.locate(&ctx), Err(AmbientUnavailable::NotFound));
}

#[test]
fn a_tool_call_after_the_block_limit_keeps_the_turn_open() {
    let mut blocks = vec![json!({"type":"text","text":"narration"}); super::super::json::MAX_ITEMS];
    blocks.push(json!({"type":"toolCall","id":"tool"}));
    let lines = [
        assistant("stop", blocks.into()),
        message("user", json!({"content":[]})),
    ];
    assert!(feed_quiet(&pi(), &lines, |_| true).is_empty());
}
