//! Synthetic, credential-free app-server peer; deliberately reordered failure cases.
use serde_json::{Value, json};
use std::io::{BufRead, Write};

fn emit(value: &Value) {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{value}").unwrap();
    stdout.flush().unwrap();
}
fn response(request: &Value, result: &Value) {
    emit(&json!({"id":request["id"],"result":result}));
}
fn subagent_notifications(fixtures: &Value) {
    emit(&json!({"method":"thread/started","params":{"thread":{"id":"child"}}}));
    for key in ["started", "delta", "completed"] {
        let mut event = fixtures[key].clone();
        event["params"]["threadId"] = json!("child");
        emit(&event);
    }
    emit(&json!({"method":"serverRequest/resolved","params":{"threadId":"child","requestId":41}}));
}
fn park() {
    loop {
        std::thread::park();
    }
}

pub(super) fn run(scene: &str) {
    assert!(std::env::var_os("CODEX_THREAD_ID").is_none());
    assert!(std::env::var_os("OPENAI_API_KEY").is_none());
    let fixtures: Value =
        serde_json::from_str(include_str!("../../fixtures/codex-app-server/v2.json")).unwrap();
    let mut initialized = false;
    let mut turn_request = None;
    let mut turn_starts = 0;
    let mut thread_requests = 0;
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        match request.get("method").and_then(Value::as_str) {
            Some("initialize") => response(&request, &fixtures["initialize"]),
            Some("initialized") => {
                initialized = true;
                emit(&json!({"method":"fixture/ready","params":{}}));
            }
            Some("thread/start" | "thread/resume") => {
                thread_requests += 1;
                assert!(initialized, "initialize/initialized ordering");
                if scene == "resume-missing" {
                    emit(
                        &json!({"id":request["id"],"error":{"code":-32600,"message":"history missing"}}),
                    );
                    continue;
                }
                if scene == "resume-active" {
                    assert_eq!(request["params"]["threadId"], "t");
                    let mut result = fixtures["activeThread"].clone();
                    if request["params"]["excludeTurns"] == true {
                        result["thread"]["turns"] = json!([]);
                    }
                    result["fixtureThreadRequests"] = json!(thread_requests);
                    response(&request, &result);
                    continue;
                }
                if matches!(scene, "resume" | "resume-file-approval") {
                    assert_eq!(request["params"]["threadId"], "t");
                    emit(
                        &fixtures[if scene == "resume-file-approval" {
                            "fileApproval"
                        } else {
                            "approval"
                        }],
                    );
                }
                if scene == "subagent" {
                    subagent_notifications(&fixtures);
                }
                response(&request, &fixtures["thread"]);
                if scene == "blocked" {
                    emit(&json!({"method":"fixture/blocked","params":{}}));
                    park();
                }
            }
            Some("turn/start") => {
                turn_starts += 1;
                if let Some(path) = std::env::var_os("AGENT_LINK_TEST_START_COUNT") {
                    std::fs::write(path, turn_starts.to_string()).unwrap();
                }
                if let Some(path) = std::env::var_os("AGENT_LINK_TEST_START_MARKER") {
                    std::fs::write(path, b"received").unwrap();
                }
                assert_eq!(request["params"]["threadId"], "t");
                if scene == "large" {
                    let text = request["params"]["input"][0]["text"].as_str().unwrap();
                    assert_eq!(text.len(), 6 * 1024 * 1024);
                    emit(&fixtures["started"]);
                    let mut delta = fixtures["delta"].clone();
                    delta["params"]["delta"] = json!(text);
                    emit(&delta);
                    let mut approval = fixtures["approval"].clone();
                    approval["params"]["command"] = json!(text);
                    emit(&approval);
                    turn_request = Some(request);
                    continue;
                }
                if scene == "terminal-response" {
                    response(
                        &request,
                        &json!({"turn":fixtures["completed"]["params"]["turn"]}),
                    );
                    continue;
                }
                if scene == "lost-receipt" {
                    emit(&fixtures["completed"]);
                    continue;
                }
                if scene == "stale-terminal" {
                    let mut stale = fixtures["completed"].clone();
                    stale["params"]["turn"]["id"] = json!("unrelated");
                    emit(&stale);
                }
                if scene == "hang" {
                    if let Some(path) = std::env::var_os("AGENT_LINK_TEST_HANG_MARKER") {
                        std::fs::write(path, b"received").unwrap();
                    }
                    emit(&json!({"method":"fixture/accepted","params":{}}));
                    continue;
                }
                if scene == "eof" {
                    return;
                }
                if scene == "invalid" {
                    println!("not JSON");
                    std::io::stdout().flush().unwrap();
                    park();
                }
                if scene == "utf8" {
                    std::io::stdout().write_all(b"\xff\n").unwrap();
                    std::io::stdout().flush().unwrap();
                    park();
                }
                if scene == "flood" {
                    for _ in 0..10 {
                        emit(&fixtures["delta"]);
                    }
                    response(&request, &json!({"turn":{"id":"a","status":"inProgress"}}));
                    continue;
                }
                emit(&fixtures["started"]);
                emit(&fixtures["delta"]);
                if scene == "projection" {
                    for key in ["commentary", "final", "tool", "usage", "extension"] {
                        emit(&fixtures[key]);
                    }
                }
                if scene == "subagent" {
                    subagent_notifications(&fixtures);
                }
                if scene == "response-exit" {
                    response(&request, &json!({"turn":{"id":"a","status":"inProgress"}}));
                    emit(&fixtures["completed"]);
                    return;
                }
                if matches!(scene, "approval" | "file-approval" | "unsupported") {
                    turn_request = Some(request);
                    if scene == "unsupported" {
                        emit(&json!({"id":"unknown","method":"future/request","params":{}}));
                    } else if scene == "file-approval" {
                        emit(&fixtures["fileApproval"]);
                    } else {
                        emit(&fixtures["approval"]);
                    }
                } else if matches!(scene, "interrupt" | "subagent") {
                    response(&request, &json!({"turn":{"id":"a","status":"inProgress"}}));
                } else {
                    emit(&fixtures["completed"]); // Terminal notification BEFORE response.
                    response(&request, &json!({"turn":{"id":"a","status":"inProgress"}}));
                }
            }
            Some("turn/steer") => {
                assert_eq!(request["params"]["expectedTurnId"], "a");
                response(
                    &request,
                    &json!({"turnId":"a", "fixtureTurnStarts":turn_starts}),
                );
            }
            Some("turn/interrupt") => {
                if let Some(path) = std::env::var_os("AGENT_LINK_TEST_START_MARKER") {
                    std::fs::write(path, b"received").unwrap();
                }
                assert_eq!(request["params"]["turnId"], "a");
                let mut completed = fixtures["completed"].clone();
                completed["params"]["turn"]["status"] = json!("interrupted");
                emit(&completed);
                response(&request, &json!({}));
            }
            None => {
                if scene == "unsupported" {
                    assert_eq!(request["id"], "unknown");
                    assert_eq!(request["error"]["code"], -32601);
                } else {
                    assert!(matches!(
                        request["result"]["decision"].as_str(),
                        Some("accept" | "decline")
                    ));
                    assert_eq!(
                        request["id"],
                        if matches!(scene, "file-approval" | "resume-file-approval") {
                            json!("files")
                        } else {
                            json!(41)
                        }
                    );
                }
                if let Some(original) = turn_request.take() {
                    emit(&fixtures["completed"]);
                    response(&original, &json!({"turn":{"id":"a","status":"inProgress"}}));
                }
            }
            _ => panic!("unexpected fixture method"),
        }
    }
}
