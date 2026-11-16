//! Synthetic pinned Claude stream-json peer; no SDK, config or credentials.
use serde_json::{Value, json};
use std::io::{BufRead, Write};
fn emit(value: Value) {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{value}").unwrap();
    stdout.flush().unwrap();
}
pub(super) fn run(scene: &str) {
    assert!(std::env::var_os("CLAUDECODE").is_none());
    assert!(std::env::var_os("ANTHROPIC_API_KEY").is_none());
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../fixtures/claude-stream-json/events.json"
    ))
    .unwrap();
    for line in std::io::stdin().lock().lines() {
        let value: Value = serde_json::from_str(&line.unwrap()).unwrap();
        match value["type"].as_str() {
            Some("control_request") => {
                if value["request"]["subtype"] == "initialize" {
                    emit(fixtures["init"].clone());
                }
                emit(
                    json!({"type":"control_response","response":{"subtype":"success","request_id":value["request_id"],"response":{}}}),
                );
            }
            Some("user") => {
                if scene == "before-result-eof" {
                    return;
                }
                if scene == "silent" {
                    continue;
                }
                if scene == "large" {
                    let text = value["message"]["content"].as_str().unwrap();
                    assert_eq!(text.len(), 6 * 1024 * 1024);
                    emit(
                        json!({"type":"assistant","message":{"id":"large","content":[{"type":"text","text":text}]}}),
                    );
                    let mut approval = fixtures["approval"].clone();
                    approval["request"]["input"]["file_path"] = json!(text);
                    emit(approval);
                    continue;
                }
                let mut result = fixtures["result"].clone();
                result["user_message_uuids"][0] = value["uuid"].clone();
                if scene == "failed-result" {
                    result["subtype"] = json!("error_during_execution");
                    result["is_error"] = json!(true);
                } else if scene == "unknown-result" {
                    result["subtype"] = json!("future_result");
                }
                emit(result);
                if scene == "eof" {
                    return;
                }
                emit(
                    json!({"type":"system","subtype":"session_state_changed","state":"running","session_id":"provider-session"}),
                );
                emit(fixtures["approval"].clone());
            }
            Some("control_response") => emit(
                json!({"type":"system","subtype":"session_state_changed","state":"idle","session_id":"provider-session"}),
            ),
            _ => panic!("unexpected fixture input"),
        }
    }
}
