use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const SESSION: &str = "ses_local";
const LIST: &str = include_str!("../../../../tests/fixtures/questions/opencode-1.18.21/list.json");

struct Call {
    line: String,
    body: Vec<u8>,
}

struct Peer(BufReader<TcpStream>);

impl Peer {
    fn read_call(&mut self) -> Call {
        let mut line = String::new();
        self.0.read_line(&mut line).unwrap();
        let mut length = 0;
        let mut auth = false;
        loop {
            let mut header = String::new();
            assert!(self.0.read_line(&mut header).unwrap() > 0);
            if header == "\r\n" {
                break;
            }
            if let Some(value) = header.strip_prefix("Content-Length: ") {
                length = value.trim().parse().unwrap();
            }
            auth |= header.trim_end() == "Authorization: Basic dTpw";
        }
        assert!(auth);
        let mut body = vec![0; length];
        self.0.read_exact(&mut body).unwrap();
        Call { line, body }
    }

    fn send(&mut self, code: u16, body: &str) {
        write!(
            self.0.get_mut(),
            "HTTP/1.1 {code} Response\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }
}

fn serve(script: impl FnOnce(Peer) + Send + 'static) -> (OpenCodeQuestions, JoinHandle<()>) {
    let listen = TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = TcpStream::connect(listen.local_addr().unwrap()).unwrap();
    let join = thread::spawn(move || {
        let (peer, _) = listen.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        script(Peer(BufReader::new(peer)));
    });
    let cwd = std::env::current_dir().unwrap().join("survey & test");
    (
        OpenCodeQuestions::from_verified_stream(stream, &cwd, SESSION, "u", "p").unwrap(),
        join,
    )
}

#[test]
fn native_list_and_reply_revalidate_on_one_bound_connection() {
    let (feed, peer) = serve(|mut peer| {
        for _ in 0..2 {
            let request = peer.read_call();
            assert!(request.line.starts_with("GET /question?directory="));
            assert!(request.line.contains("survey%20%26%20test HTTP/1.1"));
            assert!(request.body.is_empty());
            peer.send(200, LIST);
        }
        let request = peer.read_call();
        assert!(
            request
                .line
                .starts_with("POST /question/que_local/reply?directory=")
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
            serde_json::json!({"answers":[["Amber"]]})
        );
        peer.send(200, "true");
    });
    let pending = feed.pending().unwrap().pop().unwrap();
    assert_eq!(
        pending.questions[0].prompt,
        "Which color should the sample use?"
    );
    assert!(pending.questions[0].free_text);
    feed.answer(&pending, &[Answer::Choice(1)], &|| true)
        .unwrap();
    peer.join().unwrap();
}

#[test]
fn foreign_changed_and_ambiguous_requests_do_not_authorize_a_post() {
    let (feed, peer) = serve(|mut peer| {
        for body in [
            LIST.to_owned(),
            LIST.replace("Blue", "Green"),
            LIST.replace("ses_local", "ses_other"),
        ] {
            assert!(peer.read_call().line.starts_with("GET /question?"));
            peer.send(200, &body);
        }
    });
    let pending = feed.pending().unwrap().pop().unwrap();
    assert_eq!(
        feed.answer(&pending, &[Answer::Choice(0)], &|| true),
        Err(AnswerError::Changed)
    );
    assert_eq!(
        feed.answer(&pending, &[Answer::Choice(0)], &|| true),
        Err(AnswerError::NotOpen)
    );
    peer.join().unwrap();
    let mut both: Vec<serde_json::Value> = serde_json::from_str(LIST).unwrap();
    let mut decoy = both[0].clone();
    decoy["sessionID"] = "ses_other".into();
    both.push(decoy);
    assert!(matches!(
        parse_pending(&serde_json::to_vec(&both).unwrap(), SESSION),
        Err(AnswerError::Changed)
    ));
    both[1]["id"] = "que_other".into();
    assert_eq!(
        parse_pending(&serde_json::to_vec(&both).unwrap(), SESSION)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn truncated_post_ack_is_ambiguous_and_revokes_the_connection() {
    let (feed, peer) = serve(|mut peer| {
        assert!(peer.read_call().line.starts_with("GET "));
        peer.send(200, LIST);
        assert!(peer.read_call().line.starts_with("POST "));
        peer.0
            .get_mut()
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ntr")
            .unwrap();
    });
    let pending = parse_pending(LIST.as_bytes(), SESSION)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        feed.answer(&pending, &[Answer::Text("Teal".into())], &|| true),
        Err(AnswerError::Unconfirmed)
    );
    assert!(matches!(feed.pending(), Err(AnswerError::NotOpen)));
    peer.join().unwrap();
}

#[test]
fn a_withdrawn_request_and_an_ambiguous_acknowledgment_have_distinct_outcomes() {
    for (status, ack, expected) in [
        (404, "{}", AnswerError::NotOpen),
        (200, "false", AnswerError::Unconfirmed),
        (307, "", AnswerError::Unconfirmed),
    ] {
        let (feed, peer) = serve(move |mut peer| {
            assert!(peer.read_call().line.starts_with("GET "));
            peer.send(200, LIST);
            assert!(peer.read_call().line.starts_with("POST "));
            peer.send(status, ack);
            let mut byte = [0];
            assert_eq!(peer.0.read(&mut byte).unwrap(), 0);
        });
        let pending = parse_pending(LIST.as_bytes(), SESSION)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            feed.answer(&pending, &[Answer::Choice(0)], &|| true),
            Err(expected)
        );
        if expected == AnswerError::Unconfirmed {
            assert!(matches!(feed.pending(), Err(AnswerError::NotOpen)));
        }
        drop(feed);
        peer.join().unwrap();
    }
}

#[test]
fn revoked_binding_and_invalid_ids_never_write_credentials_or_answers() {
    let (feed, peer) = serve(|mut peer| {
        let mut byte = [0];
        assert_eq!(peer.0.read(&mut byte).unwrap(), 0);
    });
    let mut pending = parse_pending(LIST.as_bytes(), SESSION)
        .unwrap()
        .pop()
        .unwrap();
    pending.id = "que_../session".into();
    assert_eq!(
        feed.answer(&pending, &[Answer::Choice(0)], &|| true),
        Err(AnswerError::NotOpen)
    );
    feed.revoke();
    assert!(matches!(feed.pending(), Err(AnswerError::NotOpen)));
    peer.join().unwrap();
}

#[test]
fn losing_terminal_authority_after_the_last_get_withholds_the_post() {
    let (feed, peer) = serve(|mut peer| {
        assert!(peer.read_call().line.starts_with("GET "));
        peer.send(200, LIST);
        let mut byte = [0];
        assert_eq!(peer.0.read(&mut byte).unwrap(), 0);
    });
    let pending = parse_pending(LIST.as_bytes(), SESSION)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        feed.answer(&pending, &[Answer::Choice(0)], &|| false),
        Err(AnswerError::NotOpen)
    );
    feed.revoke();
    peer.join().unwrap();
}

#[test]
fn bounded_multiselects_and_free_text_are_encoded_as_literal_labels() {
    let item = QuestionItem::new("Color", "Pick colors", vec!["A".into(), "B".into()])
        .with_multi_select(true);
    let items = [item];
    let body = encode_answers(&items, &[Answer::Choices(vec![1, 0])]).unwrap();
    assert_eq!(body, br#"{"answers":[["B","A"]]}"#);
    for answers in [
        Answer::Choice(2),
        Answer::Choices(vec![1, 1]),
        Answer::Choices(vec![2]),
        Answer::Text("custom".into()),
    ] {
        assert_eq!(
            encode_answers(&items, &[answers]),
            Err(AnswerError::Unsupported)
        );
    }
    assert!(matches!(
        parse_pending(
            LIST.replace("Blue", &"X".repeat(MAX_FIELD + 1)).as_bytes(),
            SESSION
        ),
        Err(AnswerError::InputTooLarge)
    ));
    assert!(matches!(
        parse_pending(LIST.replace("Blue", "Amber").as_bytes(), SESSION),
        Err(AnswerError::Changed)
    ));
}

#[test]
fn chunked_list_is_bounded_and_conflicting_framing_discards_the_socket() {
    let (feed, peer) = serve(|mut peer| {
        peer.read_call();
        write!(
            peer.0.get_mut(),
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{LIST}\r\n0\r\n\r\n",
            LIST.len()
        )
        .unwrap();
        peer.read_call();
        peer.0
            .get_mut()
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 1\r\n\r\n",
            )
            .unwrap();
    });
    assert_eq!(feed.pending().unwrap().len(), 1);
    assert!(matches!(feed.pending(), Err(AnswerError::Io)));
    assert!(matches!(feed.pending(), Err(AnswerError::NotOpen)));
    peer.join().unwrap();
}

#[test]
fn a_list_on_a_closing_connection_cannot_open_an_answerable_question() {
    let (feed, peer) = serve(|mut peer| {
        assert!(peer.read_call().line.starts_with("GET "));
        write!(
            peer.0.get_mut(),
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{LIST}",
            LIST.len()
        )
        .unwrap();
    });
    assert!(matches!(feed.pending(), Err(AnswerError::NotOpen)));
    assert!(matches!(feed.pending(), Err(AnswerError::NotOpen)));
    peer.join().unwrap();
}
