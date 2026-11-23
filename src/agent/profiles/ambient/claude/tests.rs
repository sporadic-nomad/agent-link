use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;
use crate::agent::profiles::ambient::testing::{
    Seen, canonical, ctx, dirs, ended, feed, fixture, line, start,
};

// --- turns ------------------------------------------------------------------------------

fn claude() -> ClaudeAmbient {
    ClaudeAmbient
}

fn assistant(id: &str, stop: Option<&str>, content: Value) -> String {
    line(
        json!({"type": "assistant", "message": {"id": id, "stop_reason": stop, "content": content}}),
    )
}

fn text(text: &str) -> Value {
    json!([{"type": "text", "text": text}])
}

fn tool_use() -> Value {
    json!([{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}}])
}

fn prompt(text: &str) -> String {
    line(json!({"type": "user", "message": {"role": "user", "content": text}}))
}

fn tool_result() -> String {
    line(
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1"}]}}),
    )
}

fn turn_end() -> String {
    line(json!({"type": "system", "subtype": "turn_duration", "durationMs": 5}))
}

/// The captured turns (2.1.296): a plain answer, an answer after a tool call, and a turn
/// of thinking and a tool call only (its final message removed from the capture).
#[test]
fn the_captured_turns_report_each_final_message() {
    assert_eq!(
        feed(&claude(), &fixture("ambient/claude-2.1.296/turns.jsonl")),
        [
            ended("first answer"),
            ended("after tool"),
            Seen::Ended(None)
        ]
    );
}

/// The controlled tier's paired capture of the same version: two text fragments of one
/// message are joined; Esc interrupted a turn mid-stream (`isAbortedMidStream`, then a
/// `user` line with `interruptedMessageId` and no `turn_duration`).
#[test]
fn the_controlled_capture_reads_the_same_with_its_interruption() {
    let dir = "claude-2.1.296";
    assert_eq!(
        feed(&claude(), &fixture(&format!("{dir}/session-a.jsonl"))),
        [
            ended("Copper pebble"),
            Seen::Ended(None),
            ended("Amber petal\n\nSecond petal")
        ]
    );
    assert_eq!(
        feed(&claude(), &fixture(&format!("{dir}/session-b.jsonl"))),
        [ended("Violet acorn"), ended("Silver pumice")]
    );
    // After `/clear`: meta lines, the local command and its record end nothing.
    assert_eq!(
        feed(&claude(), &fixture(&format!("{dir}/after-clear.jsonl"))),
        [ended("Lime button")]
    );
    // A second CLI resumed the live session: both terminals' turns are in one file, and
    // nothing tells them apart (no attribution).
    assert_eq!(
        feed(&claude(), &fixture(&format!("{dir}/overlap.jsonl"))),
        [
            ended("Copper pebble"),
            ended("Violet acorn"),
            ended("Lime button")
        ]
    );
}

#[test]
fn a_message_split_over_lines_is_joined() {
    let lines = [
        assistant("m1", None, text("one")),
        assistant("m1", Some("end_turn"), text("two")),
        turn_end(),
    ];
    assert_eq!(feed(&claude(), &lines), [ended("one\n\ntwo")]);
}

/// Only the turn's latest message can be the reply, and only when it stopped with
/// `end_turn`: an earlier answer does not stand in for a later message cut short, or for
/// one a tool result followed.
#[test]
fn only_the_latest_message_can_be_the_reply() {
    let lines = [
        assistant("m1", Some("end_turn"), text("first")),
        assistant("m2", Some("end_turn"), text("second")),
        turn_end(),
    ];
    assert_eq!(feed(&claude(), &lines), [ended("second")]);
    let lines = [
        assistant("m1", Some("end_turn"), text("first")),
        assistant("m2", Some("max_tokens"), text("cut")),
        turn_end(),
    ];
    assert_eq!(feed(&claude(), &lines), [Seen::Ended(None)]);
    let lines = [
        assistant("m1", Some("end_turn"), text("answer")),
        assistant("m2", Some("tool_use"), tool_use()),
        tool_result(),
        line(json!({"type": "user", "isMeta": true, "message": {"content": "caveat"}})),
        turn_end(),
    ];
    assert_eq!(feed(&claude(), &lines), [Seen::Ended(None)]);
}

/// Each turn starts afresh: a turn without an `end_turn` message has no reply, not the
/// previous turn's.
#[test]
fn a_turn_without_an_end_turn_message_has_no_reply() {
    let lines = [
        assistant("m1", Some("end_turn"), text("first")),
        turn_end(),
        assistant("m2", Some("tool_use"), tool_use()),
        turn_end(),
    ];
    assert_eq!(feed(&claude(), &lines), [ended("first"), Seen::Ended(None)]);
}

/// A prompt drops what came before it: an interrupted turn of an older build (no
/// `interruptedMessageId`) never answers the next one.
#[test]
fn a_prompt_starts_the_turn_afresh() {
    let lines = [
        assistant("m1", Some("end_turn"), text("stale")),
        assistant("m2", Some("tool_use"), tool_use()),
        prompt("[Request interrupted by user]"),
        prompt("next"),
        assistant("m3", Some("tool_use"), tool_use()),
        tool_result(),
        turn_end(),
    ];
    assert_eq!(feed(&claude(), &lines), [Seen::Ended(None)]);
}

/// An interruption record ends the open turn once; with no turn open it ends nothing.
#[test]
fn an_interruption_ends_an_open_turn_once() {
    let interrupted = line(json!({"type": "user", "interruptedMessageId": "m1",
        "message": {"content": [{"type": "text", "text": "[Request interrupted by user]"}]}}));
    let lines = [
        prompt("go"),
        interrupted.clone(),
        interrupted.clone(),
        prompt("again"),
        assistant("m2", Some("end_turn"), text("done")),
        turn_end(),
    ];
    assert_eq!(feed(&claude(), &lines), [Seen::Ended(None), ended("done")]);
    assert_eq!(feed(&claude(), &[interrupted]), []);
}

/// Thinking and narration before a tool call never contribute, even a thinking block that
/// carries a `text` value.
#[test]
fn thinking_and_narration_never_contribute() {
    let mut lines = fixture("ambient/claude-2.1.296/turns.jsonl");
    let tool = position(&lines, |l| l["message"]["stop_reason"] == "tool_use");
    let mut narrated: Value = serde_json::from_str(&lines[tool]).unwrap();
    let content = narrated["message"]["content"].as_array_mut().unwrap();
    content.insert(0, json!({"type": "text", "text": "Let me list the files."}));
    lines[tool] = narrated.to_string();
    let last = position(&lines, |l| l["message"]["stop_reason"] == "end_turn");
    let mut thought: Value = serde_json::from_str(&lines[last]).unwrap();
    let content = thought["message"]["content"].as_array_mut().unwrap();
    content.insert(
        0,
        json!({"type": "thinking", "thinking": "Two words.", "text": "Two words.", "signature": "c2ln"}),
    );
    lines[last] = thought.to_string();
    assert_eq!(
        feed(&claude(), &lines),
        [
            ended("first answer"),
            ended("after tool"),
            Seen::Ended(None)
        ]
    );
}

/// A subagent's (`isSidechain`) message never counts. An API error message written after
/// the turn's answer replaces it: the turn ends without a reply.
#[test]
fn sidechain_lines_never_count_and_an_api_error_leaves_no_reply() {
    let first_turn = |extra: Value| {
        let mut lines = fixture("ambient/claude-2.1.296/turns.jsonl");
        let end = position(&lines, |l| l["subtype"] == "turn_duration");
        lines.truncate(end + 1);
        let last = position(&lines, |l| l["message"]["stop_reason"] == "end_turn");
        let mut added: Value = serde_json::from_str(&lines[last]).unwrap();
        for (key, value) in extra.as_object().unwrap() {
            added[key] = value.clone();
        }
        added["message"]["id"] = json!("msg_other");
        added["message"]["content"] = text("not the answer");
        lines.insert(end, added.to_string());
        lines
    };
    assert_eq!(
        feed(&claude(), &first_turn(json!({"isSidechain": true}))),
        [ended("first answer")]
    );
    assert_eq!(
        feed(&claude(), &first_turn(json!({"isApiErrorMessage": true}))),
        [Seen::Ended(None)]
    );
    assert_eq!(
        feed(&claude(), &first_turn(json!({"isMeta": true}))),
        [ended("first answer")]
    );
}

/// A known record without the fields it is read for is reported, and reading goes on.
#[test]
fn a_known_record_without_its_fields_is_unrecognised() {
    let lines = [
        line(json!({"type": "assistant", "message": {"id": "m0"}})),
        line(json!({"type": "assistant", "message": {"id": "m0", "content": "text"}})),
        line(json!({"type": "assistant"})),
        line(json!({"type": "system"})),
        line(json!({"type": "system", "subtype": 3})),
        assistant("m1", Some("end_turn"), text("answer")),
        turn_end(),
    ];
    assert_eq!(
        feed(&claude(), &lines),
        [
            Seen::Unrecognised,
            Seen::Unrecognised,
            Seen::Unrecognised,
            Seen::Unrecognised,
            Seen::Unrecognised,
            ended("answer")
        ]
    );
}

#[test]
fn an_unreadable_assistant_message_retires_the_previous_reply() {
    for message in [json!({"id": "m2"}), json!({"id": "m2", "content": "text"})] {
        let lines = [
            assistant("m1", Some("end_turn"), text("stale")),
            line(json!({"type": "assistant", "message": message})),
            turn_end(),
        ];
        assert_eq!(
            feed(&claude(), &lines),
            [Seen::Unrecognised, Seen::Ended(None)]
        );
    }
    let lines = [
        assistant("m1", Some("end_turn"), text("answer")),
        line(json!({"type": "assistant", "isMeta": true})),
        turn_end(),
    ];
    assert_eq!(
        feed(&claude(), &lines),
        [Seen::Unrecognised, ended("answer")]
    );
}

/// Unknown kinds, fields and block types change nothing.
#[test]
fn unknown_kinds_fields_and_blocks_are_skipped() {
    let lines = [
        line(json!({"type": "progress", "data": {"anything": [1, 2]}})),
        line(json!({"type": "mode", "mode": "normal"})),
        line(
            json!({"type": "user", "newField": {"x": 1}, "message": {"content": [{"type": "image"}]}}),
        ),
        line(
            json!({"type": "assistant", "newField": true, "message": {"id": "m1",
            "stop_reason": "end_turn", "content": [
                {"type": "server_tool_use", "input": {}}, "odd", 7,
                {"type": "text", "text": "kept", "citations": [{"url": "u"}]}
            ]}}),
        ),
        line(json!({"type": "system", "subtype": "stop_hook_summary", "hookCount": 1})),
        line(json!({"type": "system", "subtype": "turn_duration", "durationMs": "not a number"})),
    ];
    assert_eq!(feed(&claude(), &lines), [ended("kept")]);
}

/// The reply keeps at most the budget, cut at a UTF-8 boundary.
#[test]
fn a_long_reply_is_cut_to_the_budget() {
    let mut parser = claude().parser();
    let _ = parser.record(assistant("m1", Some("end_turn"), text("ééé")).as_bytes(), 3);
    let Some(AmbientRecord::Ended(Some(reply))) = parser.record(turn_end().as_bytes(), 3) else {
        panic!("an explicit end");
    };
    assert_eq!(reply.text, "é");
    assert!(reply.truncated);
}

fn position(lines: &[String], pick: impl Fn(&Value) -> bool) -> usize {
    lines
        .iter()
        .position(|l| pick(&serde_json::from_str(l).unwrap()))
        .expect("a matching line")
}

// --- lookup -----------------------------------------------------------------------------

/// claude's project directory name, by its rule: every char outside `[A-Za-z0-9]`
/// becomes `-`.
fn claude_dir_name(path: &Path) -> String {
    path.to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn transcript_of(root: &Path, project: &Path, session: &str) -> PathBuf {
    root.join("projects")
        .join(claude_dir_name(&canonical(project)))
        .join(format!("{session}.jsonl"))
}

fn write_json(path: &Path, value: Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, value.to_string()).unwrap();
}

/// A pid no process has (above every platform's pid range).
const DEAD_PID: u32 = 999_999_999;

/// A pid file of a live claude (this test process) for `cwd`, as 2.1.296 writes one:
/// `entrypoint` `cli` for the TUI, `sdk-cli` for print and stream-json mode; started `secs`
/// after [`start`].
fn live_entry(id: &str, cwd: &str, entrypoint: &str, secs: i64) -> Value {
    let started = 1_700_000_000_000_i64 + secs * 1000;
    json!({
        "pid": std::process::id(), "sessionId": id, "cwd": cwd, "startedAt": started,
        "kind": "interactive", "entrypoint": entrypoint,
    })
}

fn launched(project: &Path, home: &Path) -> AmbientContext {
    let mut ctx = ctx(project, home, &[]);
    ctx.launched_at = Some(start());
    ctx
}

#[test]
fn the_session_comes_from_the_agents_pid_file() {
    let (_tmp, project, home) = dirs();
    let root = home.join(".claude");
    write_json(&root.join("sessions/4242.json"), json!({"sessionId": "S"}));
    let mut ctx = ctx(&project, &home, &[]);
    ctx.program = Some(4242);
    assert_eq!(
        claude().locate(&ctx),
        Ok(transcript_of(&root, &project, "S"))
    );
}

#[test]
fn the_root_is_claude_config_dir_when_set() {
    let (tmp, project, home) = dirs();
    let root = tmp.path().join("claude-config");
    write_json(&root.join("sessions/4242.json"), json!({"sessionId": "S"}));
    // A pid file under the home's `.claude` too: not the one read.
    write_json(
        &home.join(".claude/sessions/4242.json"),
        json!({"sessionId": "H"}),
    );
    let mut ctx = ctx(&project, &home, &[("CLAUDE_CONFIG_DIR", &root)]);
    ctx.program = Some(4242);
    assert_eq!(
        claude().locate(&ctx),
        Ok(transcript_of(&root, &project, "S"))
    );
}

#[test]
fn without_a_pid_file_the_live_session_of_the_project_is_used() {
    let (tmp, project, home) = dirs();
    let root = home.join(".claude");
    let sessions = root.join("sessions");
    let cwd = canonical(&project).to_str().unwrap().to_string();
    // Each excluded by one thing only: a dead pid, another cwd.
    let mut dead = live_entry("DEAD", &cwd, "cli", 20);
    dead["pid"] = json!(DEAD_PID);
    write_json(&sessions.join("1.json"), dead);
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let other = canonical(&other).to_str().unwrap().to_string();
    write_json(
        &sessions.join("2.json"),
        live_entry("OTHER", &other, "cli", 30),
    );
    write_json(
        &sessions.join("3.json"),
        live_entry("LIVE", &cwd, "cli", 10),
    );
    std::fs::write(sessions.join("notes.txt"), "{}").unwrap();
    let expected = transcript_of(&root, &project, "LIVE");
    // The agent's program not known yet (launched, not seen in front yet).
    let mut ctx = launched(&project, &home);
    assert_eq!(claude().locate(&ctx), Ok(expected.clone()));
    // Known, without a pid file of its own (a wrapper leads the job).
    ctx.program = Some(4243);
    assert_eq!(claude().locate(&ctx), Ok(expected));
}

#[test]
fn without_a_session_nothing_is_located() {
    let (_tmp, project, home) = dirs();
    let mut ctx = launched(&project, &home);
    ctx.program = Some(4242);
    assert_eq!(claude().locate(&ctx), Err(AmbientUnavailable::NotFound));
    let cwd = canonical(&project).to_str().unwrap().to_string();
    let mut dead = live_entry("DEAD", &cwd, "cli", 20);
    dead["pid"] = json!(DEAD_PID);
    write_json(&home.join(".claude/sessions/1.json"), dead);
    assert_eq!(claude().locate(&ctx), Err(AmbientUnavailable::NotFound));
}

#[test]
fn an_incomplete_fallback_scan_reports_its_read_limit() {
    let (_tmp, project, home) = dirs();
    let root = home.join(".claude");
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let cwd = canonical(&project);
    let live = live_entry("LIVE", cwd.to_str().unwrap(), "cli", 10);
    let mut bytes = serde_json::to_vec(&live).unwrap();
    bytes.resize(SESSION_FILE_CAP as usize, b' ');
    let last = super::super::READ_BUDGET / SESSION_FILE_CAP;
    for n in 0..=last {
        std::fs::write(sessions.join(format!("{n}.json")), &bytes).unwrap();
    }
    let mut ctx = launched(&project, &home);
    ctx.program = Some(4242); // The launched program has no PID file.
    assert_eq!(
        claude().locate(&ctx),
        Err(AmbientUnavailable::ReadBudgetExceeded)
    );
    std::fs::remove_file(sessions.join(format!("{last}.json"))).unwrap();
    assert_eq!(
        claude().locate(&ctx),
        Ok(transcript_of(&root, &project, "LIVE"))
    );
}

#[test]
fn without_a_home_or_config_dir_there_is_no_root() {
    let (_tmp, project, _home) = dirs();
    let env = crate::env::EnvPolicy {
        inherit: false,
        ..crate::env::EnvPolicy::default()
    };
    assert_eq!(
        claude().locate(&AmbientContext::new(&project, env)),
        Err(AmbientUnavailable::NoConfigRoot)
    );
}

#[test]
fn an_unreadable_pid_file_falls_back_to_the_live_session() {
    let (_tmp, project, home) = dirs();
    let sessions = home.join(".claude/sessions");
    let cwd = canonical(&project);
    write_json(
        &sessions.join("9.json"),
        live_entry("LIVE", cwd.to_str().unwrap(), "cli", 10),
    );
    let mut ctx = launched(&project, &home);
    ctx.program = Some(4242);
    let expected = transcript_of(&home.join(".claude"), &project, "LIVE");
    let long = format!("{{\"sessionId\":\"X\",\"pad\":\"{}\"}}", "x".repeat(70_000));
    for broken in [
        "{not json",
        "{\"pid\": 4242}",
        "{\"sessionId\": 7}",
        "",
        "[\"S\"]",
        &long,
    ] {
        std::fs::write(sessions.join("4242.json"), broken).unwrap();
        assert_eq!(claude().locate(&ctx), Ok(expected.clone()), "{broken:.40}");
    }
}

/// Without a launch time no live session can be told to be the agent's.
#[test]
fn the_fallback_needs_a_launch_time() {
    let (_tmp, project, home) = dirs();
    let cwd = canonical(&project).to_str().unwrap().to_string();
    write_json(
        &home.join(".claude/sessions/3.json"),
        live_entry("LIVE", &cwd, "cli", 10),
    );
    assert_eq!(
        claude().locate(&ctx(&project, &home, &[])),
        Err(AmbientUnavailable::AgentUnknown)
    );
}

/// A print or stream-json claude (`entrypoint: sdk-cli`) in the same project writes a pid
/// file too: neither the fallback nor the agent's own pid file takes it.
#[test]
fn print_and_sdk_sessions_are_not_the_agent() {
    let (_tmp, project, home) = dirs();
    let root = home.join(".claude");
    let cwd = canonical(&project).to_str().unwrap().to_string();
    write_json(
        &root.join("sessions/1.json"),
        live_entry("PRINT", &cwd, "sdk-cli", 20),
    );
    write_json(
        &root.join("sessions/2.json"),
        live_entry("TUI", &cwd, "cli", 10),
    );
    let mut ctx = launched(&project, &home);
    assert_eq!(
        claude().locate(&ctx),
        Ok(transcript_of(&root, &project, "TUI"))
    );
    let mut own = live_entry("OWN-PRINT", &cwd, "sdk-cli", 30);
    own["pid"] = json!(4242);
    write_json(&root.join("sessions/4242.json"), own);
    ctx.program = Some(4242);
    assert_eq!(
        claude().locate(&ctx),
        Ok(transcript_of(&root, &project, "TUI"))
    );
}

/// A claude started before the launch (another terminal's) is not the agent.
#[test]
fn the_fallback_takes_only_sessions_started_since_the_launch() {
    let (_tmp, project, home) = dirs();
    let cwd = canonical(&project).to_str().unwrap().to_string();
    write_json(
        &home.join(".claude/sessions/1.json"),
        live_entry("BEFORE", &cwd, "cli", -60),
    );
    assert_eq!(
        claude().locate(&launched(&project, &home)),
        Err(AmbientUnavailable::NotFound)
    );
}

/// A pid file left by a dead claude whose pid the agent reuses was written before the
/// launch: not the agent's (until claude writes its own).
#[test]
fn a_pid_file_from_before_the_launch_is_not_used() {
    let (_tmp, project, home) = dirs();
    let root = home.join(".claude");
    let cwd = canonical(&project).to_str().unwrap().to_string();
    let mut stale = live_entry("STALE", &cwd, "cli", -3600);
    stale["pid"] = json!(4242);
    write_json(&root.join("sessions/4242.json"), stale);
    let mut ctx = launched(&project, &home);
    ctx.program = Some(4242);
    assert_eq!(claude().locate(&ctx), Err(AmbientUnavailable::NotFound));
    let mut own = live_entry("OWN", &cwd, "cli", 5);
    own["pid"] = json!(4242);
    write_json(&root.join("sessions/4242.json"), own);
    assert_eq!(
        claude().locate(&ctx),
        Ok(transcript_of(&root, &project, "OWN"))
    );
}

/// A relative `CLAUDE_CONFIG_DIR` is taken against the agent's cwd; the path is absolute.
#[test]
fn a_relative_config_dir_is_taken_against_the_agents_cwd() {
    let (_tmp, project, home) = dirs();
    write_json(
        &project.join("rel-config/sessions/4242.json"),
        json!({"sessionId": "S"}),
    );
    let mut ctx = ctx(
        &project,
        &home,
        &[("CLAUDE_CONFIG_DIR", Path::new("rel-config"))],
    );
    ctx.program = Some(4242);
    let path = claude().locate(&ctx).unwrap();
    assert!(path.is_absolute(), "{}", path.display());
    assert_eq!(
        path,
        project
            .join("rel-config/projects")
            .join(claude_dir_name(&canonical(&project)))
            .join("S.jsonl")
    );
}

/// claude names its project directory after the cwd it started in, which its pid file
/// records: a claude started elsewhere than the terminal's cwd is still found.
#[test]
fn the_project_directory_follows_the_cwd_in_the_pid_file() {
    let (_tmp, project, home) = dirs();
    write_json(
        &home.join(".claude/sessions/4242.json"),
        json!({"sessionId": "S", "cwd": "/elsewhere/dir"}),
    );
    let mut ctx = ctx(&project, &home, &[]);
    ctx.program = Some(4242);
    assert_eq!(
        claude().locate(&ctx),
        Ok(home.join(".claude/projects/-elsewhere-dir/S.jsonl"))
    );
}

/// A session id is a file name: one that would leave the project directory is not used.
#[test]
fn a_session_id_with_a_path_in_it_is_not_used() {
    let (_tmp, project, home) = dirs();
    for id in ["../../escape", "a/b", "x.y"] {
        write_json(
            &home.join(".claude/sessions/4242.json"),
            json!({"sessionId": id}),
        );
        let mut ctx = launched(&project, &home);
        ctx.program = Some(4242);
        assert_eq!(
            claude().locate(&ctx),
            Err(AmbientUnavailable::NotFound),
            "{id}"
        );
    }
}

/// `/clear` gives the running claude a new session id in its pid file: the next lookup
/// locates the new transcript.
#[test]
fn a_new_session_after_clear_is_followed() {
    let (_tmp, project, home) = dirs();
    let root = home.join(".claude");
    let pid_file = root.join("sessions/4242.json");
    let mut ctx = ctx(&project, &home, &[]);
    ctx.program = Some(4242);
    for id in ["S1", "S2"] {
        write_json(
            &pid_file,
            json!({"sessionId": id, "cwd": canonical(&project)}),
        );
        assert_eq!(
            claude().locate(&ctx),
            Ok(transcript_of(&root, &project, id))
        );
    }
}

/// The project through a symlink: claude names its directory after the canonical path.
#[cfg(unix)]
#[test]
fn the_project_directory_is_named_after_the_canonical_path() {
    let (tmp, project, home) = dirs();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&project, &link).unwrap();
    write_json(
        &home.join(".claude/sessions/4242.json"),
        json!({"sessionId": "S"}),
    );
    let mut ctx = ctx(&link, &home, &[]);
    ctx.program = Some(4242);
    assert_eq!(
        claude().locate(&ctx),
        Ok(transcript_of(&home.join(".claude"), &project, "S"))
    );
}

/// claude cuts a project directory name longer than 200 chars and appends a hash: the
/// directory sharing the first 200 that holds the transcript, else the only one sharing
/// them (before the first transcript).
#[test]
fn a_long_project_name_matches_by_its_first_200_chars() {
    let (tmp, _project, home) = dirs();
    let project = tmp.path().join("p".repeat(220));
    std::fs::create_dir_all(&project).unwrap();
    let full = claude_dir_name(&canonical(&project));
    assert!(full.len() > 200);
    let projects = home.join(".claude/projects");
    let cut = projects.join(format!("{}-1x2y3z", &full[..200]));
    std::fs::create_dir_all(&cut).unwrap();
    write_json(
        &home.join(".claude/sessions/4242.json"),
        json!({"sessionId": "NEW"}),
    );
    let mut ctx = ctx(&project, &home, &[]);
    ctx.program = Some(4242);
    assert_eq!(claude().locate(&ctx), Ok(cut.join("NEW.jsonl")));
    let other = projects.join(format!("{}-other", &full[..200]));
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("NEW.jsonl"), "").unwrap();
    assert_eq!(claude().locate(&ctx), Ok(other.join("NEW.jsonl")));
}

/// Path building only (Windows is not run here): the default root of a Windows home, and
/// a Windows project's directory name.
#[test]
fn windows_paths_are_built_by_the_same_rules() {
    let root = config_root(None, Some(PathBuf::from(r"C:\Users\me")), ".claude").unwrap();
    assert_eq!(
        root.to_str().unwrap().replace('/', "\\"),
        r"C:\Users\me\.claude"
    );
    assert_eq!(dashed(Path::new(r"C:\Users\me\proj")), "C--Users-me-proj");
}

// --- questions --------------------------------------------------------------------------

#[cfg(feature = "questions")]
mod questions {
    use serde_json::json;

    use super::{claude, line, prompt, turn_end};
    use crate::agent::profiles::ambient::testing::{fixture, questions};
    use crate::agent::questions::QuestionItem;

    fn item(header: &str, prompt: &str, options: &[&str]) -> QuestionItem {
        QuestionItem::new(
            header,
            prompt,
            options.iter().map(|o| o.to_string()).collect(),
        )
    }

    fn ask(id: &str) -> String {
        line(
            json!({"type": "assistant", "isSidechain": false, "message": {
            "role": "assistant", "content": [{"type": "tool_use", "id": id, "name": "AskUserQuestion",
            "input": {"questions": [{"header": "Pick", "question": "Pick one?", "multiSelect": false,
                "options": [{"label": "Yes", "description": "y"}, {"label": "No", "description": "n"}]}]}}]}}),
        )
    }

    fn answer(id: &str) -> String {
        line(json!({"type": "user", "isSidechain": false, "message": {
            "role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": "done"}]}}))
    }

    /// The captured single-choice question (2.1.296, in `bypassPermissions` mode): asked
    /// from its call after the prompt until its result; the prompt and the turn's end
    /// withdraw. Claude offers a typed answer on a single-choice question.
    #[test]
    fn a_captured_question_is_asked_after_its_prompt_until_its_result() {
        let (told, asked) = questions(
            &claude(),
            &fixture("ambient/claude-2.1.296/question-single.jsonl"),
        );
        assert_eq!(
            told,
            [
                "withdrawn",
                "asked toolu_011jKCS6TEG5MdXsjNQxL1vx",
                "answered toolu_011jKCS6TEG5MdXsjNQxL1vx",
                "withdrawn"
            ]
        );
        assert_eq!(
            asked[0].questions,
            [item("Color", "Red or green?", &["Red", "Green"]).with_free_text(true)]
        );
    }

    /// A multiple-choice item takes no typed answer; one call can ask several items.
    #[test]
    fn captured_multiple_choice_and_multiple_item_questions_keep_their_items() {
        let (told, asked) = questions(
            &claude(),
            &fixture("ambient/claude-2.1.296/question-multiselect.jsonl"),
        );
        assert_eq!(told.len(), 4, "{told:?}");
        assert_eq!(
            asked[0].questions,
            [item(
                "Colors",
                "Which of these colors do you like?",
                &["Red", "Green", "Blue"]
            )
            .with_multi_select(true)]
        );
        let (told, asked) = questions(
            &claude(),
            &fixture("ambient/claude-2.1.296/question-multi.jsonl"),
        );
        assert_eq!(told.len(), 4, "{told:?}");
        assert_eq!(
            asked[0].questions,
            [
                item(
                    "Colors",
                    "Which of these colors do you like?",
                    &["Red", "Green", "Blue"]
                )
                .with_multi_select(true),
                item("Shape", "Pick a shape.", &["Circle", "Square"]).with_free_text(true),
            ]
        );
    }

    /// The controlled tier's capture holds the call and its result without the prompt: a
    /// call whose prompt was not read (written before observation started) is not asked.
    #[test]
    fn a_call_without_a_fresh_prompt_is_not_asked() {
        let calls = fixture("../questions/claude-2.1.296.jsonl");
        assert_eq!(questions(&claude(), &calls).0, Vec::<String>::new());
        let mut lines = vec![prompt("survey")];
        lines.extend(calls);
        let (told, asked) = questions(&claude(), &lines);
        assert_eq!(told, ["withdrawn", "asked toolu_2", "answered toolu_2"]);
        assert_eq!(asked[0].questions.len(), 2);
        assert_eq!(asked[0].questions[1].options, ["Square", "Circle"]);
    }

    #[test]
    fn question_relay_requires_positive_prompt_content() {
        let text = json!({"type":"text","text":"survey"});
        for row in [
            json!({"type":"user"}),
            json!({"type":"user","message":{"role":"user","content":" "}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"text"}]}}),
            json!({"type":"user","message":{"role":"assistant","content":[text]}}),
            json!({"type":"user","message":{"role":"user","content":[text, {"type":"tool_result"}]}}),
        ] {
            let (_, asked) = questions(&claude(), &[line(row), ask("toolu_bad")]);
            assert!(asked.is_empty());
        }
        let row = json!({"type":"user","message":{"role":"user","content":[text]}});
        let (_, asked) = questions(&claude(), &[line(row), ask("toolu_ok")]);
        assert_eq!(asked.len(), 1);
    }

    /// The next prompt, an interruption and the turn's end withdraw; after the end a call
    /// needs a new prompt; a subagent's call is not the agent's question.
    #[test]
    fn prompts_ends_and_interruptions_withdraw_and_subagents_never_ask() {
        let interrupted = line(json!({"type": "user", "interruptedMessageId": "m1",
            "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]}}));
        let sidechain = ask("toolu_side").replace("\"isSidechain\":false", "\"isSidechain\":true");
        let (told, _) = questions(
            &claude(),
            &[
                prompt("a"),
                ask("toolu_a"),
                prompt("b"),
                ask("toolu_b"),
                interrupted,
                ask("toolu_late"),
                prompt("c"),
                sidechain,
                ask("toolu_c"),
                answer("toolu_other"),
                turn_end(),
                ask("toolu_after"),
            ],
        );
        assert_eq!(
            told,
            [
                "withdrawn",
                "asked toolu_a",
                "withdrawn",
                "asked toolu_b",
                "withdrawn",
                "withdrawn",
                "asked toolu_c",
                "answered toolu_other",
                "withdrawn",
            ]
        );
    }

    /// A call the controlled tier would not read withdraws: a secret question, a
    /// question whose text is not a string, a record that is not JSON, a record over 64 KiB.
    #[test]
    fn a_call_that_cannot_be_read_withdraws() {
        let secret = ask("toolu_s").replace(
            "\"multiSelect\":false",
            "\"multiSelect\":false,\"isSecret\":true",
        );
        let textless = ask("toolu_t").replace("\"question\":\"Pick one?\"", "\"question\":7");
        let huge = ask("toolu_h").replace(
            "\"description\":\"y\"",
            &format!("\"description\":\"{}\"", "y".repeat(70_000)),
        );
        for bad in [secret, textless, "{not json".to_string(), huge] {
            let (told, _) = questions(&claude(), &[prompt("a"), ask("toolu_a"), bad.clone()]);
            assert_eq!(
                told,
                ["withdrawn", "asked toolu_a", "withdrawn"],
                "{bad:.80}"
            );
        }
    }
}
