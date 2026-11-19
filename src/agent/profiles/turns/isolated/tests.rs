use std::path::Path;

use serde_json::{Value, json};

use super::parser::{ProviderParser, header};
use crate::agent::turns::{Binding, Gap, Parser, ProviderId, Record, TranscriptSource};

const ID: &str = "22222222-2222-4222-8222-222222222222";
const OTHER: &str = "33333333-3333-4333-8333-333333333333";

fn feed(parser: &mut ProviderParser, record: Value, bytes: usize) -> Result<Record, Gap> {
    let b = Binding {
        initial: ProviderId::new("initial").unwrap(),
        current: ProviderId::new(ID).unwrap(),
        revision: 0,
        relative: "session".into(),
    };
    parser.record(&b, record.to_string().as_bytes(), bytes)
}

fn codex(event: &str, turn: &str, text: Value) -> Value {
    json!({"type":"event_msg", "payload":{"type":event, "turn_id":turn, "last_agent_message":text}})
}

#[test]
fn codex_requires_the_bound_interactive_origin_and_matching_explicit_turn_id() {
    let record = json!({"type":"session_meta","payload":{"id":ID,"cli_version":"0.154.0","originator":"codex-tui","source":"cli","thread_source":"user","cwd":"/fixture"}});
    let id = ProviderId::new(ID).unwrap();
    let accept = |v: &Value| {
        header(
            TranscriptSource::Codex,
            v.to_string().as_bytes(),
            Path::new("/fixture"),
            &id,
        )
    };
    assert_eq!(accept(&record), Ok(()));
    for (key, value) in [
        ("originator", "codex_exec"),
        ("source", "appServer"),
        ("thread_source", "subagent"),
        ("cwd", "/other"),
        ("id", OTHER),
        ("cli_version", "future"),
    ] {
        let mut wrong = record.clone();
        wrong["payload"][key] = json!(value);
        assert_eq!(accept(&wrong), Err(Gap::Uncorroborated));
    }
    let mut parser = ProviderParser::new(TranscriptSource::Codex);
    assert!(matches!(
        feed(&mut parser, codex("task_complete", ID, json!("stale")), 20),
        Ok(Record::End(None))
    ));
    for end in ["turn_aborted", "task_complete"] {
        feed(&mut parser, codex("task_started", ID, Value::Null), 20).unwrap();
        assert!(matches!(
            feed(&mut parser, codex(end, ID, Value::Null), 20),
            Ok(Record::End(None))
        ));
    }
    feed(&mut parser, codex("task_started", ID, Value::Null), 20).unwrap();
    assert!(matches!(
        feed(
            &mut parser,
            codex("task_complete", OTHER, json!("wrong")),
            20
        ),
        Err(Gap::Format)
    ));
    feed(&mut parser, codex("task_started", ID, Value::Null), 20).unwrap();
    assert!(
        matches!(feed(&mut parser,codex("task_complete",ID,json!("Ö🙂")),3),Ok(Record::End(Some(r))) if r.text=="Ö" && r.truncated)
    );
}

fn pi(id: &str, parent: Option<&str>, msg: Value) -> Value {
    json!({"type":"message","id":id,"parentId":parent,"message":msg})
}

#[test]
fn pi_discards_tool_private_error_and_branched_candidates_before_inference() {
    let start = json!({"role":"user","content":[{"type":"text","text":"prompt"}]});
    let final_msg = json!({"role":"assistant","stopReason":"stop","content":[{"type":"thinking","thinking":"private"},{"type":"text","text":" \t"},{"type":"text","text":"result"}]});
    for stop in [
        pi(
            "c",
            Some("b"),
            json!({"role":"toolResult","content":"secret"}),
        ),
        pi(
            "c",
            Some("b"),
            json!({"role":"assistant","stopReason":"stop","content":[{"type":"toolCall","name":"private"}]}),
        ),
        pi(
            "c",
            Some("b"),
            json!({"role":"assistant","stopReason":"aborted","content":[{"type":"text","text":"error"}]}),
        ),
        pi("c", Some("foreign"), final_msg.clone()),
    ] {
        let mut p = ProviderParser::new(TranscriptSource::Pi);
        feed(&mut p, pi("a", None, start.clone()), 256).unwrap();
        feed(&mut p, pi("b", Some("a"), final_msg.clone()), 256).unwrap();
        assert!(p.has_quiet_candidate());
        feed(&mut p, stop, 256).unwrap();
        assert!(p.quiet().is_none());
    }
    let mut p = ProviderParser::new(TranscriptSource::Pi);
    feed(&mut p, pi("a", None, start), 256).unwrap();
    feed(&mut p, pi("b", Some("a"), final_msg), 256).unwrap();
    assert!(
        matches!(feed(&mut p,pi("c",Some("b"),json!({"role":"bashExecution","command":"pwd"})),256),Ok(Record::End(Some(reply))) if reply.text=="result")
    );
}

fn cursor(role: &str, blocks: Value) -> Value {
    json!({"role":role,"message":{"content":blocks}})
}

#[test]
fn cursor_clears_prior_tool_narration_and_distinguishes_a_missing_end_from_success() {
    for wrong in [
        json!({"role":"turn_ended","status":"success"}),
        json!({"type":"user","message":{"content":[]}}),
        json!({"type":"turn_ended","status":"unknown"}),
    ] {
        assert_eq!(
            super::parser::cursor_marker(wrong.to_string().as_bytes()),
            Err(Gap::Format)
        );
    }
    let mut p = ProviderParser::new(TranscriptSource::CursorWithUnseparatedText);
    let prompt = cursor(
        "user",
        json!([{"type":"text","text":"<user_query>hello</user_query>"}]),
    );
    feed(&mut p, prompt.clone(), 256).unwrap();
    feed(&mut p,cursor("assistant",json!([{"type":"text","text":"earlier"},{"type":"tool_use","input":{"private":"tool arguments"},"name":"test"}])),256).unwrap();
    feed(
        &mut p,
        cursor("assistant", json!([{"type":"text","text":"later"}])),
        256,
    )
    .unwrap();
    assert!(
        matches!(feed(&mut p,prompt.clone(),256),Ok(Record::Next(Some(r))) if r.inferred && r.text=="later")
    );
    assert!(matches!(
        feed(&mut p, json!({"type":"turn_ended","status":"aborted"}), 256),
        Ok(Record::End(None))
    ));
    assert!(matches!(feed(&mut p, prompt, 256), Ok(Record::Next(None))));
    assert!(matches!(
        feed(&mut p, json!({"role":"assistant","message":{}}), 256),
        Err(Gap::Format)
    ));
}
