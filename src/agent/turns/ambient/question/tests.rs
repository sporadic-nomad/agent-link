//! What the worker keeps of a transcript's questions: only a file's own new lines ask,
//! copied history, rewrites and lines over the budget withdraw, and the slot tells
//! whether the read that set it was settled. Synthetic format, no terminal.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::super::watch::{Files, Seen, Watched};
use super::*;
use crate::agent::questions::QuestionItem;
use crate::agent::turns::TurnOptions;
use crate::agent::turns::ambient::{
    AmbientContext, AmbientRecord, AmbientSource, AmbientUnavailable,
};

/// `{"ask":id}` asks "Pick?" (Yes / No) under `id`, `{"answer":id}` answers it,
/// `{"prompt":1}` withdraws; `{"bad":1}` is a question record that cannot be read;
/// `{"end":…}` ends a turn (and is the line handled last).
struct Synth;

impl AmbientSource for Synth {
    fn locate(&self, _: &AmbientContext) -> Result<PathBuf, AmbientUnavailable> {
        unreachable!("not located here")
    }

    fn parser(&self) -> Box<dyn AmbientParser> {
        Box::new(SynthParser)
    }
}

struct SynthParser;

fn pick(id: &str) -> PendingQuestion {
    PendingQuestion::new(
        id,
        vec![QuestionItem::new(
            "Pick",
            "Pick?",
            vec!["Yes".into(), "No".into()],
        )],
    )
}

impl AmbientParser for SynthParser {
    fn record(&mut self, line: &[u8], _: usize) -> Option<AmbientRecord> {
        let value: Value = serde_json::from_slice(line).ok()?;
        value.get("end").map(|_| AmbientRecord::Ended(None))
    }

    fn question(&mut self, line: &[u8]) -> Option<AmbientQuestion> {
        let value: Value = serde_json::from_slice(line).ok()?;
        if let Some(id) = value["ask"].as_str() {
            return Some(AmbientQuestion::Asked(pick(id)));
        }
        if let Some(id) = value["answer"].as_str() {
            return Some(AmbientQuestion::Answered(id.into()));
        }
        if value.get("prompt").is_some() || value.get("bad").is_some() {
            return Some(AmbientQuestion::Withdrawn);
        }
        if value.get("huge").is_some() {
            let item = QuestionItem::new("Pick", "x".repeat(9 * 1024), vec!["Yes".into()]);
            return Some(AmbientQuestion::Asked(PendingQuestion::new(
                "q9",
                vec![item],
            )));
        }
        if value.get("anonymous").is_some() {
            return Some(AmbientQuestion::Asked(pick("")));
        }
        None
    }
}

fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

/// Reads `path` (watched in `files`) to its end; what it shows asked and whether settled.
fn read(files: &mut Files, path: &Path, seen: &mut Seen) -> (Option<String>, bool) {
    let watched: &mut Watched = files
        .watch(
            path,
            &Synth,
            super::super::Capacity::new(TurnOptions::default()),
            seen,
        )
        .unwrap();
    loop {
        let report = watched.read(&Synth, seen);
        assert!(report.error.is_none(), "{report:?}");
        if !report.more {
            let asked = watched.asked().map(|q| q.id.clone());
            return (asked, watched.settled());
        }
    }
}

fn files() -> Files {
    // Everything created from a minute ago on is new.
    Files::new(SystemTime::now() - Duration::from_secs(60))
}

#[test]
fn a_question_stands_from_its_call_until_its_result_or_a_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let (mut files, mut seen) = (files(), Seen::default());
    assert_eq!(read(&mut files, &path, &mut seen), (None, true));
    append(&path, "{\"ask\":\"q1\"}\n");
    assert_eq!(
        read(&mut files, &path, &mut seen),
        (Some("q1".into()), true)
    );
    // Another id's result leaves it; its own ends it.
    append(&path, "{\"answer\":\"q0\"}\n{\"other\":1}\n");
    assert_eq!(
        read(&mut files, &path, &mut seen),
        (Some("q1".into()), true)
    );
    append(&path, "{\"answer\":\"q1\"}\n");
    assert_eq!(read(&mut files, &path, &mut seen), (None, true));
    // A later call replaces an earlier one; a prompt or an unreadable record withdraws.
    append(&path, "{\"ask\":\"q2\"}\n{\"ask\":\"q3\"}\n");
    assert_eq!(
        read(&mut files, &path, &mut seen),
        (Some("q3".into()), true)
    );
    append(&path, "{\"prompt\":1}\n");
    assert_eq!(read(&mut files, &path, &mut seen), (None, true));
    append(&path, "{\"ask\":\"q4\"}\n{\"bad\":1}\n");
    assert_eq!(read(&mut files, &path, &mut seen), (None, true));
}

#[test]
fn a_question_outside_the_bounds_or_without_an_id_withdraws() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let (mut files, mut seen) = (files(), Seen::default());
    append(&path, "{\"ask\":\"q1\"}\n{\"huge\":1}\n");
    assert_eq!(read(&mut files, &path, &mut seen), (None, true));
    append(&path, "{\"ask\":\"q1\"}\n{\"anonymous\":1}\n");
    assert_eq!(read(&mut files, &path, &mut seen), (None, true));
}

#[test]
fn retaining_a_valid_question_discards_the_parsers_spare_capacity() {
    struct Buffered(Option<PendingQuestion>);
    impl AmbientParser for Buffered {
        fn record(&mut self, _: &[u8], _: usize) -> Option<AmbientRecord> {
            None
        }

        fn question(&mut self, _: &[u8]) -> Option<AmbientQuestion> {
            self.0.take().map(AmbientQuestion::Asked)
        }
    }

    let mut q = pick("call");
    q.id.reserve(1 << 20);
    q.questions.reserve(1024);
    let item = &mut q.questions[0];
    item.header.reserve(1 << 20);
    item.prompt.reserve(1 << 20);
    item.options.reserve(1024);
    for option in &mut item.options {
        option.reserve(1 << 20);
    }

    let mut asking = Asking::default();
    asking.line(&mut Buffered(Some(q)), b"ask", true);
    let stored = asking.asked().unwrap();
    assert_eq!(*stored, pick("call"));
    assert_eq!(stored.id.capacity(), stored.id.len());
    assert_eq!(stored.questions.capacity(), stored.questions.len());
    let item = &stored.questions[0];
    assert_eq!(item.options.capacity(), item.options.len());
    for field in [&item.header, &item.prompt]
        .into_iter()
        .chain(&item.options)
    {
        assert_eq!(field.capacity(), field.len());
    }
}

/// A line still being written may answer the question: not settled until it ends.
#[test]
fn a_partial_line_leaves_the_question_unsettled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let (mut files, mut seen) = (files(), Seen::default());
    append(&path, "{\"ask\":\"q1\"}\n{\"answer\"");
    assert_eq!(
        read(&mut files, &path, &mut seen),
        (Some("q1".into()), false)
    );
    append(&path, ":\"q1\"}\n");
    assert_eq!(read(&mut files, &path, &mut seen), (None, true));
}

/// What a file held before observation started never asks.
#[test]
fn a_call_written_before_observation_is_not_asked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    append(&path, "{\"ask\":\"q1\"}\n");
    // Created before `since`: read from its end.
    let mut files = Files::new(SystemTime::now() + Duration::from_secs(60));
    let mut seen = Seen::default();
    assert_eq!(read(&mut files, &path, &mut seen), (None, true));
    append(&path, "{\"ask\":\"q2\"}\n");
    assert_eq!(
        read(&mut files, &path, &mut seen),
        (Some("q2".into()), true)
    );
}

/// A fork copies another transcript's lines: a call among them is history, not a
/// question of this file; the fork's own call is.
#[test]
fn copied_history_never_asks() {
    let dir = tempfile::tempdir().unwrap();
    let (first, fork) = (dir.path().join("a.jsonl"), dir.path().join("b.jsonl"));
    let (mut files, mut seen) = (files(), Seen::default());
    append(&first, "{\"prompt\":1,\"n\":1}\n{\"ask\":\"q1\"}\n");
    assert_eq!(
        read(&mut files, &first, &mut seen),
        (Some("q1".into()), true)
    );
    append(&fork, "{\"prompt\":1,\"n\":1}\n{\"ask\":\"q1\"}\n");
    assert_eq!(read(&mut files, &fork, &mut seen), (None, true));
    append(&fork, "{\"ask\":\"q5\"}\n");
    assert_eq!(
        read(&mut files, &fork, &mut seen),
        (Some("q5".into()), true)
    );
}

/// A rewritten file's lines are read again: what they tell is not taken again, even past
/// the line handled last; a call written after the re-read is.
#[test]
fn a_rewrite_withdraws_the_question_until_a_new_call() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let (mut files, mut seen) = (files(), Seen::default());
    append(&path, "{\"end\":1}\n{\"ask\":\"q1\"}\n");
    assert_eq!(
        read(&mut files, &path, &mut seen),
        (Some("q1".into()), true)
    );
    std::fs::write(&path, "{\"pad\":1}\n{\"end\":1}\n{\"ask\":\"q1\"}\n").unwrap();
    assert_eq!(read(&mut files, &path, &mut seen), (None, true));
    append(&path, "{\"ask\":\"q2\"}\n");
    assert_eq!(
        read(&mut files, &path, &mut seen),
        (Some("q2".into()), true)
    );
}

#[test]
fn replayed_prompts_cannot_arm_a_later_claude_or_codex_question() {
    use crate::agent::profiles::ambient::{
        claude::ClaudeAmbient, codex::CodexAmbient, testing::fixture,
    };

    for (source, file, prompt_index, call_index) in [
        (
            &ClaudeAmbient as &dyn AmbientSource,
            "ambient/claude-2.1.296/question-single.jsonl",
            1,
            2,
        ),
        (
            &CodexAmbient::default(),
            "ambient/codex-0.154.0/question-plan.jsonl",
            1,
            3,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let lines = fixture(file);
        let prompt = format!("{}\n", lines[prompt_index]);
        let call = format!("{}\n", lines[call_index]);
        let mut files = Files::new(SystemTime::now() + Duration::from_secs(60));
        append(&path, &prompt);
        let watched = files
            .watch(
                &path,
                source,
                super::super::Capacity::new(TurnOptions::default()),
                &Seen::default(),
            )
            .unwrap();
        let mut seen = Seen::default();
        watched.read(source, &mut seen);
        std::fs::write(&path, format!("{{}}\n{prompt}")).unwrap();
        assert!(watched.read(source, &mut seen).rewritten);
        append(&path, &call);
        watched.read(source, &mut seen);
        assert!(watched.asked().is_none(), "{file}: historical prompt");
        append(&path, &(prompt + &call));
        watched.read(source, &mut seen);
        assert!(watched.asked().is_some(), "{file}: fresh prompt");
    }
}

/// A line over the line budget may have been the call's result: it withdraws.
#[test]
fn a_line_over_the_budget_withdraws() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let limits = TurnOptions::default();
    let (mut files, mut seen) = (files(), Seen::default());
    append(&path, "{\"ask\":\"q1\"}\n");
    assert_eq!(
        read(&mut files, &path, &mut seen),
        (Some("q1".into()), true)
    );
    append(&path, &format!("{}\n", "x".repeat(limits.line_bytes + 1)));
    assert_eq!(read(&mut files, &path, &mut seen), (None, true));
}

#[test]
fn the_slot_follows_launches_and_unreadable_transcripts() {
    let slot = Slot::default();
    let q = Some(Arc::new(pick("q1")));
    slot.set(q.clone(), true);
    assert!(slot.lock().question.is_some() && slot.lock().settled);
    slot.unsettle();
    assert!(slot.lock().question.is_some() && !slot.lock().settled);
    slot.set(q, true);
    slot.restart(7);
    let state = slot.lock();
    assert!(state.question.is_none() && state.settled && state.lifecycle == 7);
    drop(state);
    slot.set(Some(Arc::new(pick("q2"))), true);
    slot.clear();
    assert!(slot.lock().question.is_none());
    let shown = format!("{slot:?}");
    assert!(!shown.contains("q2") && !shown.contains("Pick"), "{shown}");
}

/// Without its terminal, or once its receiver stopped, a source has no authority.
#[test]
fn a_stopped_or_unbound_attachment_answers_nothing() {
    let (cancel, _cancelled) = crossbeam_channel::bounded(1);
    let receiver = AmbientReceiver {
        shared: Arc::new(Shared::new(TurnOptions::default(), cancel)),
    };
    let source = receiver.questions();
    assert!(!source.reads_transcript());
    assert_eq!(source.pending_question(), Err(AnswerError::NotOpen));
    receiver
        .shared
        .question
        .set(Some(Arc::new(pick("q1"))), true);
    receiver.stop();
    assert_eq!(source.pending_question(), Err(AnswerError::NotOpen));
}

#[test]
fn debug_shows_no_question_content() {
    let shown = format!(
        "{:?} {:?} {:?}",
        AmbientQuestion::Asked(pick("call_secret")),
        AmbientQuestion::Answered("call_secret".into()),
        AmbientQuestion::Withdrawn
    );
    assert!(
        !shown.contains("call_secret") && !shown.contains("Pick"),
        "{shown}"
    );
}
