use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::*;
use crate::agent::turns::TurnOptions;
use crate::env::EnvPolicy;

use crate::agent::loopback::{http_stub, opencode_stub};

use http_stub::{Reply, Stub, basic};
use opencode_stub::{OpenCode, SESSION, assistant, assistant_to, now_ms, text_part, user};

const PASSWORD: &str = "0123456789abcdef0123456789abcdef";

fn events(body: &[u8], reply_bytes: usize) -> serde_json::Result<Vec<AmbientFeedEvent>> {
    let mut page = turn_page(body, None, 0, usize::MAX, reply_bytes)?;
    if let Some((_, turn)) = page.trailing.take() {
        page.push(turn);
    }
    Ok(page.events)
}

/// The rules without a server: the turn end a message list shows, the events, the launch.
mod rules {
    use super::*;

    /// A turn end's key, end (ms) and reply text.
    type Told = Vec<(String, u64, Option<String>)>;

    fn turn(body: &str) -> Result<Told, &'static str> {
        events(body.as_bytes(), 1024)
            .map_err(|_| "invalid list")
            .map(|events| {
                events
                    .into_iter()
                    .map(|event| match event {
                        AmbientFeedEvent::Ended { key, ended, reply } => (
                            key,
                            ended.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
                            reply.map(|r| r.text),
                        ),
                        other => panic!("{other:?}"),
                    })
                    .collect()
            })
    }

    fn list(messages: &[String]) -> String {
        format!("[{}]", messages.join(","))
    }

    #[test]
    fn the_reply_is_the_stopped_message_s_own_text_parts() {
        let parts = [
            r#"{"type":"step-start"}"#,
            &text_part("one"),
            r#"{"type":"reasoning","text":"thought"}"#,
            r#"{"type":"tool","metadata":{"providerExecuted":true},"state":{"output":"tool output"}}"#,
            r#"{"type":"text","text":"synthetic","synthetic":true}"#,
            r#"{"type":"text","text":"ignored","ignored":true}"#,
            r#"{"type":"text","text":"kept","synthetic":false,"ignored":false}"#,
            &text_part(" \n "),
            r#"{"type":"text"}"#,
        ]
        .join(",");
        let body = list(&[user("u", "hi"), assistant("a", "\"stop\"", 2000, &parts)]);
        assert_eq!(
            turn(&body),
            Ok(vec![("a".into(), 2000, Some("one\n\nkept".into()))])
        );
    }

    #[test]
    fn a_turn_that_did_not_stop_ends_without_a_reply_and_no_earlier_one_stands_in() {
        for finish in ["\"length\"", "\"error\"", "null"] {
            let body = list(&[
                user("u1", "a"),
                assistant_to("old", "u1", "\"stop\"", 1000, &text_part("old")),
                user("u2", "b"),
                assistant_to("a", "u2", finish, 2000, &text_part("partial")),
            ]);
            assert_eq!(
                turn(&body),
                Ok(vec![
                    ("old".into(), 1000, Some("old".into())),
                    ("a".into(), 2000, None)
                ]),
                "{finish}"
            );
        }
    }

    #[test]
    fn a_new_running_message_after_idle_keeps_its_id_for_its_own_completion() {
        let old = assistant("a", "\"stop\"", 1000, &text_part("first"));
        let mut running = serde_json::json!({"info":{"id":"b","role":"assistant","parentID":"u","time":{"created":2000}},"parts":[{"type":"text","text":"second"}]});
        assert_eq!(turn(&list(&[old.clone(), running.to_string()])), Ok(vec![]));
        running["info"]["time"]["completed"] = 2100.into();
        running["info"]["finish"] = "stop".into();
        assert_eq!(
            turn(&list(&[old, running.to_string()])),
            Ok(vec![("b".into(), 2100, Some("second".into()))])
        );
    }

    #[test]
    fn a_compaction_summary_is_not_the_reply_and_completed_falls_back_to_created() {
        let answer = r#"{"info":{"id":"a","role":"assistant","parentID":"u","time":{"created":1500},"finish":"stop"},"parts":[{"type":"text","text":"answer"}]}"#;
        let summary = r#"{"info":{"id":"s","role":"assistant","parentID":"u","summary":true,"time":{"created":1600,"completed":1700},"finish":"stop"},"parts":[{"type":"text","text":"summary"}]}"#;
        // A user message's summary is an object, not a compaction mark.
        let body = format!("[{},{answer},{summary}]", user("u", "q"));
        assert_eq!(
            turn(&body),
            Ok(vec![("a".into(), 1500, Some("answer".into()))])
        );
    }

    #[test]
    fn auto_compaction_keeps_tool_steps_inside_the_original_turn() {
        let marker =
            r#"{"info":{"id":"c","role":"user"},"parts":[{"type":"compaction","auto":true}]}"#;
        let summary = r#"{"info":{"id":"s","role":"assistant","parentID":"u","summary":true,"finish":"stop","time":{"created":3}},"parts":[{"type":"text","text":"summary"}]}"#;
        let continuation = r#"{"info":{"id":"k","role":"user"},"parts":[{"type":"text","synthetic":true,"metadata":{"compaction_continue":true},"text":"Continue"}]}"#;
        assert_eq!(
            turn(&list(&[
                user("u", "original"),
                assistant("step", "\"tool-calls\"", 2, ""),
                continuation.into(),
            ])),
            Ok(vec![])
        );
        // On overflow OpenCode can replay the original user parts without a synthetic flag.
        for next in [continuation.to_owned(), user("replay", "original")] {
            let mut rows = vec![
                user("u", "original"),
                assistant("step", "\"tool-calls\"", 2, ""),
                marker.into(),
                summary.into(),
                next.clone(),
            ];
            assert_eq!(turn(&list(&rows)), Ok(vec![]));
            rows.push(assistant_to(
                "a",
                if next.contains("replay") {
                    "replay"
                } else {
                    "k"
                },
                "\"stop\"",
                4,
                &text_part("answer"),
            ));
            assert_eq!(
                turn(&list(&rows)),
                Ok(vec![("a".into(), 4, Some("answer".into()))])
            );
            rows.push(user("u2", "Continue"));
            rows.push(assistant_to("b", "u2", "\"stop\"", 5, &text_part("next")));
            assert_eq!(
                turn(&list(&rows)),
                Ok(vec![
                    ("a".into(), 4, Some("answer".into())),
                    ("b".into(), 5, Some("next".into())),
                ])
            );
        }
    }

    #[test]
    fn queued_prompts_and_stopped_tool_steps_do_not_publish_narration() {
        for state in ["pending", "running", "completed", "error"] {
            let parts = format!(
                r#"{},{{"type":"tool","state":{{"status":"{state}"}}}}"#,
                text_part("working")
            );
            let mut rows = vec![
                user("u1", "first"),
                assistant_to("step", "u1", "\"stop\"", 10, &parts),
                user("u2", "queued"),
            ];
            assert_eq!(turn(&list(&rows)), Ok(vec![]), "{state}");
            // Even the last step in an idle snapshot needs a tool follow-up.
            assert_eq!(turn(&list(&rows[..2])), Ok(vec![]), "{state}");
            // The first prompt can keep writing after the second was saved.
            rows.push(assistant_to("step2", "u1", "\"stop\"", 20, &parts));
            assert_eq!(turn(&list(&rows)), Ok(vec![]), "{state}");
            rows.push(assistant_to(
                "answer",
                "u2",
                "\"stop\"",
                30,
                &text_part("final"),
            ));
            assert_eq!(
                turn(&list(&rows)),
                Ok(vec![("answer".into(), 30, Some("final".into()))])
            );
        }
    }

    #[test]
    fn provider_tools_and_interrupted_orphans_allow_a_final_reply() {
        for tool in [
            r#"{"type":"tool","metadata":{"providerExecuted":true},"state":{"status":"completed"}}"#,
            r#"{"type":"tool","state":{"status":"error","metadata":{"interrupted":true}}}"#,
        ] {
            let parts = format!("{},{}", text_part("final"), tool);
            assert_eq!(
                turn(&list(&[
                    user("u", "q"),
                    assistant("a", "\"stop\"", 10, &parts)
                ])),
                Ok(vec![("a".into(), 10, Some("final".into()))])
            );
        }
    }

    #[test]
    fn an_assistant_for_an_older_prompt_is_not_the_idle_tail() {
        let rows = vec![
            user("u1", "first"),
            user("u2", "queued"),
            assistant_to("old", "u1", "\"stop\"", 10, &text_part("old answer")),
        ];
        assert_eq!(turn(&list(&rows)), Ok(vec![]));
    }

    #[test]
    fn a_session_without_an_assistant_message_has_no_turn_end() {
        assert_eq!(turn("[]"), Ok(vec![]));
        assert_eq!(turn(&list(&[user("u", "q")])), Ok(vec![]));
    }

    #[test]
    fn a_message_list_of_another_shape_is_unrecognised() {
        for body in [
            r#"{"messages":[]}"#,
            r#"[{"info":{"id":"a"},"parts":[]}]"#,
            r#"[{"info":{"id":"a","role":"assistant","parentID":"u","finish":"stop"},"parts":[]}]"#,
            r#"[{"info":{"id":"a","role":"assistant","parentID":"u","finish":"stop","time":{"created":1}}}]"#,
            r#"[{"info":{"id":"a","role":"assistant","parentID":"u","time":{"created":1}},"parts":null,"parts":[]}]"#,
            r#"[{"info":{"id":"a","role":"assistant","parentID":"u","finish":"stop","time":{"created":1}},"parts":[{"type":"text","text":"already full"},false]}]"#,
            r#"[{"parts":[]}]"#,
            "not json",
        ] {
            assert!(turn(body).is_err(), "{body}");
        }
    }

    #[test]
    fn the_reply_is_kept_within_reply_bytes_and_flagged() {
        let parts = format!("{},{}", text_part("abcdef"), text_part("ghij"));
        let body = list(&[assistant("a", "\"stop\"", 5, &parts)]);
        let page = events(body.as_bytes(), 7).unwrap();
        let [
            AmbientFeedEvent::Ended {
                reply: Some(reply), ..
            },
        ] = page.as_slice()
        else {
            panic!("a turn");
        };
        assert_eq!(reply.text, "abcdef\n");
        assert!(reply.truncated);
    }

    #[test]
    fn a_message_with_many_small_parts_still_reports_its_bounded_reply() {
        let mut body = String::with_capacity(MESSAGES_BODY);
        body.push_str(r#"[{"parts":[{"type":"text","text":"kept"},"#);
        while body.len() < MESSAGES_BODY - 256 {
            body.push_str("{},");
        }
        body.push_str(
            r#"{"type":"text","text":"tail"}],"info":{"id":"a","role":"assistant","parentID":"u","time":{"created":5},"finish":"stop"}}]"#,
        );
        assert!(body.len() <= MESSAGES_BODY);
        let events = events(body.as_bytes(), 8).unwrap();
        let [
            AmbientFeedEvent::Ended {
                reply: Some(reply), ..
            },
        ] = events.as_slice()
        else {
            panic!("expected one completed reply");
        };
        assert_eq!(reply.text, "kept\n\nta");
        assert!(reply.truncated);
    }

    #[test]
    fn a_session_list_rolls_over_in_order_and_commits_only_a_valid_body() {
        let mut reader = Reader::new(None);
        reader.learn("ses_existing", false);
        reader.sessions.get_mut("ses_existing").unwrap().cursor = Some(MessageCursor {
            id: "last".into(),
            completed: 42,
        });
        let existing = r#"{"id":"ses_existing","parentID":"p"}"#;
        reader
            .read_sessions(format!("[{existing}]").as_bytes())
            .unwrap();
        assert!(reader.sessions["ses_existing"].subagent);
        assert_eq!(
            reader.sessions["ses_existing"].cursor.as_ref().unwrap().id,
            "last"
        );

        let mut list = format!("[{existing},");
        for n in 0..SESSIONS_KEPT {
            use std::fmt::Write as _;
            write!(list, r#"{{"id":"ses_{n}"}},"#).unwrap();
        }
        list.push_str(r#"{"id":"../invalid"},{"id":"ses_0","parentID":"p"}]"#);
        assert!(
            reader
                .read_sessions(format!("{list} null").as_bytes())
                .is_err()
        );
        assert_eq!(reader.sessions.len(), 1);
        assert_eq!(
            reader.sessions["ses_existing"].cursor.as_ref().unwrap().id,
            "last"
        );

        reader.read_sessions(list.as_bytes()).unwrap();
        assert_eq!(reader.sessions.len(), 2);
        assert!(reader.sessions["ses_0"].subagent);
        assert!(!reader.sessions[&format!("ses_{}", SESSIONS_KEPT - 1)].subagent);
    }

    fn reader_after(events: &[&str]) -> (Reader, Vec<AmbientFeedEvent>) {
        let mut reader = Reader::new(None);
        let mut told = Vec::new();
        for event in events {
            reader.event(event.as_bytes(), &mut told);
        }
        (reader, told)
    }

    #[test]
    fn idle_sessions_are_queued_once_each_in_order() {
        let (reader, told) = reader_after(&[
            r#"{"type":"session.status","properties":{"sessionID":"ses_b","status":{"type":"busy"}}}"#,
            r#"{"type":"session.idle","properties":{"sessionID":"ses_b"}}"#,
            r#"{"type":"session.status","properties":{"sessionID":"ses_b","status":{"type":"idle"}}}"#,
            r#"{"type":"session.status","properties":{"sessionID":"ses_a","status":{"type":"idle"}}}"#,
            r#"{"type":"message.part.delta","properties":{"delta":"x"}}"#,
            r#"{"type":"server.heartbeat","properties":{}}"#,
        ]);
        assert_eq!(reader.idle, ["ses_b", "ses_a"]);
        assert!(told.is_empty(), "{told:?}");
    }

    #[test]
    fn sessions_are_learnt_from_their_events() {
        let (reader, told) = reader_after(&[
            r#"{"type":"session.created","properties":{"info":{"id":"ses_c","parentID":"ses_p"}}}"#,
            r#"{"type":"session.updated","properties":{"info":{"id":"ses_p","parentID":null}}}"#,
            r#"{"type":"session.created","properties":{"info":{"id":"ses_gone"}}}"#,
            r#"{"type":"session.deleted","properties":{"info":{"id":"ses_gone"}}}"#,
        ]);
        assert_eq!(reader.sessions.get("ses_c").map(|s| s.subagent), Some(true));
        assert_eq!(
            reader.sessions.get("ses_p").map(|s| s.subagent),
            Some(false)
        );
        assert!(!reader.sessions.contains_key("ses_gone"));
        assert!(told.is_empty(), "{told:?}");
    }

    #[test]
    fn an_event_of_another_shape_is_unrecognised_and_reading_goes_on() {
        for data in [
            "{not json",
            r#"{"properties":{}}"#,
            r#"{"type":7}"#,
            r#"{"type":"session.idle","properties":{}}"#,
            r#"{"type":"session.idle","properties":{"sessionID":"../x"}}"#,
            r#"{"type":"session.status","properties":{"sessionID":"s"}}"#,
            r#"{"type":"session.created","properties":{}}"#,
        ] {
            let (reader, told) = reader_after(&[data]);
            assert!(
                matches!(told.as_slice(), [AmbientFeedEvent::Unrecognised]),
                "{data}: {told:?}"
            );
            assert!(reader.idle.is_empty(), "{data}");
        }
    }

    #[test]
    fn the_idle_queue_is_bounded() {
        let mut reader = Reader::new(None);
        let mut told = Vec::new();
        for n in 0..=IDLE_KEPT {
            let event =
                format!(r#"{{"type":"session.idle","properties":{{"sessionID":"ses_{n}"}}}}"#);
            reader.event(event.as_bytes(), &mut told);
        }
        assert_eq!(reader.idle.len(), IDLE_KEPT);
        assert_eq!(reader.idle[0], "ses_1", "the oldest dropped");
        assert!(matches!(told.as_slice(), [AmbientFeedEvent::Unrecognised]));
    }

    #[test]
    fn session_ids_are_what_opencode_gives_and_nothing_a_path_could_carry() {
        assert!(valid_id("ses_ed684409effeca18yu6v6G4fA1"));
        assert!(valid_id("S") && valid_id("a-b_c"));
        for id in ["", "../x", "a/b", "a b", "a?b", "a%2F", &"x".repeat(129)] {
            assert!(!valid_id(id), "{id}");
        }
    }

    #[test]
    fn the_backoff_doubles_up_to_its_cap() {
        let ms: Vec<u128> = (1..=8).map(|n| backoff(n).as_millis()).collect();
        assert_eq!(ms, [250, 500, 1000, 2000, 4000, 8000, 8000, 8000]);
        assert_eq!(backoff(u32::MAX), RETRY_MAX);
    }

    /// Each launch gets its own port and password; a password or user the plan had is
    /// replaced; a plan that already picks a port is refused.
    #[test]
    fn the_launch_adds_a_loopback_port_and_a_fresh_password() {
        let mut plan = LaunchPlan::new("opencode");
        plan.env = vec![
            (PASSWORD_ENV.into(), "mine".into()),
            (USERNAME_ENV.into(), "me".into()),
            ("OTHER".into(), "1".into()),
        ];
        plan.env_defaults = vec![(PASSWORD_ENV.into(), "default".into())];
        let endpoint = launch_server(&mut plan).expect("a loopback port and randomness");
        let keys: Vec<&str> = plan.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["OTHER", USERNAME_ENV, PASSWORD_ENV]);
        assert!(plan.env_defaults.is_empty());
        assert_eq!(plan.env[1].1, "opencode");
        assert_eq!(plan.env[2].1, endpoint.password());
        assert_eq!(endpoint.password().len(), 32);
        assert!(endpoint.password().bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(endpoint.addr().ip().is_loopback());
        let port = endpoint.addr().port().to_string();
        assert_eq!(
            plan.args,
            ["--port", port.as_str(), "--hostname", "127.0.0.1"]
        );
        let again = launch_server(&mut LaunchPlan::new("opencode")).unwrap();
        assert_ne!(again.password(), endpoint.password());
        let mut taken = LaunchPlan::new("opencode");
        taken.args.push("--port=4096".into());
        assert!(launch_server(&mut taken).is_err());
        for args in [
            &["--mini=false"][..],
            &["--mini", "false"],
            &["--mini", "--no-mini"],
        ] {
            let mut plan = LaunchPlan::new("opencode");
            plan.args.extend(args.iter().map(|arg| (*arg).to_owned()));
            assert!(launch_server(&mut plan).is_ok());
        }
    }

    #[test]
    fn a_feed_without_an_endpoint_says_the_launch_serves_nothing() {
        let ctx = AmbientContext::new("/x", EnvPolicy::default());
        let mut reader = OpencodeServer.open(&ctx, None);
        let poll = reader.poll(&ctx, &budget());
        assert_eq!(
            poll.state,
            AmbientFeedState::Ended(feed(AmbientFeedUnavailable::NotServed))
        );
    }
}

fn budget() -> AmbientFeedBudget {
    AmbientFeedBudget::new(
        Instant::now() + Duration::from_secs(2),
        &TurnOptions::default(),
    )
}

/// This process's program id: its process group (Unix) or itself (Windows). The stubs run
/// in this process, so they are served by "the agent".
fn own_program() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: no arguments, always succeeds.
        u32::try_from(unsafe { libc::getpgrp() }).unwrap()
    }
    #[cfg(windows)]
    {
        std::process::id()
    }
}

fn ctx(program: u32) -> AmbientContext {
    let mut ctx = AmbientContext::new("/x", EnvPolicy::default());
    ctx.program = Some(program);
    ctx.launched_at = Some(SystemTime::now());
    ctx
}

fn endpoint(stub: &Stub) -> AmbientEndpoint {
    AmbientEndpoint::new(stub.addr(), USERNAME, PASSWORD).unwrap()
}

/// An OpenCode-like stub taking the test password.
fn opencode() -> (Arc<OpenCode>, Stub) {
    let server = OpenCode::new(USERNAME, Some(PASSWORD));
    let stub = Stub::start(server.handler());
    (server, stub)
}

/// Polls until `done` holds for a poll (returned), at most five seconds.
fn poll_until(
    reader: &mut Reader,
    ctx: &AmbientContext,
    mut done: impl FnMut(&AmbientFeedPoll) -> bool,
) -> AmbientFeedPoll {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let poll = reader.poll(ctx, &budget());
        if done(&poll) {
            return poll;
        }
        assert!(Instant::now() < deadline, "last poll: {poll:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn replies(poll: &AmbientFeedPoll) -> Vec<Option<String>> {
    poll.events
        .iter()
        .filter_map(|event| match event {
            AmbientFeedEvent::Ended { reply, .. } => {
                Some(reply.as_ref().map(|reply| reply.text.clone()))
            }
            AmbientFeedEvent::Unrecognised => None,
        })
        .collect()
}

/// The handshake: an unauthenticated probe (refused), the authenticated list, the event
/// stream; then an idle session's stopped turn is told with its reply. Every request after
/// the probe carries the credentials, the probe none.
#[test]
fn a_verified_server_tells_an_idle_session_s_stopped_turn() {
    let (server, stub) = opencode();
    let ctx = ctx(own_program());
    let mut reader = Reader::new(Some(endpoint(&stub)));
    let poll = reader.poll(&ctx, &budget());
    assert_eq!(poll.state, AmbientFeedState::Ready);
    server.turn("First part.");
    let poll = poll_until(&mut reader, &ctx, |p| !p.events.is_empty());
    assert_eq!(replies(&poll), [Some("First part.".to_string())]);
    let requests = stub.requests();
    let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/session",
            "/session",
            "/event",
            "/session/ses_stub/message",
            "/session/status",
            "/session/ses_stub/message?limit=1",
        ]
    );
    assert_eq!(requests[0].authorization, None);
    let auth = basic(USERNAME, PASSWORD);
    for request in &requests[1..] {
        assert_eq!(request.authorization.as_deref(), Some(auth.as_str()));
    }
}

#[test]
fn the_last_reply_is_retried_when_idle_arrives_before_status_clears() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let server = OpenCode::new(USERNAME, Some(PASSWORD));
    let handler = server.handler();
    let busy = AtomicBool::new(true);
    let stub = Stub::start(move |req| {
        if req.path == "/session/status" && busy.swap(false, Ordering::SeqCst) {
            return Reply::json(200, r#"{"ses_stub":{"type":"busy"}}"#);
        }
        handler(req)
    });
    let ctx = ctx(own_program());
    let mut reader = Reader::new(Some(endpoint(&stub)));
    assert_eq!(reader.poll(&ctx, &budget()).state, AmbientFeedState::Ready);
    server.turn("last");
    let first = poll_until(&mut reader, &ctx, |_| stub.hits("/session/status") == 1);
    assert!(first.events.is_empty());
    assert_eq!(stub.hits("/session/ses_stub/message?limit=1"), 0);
    let next = reader.poll(&ctx, &budget());
    assert_eq!(replies(&next), [Some("last".into())]);
    assert_eq!(stub.hits("/session/ses_stub/message?limit=1"), 1);
    assert!(reader.poll(&ctx, &budget()).events.is_empty());
}

#[test]
fn a_final_poll_cannot_reconnect_to_finish_a_trailing_turn() {
    let server = OpenCode::new(USERNAME, Some(PASSWORD));
    let handler = server.handler();
    let stub = Stub::start(move |req| {
        let mut reply = handler(req);
        if req.path == "/session/ses_stub/message" {
            if let Reply::Fixed { close, .. } = &mut reply {
                *close = true;
            }
        }
        reply
    });
    let ctx = ctx(own_program());
    let mut reader = Reader::new(Some(endpoint(&stub)));
    assert_eq!(reader.poll(&ctx, &budget()).state, AmbientFeedState::Ready);
    server.turn("one");
    server.turn("two");
    let mut last = budget();
    last.last = true;
    // The request socket can fetch the messages once. The trailing turn would
    // need a fresh connection for its idle check.
    let mut got = Vec::new();
    let poll = loop {
        let poll = reader.poll(&ctx, &last);
        got.extend(replies(&poll));
        if matches!(poll.state, AmbientFeedState::Ended(_)) || stub.hits("/session/status") > 0 {
            break poll;
        }
        assert!(Instant::now() < last.deadline, "final poll: {poll:?}");
    };
    assert_eq!(stub.hits("/session/status"), 0);
    assert_eq!(got, [Some("one".into())]);
    assert!(matches!(poll.state, AmbientFeedState::Ended(_)) && poll.more);
}

#[test]
fn exhausted_final_fetches_preserve_the_completed_prefix_and_signal_loss() {
    let server = OpenCode::new(USERNAME, Some(PASSWORD));
    let handler = server.handler();
    let stub = Stub::start(move |req| {
        if req.path == "/session/status" {
            Reply::status(503)
        } else {
            handler(req)
        }
    });
    let ctx = ctx(own_program());
    let mut reader = Reader::new(Some(endpoint(&stub)));
    assert_eq!(reader.poll(&ctx, &budget()).state, AmbientFeedState::Ready);
    let ms = now_ms();
    server.set_messages(
        SESSION,
        vec![
            user("u1", "first prompt"),
            assistant_to("a1", "u1", "\"stop\"", ms, &text_part("complete")),
            user("u2", "second prompt"),
            assistant_to(
                "a2",
                "u2",
                "\"stop\"",
                ms,
                &text_part("awaiting idle confirmation"),
            ),
        ],
    );
    reader.went_idle(Some(SESSION), &mut Vec::new());
    let mut last = budget();
    last.last = true;
    let mut got = Vec::new();
    let poll = loop {
        let poll = reader.poll(&ctx, &last);
        got.extend(replies(&poll));
        if matches!(poll.state, AmbientFeedState::Ended(_)) || !poll.more {
            break poll;
        }
        assert!(Instant::now() < last.deadline, "final poll: {poll:?}");
    };
    assert_eq!(got, [Some("complete".into())]);
    // One attempt with a publishable prefix, then the pending trailing turn's retries.
    assert_eq!(stub.hits("/session/status"), 1 + READ_ATTEMPTS as usize);
    assert!(matches!(poll.state, AmbientFeedState::Ended(_)) && poll.more);
}

#[test]
fn coalesced_idle_events_return_each_completed_turn_in_order() {
    for room in [1, 2] {
        let (server, stub) = opencode();
        let ctx = ctx(own_program());
        let mut reader = Reader::new(Some(endpoint(&stub)));
        assert_eq!(reader.poll(&ctx, &budget()).state, AmbientFeedState::Ready);
        let ms = now_ms();
        server.turn_at("one", ms);
        server.turn_at("two", ms);
        let until = Instant::now() + Duration::from_secs(5);
        let first = loop {
            let mut limit = budget();
            limit.events = room;
            let poll = reader.poll(&ctx, &limit);
            if !poll.events.is_empty() {
                break poll;
            }
            assert!(Instant::now() < until, "no completions: {poll:?}");
        };
        let mut got = replies(&first);
        if room == 1 {
            assert!(first.more);
            got.extend(replies(&reader.poll(&ctx, &budget())));
        }
        assert_eq!(got, [Some("one".into()), Some("two".into())]);
        let fetches = stub.hits("/session/ses_stub/message");
        server.events.send(&opencode_stub::idle(SESSION));
        let repeated = poll_until(&mut reader, &ctx, |_| {
            stub.hits("/session/ses_stub/message") > fetches
        });
        assert!(repeated.events.is_empty());
        server.turn_at("three", ms);
        assert_eq!(
            replies(&poll_until(&mut reader, &ctx, |poll| !poll
                .events
                .is_empty())),
            [Some("three".into())]
        );
    }
}

#[test]
fn an_earlier_idle_cannot_finish_a_stale_snapshot_of_the_next_prompt() {
    // On the second run another assistant message arrives between the list and status
    // lookup. The idle state is then genuine, but the earlier list is out of date.
    for finishes_during_lookup in [false, true] {
        let server = OpenCode::new(USERNAME, Some(PASSWORD));
        let mut running = vec![
            user("u1", "first"),
            assistant_to("first", "u1", "\"stop\"", 100, &text_part("one")),
            user("u2", "second"),
            assistant_to("step", "u2", "\"stop\"", 200, ""),
        ];
        server.set_messages(SESSION, running.clone());
        running.push(assistant_to(
            "final",
            "u2",
            "\"stop\"",
            300,
            &text_part("two"),
        ));
        let finished = running;
        let busy = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let handler = server.handler();
        let stub = Stub::start({
            let server = server.clone();
            let busy = busy.clone();
            let finished = finished.clone();
            move |request| {
                if request.path == "/session/status"
                    && busy.load(std::sync::atomic::Ordering::SeqCst)
                {
                    if finishes_during_lookup {
                        server.set_messages(SESSION, finished.clone());
                    } else {
                        return Reply::json(200, r#"{"ses_stub":{"type":"busy"}}"#);
                    }
                }
                handler(request)
            }
        });
        let ctx = ctx(own_program());
        let mut reader = Reader::new(Some(endpoint(&stub)));
        assert_eq!(reader.poll(&ctx, &budget()).state, AmbientFeedState::Ready);
        let mut events = Vec::new();
        reader.event(br#"{"type":"session.status","properties":{"sessionID":"ses_stub","status":{"type":"idle"}}}"#, &mut events);
        reader.event(opencode_stub::idle(SESSION).as_bytes(), &mut events);
        assert!(events.is_empty());
        let old = poll_until(&mut reader, &ctx, |poll| !poll.events.is_empty());
        assert_eq!(replies(&old), [Some("one".into())]);
        busy.store(false, std::sync::atomic::Ordering::SeqCst);
        server.set_messages(SESSION, finished);
        // No further idle event follows the status transition.
        let new = poll_until(&mut reader, &ctx, |poll| !poll.events.is_empty());
        assert_eq!(replies(&new), [Some("two".into())]);
    }
}

/// A port squatter — a process other than the launched agent serving the endpoint,
/// answering `401` like a protected OpenCode would — is never sent anything: not the
/// probe, not the password. The feed ends for the launch.
#[cfg(unix)]
#[test]
fn a_port_squatter_is_never_sent_the_password() {
    use std::os::unix::process::CommandExt;

    let stub = Stub::start(|_| Reply::status(401));
    let mut agent = std::process::Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let mut reader = Reader::new(Some(endpoint(&stub)));
    let poll = reader.poll(&ctx(agent.id()), &budget());
    let _ = agent.kill();
    let _ = agent.wait();
    assert_eq!(
        poll.state,
        AmbientFeedState::Ended(feed(AmbientFeedUnavailable::OwnerMismatch))
    );
    assert!(stub.requests().is_empty(), "{:?}", stub.requests());
}

/// The agent's own server answering without credentials does not enforce the launch's
/// password: the feed ends, having sent nothing but the unauthenticated probe.
#[test]
fn a_server_answering_without_credentials_is_unprotected() {
    let server = OpenCode::new(USERNAME, None);
    let stub = Stub::start(server.handler());
    let mut reader = Reader::new(Some(endpoint(&stub)));
    let poll = reader.poll(&ctx(own_program()), &budget());
    assert_eq!(
        poll.state,
        AmbientFeedState::Ended(feed(AmbientFeedUnavailable::Unprotected))
    );
    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].authorization, None);
}

#[test]
fn an_unprotected_response_head_ends_the_feed_even_if_its_body_never_arrives() {
    for length in [32, 128 << 10] {
        let stub = Stub::start(move |_| Reply::Stall {
            status: 200,
            length,
        });
        let mut reader = Reader::new(Some(endpoint(&stub)));
        assert_eq!(
            reader.poll(&ctx(own_program()), &budget()).state,
            AmbientFeedState::Ended(feed(AmbientFeedUnavailable::Unprotected))
        );
        let requests = stub.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].authorization, None);
    }
}

#[test]
fn a_server_refusing_the_credentials_ends_the_feed() {
    let server = OpenCode::new(USERNAME, Some("another password"));
    let stub = Stub::start(server.handler());
    let mut reader = Reader::new(Some(endpoint(&stub)));
    let poll = reader.poll(&ctx(own_program()), &budget());
    assert_eq!(
        poll.state,
        AmbientFeedState::Ended(feed(AmbientFeedUnavailable::CredentialsRefused))
    );
}

#[test]
fn rejected_credentials_end_the_feed_regardless_of_the_response_body() {
    for (path, status, length) in [
        ("/session", 401, 32),
        ("/event", 403, 1 << 80),
        ("/session/ses_stub/message", 403, 16 << 20),
    ] {
        let server = OpenCode::new(USERNAME, Some(PASSWORD));
        let handler = server.handler();
        let stub = Stub::start(move |req| {
            if req.path == path && req.authorization.is_some() {
                return Reply::Stall { status, length };
            }
            handler(req)
        });
        let ctx = ctx(own_program());
        let mut reader = Reader::new(Some(endpoint(&stub)));
        let mut poll = reader.poll(&ctx, &budget());
        if path.ends_with("/message") {
            assert_eq!(poll.state, AmbientFeedState::Ready);
            server.turn("new reply");
            poll = poll_until(&mut reader, &ctx, |_| stub.hits(path) > 0);
        }
        assert_eq!(
            poll.state,
            AmbientFeedState::Ended(feed(AmbientFeedUnavailable::CredentialsRefused)),
            "{path}"
        );
        assert!(poll.events.is_empty());
        assert!(reader.request.is_none());
    }
}

/// Nothing listening yet since the launch is asked again at every poll; once the server
/// answered, a refused connection backs off.
#[test]
fn a_server_not_listening_yet_is_asked_again_and_a_lost_one_backs_off() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut reader = Reader::new(AmbientEndpoint::new(port, USERNAME, PASSWORD));
    let ctx = ctx(own_program());
    for _ in 0..3 {
        let poll = reader.poll(&ctx, &budget());
        assert_eq!(
            poll.state,
            AmbientFeedState::Unavailable(feed(AmbientFeedUnavailable::NotListening))
        );
        assert!(reader.retry_at.is_none());
    }
    reader.answered = true;
    reader.poll(&ctx, &budget());
    assert!(reader.retry_at.is_some(), "backing off");
}

/// No program known yet: nothing is connected (the owner cannot be checked).
#[test]
fn nothing_is_connected_before_the_agent_s_program_is_known() {
    let (_server, stub) = opencode();
    let mut reader = Reader::new(Some(endpoint(&stub)));
    let poll = reader.poll(&AmbientContext::new("/x", EnvPolicy::default()), &budget());
    assert_eq!(
        poll.state,
        AmbientFeedState::Unavailable(AmbientUnavailable::AgentUnknown)
    );
    assert!(stub.requests().is_empty());
}

/// The stream ends after each connection: it is opened again with backoff, not at
/// every poll; the turn its replay repeats is told again (the worker reports it once).
#[test]
fn a_dropped_stream_is_reopened_with_backoff() {
    let (server, stub) = opencode();
    let ctx = ctx(own_program());
    let mut reader = Reader::new(Some(endpoint(&stub)));
    assert_eq!(reader.poll(&ctx, &budget()).state, AmbientFeedState::Ready);
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(1500) {
        server.events.close_all();
        reader.poll(&ctx, &budget());
        std::thread::sleep(Duration::from_millis(10));
    }
    let opened = stub.hits("/event");
    // 250 + 500 ms of backoff in 1.5 s: at most a few reconnects, not one per poll.
    assert!((2..=5).contains(&opened), "{opened} streams");
}

#[test]
fn a_subagent_s_session_is_not_a_turn_and_an_unknown_one_is_looked_up() {
    let (server, stub) = opencode();
    server.add_session("ses_child", Some(SESSION));
    let ctx = ctx(own_program());
    let mut reader = Reader::new(Some(endpoint(&stub)));
    assert_eq!(reader.poll(&ctx, &budget()).state, AmbientFeedState::Ready);
    let stopped = |id| vec![assistant(id, "\"stop\"", now_ms(), &text_part("x"))];
    server.set_messages("ses_child", stopped("a1"));
    server.events.send(&opencode_stub::idle("ses_child"));
    // Created while the stream was down: not in the list the reader read.
    server.add_session("ses_late", None);
    server.set_messages("ses_late", stopped("a2"));
    server.events.send(&opencode_stub::idle("ses_late"));
    let poll = poll_until(&mut reader, &ctx, |p| !p.events.is_empty());
    assert_eq!(replies(&poll), [Some("x".to_string())]);
    assert_eq!(stub.hits("/session/ses_late"), 1, "looked up");
    assert_eq!(stub.hits("/session/ses_child/message"), 0);
}

#[test]
fn mismatched_session_metadata_is_retried_before_fetching_messages() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let server = OpenCode::new(USERNAME, Some(PASSWORD));
    let handler = server.handler();
    let mismatch = AtomicBool::new(true);
    let stub = Stub::start(move |req| {
        if req.path == "/session/ses_child" && mismatch.swap(false, Ordering::SeqCst) {
            return Reply::json(200, opencode_stub::session_json(SESSION, None));
        }
        handler(req)
    });
    let ctx = ctx(own_program());
    let mut reader = Reader::new(Some(endpoint(&stub)));
    assert_eq!(reader.poll(&ctx, &budget()).state, AmbientFeedState::Ready);
    server.add_session("ses_child", Some(SESSION));
    server.set_messages(
        "ses_child",
        vec![assistant_to(
            "a1",
            "u1",
            "\"stop\"",
            now_ms(),
            &text_part("child"),
        )],
    );
    server.events.send(&opencode_stub::idle("ses_child"));
    let first = poll_until(&mut reader, &ctx, |_| stub.hits("/session/ses_child") > 0);
    assert!(first.events.is_empty());
    let retry = reader.poll(&ctx, &budget());
    assert!(retry.events.is_empty());
    assert_eq!(stub.hits("/session/ses_child"), 2);
    assert_eq!(stub.hits("/session/ses_child/message"), 0);
}

/// A message list over its bound is skipped and counted; a fetch that hangs ends by the
/// poll's deadline and is tried again, three times in all, then given up.
#[test]
fn an_oversized_or_hanging_reply_fetch_is_bounded() {
    let server = OpenCode::new(USERNAME, Some(PASSWORD));
    let handler = server.handler();
    let stub = Stub::start(move |request| match request.path.as_str() {
        "/session/ses_big/message" => Reply::json(200, vec![b' '; MESSAGES_BODY + 1]),
        "/session/ses_slow/message" => Reply::Hang,
        _ => handler(request),
    });
    server.add_session("ses_big", None);
    server.add_session("ses_slow", None);
    let ctx = ctx(own_program());
    let mut reader = Reader::new(Some(endpoint(&stub)));
    assert_eq!(reader.poll(&ctx, &budget()).state, AmbientFeedState::Ready);
    server.events.send(&opencode_stub::idle("ses_big"));
    let poll = poll_until(&mut reader, &ctx, |p| p.oversized > 0);
    assert_eq!(poll.oversized, 1);
    server.events.send(&opencode_stub::idle("ses_slow"));
    let mut tries = 0;
    let poll = poll_until(&mut reader, &ctx, |p| {
        tries += 1;
        !p.events.is_empty()
    });
    assert!(matches!(
        poll.events.as_slice(),
        [AmbientFeedEvent::Unrecognised]
    ));
    assert_eq!(
        stub.hits("/session/ses_slow/message"),
        READ_ATTEMPTS as usize
    );
    assert!(reader.idle.is_empty());
}

#[test]
fn an_evicted_idle_session_gets_fresh_attempts_when_it_returns() {
    let (server, stub) = opencode();
    let children: Vec<_> = (0..IDLE_KEPT).map(|n| format!("ses_child_{n}")).collect();
    for child in &children {
        server.add_session(child, Some(SESSION));
    }
    let ctx = ctx(own_program());
    let mut reader = Reader::new(Some(endpoint(&stub)));
    assert_eq!(reader.poll(&ctx, &budget()).state, AmbientFeedState::Ready);

    // The session has no messages yet. Leave it one attempt short of giving up.
    let mut events = Vec::new();
    reader.went_idle(Some(SESSION), &mut events);
    for _ in 1..READ_ATTEMPTS {
        assert!(reader.poll(&ctx, &budget()).events.is_empty());
    }
    // Fill the queue with subagents, which need no message fetch of their own.
    for child in &children {
        reader.went_idle(Some(child), &mut events);
    }
    reader.went_idle(Some(SESSION), &mut events);
    assert!(
        reader.poll(&ctx, &budget()).events.is_empty(),
        "still retrying"
    );

    let message = assistant("msg_a1", "\"stop\"", now_ms(), &text_part("recovered"));
    server.set_messages(SESSION, vec![message]);
    assert_eq!(
        replies(&reader.poll(&ctx, &budget())),
        [Some("recovered".into())]
    );
}

/// A turn whose reply is not cut by the reader keeps the poll's reply budget.
#[test]
fn the_reply_respects_the_poll_s_reply_bytes() {
    let (server, stub) = opencode();
    let ctx = ctx(own_program());
    let mut reader = Reader::new(Some(endpoint(&stub)));
    assert_eq!(reader.poll(&ctx, &budget()).state, AmbientFeedState::Ready);
    server.turn(&"é".repeat(100));
    let limits = TurnOptions {
        reply_bytes: 5,
        ..TurnOptions::default()
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let poll = loop {
        let poll = reader.poll(
            &ctx,
            &AmbientFeedBudget::new(Instant::now() + Duration::from_secs(2), &limits),
        );
        if !poll.events.is_empty() || Instant::now() > deadline {
            break poll;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let Some(AmbientFeedEvent::Ended {
        reply: Some(reply), ..
    }) = poll.events.first()
    else {
        panic!("{poll:?}");
    };
    assert_eq!(reply.text, "éé");
    assert!(reply.truncated);
}

/// The password is in no `Debug` output of the reader or its endpoint.
#[test]
fn the_password_is_never_shown() {
    let (_server, stub) = opencode();
    let mut reader = Reader::new(Some(endpoint(&stub)));
    reader.poll(&ctx(own_program()), &budget());
    let shown = format!("{reader:?}");
    assert!(!shown.contains(PASSWORD), "{shown}");
    let encoded = &basic(USERNAME, PASSWORD)["Basic ".len()..];
    assert!(!shown.contains(encoded), "{shown}");
}

#[test]
fn a_user_message_after_the_turn_is_not_the_turn() {
    // The prompt of a turn still running: the last assistant message is the previous turn's.
    let body = format!(
        "[{},{},{}]",
        user("u1", "a"),
        assistant_to("a1", "u1", "\"stop\"", 10, &text_part("done")),
        user("u2", "b")
    );
    let page = events(body.as_bytes(), 64).unwrap();
    assert!(page.is_empty());
}

/// A server that never answers holds a poll no longer than its budget, and the attempt
/// backs off instead of being repeated at every poll.
#[test]
fn a_poll_returns_by_its_deadline_whatever_the_server_does() {
    let stub = Stub::start(|_| Reply::Hang);
    let mut reader = Reader::new(Some(endpoint(&stub)));
    let ctx = ctx(own_program());
    let started = Instant::now();
    let limits = TurnOptions::default();
    let poll = reader.poll(
        &ctx,
        &AmbientFeedBudget::new(Instant::now() + Duration::from_millis(300), &limits),
    );
    let took = started.elapsed();
    assert!(took < Duration::from_millis(600), "{took:?}");
    assert_eq!(
        poll.state,
        AmbientFeedState::Unavailable(feed(AmbientFeedUnavailable::Protocol))
    );
    assert!(reader.retry_at.is_some());
    // Within the backoff: no new connection.
    reader.poll(&ctx, &budget());
    assert_eq!(stub.requests().len(), 1);
}
