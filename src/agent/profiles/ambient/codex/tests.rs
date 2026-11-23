use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::json;

use super::*;
use crate::agent::profiles::ambient::testing::{
    Seen, at, canonical, ctx, dirs, ended, feed, fixture, line, start, write_at,
};

// --- turns ------------------------------------------------------------------------------

fn codex() -> CodexAmbient {
    CodexAmbient::default()
}

/// The captured interactive rollout (0.154.0): the first turn's final message, then a turn
/// interrupted with Esc (`turn_aborted`), which ends with no reply.
#[test]
fn the_captured_rollout_reports_each_final_message_and_an_aborted_turn() {
    assert_eq!(
        feed(&codex(), &fixture("ambient/codex-0.154.0/turns.jsonl")),
        [ended("first answer"), Seen::Ended(None)]
    );
    // The controlled tier's capture of the same version reads the same way.
    assert_eq!(
        feed(&codex(), &fixture("codex-0.154.0/turns.jsonl")),
        [ended("Sample confirmed.")]
    );
}

/// A failed turn (0.154.0: an unsupported model) completes with a `null` message.
#[test]
fn a_task_complete_with_a_null_message_has_no_reply() {
    let lines = [line(json!({"type": "event_msg", "payload": {
        "type": "task_complete", "last_agent_message": null, "error": {"message": "400"}
    }}))];
    assert_eq!(feed(&codex(), &lines), [Seen::Ended(None)]);
}

/// A known record without the fields it is read for is reported, and reading goes on.
#[test]
fn a_known_record_without_its_fields_is_unrecognised() {
    let mut lines = fixture("ambient/codex-0.154.0/turns.jsonl");
    let at = lines
        .iter()
        .position(|l| l.contains("task_started"))
        .unwrap();
    lines.insert(at + 1, line(json!({"type": "event_msg"})));
    lines.insert(
        at + 1,
        line(json!({"type": "event_msg", "payload": {"type": 7}})),
    );
    lines.insert(
        at + 1,
        line(json!({"type": "event_msg", "payload": {"type": "task_complete"}})),
    );
    assert_eq!(
        feed(&codex(), &lines),
        [
            Seen::Unrecognised,
            Seen::Unrecognised,
            Seen::Unrecognised,
            ended("first answer"),
            Seen::Ended(None)
        ]
    );
}

/// Unknown kinds and fields, other events and lines that are no JSON object end nothing.
#[test]
fn other_lines_end_nothing() {
    let lines: Vec<String> = [
        json!({"type": "response_item", "payload": {"type": "message", "role": "assistant"}}),
        json!({"type": "event_msg", "payload": {"type": "task_started", "new_field": [1]}}),
        json!({"type": "session_meta"}),
        json!({"type": "something_new", "payload": {"type": "task_complete"}}),
        json!(["event_msg"]),
    ]
    .into_iter()
    .map(line)
    .chain([
        "not json".to_string(),
        "{\"type\":\"event_msg\",".to_string(),
    ])
    .collect();
    assert_eq!(feed(&codex(), &lines), []);
}

/// The final message keeps at most the reply budget, cut at a UTF-8 boundary.
#[test]
fn a_long_final_message_is_cut_to_the_budget() {
    let event = json!({"type": "event_msg", "payload": {
        "type": "task_complete", "last_agent_message": "ééé"
    }});
    let mut parser = codex().parser();
    let Some(AmbientRecord::Ended(Some(reply))) = parser.record(line(event).as_bytes(), 3) else {
        panic!("an explicit end");
    };
    assert_eq!(reply.text, "é");
    assert!(reply.truncated);
}

// --- lookup -----------------------------------------------------------------------------

/// 2026-10-11T12:00:00Z.
fn noon_2026_10_11() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_791_720_000)
}

/// An interactive TUI rollout in `sessions/<day>/` whose `session_meta` names `cwd` (as
/// 0.154.0 writes it), modified at `start() + secs`.
fn rollout(root: &Path, day: &str, name: &str, cwd: &Path, secs: i64) -> PathBuf {
    let payload = json!({
        "cwd": cwd, "originator": "codex-tui", "source": "cli", "thread_source": "user",
    });
    rollout_with(root, day, name, payload, secs)
}

fn rollout_with(
    root: &Path,
    day: &str,
    name: &str,
    payload: serde_json::Value,
    secs: i64,
) -> PathBuf {
    let path = root
        .join("sessions")
        .join(day)
        .join(format!("rollout-{name}.jsonl"));
    let meta = json!({"timestamp": "t", "type": "session_meta", "payload": payload});
    write_at(
        &path,
        &format!("{meta}\n{{\"type\":\"event_msg\"}}\n"),
        at(secs),
    );
    path
}

fn codex_ctx(project: &Path, home: &Path, vars: &[(&str, &Path)]) -> AmbientContext {
    let mut ctx = ctx(project, home, vars);
    ctx.launched_at = Some(start());
    ctx
}

fn locate(ctx: &AmbientContext) -> Result<PathBuf, AmbientUnavailable> {
    codex().rollout_at(ctx, noon_2026_10_11())
}

#[test]
fn the_newest_rollout_of_the_project_since_the_launch_is_located() {
    let (_tmp, project, home) = dirs();
    let root = home.join(".codex");
    let cwd = canonical(&project);
    rollout(&root, "2026/10/11", "a", &cwd, 10);
    let newest = rollout(&root, "2026/10/11", "b", &cwd, 30);
    rollout(&root, "2026/10/10", "c", &cwd, 20);
    assert_eq!(locate(&codex_ctx(&project, &home, &[])), Ok(newest));
}

#[cfg(unix)]
#[test]
fn a_symlinked_rollout_becomes_eligible_when_its_target_is_written() {
    let (tmp, project, home) = dirs();
    let ctx = codex_ctx(&project, &home, &[]);
    let link = rollout(&home.join(".codex"), "2026/10/11", "linked", &project, -10);
    let target = tmp.path().join("rollout-data");
    std::fs::rename(&link, &target).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_eq!(locate(&ctx), Err(AmbientUnavailable::NotFound));
    crate::agent::profiles::ambient::testing::stamp(&target, at(5));
    assert_eq!(locate(&ctx), Ok(link));
}

#[test]
fn the_root_is_codex_home_when_set() {
    let (tmp, project, home) = dirs();
    let root = tmp.path().join("codex-home");
    let cwd = canonical(&project);
    let expected = rollout(&root, "2026/10/11", "a", &cwd, 10);
    // A newer one under the home's `.codex`: not looked at.
    rollout(&home.join(".codex"), "2026/10/11", "b", &cwd, 20);
    let ctx = codex_ctx(&project, &home, &[("CODEX_HOME", &root)]);
    assert_eq!(locate(&ctx), Ok(expected));
}

#[test]
fn another_project_and_rollouts_older_than_the_launch_are_not_located() {
    let (tmp, project, home) = dirs();
    let root = home.join(".codex");
    let cwd = canonical(&project);
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    rollout(&root, "2026/10/11", "older", &cwd, -60);
    rollout(&root, "2026/10/11", "other", &canonical(&other), 50);
    let ctx = codex_ctx(&project, &home, &[]);
    assert_eq!(locate(&ctx), Err(AmbientUnavailable::NotFound));
    let ours = rollout(&root, "2026/10/11", "ours", &cwd, 5);
    assert_eq!(locate(&ctx), Ok(ours));
}

#[test]
fn today_and_yesterday_are_listed() {
    let (_tmp, project, home) = dirs();
    let root = home.join(".codex");
    let cwd = canonical(&project);
    // Three days back: not listed, however recent its modification.
    rollout(&root, "2026/10/08", "old-day", &cwd, 90);
    let ctx = codex_ctx(&project, &home, &[]);
    assert_eq!(locate(&ctx), Err(AmbientUnavailable::NotFound));
    let yesterday = rollout(&root, "2026/10/10", "yesterday", &cwd, 10);
    assert_eq!(locate(&ctx), Ok(yesterday));
}

#[test]
fn the_followed_rollout_stays_eligible_after_its_start_day_ages_out() {
    let (_tmp, project, home) = dirs();
    let root = home.join(".codex");
    let mut ctx = codex_ctx(&project, &home, &[]);
    let cwd = canonical(&project);
    let original = rollout(&root, "2026/10/11", "a", &cwd, 10);
    let source = codex();
    let day = noon_2026_10_11();
    ctx.current_transcript = Some(source.rollout_at(&ctx, day).unwrap());
    let later = day + Duration::from_secs(4 * 86_400);
    let recent = rollout(&root, "2026/10/15", "b", &cwd, 5);
    assert_eq!(source.rollout_at(&ctx, later), Ok(original.clone()));
    // A newer eligible rollout wins. Replacing the old header cannot revive it.
    rollout(&root, "2026/10/15", "b", &cwd, 30);
    assert_eq!(source.rollout_at(&ctx, later), Ok(recent.clone()));
    write_at(&original, "{}\n", at(40));
    assert_eq!(source.rollout_at(&ctx, later), Ok(recent));
}

/// Today and yesterday across a year boundary, and across a leap day.
#[test]
fn date_directories_follow_the_calendar() {
    let (_tmp, project, home) = dirs();
    let root = home.join(".codex");
    let cwd = canonical(&project);
    let ctx = codex_ctx(&project, &home, &[]);
    // 2026-01-01T00:30:00Z: yesterday is 2025-12-31.
    let new_year = UNIX_EPOCH + Duration::from_secs(1_767_227_400);
    let dec31 = rollout(&root, "2025/12/31", "dec31", &cwd, 10);
    assert_eq!(codex().rollout_at(&ctx, new_year), Ok(dec31));
    // 2024-03-01T12:00:00Z: yesterday is 2024-02-29.
    let march1 = UNIX_EPOCH + Duration::from_secs(1_709_294_400);
    let feb29 = rollout(&root, "2024/02/29", "feb29", &cwd, 20);
    assert_eq!(codex().rollout_at(&ctx, march1), Ok(feb29));
}

/// codex records the project's canonical path: a project reached through a symlink still
/// matches.
#[cfg(unix)]
#[test]
fn the_canonical_project_path_is_compared() {
    let (tmp, project, home) = dirs();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&project, &link).unwrap();
    let ours = rollout(
        &home.join(".codex"),
        "2026/10/11",
        "a",
        &canonical(&project),
        5,
    );
    assert_eq!(locate(&codex_ctx(&link, &home, &[])), Ok(ours));
}

/// Only the interactive TUI's rollouts: a subagent's, `codex exec`'s and `codex
/// app-server`'s carry the project's cwd too, and are newer here.
#[test]
fn only_interactive_tui_rollouts_are_located() {
    let (_tmp, project, home) = dirs();
    let root = home.join(".codex");
    let cwd = canonical(&project);
    let tui = rollout(&root, "2026/10/11", "tui", &cwd, 5);
    let day = "2026/10/11";
    rollout_with(
        &root,
        day,
        "sub",
        json!({"cwd": cwd, "originator": "codex-tui",
            "source": {"subagent": {"thread_spawn": {"parent_thread_id": "p"}}},
            "thread_source": "subagent"}),
        50,
    );
    rollout_with(
        &root,
        day,
        "sub-cli",
        json!({"cwd": cwd, "source": "cli", "thread_source": "subagent"}),
        45,
    );
    rollout_with(
        &root,
        day,
        "app-server",
        json!({"cwd": cwd, "originator": "host", "source": "vscode"}),
        40,
    );
    rollout_with(
        &root,
        day,
        "exec",
        json!({"cwd": cwd, "originator": "codex_exec", "source": "exec", "thread_source": "user"}),
        30,
    );
    assert_eq!(locate(&codex_ctx(&project, &home, &[])), Ok(tui));
}

/// A relative `CODEX_HOME` is taken against the agent's cwd; the path is absolute.
#[test]
fn a_relative_codex_home_is_taken_against_the_agents_cwd() {
    let (_tmp, project, home) = dirs();
    let cwd = canonical(&project);
    let ours = rollout(&project.join("rel-home"), "2026/10/11", "a", &cwd, 5);
    let ctx = codex_ctx(&project, &home, &[("CODEX_HOME", Path::new("rel-home"))]);
    let path = locate(&ctx).unwrap();
    assert!(path.is_absolute(), "{}", path.display());
    assert_eq!(path, ours);
}

#[test]
fn read_passes_reuse_the_header_until_the_opened_file_changes() {
    use std::io::Seek;

    let (_tmp, project, home) = dirs();
    let ctx = codex_ctx(&project, &home, &[]);
    let source = codex();
    let root = home.join(".codex");
    let mut payload = json!({
        "cwd": canonical(&project), "source": "cli", "instructions": "x".repeat(20 * 1024),
    });
    let path = rollout_with(&root, "2026/10/11", "a", payload.clone(), 5);
    assert_eq!(source.rollout_at(&ctx, noon_2026_10_11()), Ok(path.clone()));
    for _ in 0..4 {
        let mut file = std::fs::File::open(&path).unwrap();
        assert!(source.accepts_file(&ctx, &file).unwrap());
        assert_eq!(file.stream_position().unwrap(), 0, "reread the header");
    }

    // Replace it between lookup and a read, preserving size and modification time.
    payload["source"] = json!("sdk");
    let other = rollout_with(&root, "2026/10/11", "b", payload, 5);
    std::fs::rename(other, &path).unwrap();
    let file = std::fs::File::open(path).unwrap();
    assert!(!source.accepts_file(&ctx, &file).unwrap());
}

#[test]
fn rewriting_a_cached_rollout_rechecks_its_source() {
    let (_tmp, project, home) = dirs();
    let cwd = canonical(&project);
    let ours = rollout(&home.join(".codex"), "2026/10/11", "a", &cwd, 5);
    let source = codex();
    let ctx = codex_ctx(&project, &home, &[]);
    assert_eq!(source.rollout_at(&ctx, noon_2026_10_11()), Ok(ours.clone()));
    let meta = json!({"type":"session_meta","payload":{"cwd":cwd,"source":"vscode"}});
    let pad = " ".repeat(std::fs::metadata(&ours).unwrap().len() as usize);
    write_at(&ours, &format!("{meta}\n{pad}\n"), at(5));
    assert_eq!(
        source.rollout_at(&ctx, noon_2026_10_11()),
        Err(AmbientUnavailable::NotFound)
    );
}

/// A rollout cut below the first line read before is read again.
#[test]
fn a_rollout_cut_below_its_first_line_is_read_again() {
    let (_tmp, project, home) = dirs();
    let cwd = canonical(&project);
    let ours = rollout(&home.join(".codex"), "2026/10/11", "a", &cwd, 5);
    let source = codex();
    let ctx = codex_ctx(&project, &home, &[]);
    assert_eq!(source.rollout_at(&ctx, noon_2026_10_11()), Ok(ours.clone()));
    let short = json!({"type": "session_meta", "payload": {"cwd": "/x", "source": "cli"}});
    write_at(&ours, &format!("{short}\n"), at(6));
    assert_eq!(
        source.rollout_at(&ctx, noon_2026_10_11()),
        Err(AmbientUnavailable::NotFound)
    );
}

/// A rollout just created (its first line not complete yet), one of another format and
/// other files are passed over.
#[test]
fn rollouts_it_cannot_tell_are_passed_over() {
    let (_tmp, project, home) = dirs();
    let root = home.join(".codex");
    let cwd = canonical(&project);
    let ours = rollout(&root, "2026/10/11", "ours", &cwd, 5);
    let day = root.join("sessions/2026/10/11");
    write_at(&day.join("rollout-empty.jsonl"), "", at(9));
    write_at(
        &day.join("rollout-partial.jsonl"),
        "{\"type\":\"session_meta\",",
        at(9),
    );
    write_at(
        &day.join("rollout-foreign.jsonl"),
        "{\"type\":\"event_msg\"}\n",
        at(9),
    );
    write_at(&day.join("notes.jsonl"), "{}\n", at(9));
    std::fs::create_dir_all(day.join("rollout-dir.jsonl")).unwrap();
    assert_eq!(locate(&codex_ctx(&project, &home, &[])), Ok(ours));
}

#[test]
fn without_a_rollout_or_a_root_nothing_is_located() {
    let (_tmp, project, home) = dirs();
    assert_eq!(
        locate(&codex_ctx(&project, &home, &[])),
        Err(AmbientUnavailable::NotFound)
    );
    let env = crate::env::EnvPolicy {
        inherit: false,
        ..crate::env::EnvPolicy::default()
    };
    assert_eq!(
        locate(&AmbientContext::new(&project, env)),
        Err(AmbientUnavailable::NoConfigRoot)
    );
}

/// More directory entries than one lookup walks: it declines to guess.
#[test]
fn a_lookup_walks_a_bounded_number_of_entries() {
    let (_tmp, project, home) = dirs();
    let day = home.join(".codex/sessions/2026/10/11");
    std::fs::create_dir_all(&day).unwrap();
    for i in 0..=crate::agent::profiles::ambient::MAX_ENTRIES {
        std::fs::write(day.join(format!("x{i}")), "").unwrap();
    }
    assert_eq!(
        locate(&codex_ctx(&project, &home, &[])),
        Err(AmbientUnavailable::Ambiguous)
    );
}

/// Path building only (Windows is not run here): the default root of a Windows home.
#[test]
fn the_default_root_of_a_windows_home() {
    let root = config_root(None, Some(PathBuf::from(r"C:\Users\me")), ".codex").unwrap();
    assert_eq!(
        root.to_str().unwrap().replace('/', "\\"),
        r"C:\Users\me\.codex"
    );
}

// --- questions --------------------------------------------------------------------------

#[cfg(feature = "questions")]
mod questions {
    use serde_json::json;

    use super::codex;
    use crate::agent::profiles::ambient::testing::{fixture, line, questions};
    use crate::agent::questions::QuestionItem;

    fn event(kind: &str) -> String {
        line(json!({"type": "event_msg", "payload": {"type": kind, "turn_id": "t1"}}))
    }

    /// The captured Plan-mode question (0.154.0): asked from its call after
    /// `task_started` until its output; `task_complete` withdraws. Codex offers notes
    /// ("None of the above") unless the call says `isOther: false`.
    #[test]
    fn a_captured_plan_question_is_asked_in_its_turn_until_its_output() {
        let (told, asked) = questions(
            &codex(),
            &fixture("ambient/codex-0.154.0/question-plan.jsonl"),
        );
        assert_eq!(
            told,
            [
                "withdrawn",
                "asked call_Z7wxvqhUaTfNH6busGeNCvhZ",
                "answered call_Z7wxvqhUaTfNH6busGeNCvhZ",
                "withdrawn"
            ]
        );
        assert_eq!(
            asked[0].questions,
            [QuestionItem::new(
                "Color",
                "Pick red or green.",
                vec!["Red".into(), "Green".into()]
            )
            .with_free_text(true)]
        );
        let (told, asked) = questions(
            &codex(),
            &fixture("ambient/codex-0.154.0/question-multi.jsonl"),
        );
        assert_eq!(told.len(), 4, "{told:?}");
        let prompts: Vec<&str> = asked[0]
            .questions
            .iter()
            .map(|q| q.prompt.as_str())
            .collect();
        assert_eq!(prompts, ["Pick red or green.", "Pick circle or square."]);
    }

    /// The controlled tier's capture holds the call and its output without the turn's
    /// start: not asked. After `task_started` it is.
    #[test]
    fn a_call_without_its_turn_start_is_not_asked() {
        let calls = fixture("../questions/codex-0.154.0.jsonl");
        assert_eq!(questions(&codex(), &calls).0, Vec::<String>::new());
        let mut lines = vec![event("task_started")];
        lines.extend(calls);
        let (told, asked) = questions(&codex(), &lines);
        assert_eq!(told, ["withdrawn", "asked fc_1", "answered fc_1"]);
        assert_eq!(asked[0].questions[0].options, ["Blue", "Amber"]);
    }

    /// `turn_aborted` (Esc) withdraws, and so does a call that cannot be read.
    #[test]
    fn an_aborted_turn_or_an_unreadable_call_withdraws() {
        let calls = fixture("../questions/codex-0.154.0.jsonl");
        let broken = calls[0].replace("\\\"questions\\\"", "\\\"qs\\\"");
        let (told, _) = questions(
            &codex(),
            &[
                event("task_started"),
                calls[0].clone(),
                event("turn_aborted"),
                calls[0].clone(),
                event("task_started"),
                calls[0].clone(),
                broken,
            ],
        );
        assert_eq!(
            told,
            [
                "withdrawn",
                "asked fc_1",
                "withdrawn",
                "withdrawn",
                "asked fc_1",
                "withdrawn"
            ]
        );
    }
}
