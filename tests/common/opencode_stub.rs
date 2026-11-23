//! A synthetic OpenCode-like server on [`http_stub`]: the routes and shapes the ambient
//! feed reads — `GET /session`, `GET /session/status`, `GET /session/{id}`, the session’s
//! message list (including `?limit=1`) and the
//! `GET /event` stream — as read from OpenCode 1.18.21's own server code by the
//! predecessor crate, **not captured from a running OpenCode**; and the question routes
//! the question feeds use — `GET /question` (the shape of the recorded
//! `tests/fixtures/questions/opencode-1.18.21/list.json`) and
//! `POST /question/{id}/reply` (`true`, or `404` for a request not pending). Requests
//! need Basic auth (`401` without it) unless the server is `open`. Std-only: the fake
//! agent serves it from the launched process.
#![allow(dead_code)]

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use super::http_stub::{Events, Reply, Request, basic};

/// The session the scripted turns belong to.
pub const SESSION: &str = "ses_stub";

/// What the stub serves; change it while it runs.
pub struct OpenCode {
    /// The `Authorization` value it takes (`None`: any request is answered: an
    /// unprotected server).
    pub expected: Option<String>,
    pub sessions: Mutex<Vec<(String, Option<String>)>>,
    /// The message list of each session, as JSON.
    messages: Mutex<Vec<(String, Vec<String>)>>,
    pub events: Events,
    /// Each new `/event` stream starts by telling these sessions went idle (as a server
    /// that repeats itself after a reconnect).
    pub replay_idle: Mutex<Vec<String>>,
    /// The pending question requests: id and JSON, in the order asked.
    pub questions: Mutex<Vec<(String, String)>>,
    /// The replies taken: request id and the posted body.
    pub replies: Mutex<Vec<(String, String)>>,
    /// Each reply taken is also appended to this file as `<id> <body>` (a server in
    /// another process).
    pub reply_log: Mutex<Option<std::path::PathBuf>>,
}

impl OpenCode {
    /// A server taking `user:password`, or any request when `password` is `None`.
    pub fn new(user: &str, password: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            expected: password.map(|password| basic(user, password)),
            sessions: Mutex::new(vec![(SESSION.into(), None)]),
            messages: Mutex::new(Vec::new()),
            events: Events::default(),
            replay_idle: Mutex::new(Vec::new()),
            questions: Mutex::new(Vec::new()),
            replies: Mutex::new(Vec::new()),
            reply_log: Mutex::new(None),
        })
    }

    /// `session` asks one single-choice question (`que_<id>`), with a typed answer allowed.
    pub fn ask(&self, id: &str, session: &str, header: &str, question: &str, options: &[&str]) {
        let options: Vec<String> = options
            .iter()
            .map(|label| format!(r#"{{"label":{},"description":"d"}}"#, quoted(label)))
            .collect();
        let request = format!(
            r#"{{"id":"{id}","sessionID":"{session}","questions":[{{"question":{},"header":{},"options":[{}],"multiple":false}}],"tool":{{"messageID":"msg_q","callID":"call_q"}}}}"#,
            quoted(question),
            quoted(header),
            options.join(","),
        );
        lock(&self.questions).push((id.into(), request));
        self.events.send(&format!(
            r#"{{"type":"question.asked","properties":{{"id":"{id}","sessionID":"{session}"}}}}"#
        ));
    }

    /// The pending request `id` is answered elsewhere (at the TUI) and leaves the list.
    pub fn withdraw(&self, id: &str) {
        lock(&self.questions).retain(|(known, _)| known != id);
    }

    fn question_list(&self) -> String {
        let rows: Vec<String> = lock(&self.questions)
            .iter()
            .map(|(_, json)| json.clone())
            .collect();
        format!("[{}]", rows.join(","))
    }

    fn reply(&self, id: &str, body: &[u8]) -> Reply {
        let mut questions = lock(&self.questions);
        let Some(at) = questions.iter().position(|(known, _)| known == id) else {
            return Reply::status(404);
        };
        questions.remove(at);
        drop(questions);
        let body = String::from_utf8_lossy(body).into_owned();
        if let Some(path) = lock(&self.reply_log).as_ref() {
            use std::io::Write;
            let logged = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut file| writeln!(file, "{id} {body}"));
            if logged.is_err() {
                return Reply::status(500);
            }
        }
        lock(&self.replies).push((id.into(), body));
        Reply::json(200, "true")
    }

    /// The stub's request handler.
    pub fn handler(self: &Arc<Self>) -> impl Fn(&Request) -> Reply + Send + Sync + 'static {
        let server = Arc::clone(self);
        move |request| server.answer(request)
    }

    fn answer(&self, request: &Request) -> Reply {
        if let Some(expected) = &self.expected {
            if request.authorization.as_deref() != Some(expected.as_str()) {
                return Reply::status(401);
            }
        }
        let path = request.path.split('?').next().unwrap_or_default();
        if path == "/session/status" {
            return Reply::json(200, "{}");
        }
        if path == "/question" && request.method == "GET" {
            return Reply::json(200, self.question_list());
        }
        if let Some(id) = path
            .strip_prefix("/question/")
            .and_then(|rest| rest.strip_suffix("/reply"))
            .filter(|_| request.method == "POST")
        {
            return self.reply(id, &request.body);
        }
        match path.strip_prefix("/session").unwrap_or("-") {
            "" => Reply::json(200, self.session_list()),
            rest => match rest.strip_prefix('/').map(|r| r.split_once('/')) {
                Some(Some((id, "message"))) => {
                    match self.messages_of(id, request.path.ends_with("?limit=1")) {
                        Some(body) => Reply::json(200, body),
                        None => Reply::status(404),
                    }
                }
                Some(None) => match self.session(&rest[1..]) {
                    Some(body) => Reply::json(200, body),
                    None => Reply::status(404),
                },
                _ if path == "/event" => {
                    let stream = self.events.subscribe();
                    for session in lock(&self.replay_idle).iter() {
                        self.events.send(&idle(session));
                    }
                    stream
                }
                _ => Reply::status(404),
            },
        }
    }

    fn session_list(&self) -> String {
        let rows: Vec<String> = lock(&self.sessions)
            .iter()
            .map(|(id, parent)| session_json(id, parent.as_deref()))
            .collect();
        format!("[{}]", rows.join(","))
    }

    fn session(&self, id: &str) -> Option<String> {
        lock(&self.sessions)
            .iter()
            .find(|(known, _)| known == id)
            .map(|(id, parent)| session_json(id, parent.as_deref()))
    }

    fn messages_of(&self, id: &str, last_only: bool) -> Option<String> {
        lock(&self.messages)
            .iter()
            .find(|(session, _)| session == id)
            .map(|(_, rows)| {
                let from = if last_only {
                    rows.len().saturating_sub(1)
                } else {
                    0
                };
                format!("[{}]", rows[from..].join(","))
            })
    }

    /// Sets `session`'s message list.
    pub fn set_messages(&self, session: &str, rows: Vec<String>) {
        let mut messages = lock(&self.messages);
        messages.retain(|(known, _)| known != session);
        messages.push((session.to_string(), rows));
    }

    /// Adds a session (with its parent, for a subagent's).
    pub fn add_session(&self, id: &str, parent: Option<&str>) {
        lock(&self.sessions).push((id.into(), parent.map(Into::into)));
    }

    /// A turn of [`SESSION`] ended now with `reply` (a stopped assistant message), and the
    /// stream says the session went idle.
    pub fn turn(&self, reply: &str) {
        self.turn_at(reply, now_ms());
    }

    /// The same, the message completed at `completed` (ms since the epoch).
    pub fn turn_at(&self, reply: &str, completed: u128) {
        {
            let mut messages = lock(&self.messages);
            let at = messages
                .iter()
                .position(|(id, _)| id == SESSION)
                .unwrap_or_else(|| {
                    messages.push((SESSION.into(), Vec::new()));
                    messages.len() - 1
                });
            let turns = &mut messages[at].1;
            let n = turns.len() + 1;
            turns.push(user(&format!("msg_u{n}"), "a prompt"));
            turns.push(assistant_to(
                &format!("msg_a{n}"),
                &format!("msg_u{n}"),
                "\"stop\"",
                completed,
                &text_part(reply),
            ));
        }
        self.events.send(&idle(SESSION));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Milliseconds since the epoch, now.
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

pub fn session_json(id: &str, parent: Option<&str>) -> String {
    match parent {
        Some(parent) => format!(r#"{{"id":"{id}","title":"t","parentID":"{parent}"}}"#),
        None => format!(r#"{{"id":"{id}","title":"t"}}"#),
    }
}

/// The `data:` payload of `session.idle` for `session`.
pub fn idle(session: &str) -> String {
    format!(r#"{{"id":"e1","type":"session.idle","properties":{{"sessionID":"{session}"}}}}"#)
}

/// A JSON string literal (quotes and escapes).
pub fn quoted(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn user(id: &str, text: &str) -> String {
    format!(
        r#"{{"info":{{"id":"{id}","sessionID":"{SESSION}","role":"user","time":{{"created":1}},"summary":{{"title":"t","diffs":[]}}}},"parts":[{{"id":"p{id}","type":"text","text":{}}}]}}"#,
        quoted(text)
    )
}

/// An assistant message: `finish` as JSON (`null`: none), completed at `completed` (ms),
/// its parts as given.
pub fn assistant(id: &str, finish: &str, completed: u128, parts: &str) -> String {
    assistant_to(id, "u", finish, completed, parts)
}

/// An assistant step linked to the user prompt it actually answers.
pub fn assistant_to(id: &str, parent: &str, finish: &str, completed: u128, parts: &str) -> String {
    format!(
        r#"{{"info":{{"id":"{id}","sessionID":"{SESSION}","role":"assistant","time":{{"created":{completed},"completed":{completed}}},"parentID":"{parent}","modelID":"m","providerID":"p","mode":"build","cost":0,"finish":{finish}}},"parts":[{parts}]}}"#
    )
}

pub fn text_part(text: &str) -> String {
    format!(r#"{{"type":"text","text":{}}}"#, quoted(text))
}
