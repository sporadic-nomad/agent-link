//! The ambient OpenCode question feed against the synthetic OpenCode-like server of
//! `tests/common/opencode_stub.rs`, served from this process (so its owner check finds
//! "the agent" here), and against scripted replies of the bare stub.

use std::sync::Arc;

use super::*;
use crate::agent::questions::QuestionItem;

use crate::agent::loopback::{http_stub, opencode_stub};

use http_stub::{Reply, Request, Stub, basic};
use opencode_stub::{OpenCode, SESSION};

const USER: &str = "opencode";
const PASSWORD: &str = "0123456789abcdef0123456789abcdef";

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

fn feed_for(stub: &Stub, program: Option<u32>) -> OpenCodeAmbientQuestions {
    bound_feed(stub, SESSION, program)
}

fn bound_feed(stub: &Stub, session: &str, program: Option<u32>) -> OpenCodeAmbientQuestions {
    let endpoint = AmbientEndpoint::new(stub.addr(), USER, PASSWORD).unwrap();
    OpenCodeAmbientQuestions::with_program(endpoint, session, Box::new(move || program))
}

fn opencode() -> (Arc<OpenCode>, Stub) {
    let server = OpenCode::new(USER, Some(PASSWORD));
    let stub = Stub::start(server.handler());
    (server, stub)
}

fn color(server: &OpenCode, id: &str, session: &str) {
    server.ask(
        id,
        session,
        "Color",
        "Which color should the sample use?",
        &["Blue", "Amber"],
    );
}

fn paths(stub: &Stub) -> Vec<String> {
    stub.requests()
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect()
}

/// The handshake (an unauthenticated probe, refused), the list and the session lookup;
/// an answer posts the selected labels on a verified connection, every request but the
/// probe with the launch's credentials; the answered request leaves the list.
#[test]
fn the_root_session_s_question_is_listed_and_answered_with_its_labels() {
    let (server, stub) = opencode();
    let feed = feed_for(&stub, Some(own_program()));
    assert_eq!(feed.pending(), Ok(Vec::new()));
    color(&server, "que_1", SESSION);
    let pending = feed.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "que_1");
    assert_eq!(
        pending[0].questions,
        [QuestionItem::new(
            "Color",
            "Which color should the sample use?",
            vec!["Blue".into(), "Amber".into()],
        )
        .with_free_text(true)]
    );
    assert_eq!(
        feed.answer(&pending[0], &[Answer::Choice(1)], &|| true),
        Ok(())
    );
    assert_eq!(
        *server.replies.lock().unwrap(),
        [(
            "que_1".to_string(),
            r#"{"answers":[["Amber"]]}"#.to_string()
        )]
    );
    assert_eq!(feed.pending(), Ok(Vec::new()));
    assert_eq!(
        paths(&stub),
        [
            "GET /question",
            "GET /question",
            "GET /question",
            "GET /session/ses_stub",
            "GET /question",
            "POST /question/que_1/reply",
            "GET /question",
        ]
    );
    let requests = stub.requests();
    assert_eq!(requests[0].authorization, None, "the probe");
    let auth = basic(USER, PASSWORD);
    assert!(
        requests[1..]
            .iter()
            .all(|r| r.authorization.as_deref() == Some(auth.as_str()))
    );
}

#[test]
fn only_the_bound_conversations_questions_can_be_listed_or_answered() {
    let (server, stub) = opencode();
    server.add_session("ses_other", None);
    color(&server, "que_other", "ses_other");
    let feed = feed_for(&stub, Some(own_program()));
    assert_eq!(feed.pending(), Ok(Vec::new()));

    color(&server, "que_main", SESSION);
    let listed = feed.pending().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "que_main");
    assert_eq!(
        feed.answer(&listed[0], &[Answer::Choice(0)], &|| true),
        Ok(())
    );
    assert_eq!(feed.pending(), Ok(Vec::new()));
    assert_eq!(stub.hits("/session/ses_other"), 0);
    assert_eq!(stub.hits("/question/que_other/reply"), 0);
    assert_eq!(stub.hits("/question/que_main/reply"), 1);
}

#[test]
fn revalidation_rejects_the_same_request_id_and_questions_in_another_session() {
    let (server, stub) = opencode();
    server.add_session("ses_other", None);
    color(&server, "que_1", SESSION);
    let feed = feed_for(&stub, Some(own_program()));
    let pending = feed.pending().unwrap().remove(0);

    server.withdraw("que_1");
    color(&server, "que_1", "ses_other");
    assert_eq!(
        feed.answer(&pending, &[Answer::Choice(0)], &|| true),
        Err(AnswerError::NotOpen)
    );
    assert_eq!(feed.pending(), Ok(Vec::new()));
    assert!(server.replies.lock().unwrap().is_empty());
}

#[test]
fn binding_a_subagent_session_withholds_its_questions() {
    let (server, stub) = opencode();
    server.add_session("ses_child", Some(SESSION));
    color(&server, "que_sub", "ses_child");
    let feed = bound_feed(&stub, "ses_child", Some(own_program()));
    assert_eq!(feed.pending(), Ok(Vec::new()));
    assert_eq!(feed.pending(), Ok(Vec::new()));
    assert_eq!(stub.hits("/session/ses_child"), 1);
}

/// Refused credentials and a server answering without them end the feed; a server that is
/// not the agent's is never sent anything.
#[test]
fn refused_credentials_or_an_unprotected_server_end_the_feed() {
    let refusing = OpenCode::new(USER, Some("another password"));
    let stub = Stub::start(refusing.handler());
    let feed = feed_for(&stub, Some(own_program()));
    assert_eq!(feed.pending(), Err(AnswerError::NotOpen));
    let sent = stub.requests().len();
    assert_eq!(feed.pending(), Err(AnswerError::NotOpen));
    assert_eq!(stub.requests().len(), sent, "nothing after the refusal");

    let open = OpenCode::new(USER, None);
    let stub = Stub::start(open.handler());
    let feed = feed_for(&stub, Some(own_program()));
    assert_eq!(feed.pending(), Err(AnswerError::NotOpen));
    let requests = stub.requests();
    assert_eq!(requests.len(), 1, "the probe only");
    assert_eq!(requests[0].authorization, None);
}

/// A port squatter — a process other than the launched agent serving the endpoint,
/// answering `401` like a protected OpenCode would — gets nothing: no probe, no list, no
/// answer, no password.
#[cfg(unix)]
#[test]
fn a_port_squatter_is_sent_nothing() {
    use std::os::unix::process::CommandExt;

    let stub = Stub::start(|_| Reply::status(401));
    let mut agent = std::process::Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let feed = feed_for(&stub, Some(agent.id()));
    let pending = PendingQuestion::new(
        "que_1",
        vec![QuestionItem::new("Color", "Pick", vec!["Blue".into()]).with_free_text(true)],
    );
    let listed = feed.pending();
    let answered = feed.answer(&pending, &[Answer::Choice(0)], &|| true);
    let _ = agent.kill();
    let _ = agent.wait();
    assert_eq!(listed, Err(AnswerError::NotOpen));
    assert_eq!(answered, Err(AnswerError::NotOpen));
    assert!(stub.requests().is_empty(), "{:?}", stub.requests());
}

/// The agent's program not known yet: nothing is connected, and the next poll tries again.
#[test]
fn an_unknown_program_is_tried_again_without_connecting() {
    let (_server, stub) = opencode();
    let feed = feed_for(&stub, None);
    assert_eq!(feed.pending(), Err(AnswerError::Io));
    assert!(stub.requests().is_empty());
}

/// A reply the server may have taken without saying so is `Unconfirmed`; a request that
/// is gone is `NotOpen`; losing the terminal after the last list posts nothing; a request
/// whose questions changed is not answered.
#[test]
fn ambiguous_gone_unauthorized_and_changed_answers() {
    for (reply, expected) in [
        (Some(r#"false"#), AnswerError::Unconfirmed),
        (Some(r#"{"ok":true}"#), AnswerError::Unconfirmed),
        (None, AnswerError::Unconfirmed),
    ] {
        let server = OpenCode::new(USER, Some(PASSWORD));
        let handler = server.handler();
        let stub = Stub::start(move |request: &Request| {
            if request.method == "POST" {
                match reply {
                    Some(body) => Reply::json(200, body),
                    None => Reply::Close,
                }
            } else {
                handler(request)
            }
        });
        color(&server, "que_1", SESSION);
        let feed = feed_for(&stub, Some(own_program()));
        let pending = feed.pending().unwrap().remove(0);
        assert_eq!(
            feed.answer(&pending, &[Answer::Choice(0)], &|| true),
            Err(expected),
            "{reply:?}"
        );
        // Still listed: the feed goes on, on a new verified connection.
        assert_eq!(feed.pending().unwrap().len(), 1);
    }

    let (server, stub) = opencode();
    color(&server, "que_1", SESSION);
    let feed = feed_for(&stub, Some(own_program()));
    let pending = feed.pending().unwrap().remove(0);
    assert_eq!(
        feed.answer(&pending, &[Answer::Choice(0)], &|| false),
        Err(AnswerError::NotOpen)
    );
    let mut changed = pending.clone();
    changed.questions[0].options.reverse();
    assert_eq!(
        feed.answer(&changed, &[Answer::Choice(0)], &|| true),
        Err(AnswerError::Changed)
    );
    assert_eq!(
        feed.answer(&pending, &[Answer::Choice(5)], &|| true),
        Err(AnswerError::Unsupported)
    );
    assert!(!paths(&stub).iter().any(|p| p.starts_with("POST")));
    server.withdraw("que_1");
    assert_eq!(
        feed.answer(&pending, &[Answer::Choice(0)], &|| true),
        Err(AnswerError::NotOpen)
    );
    // A request the server lost between the list and the post.
    let lost = OpenCode::new(USER, Some(PASSWORD));
    let handler = lost.handler();
    let stub = Stub::start(move |request: &Request| {
        if request.method == "POST" {
            Reply::status(404)
        } else {
            handler(request)
        }
    });
    color(&lost, "que_1", SESSION);
    let feed = feed_for(&stub, Some(own_program()));
    let pending = feed.pending().unwrap().remove(0);
    assert_eq!(
        feed.answer(&pending, &[Answer::Choice(0)], &|| true),
        Err(AnswerError::NotOpen)
    );
}

#[test]
fn a_redirect_or_missing_route_on_the_probe_ends_polling() {
    for status in [302, 404, 410] {
        let stub = Stub::start(move |_| Reply::status(status));
        let feed = feed_for(&stub, Some(own_program()));
        assert_eq!(feed.pending(), Err(AnswerError::NotOpen));
        assert_eq!(feed.pending(), Err(AnswerError::NotOpen));
        assert_eq!(paths(&stub), ["GET /question"]);
        assert_eq!(stub.requests()[0].authorization, None);
    }
}

#[test]
fn an_unprotected_question_list_ends_polling_from_its_response_head() {
    let stub = Stub::start(|_| Reply::Stall {
        status: 200,
        length: 128 << 10,
    });
    let feed = feed_for(&stub, Some(own_program()));
    assert_eq!(feed.pending(), Err(AnswerError::NotOpen));
    assert_eq!(feed.pending(), Err(AnswerError::NotOpen));
    assert_eq!(paths(&stub), ["GET /question"]);
    assert_eq!(stub.requests()[0].authorization, None);
}

/// A server without the question route (or one that redirects it) serves no questions for
/// this launch; a request of another shape withholds the list.
#[test]
fn a_missing_route_ends_the_feed_and_a_malformed_list_is_withheld() {
    let stub = Stub::start(|request: &Request| {
        if request.authorization.is_none() {
            Reply::status(401)
        } else {
            Reply::status(404)
        }
    });
    let feed = feed_for(&stub, Some(own_program()));
    assert_eq!(feed.pending(), Err(AnswerError::NotOpen));

    for (body, expected) in [
        ("{}", AnswerError::Io),
        (
            r#"[{"id":"que_1","sessionID":"../x","questions":[]}]"#,
            AnswerError::Changed,
        ),
    ] {
        let stub = Stub::start(move |request: &Request| {
            if request.authorization.is_none() {
                Reply::status(401)
            } else {
                Reply::json(200, body)
            }
        });
        let feed = feed_for(&stub, Some(own_program()));
        assert_eq!(feed.pending(), Err(expected), "{body}");
    }
}

#[test]
fn debug_never_shows_the_password() {
    let (_server, stub) = opencode();
    let feed = feed_for(&stub, Some(own_program()));
    let shown = format!("{feed:?}");
    assert!(!shown.contains(PASSWORD), "{shown}");
    let encoded = basic(USER, PASSWORD);
    assert!(
        !shown.contains(encoded.trim_start_matches("Basic ")),
        "{shown}"
    );
}
