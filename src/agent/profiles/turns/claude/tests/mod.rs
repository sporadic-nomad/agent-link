use std::path::Path;

use serde_json::{Value, json};

use super::parser::ClaudeParser;
use crate::agent::turns::{Binding, Gap, Parser, ProviderId, Record, Reply};

mod launch;
#[cfg(target_os = "macos")]
mod pty;

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/transcripts/claude-2.1.296/",
            $name
        ))
    };
}

const A: &str = fixture!("session-a.jsonl");
const B: &str = fixture!("session-b.jsonl");
const CLEAR: &str = fixture!("after-clear.jsonl");
const CAPTURE: &str = fixture!("capture.json");
const CWD: &str = "/fixture/project";

fn binding(data: &str, initial: &str) -> Binding {
    let header: Value = serde_json::from_str(data.lines().next().unwrap()).unwrap();
    let id = header["sessionId"].as_str().unwrap();
    Binding {
        initial: ProviderId::new(initial).unwrap(),
        current: ProviderId::new(id).unwrap(),
        relative: format!("{id}.jsonl").into(),
        revision: 0,
    }
}

fn replay(lines: &str, binding: &Binding) -> (usize, Vec<String>) {
    let mut parser = ClaudeParser::new(CWD.into());
    let mut users = 0;
    let mut replies = Vec::new();
    for line in lines.lines() {
        match parser.record(binding, line.as_bytes(), 4096).unwrap() {
            Record::User => users += 1,
            Record::End(Some(r)) => {
                assert!(!r.truncated);
                replies.push(r.text);
            }
            _ => {}
        }
    }
    (users, replies)
}

#[test]
fn paired_capture_matches_visible_turns_and_tracks_the_writer_across_clear() {
    let capture: Value = serde_json::from_str(CAPTURE).unwrap();
    let pair = &capture["twoTerminals"];
    let writers = &pair["writers"];
    for (data, label, replies, prompts) in [
        (
            A,
            "A",
            vec!["Copper pebble", "Amber petal\n\nSecond petal"],
            3,
        ),
        (B, "B", vec!["Violet acorn", "Silver pumice"], 2),
        (CLEAR, "A", vec!["Lime button"], 1),
    ] {
        let index = usize::from(label == "B");
        let launch = &pair["launches"][index];
        let initial = launch["argv"][2].as_str().unwrap();
        let witness =
            &writers[format!("{label}-{}", if data == CLEAR { "clear" } else { "start" })];
        let bound = binding(data, initial);
        assert_eq!(witness["pid"], launch["pid"]);
        assert_eq!(witness["cwd"], CWD);
        assert_eq!(witness["sessionId"], bound.current.as_str());
        assert_eq!(
            witness["startedAt"],
            writers[format!("{label}-start")]["startedAt"]
        );
        let (observed, actual) = replay(data, &bound);
        assert_eq!(observed, prompts);
        assert_eq!(actual, replies);
        let visible =
            &pair["visible"][format!("{label}-{}", if data == CLEAR { "end" } else { "long" })];
        for block in replies.into_iter().flat_map(|s| s.split("\n\n")) {
            assert!(
                visible
                    .as_array()
                    .unwrap()
                    .contains(&json!(format!("⏺ {block}")))
            );
        }
    }
}

#[test]
fn live_resume_has_two_writers_with_indistinguishable_turn_records() {
    let capture: Value = serde_json::from_str(CAPTURE).unwrap();
    let overlap = &capture["overlappingResume"];
    let a = &overlap["writers"][0];
    let b = &overlap["writers"][1];
    assert_ne!(a["pid"], b["pid"]);
    assert_eq!(a["sessionId"], b["sessionId"]);
    let file = fixture!("overlap.jsonl");
    let bound = binding(file, a["sessionId"].as_str().unwrap());
    assert_eq!(
        replay(file, &bound).1,
        ["Copper pebble", "Violet acorn", "Lime button"]
    );
    assert!(
        !overlap["visible"]["A-end"]
            .as_array()
            .unwrap()
            .contains(&json!("⏺ Violet acorn"))
    );
}

struct Turn {
    user: Value,
    assistant: Value,
    end: Value,
    binding: Binding,
}

impl Turn {
    fn recorded() -> Self {
        let mut rows = A.lines().skip(1).map(|s| serde_json::from_str(s).unwrap());
        let user: Value = rows.next().unwrap();
        Self {
            binding: binding(A, user["sessionId"].as_str().unwrap()),
            user,
            assistant: rows.next().unwrap(),
            end: rows.next().unwrap(),
        }
    }

    fn run(&self, rows: &[&Value], limit: usize) -> Result<Vec<Reply>, Gap> {
        let mut parser = ClaudeParser::new(Path::new(CWD).into());
        let mut replies = Vec::new();
        for row in rows {
            if let Record::End(Some(reply)) =
                parser.record(&self.binding, &serde_json::to_vec(&row).unwrap(), limit)?
            {
                replies.push(reply);
            }
        }
        Ok(replies)
    }
}

#[test]
fn only_the_latest_final_message_of_a_fresh_human_turn_can_publish() {
    let t = Turn::recorded();
    let mut tool = t.assistant.clone();
    tool["message"]["id"] = json!("tool");
    tool["message"]["content"] =
        json!([{"type":"text","text":"tool narration"},{"type":"tool_use"}]);
    tool["message"]["stop_reason"] = json!("tool_use");
    let mut result = t.user.clone();
    result["message"]["content"] = json!([{"type":"tool_result","content":"tool output"}]);
    let mut side = t.assistant.clone();
    side["isSidechain"] = json!(true);
    let mut meta = t.user.clone();
    meta["isMeta"] = json!(true);
    let mut error = t.assistant.clone();
    error["isApiErrorMessage"] = json!(true);
    let mut meta_error = error.clone();
    meta_error["isMeta"] = json!(true);
    let mut meta_abort = t.assistant.clone();
    meta_abort["isMeta"] = json!(true);
    meta_abort["isAbortedMidStream"] = json!(true);
    let mut meta_end = t.end.clone();
    meta_end["isMeta"] = json!(true);
    let mut unconfirmed = t.assistant.clone();
    unconfirmed["message"]["stop_reason"] = Value::Null;
    let extension = json!({"type":"future-kind","text":"unknown private bytes"});

    for rows in [
        vec![&meta, &t.assistant, &t.end],
        vec![&result, &t.assistant, &t.end],
        vec![&t.user, &side, &t.end],
        vec![&t.user, &t.assistant, &tool, &result, &t.end],
        vec![&t.user, &t.assistant, &error, &t.end],
        vec![&t.user, &t.assistant, &meta_error, &t.end],
        vec![&t.user, &t.assistant, &meta_abort, &t.end],
        vec![&t.user, &t.assistant, &meta_end],
        vec![&t.user, &unconfirmed, &t.end],
        vec![&t.user, &t.assistant, &t.user, &t.end],
        vec![&t.user, &t.assistant], // EOF
    ] {
        assert!(t.run(&rows, 50).unwrap().is_empty());
    }
    let rows = [
        &t.user,
        &tool,
        &result,
        &t.assistant,
        &extension,
        &side,
        &meta_end,
        &t.end,
        &t.end,
        &t.user,
        &t.assistant,
        &t.end,
    ];
    assert_eq!(
        t.run(&rows, 50)
            .unwrap()
            .into_iter()
            .map(|r| r.text)
            .collect::<Vec<_>>(),
        ["Copper pebble"; 2]
    );
}

#[test]
fn blocks_only_contribute_text_with_bounded_utf8_truncation() {
    let t = Turn::recorded();
    let mut msg = t.assistant.clone();
    msg["message"]["content"] = json!([
        {"type":"thinking","thinking":"private reasoning"},
        {"type":"text","text":"A"},{"type":"text","text":""},{"type":"text","text":"é"}
    ]);
    for (limit, expected, truncated) in [(5, "A\n\né", false), (4, "A\n\n", true), (1, "A", true)]
    {
        let replies = t.run(&[&t.user, &msg, &t.end], limit).unwrap();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].text, expected);
        assert_eq!(replies[0].truncated, truncated);
    }
    msg["message"]["content"] = json!(vec![json!({"type":"text","text":""}); 256]);
    msg["message"]["content"][0]["text"] = json!("first");
    assert_eq!(
        t.run(&[&t.user, &msg, &t.end], 50).unwrap()[0].text,
        "first"
    );
    let mut next = t.assistant.clone();
    next["apiBlockIndex"] = json!(256);
    assert_eq!(
        t.run(&[&t.user, &msg, &next], 50).unwrap_err(),
        Gap::Oversized
    );
    msg["message"]["id"] = json!("x".repeat(257));
    assert_eq!(t.run(&[&t.user, &msg], 50).unwrap_err(), Gap::Oversized);
}

#[test]
fn invalid_required_fields_cannot_turn_private_data_into_diagnostics_or_replies() {
    let t = Turn::recorded();
    for field in ["promptSource", "origin", "turnOrigin"] {
        let mut partial = t.user.clone();
        partial.as_object_mut().unwrap().remove(field);
        assert_eq!(
            t.run(&[&t.user, &t.assistant, &partial, &t.end], 50)
                .unwrap_err(),
            Gap::Format
        );
    }
    for (base, field, value, reason) in [
        (&t.user, "/message/content", Value::Null, Gap::Format),
        (&t.user, "/origin", json!(["human"]), Gap::Format),
        (
            &t.assistant,
            "/message",
            json!(["msg", "assistant", [{"type":"text","text":"sentinel"}], "end_turn"]),
            Gap::Format,
        ),
        (&t.assistant, "/message/id", Value::Null, Gap::Format),
        (
            &t.assistant,
            "/message/content",
            json!([["text", "sentinel"]]),
            Gap::Format,
        ),
        (
            &t.assistant,
            "/message/content",
            json!("not blocks"),
            Gap::Format,
        ),
        (
            &t.assistant,
            "/message/content/0/text",
            json!(42),
            Gap::Format,
        ),
        (&t.assistant, "/message/stop_reason", json!(42), Gap::Format),
        (&t.assistant, "/apiBlockIndex", json!(1), Gap::Format),
        (&t.end, "/subtype", Value::Null, Gap::Format),
        (&t.end, "/durationMs", json!("fast"), Gap::Format),
        (&t.end, "/isSidechain", json!(1), Gap::Format),
        (
            &t.assistant,
            "/cwd",
            json!("path-sentinel"),
            Gap::Uncorroborated,
        ),
        (
            &t.assistant,
            "/entrypoint",
            json!("sdk-cli"),
            Gap::Uncorroborated,
        ),
        (
            &t.assistant,
            "/sessionId",
            json!("id-sentinel"),
            Gap::SessionChanged,
        ),
    ] {
        let mut row = base.clone();
        *row.pointer_mut(field).unwrap() = value;
        let failure = t.run(&[&t.user, &row], 50).unwrap_err();
        assert_eq!(failure, reason);
        assert!(!format!("{failure:?}").contains("sentinel"));
    }
    let mut msg = t.assistant.clone();
    msg["message"]["content"][0]["text"] = json!("reply-sentinel");
    let mut user = t.user.clone();
    user["message"]["content"] = json!("prompt-sentinel");
    let replies = t.run(&[&user, &msg, &t.end], 50).unwrap();
    assert_eq!(replies[0].text, "reply-sentinel");
    assert!(!format!("{replies:?}").contains("sentinel"));
}

#[test]
fn contradictory_context_on_any_turn_boundary_invalidates_the_candidate() {
    let t = Turn::recorded();
    for row in [&t.user, &t.assistant, &t.end] {
        let mut foreign = row.clone();
        foreign["session_id"] = json!("other-launch");
        assert_eq!(
            t.run(&[&t.user, &t.assistant, &foreign, &t.end], 50)
                .unwrap_err(),
            Gap::Uncorroborated
        );
    }
    let unsupported = json!({"type":"mode","sessionId":t.binding.current.as_str(),"mode":"other"});
    assert_eq!(
        t.run(&[&t.user, &t.assistant, &unsupported, &t.end], 50)
            .unwrap_err(),
        Gap::Uncorroborated
    );
    let mut narration = t.assistant.clone();
    narration["message"]["stop_reason"] = json!("tool_use");
    let mut continuation = t.assistant.clone();
    continuation["apiBlockIndex"] = json!(1);
    assert!(
        t.run(&[&t.user, &narration, &continuation, &t.end], 50)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn replayed_assistant_ids_cannot_replace_a_later_message_in_the_same_turn() {
    let t = Turn::recorded();
    let mut newer = t.assistant.clone();
    newer["message"]["id"] = json!("next-message");
    newer["message"]["content"][0]["text"] = json!("later reply");
    let mut result = t.user.clone();
    result["message"]["content"] = json!([{"type":"tool_result"}]);
    for boundary in [&newer, &result] {
        assert_eq!(
            t.run(&[&t.user, &t.assistant, boundary, &t.assistant, &t.end], 50)
                .unwrap_err(),
            Gap::Format
        );
    }
    assert_eq!(
        t.run(&[&t.user, &t.assistant, &newer, &t.end], 50).unwrap()[0].text,
        "later reply"
    );
}

#[test]
fn distinct_message_ids_are_bounded_per_turn() {
    let t = Turn::recorded();
    let mut message = t.assistant.clone();
    let user = t.user.to_string();
    let mut parser = ClaudeParser::new(CWD.into());
    for _ in 0..2 {
        parser.record(&t.binding, user.as_bytes(), 1).unwrap();
        for i in 0..256 {
            message["message"]["id"] = json!(format!("message-{i}"));
            parser
                .record(&t.binding, message.to_string().as_bytes(), 1)
                .unwrap();
        }
    }
    let extra = t.assistant.to_string();
    assert!(matches!(
        parser.record(&t.binding, extra.as_bytes(), 1),
        Err(Gap::Oversized)
    ));
}

#[test]
fn malformed_text_gaps_even_when_no_reply_bytes_would_be_retained() {
    let t = Turn::recorded();
    let mut unarmed = t.assistant.clone();
    unarmed["message"]["content"] = json!([{"type":"text"}]);
    let mut result = t.user.clone();
    result["message"]["content"] = json!([{"type":"tool_result"},{"type":"text","text":false}]);
    for row in [&unarmed, &result, &json!(["future-extension", false])] {
        assert_eq!(t.run(&[row], 1).unwrap_err(), Gap::Format);
    }
    for invalid in [r"\uDC00", r"\uD83D", r"\uD83D\u0041"] {
        let mut parser = ClaudeParser::new(CWD.into());
        let user = t.user.to_string();
        let assistant = t.assistant.to_string().replace(
            "Copper pebble",
            &format!("{}{}", r"\u0061".repeat(2048), invalid),
        );
        let malformed_prompt = user.replace("short-A", invalid);
        assert!(matches!(
            parser.record(&t.binding, malformed_prompt.as_bytes(), 1),
            Err(Gap::Format)
        ));
        assert!(matches!(
            parser.record(&t.binding, user.as_bytes(), 1),
            Ok(Record::User)
        ));
        assert!(matches!(
            parser.record(&t.binding, assistant.as_bytes(), 1),
            Err(Gap::Format)
        ));
    }
}

#[test]
fn queued_escaped_replies_fit_the_decoded_byte_budget() {
    let t = Turn::recorded();
    let user = t.user.to_string();
    let end = t.end.to_string();
    let assistant = t
        .assistant
        .to_string()
        .replace("Copper pebble", &r"\u0061".repeat(1024));
    let reply_limit = 6 * 1024;
    let queue_budget = 8 * 1024;
    let mut parser = ClaudeParser::new(CWD.into());
    let mut queued = Vec::new();
    for _ in 0..8 {
        parser
            .record(&t.binding, user.as_bytes(), reply_limit)
            .unwrap();
        parser
            .record(&t.binding, assistant.as_bytes(), reply_limit)
            .unwrap();
        let Record::End(Some(reply)) = parser
            .record(&t.binding, end.as_bytes(), reply_limit)
            .unwrap()
        else {
            panic!("expected a complete reply");
        };
        assert_eq!(reply.text.len(), 1024);
        assert!(!reply.truncated);
        queued.push(reply);
    }
    let allocation: usize = queued.iter().map(|reply| reply.text.capacity()).sum();
    assert!(
        allocation <= queue_budget,
        "retained {allocation} bytes for {queue_budget} decoded bytes"
    );
}

#[test]
fn escaped_text_obeys_the_decoded_utf8_reply_budget() {
    let t = Turn::recorded();
    let user = t.user.to_string();
    let end = t.end.to_string();
    let assistant = t
        .assistant
        .to_string()
        .replace("Copper pebble", r#"A\u00E9\uD83D\uDE80\n\t\"\\\/\b\f\r"#);
    for (limit, expected, truncated) in [
        (2, "A", true),
        (3, "Aé", true),
        (6, "Aé", true),
        (7, "Aé🚀", true),
        (14, "Aé🚀\n\t\"\\/\x08\x0c", true),
        (15, "Aé🚀\n\t\"\\/\x08\x0c\r", false),
    ] {
        let mut parser = ClaudeParser::new(CWD.into());
        parser.record(&t.binding, user.as_bytes(), limit).unwrap();
        parser
            .record(&t.binding, assistant.as_bytes(), limit)
            .unwrap();
        let Record::End(Some(reply)) = parser.record(&t.binding, end.as_bytes(), limit).unwrap()
        else {
            panic!("expected a complete reply");
        };
        assert_eq!(
            (reply.text.as_str(), reply.truncated),
            (expected, truncated)
        );
    }
}

#[test]
fn transcript_fields_match_escaped_keys_and_reject_duplicate_spellings() {
    let t = Turn::recorded();
    let mut parser = ClaudeParser::new(CWD.into());
    let user = t.user.to_string();
    let mut assistant = t.assistant.clone();
    assistant["message"]["content"][0]["extra"] = json!({
        "text": [r#"nested } , ] : \\ "text""#, {"content": []}],
    });
    let assistant = assistant
        .to_string()
        .replace(r#""message":"#, r#""m\u0065ssage" : "#)
        .replace(r#""content":"#, r#""cont\u0065nt" : "#)
        .replace(r#""text":"#, r#""te\u0078t" : "#);
    let spaced = format!(" \t {assistant} \r");
    parser.record(&t.binding, user.as_bytes(), 256).unwrap();
    parser.record(&t.binding, spaced.as_bytes(), 256).unwrap();
    let Record::End(Some(reply)) = parser
        .record(&t.binding, t.end.to_string().as_bytes(), 256)
        .unwrap()
    else {
        panic!("expected a recorded turn")
    };
    assert_eq!(reply.text, "Copper pebble");

    let duplicate = assistant.replace(r#""te\u0078t""#, r#""text":null,"te\u0078t""#);
    assert!(matches!(
        parser.record(&t.binding, duplicate.as_bytes(), 256),
        Err(Gap::Format)
    ));
}

#[test]
fn ambiguous_fields_and_oversized_arrays_fail_before_a_reply_is_returned() {
    let t = Turn::recorded();
    let mut parser = ClaudeParser::new(CWD.into());
    let row = t.user.to_string().replacen(
        r#""isSidechain":false"#,
        r#""isSidechain":true,"isSidechain":false"#,
        1,
    );
    assert!(matches!(
        parser.record(&t.binding, row.as_bytes(), 256),
        Err(Gap::Format)
    ));
    let mut msg = t.assistant.clone();
    msg["message"]
        .as_object_mut()
        .unwrap()
        .remove("stop_reason");
    assert_eq!(t.run(&[&t.user, &msg], 256).unwrap_err(), Gap::Format);
    msg = t.assistant.clone();
    msg["message"]["content"] = json!(vec![json!({"type":"thinking"}); 257]);
    assert_eq!(t.run(&[&t.user, &msg], 256).unwrap_err(), Gap::Oversized);
    let mut empty = t.user.clone();
    for content in [json!(""), json!([]), json!([{"type":"image"}])] {
        empty["message"]["content"] = content;
        assert!(
            t.run(&[&empty, &t.assistant, &t.end], 256)
                .unwrap()
                .is_empty()
        );
    }
}
