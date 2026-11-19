use super::providers::{self, Provider};
use super::*;
use std::collections::HashMap;
use std::sync::OnceLock;

fn frame(name: &str) -> &'static str {
    static FRAMES: OnceLock<HashMap<String, String>> = OnceLock::new();
    &FRAMES.get_or_init(|| {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/questions/screens.json"
        ))
        .unwrap()
    })[name]
}
fn color() -> QuestionItem {
    QuestionItem::new(
        "Color",
        "Which color should the sample use?",
        vec!["Blue".into(), "Amber".into()],
    )
    .with_free_text(true)
}
fn shape() -> QuestionItem {
    QuestionItem::new(
        "Shape",
        "Which shape should the sample use?",
        vec!["Square".into(), "Circle".into()],
    )
    .with_free_text(true)
}
fn matches(step: &KeyStep, name: &str) -> bool {
    step.guard.as_ref().is_none_or(|guard| guard(frame(name)))
}

#[test]
fn claude_native_selection_and_review_are_each_bound_to_their_current_page_and_choices() {
    let q = PendingQuestion::new("call", vec![color(), shape()]);
    let keys = providers::keys(
        Provider::Claude,
        frame("claude_multi"),
        &q,
        &[Answer::Choice(1), Answer::Choice(0)],
    )
    .unwrap();
    assert_eq!(
        keys.iter().map(|s| s.bytes.as_slice()).collect::<Vec<_>>(),
        [b"2", b"1", b"\r"]
    );
    for (step, screen) in keys
        .iter()
        .zip(["claude_multi", "claude_second", "claude_review"])
    {
        assert!(matches(step, screen), "{screen}");
        assert!(providers::shown(Provider::Claude, frame(screen), &q));
    }
    assert!(!matches(&keys[0], "claude_second"));
    assert!(!keys[2].guard.as_ref().unwrap()(
        &frame("claude_review").replace("Amber", "Blue")
    ));
    let mut wrong = q.clone();
    wrong.questions[0].options.reverse();
    assert!(!providers::shown(
        Provider::Claude,
        frame("claude_multi"),
        &wrong
    ));
    assert!(!providers::shown(
        Provider::Claude,
        &format!("{}\n❯ unrelated composer", frame("claude_multi")),
        &q
    ));
}

#[test]
fn claude_multiselect_checks_the_prior_selection_before_toggling_or_submitting() {
    let item = color().with_multi_select(true).with_free_text(false);
    let q = PendingQuestion::new("call", vec![item]);
    let keys = providers::keys(
        Provider::Claude,
        frame("claude_checks"),
        &q,
        &[Answer::Choices(vec![1, 0])],
    )
    .unwrap();
    assert_eq!(
        keys.iter().map(|s| s.bytes.as_slice()).collect::<Vec<_>>(),
        [b"1", b"2", b"\t", b"\r"]
    );
    assert!(matches(&keys[0], "claude_checks"));
    assert!(!matches(&keys[1], "claude_checks"));
    assert!(matches(&keys[2], "claude_checked"));
    assert!(matches(&keys[3], "claude_checks_review"));
    assert!(!matches(&keys[3], "claude_checked"));
}

#[test]
fn a_text_submit_requires_a_dedicated_editor_and_the_complete_literal_draft() {
    let q = PendingQuestion::new("call", vec![color()]);
    for (provider, initial, empty, filled) in [
        (
            Provider::Claude,
            "claude_single",
            "claude_text_empty",
            "claude_text_filled",
        ),
        (
            Provider::Codex,
            "codex_single",
            "codex_empty",
            "codex_filled",
        ),
    ] {
        let steps = providers::keys(
            provider,
            frame(initial),
            &q,
            &[Answer::Text("Green".into())],
        )
        .unwrap();
        let paste = &steps[steps.len() - 2];
        let submit = steps.last().unwrap();
        assert_eq!(paste.bytes, b"\x1b[200~Green\x1b[201~");
        assert_eq!(submit.bytes, b"\r");
        assert!(matches(paste, empty));
        assert!(!matches(paste, initial));
        assert!(!matches(submit, empty));
        assert!(matches(submit, filled));
        if provider == Provider::Codex {
            assert!(!matches(submit, "codex_wrong_note"));
        }
        assert!(!submit.guard.as_ref().unwrap()(
            &frame(filled).replace("Green", "Green tea")
        ));
        assert_eq!(
            providers::keys(
                provider,
                frame(initial),
                &q,
                &[Answer::Text("line\nbreak".into())]
            )
            .unwrap_err(),
            AnswerError::Unsupported
        );
    }
}

#[test]
fn codex_multiple_pages_confirm_selected_index_and_item_before_each_enter() {
    let q = PendingQuestion::new("call", vec![color(), shape()]);
    let steps = providers::keys(
        Provider::Codex,
        frame("codex_multi"),
        &q,
        &[Answer::Choice(1), Answer::Choice(0)],
    )
    .unwrap();
    assert_eq!(
        steps.iter().map(|s| s.bytes.as_slice()).collect::<Vec<_>>(),
        [b"\x1b[B".as_slice(), b"\r", b"\r"]
    );
    assert!(matches(&steps[0], "codex_multi"));
    assert!(!matches(&steps[1], "codex_multi"));
    assert!(matches(&steps[2], "codex_second"));
    assert!(!matches(&steps[2], "codex_multi"));
}

#[test]
fn cursor_requires_the_entire_measured_command_and_menu() {
    let original = frame("cursor");
    let q = providers::cursor(original).unwrap();
    assert_eq!(q.questions[0].prompt, "pwd in /sample");
    for (i, key) in [b"y", b"n"].into_iter().enumerate() {
        assert_eq!(
            providers::keys(Provider::Cursor, original, &q, &[Answer::Choice(i)]).unwrap()[0].bytes,
            key
        );
    }
    for screen in [
        frame("cursor_wrapped").to_owned(),
        original.replace("pwd", "…pwd"),
        original.replace("    Skip", "    Run Everything (shift+tab)\n    Skip"),
        original.replace(" $  pwd", "    continuation"),
        format!("{original}\n❯ "),
    ] {
        assert!(providers::cursor(&screen).is_none());
        assert!(!providers::shown(Provider::Cursor, &screen, &q));
    }
    assert!(!providers::shown(
        Provider::Cursor,
        &original.replace("pwd", "ls"),
        &q
    ));
}

#[cfg(unix)]
#[test]
fn recorded_transcript_requests_resolve_only_the_same_provider_call() {
    for (provider, data, n) in [
        (
            "claude",
            include_str!("../../../tests/fixtures/questions/claude-2.1.296.jsonl"),
            2,
        ),
        (
            "codex",
            include_str!("../../../tests/fixtures/questions/codex-0.154.0.jsonl"),
            1,
        ),
    ] {
        let mut records = data.lines();
        let open = records.next().unwrap();
        let TranscriptChange::Open(pending) = provider_record(provider, open.as_bytes()) else {
            panic!("no {provider} question");
        };
        assert_eq!(pending.questions[0], color());
        assert_eq!(pending.questions.len(), n);
        let TranscriptChange::Resolved(id) =
            provider_record(provider, records.next().unwrap().as_bytes())
        else {
            panic!("no {provider} result");
        };
        assert_eq!(pending.id, id);
        let ambiguous = open.replace("\"type\":", "\"type\":\"other\",\"type\":");
        assert!(matches!(
            provider_record(provider, ambiguous.as_bytes()),
            TranscriptChange::Clear
        ));
        assert!(matches!(
            provider_record(provider, &vec![b'x'; MAX_QUESTION_BYTES + 1]),
            TranscriptChange::Clear
        ));
    }
}

#[cfg(unix)]
#[test]
fn transcript_question_fields_reject_duplicates_and_positional_options() {
    let row = include_str!("../../../tests/fixtures/questions/claude-2.1.296.jsonl")
        .lines()
        .next()
        .unwrap();
    for malformed in [
        row.replace(r#""input":"#, r#""input":null,"in\u0070ut":"#),
        row.replace(r#""options":"#, r#""options":[],"options":"#),
        row.replace(
            r#"{"label":"Blue","description":"Use a cool shade"}"#,
            r#"["Blue"]"#,
        ),
    ] {
        assert!(matches!(
            provider_record("claude", malformed.as_bytes()),
            TranscriptChange::Clear
        ));
    }
}

#[test]
fn bounded_payloads_and_diagnostics_preserve_whole_requests_and_redact_content() {
    let item = QuestionItem::new(
        "private header",
        "private prompt",
        vec!["private option".into()],
    );
    assert!(PendingQuestion::new("private call", vec![item.clone()]).valid());
    assert!(!PendingQuestion::new("id", vec![item.clone(); MAX_QUESTIONS + 1]).valid());
    assert!(
        !PendingQuestion::new(
            "id",
            vec![QuestionItem::new(
                "header",
                "a".repeat(MAX_FIELD + 1),
                vec!["x".into()]
            )]
        )
        .valid()
    );
    let q = PendingQuestion::new("private call", vec![item.clone()]);
    let event = crate::Event::Question {
        id: SessionId(1),
        qid: QuestionId {
            session: SessionId(1),
            serial: 1,
        },
        questions: vec![item.clone()].into(),
    };
    let debug = format!(
        "{event:?} {q:?} {item:?} {:?} {:?}",
        Answer::Text("private value".into()),
        KeyStep::new(b"private".as_slice(), Duration::ZERO)
    );
    assert!(!debug.contains("private"));
    assert_eq!(
        dispatch::validate(&q, &[Answer::Choices(vec![0, 0])]),
        Err(AnswerError::Unsupported)
    );
}

#[test]
fn a_long_native_keyboard_plan_is_refused_before_dispatch() {
    let mut items = vec![color()];
    let mut answers = vec![Answer::Choice(0)];
    for n in 1..16 {
        items.push(QuestionItem::new(
            "Choices",
            format!("Question {n}"),
            (0..32).map(|i| format!("Item {i}")).collect(),
        ));
        answers.push(Answer::Choice(31));
    }
    let screen = frame("codex_single").replace("1/1 (1 unanswered)", "1/16 (16 unanswered)");
    assert_eq!(
        providers::keys(
            Provider::Codex,
            &screen,
            &PendingQuestion::new("call", items),
            &answers
        )
        .unwrap_err(),
        AnswerError::InputTooLarge
    );
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn answer_results_survive_multiple_waiters_and_late_readers() {
    let ticket = AnswerTicket::new(QuestionId {
        session: SessionId(1),
        serial: 1,
    });
    let other = ticket.clone();
    let waiter = tokio::spawn(async move { other.result().await });
    tokio::task::yield_now().await;
    ticket.finish(Err(AnswerError::Changed));
    assert_eq!(waiter.await.unwrap(), Err(AnswerError::Changed));
    assert_eq!(ticket.result().await, Err(AnswerError::Changed));
}
