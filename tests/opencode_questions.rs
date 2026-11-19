//! Native acceptance for the OpenCode v1 question list and reply routes.
#![cfg(all(feature = "questions", unix))]

use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_link::agent::{
    AgentSpec, AgentTerminal, ProfileRegistry,
    questions::{Answer, ClosedBy, opencode::OpenCodeQuestions},
};
use agent_link::{Event, PtySize, SpawnSpec};

#[test]
#[ignore = "scripts/check_opencode_questions.py supplies the isolated native server, TUI and model peer"]
fn native_question_is_answered_through_its_own_terminal() {
    let directory = PathBuf::from(std::env::var_os("AL_QUESTION_DIRECTORY").unwrap());
    let session = std::env::var("AL_QUESTION_SESSION").unwrap();
    let addr: SocketAddr = std::env::var("AL_QUESTION_ADDRESS")
        .unwrap()
        .parse()
        .unwrap();
    assert!(addr.ip().is_loopback());
    let url = format!("http://{addr}");
    let mut spec = SpawnSpec::direct(
        std::env::var_os("AL_QUESTION_BINARY").unwrap(),
        [
            "attach",
            &url,
            "--session",
            &session,
            "--dir",
            directory.to_str().unwrap(),
        ],
        &directory,
        PtySize {
            rows: 40,
            cols: 120,
        },
    );
    // The runner supplies an isolated environment and starts this test under a loopback sandbox.
    spec.env.inherit = false;
    for key in [
        "HOME",
        "TMPDIR",
        "PATH",
        "SHELL",
        "LANG",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "OPENCODE_TEST_HOME",
        "OPENCODE_DISABLE_AUTOUPDATE",
        "OPENCODE_DISABLE_MODELS_FETCH",
        "OPENCODE_CONFIG_CONTENT",
        "OPENCODE_SERVER_PASSWORD",
    ] {
        spec.env.set(key, std::env::var_os(key).unwrap());
    }
    let profile = ProfileRegistry::builtin().get("opencode").unwrap();
    let (term, events) = AgentTerminal::spawn(spec, AgentSpec::new(profile)).unwrap();
    let feed = OpenCodeQuestions::from_verified_stream(
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap(),
        &directory,
        &session,
        "opencode",
        &std::env::var("OPENCODE_SERVER_PASSWORD").unwrap(),
    )
    .unwrap();
    term.observe_feed_questions(Arc::new(feed)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let until = || {
        deadline
            .checked_duration_since(Instant::now())
            .expect("native question deadline")
    };
    let ready = PathBuf::from(std::env::var_os("AL_QUESTION_TUI_READY").unwrap());
    let mut question_id = None;
    let mut attached = false;
    let id = loop {
        if let Some(screen) = term.terminal().screen() {
            let text = screen.text();
            if !attached
                && (text.contains("Ask anything")
                    || text.contains("tab agents")
                    || text.contains("ctrl+p commands"))
            {
                std::fs::write(&ready, b"attached").unwrap();
                attached = true;
            }
            if let Some(id) = question_id.filter(|_| {
                text.contains("Which color should the sample use?") && text.contains("enter submit")
            }) {
                break id;
            }
        }
        match events
            .recv_timeout(until())
            .expect("question event or native redraw")
        {
            Event::Question { qid, questions, .. } => {
                assert_eq!(questions.len(), 1);
                assert_eq!(questions[0].prompt, "Which color should the sample use?");
                assert_eq!(questions[0].options, ["Blue", "Amber"]);
                assert!(question_id.replace(qid).is_none(), "duplicate open");
            }
            Event::Exit { .. } => panic!("native TUI exited before the question"),
            _ => {}
        }
    };
    assert_eq!(
        term.answer_question(id, vec![Answer::Choice(0)])
            .wait(until()),
        Some(Ok(()))
    );
    loop {
        match events.recv_timeout(until()).expect("question close") {
            Event::QuestionClosed { qid, by, .. } if qid == id => {
                assert_eq!(by, ClosedBy::Host);
                break;
            }
            Event::Exit { .. } => panic!("native TUI exited before the close"),
            _ => {}
        }
    }
}
