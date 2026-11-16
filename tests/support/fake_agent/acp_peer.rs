//! Credential-free stable-v1 wire peer. No real provider or filesystem operations.
use serde_json::{Value, json};
use std::io::{BufRead, Write};
fn emit(v: Value) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{v}").unwrap();
    out.flush().unwrap();
}
fn response(request: &Value, result: Value) {
    emit(json!({"jsonrpc":"2.0","id":request["id"],"result":result}));
}
fn update(text: &str) {
    emit(
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text},"future":true}}}),
    );
}
pub(super) fn run(scene: &str) {
    assert!(std::env::var_os("OPENAI_API_KEY").is_none());
    let fixture: Value =
        serde_json::from_str(include_str!("../../fixtures/acp/stable-v1.json")).unwrap();
    let mut prompt = None;
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        match request["method"].as_str() {
            Some("initialize") => {
                assert_eq!(request["params"]["protocolVersion"], 1);
                assert_eq!(request["params"]["clientCapabilities"], json!({}));
                let mut initialized = fixture["initialize"].clone();
                if scene == "version" {
                    initialized["protocolVersion"] = json!(2);
                }
                response(&request, initialized);
            }
            Some("session/new") => response(&request, json!({"sessionId":"s"})),
            Some("session/load" | "session/resume") => {
                assert_eq!(request["params"]["sessionId"], "s");
                update("history");
                response(&request, json!({"future":true}));
            }
            Some("session/prompt") => {
                assert_eq!(request["params"]["sessionId"], "s");
                if scene == "eof" {
                    return;
                }
                if scene == "silent" {
                    continue;
                }
                if scene == "remote-error" {
                    emit(
                        json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"synthetic failure"}}),
                    );
                    continue;
                }
                update("hello");
                if scene == "permission" || scene == "cancel" {
                    prompt = Some(request);
                    emit(
                        json!({"jsonrpc":"2.0","id":17,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"tool","title":"Read"},"options":[{"optionId":"yes","name":"Allow","kind":"allow_once"},{"optionId":"no","name":"Reject","kind":"reject_once"}]}}),
                    );
                } else {
                    emit(
                        json!({"jsonrpc":"2.0","id":91,"method":"fs/read_text_file","params":{"sessionId":"s","path":"unused"}}),
                    );
                    response(&request, json!({"stopReason":"end_turn","future":true}));
                    update("late");
                }
            }
            Some("session/cancel") => {}
            None if request["id"] == 17 => {
                let outcome = &request["result"]["outcome"];
                if scene == "cancel" {
                    assert_eq!(outcome["outcome"], "cancelled");
                } else {
                    assert_eq!(outcome["optionId"], "no");
                }
                response(
                    &prompt.take().unwrap(),
                    json!({"stopReason":if scene=="cancel"{"cancelled"}else{"end_turn"}}),
                );
            }
            None if request["id"] == 91 => {
                assert_eq!(request["error"]["code"], -32601);
            }
            _ => panic!("unexpected synthetic request"),
        }
    }
}
