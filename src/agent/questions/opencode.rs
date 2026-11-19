//! Pending questions on the OpenCode 1.x instance API.
//!
//! Attach to a server and session whose relationship to the PTY is known to the host.
//! The client keeps one already-connected loopback socket. It never follows redirects
//! or reconnects to a port after the server exits. The host supplies the same workspace
//! directory and session ID as its TUI, and revokes the feed before changing that binding.
//! Only the question list and reply routes are used.

mod wire;

use std::collections::HashSet;
use std::net::{Shutdown, TcpStream};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::{Engine, engine::general_purpose::STANDARD};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use serde::de::{IgnoredAny, SeqAccess, Visitor};

use super::{
    Answer, AnswerError, MAX_FEED_QUESTIONS, MAX_FIELD, MAX_OPTIONS, MAX_QUESTION_BYTES,
    MAX_QUESTIONS, PendingQuestion, QuestionFeed, QuestionItem,
};
use crate::{Error, Result};
use wire::Connection;

const MAX_ID: usize = 128;

/// Host-bound, revocable OpenCode 1.x question feed. A single socket serializes polling
/// and answers. The caller can put it in an `Arc` to retain the revocation handle.
pub struct OpenCodeQuestions {
    connection: Mutex<Connection>,
    interrupt: TcpStream,
    revoked: AtomicBool,
    session: String,
    query: String,
}

impl OpenCodeQuestions {
    /// Uses an already-connected, host-verified loopback peer and an explicit TUI
    /// workspace/session. Construction checks arguments but performs no HTTP exchange.
    /// The host must control the server identity and keep the PTY on this session until
    /// revocation, including session switches made from the keyboard. The connection
    /// authenticates HTTP requests. The host establishes which session the TUI displays.
    ///
    /// Rejects non-loopback sockets, relative or non-UTF-8 directories, invalid session
    /// IDs, and empty or oversized Basic credentials. Polls are bounded synchronous I/O
    /// on the question worker. Each reply re-reads the pending request on the same
    /// connection. An ambiguous POST returns `Unconfirmed` and closes the connection.
    pub fn from_verified_stream(
        stream: TcpStream,
        directory: &Path,
        session: &str,
        username: &str,
        password: &str,
    ) -> Result<Self> {
        let peer = stream.peer_addr().map_err(socket_error)?;
        if !peer.ip().is_loopback()
            || !stream
                .local_addr()
                .map_err(socket_error)?
                .ip()
                .is_loopback()
        {
            return Err(invalid());
        }
        let path = directory
            .to_str()
            .filter(|p| {
                directory.is_absolute() && p.len() <= 4096 && !p.chars().any(char::is_control)
            })
            .ok_or_else(invalid)?;
        if !valid_id(session, "ses_")
            || username.is_empty()
            || username.len() > 128
            || username.contains(':')
            || username.chars().any(char::is_control)
            || password.is_empty()
            || password.len() > 1024
            || password.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        stream.set_nonblocking(false).map_err(socket_error)?;
        let interrupt = stream.try_clone().map_err(socket_error)?;
        let auth = STANDARD.encode(format!("{username}:{password}"));
        Ok(Self {
            connection: Mutex::new(Connection::new(stream, peer, auth)),
            interrupt,
            revoked: AtomicBool::new(false),
            session: session.to_owned(),
            query: format!("?directory={}", utf8_percent_encode(path, NON_ALPHANUMERIC)),
        })
    }

    /// Permanently closes this socket, including I/O already in progress. An answer
    /// whose bytes were written before revocation may already have reached the server.
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        let _ = self.interrupt.shutdown(Shutdown::Both);
    }

    fn pending_on(
        &self,
        io: &mut Connection,
    ) -> std::result::Result<Vec<PendingQuestion>, AnswerError> {
        if self.revoked.load(Ordering::Acquire) {
            return Err(AnswerError::NotOpen);
        }
        let (status, data) = io
            .exchange(&format!("/question{}", self.query), None)
            .map_err(|_| {
                self.revoke();
                AnswerError::Io
            })?;
        if io.is_closed() || matches!(status, 300..=399 | 401 | 403 | 404 | 410) {
            self.revoke();
            return Err(AnswerError::NotOpen);
        }
        if status != 200 {
            return Err(AnswerError::Io);
        }
        let questions = parse_pending(&data, &self.session)?;
        if self.revoked.load(Ordering::Acquire) {
            return Err(AnswerError::NotOpen);
        }
        Ok(questions)
    }
}

impl QuestionFeed for OpenCodeQuestions {
    fn pending(&self) -> std::result::Result<Vec<PendingQuestion>, AnswerError> {
        let mut connection = self.connection.lock().map_err(|_| AnswerError::Io)?;
        self.pending_on(&mut connection)
    }

    fn answer(
        &self,
        pending: &PendingQuestion,
        answers: &[Answer],
        current: &dyn Fn() -> bool,
    ) -> std::result::Result<(), AnswerError> {
        if !valid_id(&pending.id, "que_") {
            return Err(AnswerError::NotOpen);
        }
        let mut connection = self.connection.lock().map_err(|_| AnswerError::Io)?;
        let visible = self
            .pending_on(&mut connection)?
            .into_iter()
            .find(|q| q.id == pending.id)
            .ok_or(AnswerError::NotOpen)?;
        if visible.questions != pending.questions {
            return Err(AnswerError::Changed);
        }
        let payload = encode_answers(&visible.questions, answers)?;
        if self.revoked.load(Ordering::Acquire) || !current() {
            return Err(AnswerError::NotOpen);
        }
        let path = format!("/question/{}/reply{}", pending.id, self.query);
        match connection.exchange(&path, Some(&payload)) {
            Ok((200, body)) if matches!(serde_json::from_slice::<bool>(&body), Ok(true)) => Ok(()),
            Ok((404, _)) => Err(AnswerError::NotOpen),
            _ => {
                self.revoke();
                Err(AnswerError::Unconfirmed)
            }
        }
    }
}

fn invalid() -> Error {
    Error::InvalidInput("invalid OpenCode question feed binding".into())
}

fn socket_error(source: std::io::Error) -> Error {
    Error::Io {
        context: "OpenCode question socket".into(),
        source,
    }
}

fn valid_id(value: &str, prefix: &str) -> bool {
    value.len() <= MAX_ID
        && value.strip_prefix(prefix).is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        })
}

#[derive(Deserialize)]
struct Request {
    id: String,
    #[serde(rename = "sessionID")]
    session: String,
    questions: List<Info, MAX_QUESTIONS>,
}

#[derive(Deserialize)]
struct Info {
    header: String,
    question: String,
    options: List<OptionInfo, MAX_OPTIONS>,
    #[serde(default)]
    multiple: bool,
    #[serde(default = "allow_custom")]
    custom: bool,
}

fn allow_custom() -> bool {
    true
}

#[derive(Deserialize)]
struct OptionInfo {
    label: String,
}

/// Decode at most N elements. Still consume the array so the JSON parser can validate
/// the remainder without allocating a provider struct or retaining strings for it.
struct List<T, const N: usize> {
    values: Vec<T>,
    overflow: bool,
}

impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for List<T, N> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Limited<T, const N: usize>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for Limited<T, N> {
            type Value = List<T, N>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a bounded list")
            }
            fn visit_seq<S: SeqAccess<'de>>(
                self,
                mut seq: S,
            ) -> std::result::Result<Self::Value, S::Error> {
                let mut values = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(N));
                while values.len() < N {
                    let Some(value) = seq.next_element()? else {
                        return Ok(List {
                            values,
                            overflow: false,
                        });
                    };
                    values.push(value);
                }
                let mut overflow = false;
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    overflow = true;
                }
                Ok(List { values, overflow })
            }
        }
        d.deserialize_seq(Limited::<T, N>(std::marker::PhantomData))
    }
}

fn parse_pending(
    data: &[u8],
    session: &str,
) -> std::result::Result<Vec<PendingQuestion>, AnswerError> {
    let rows: List<Request, MAX_FEED_QUESTIONS> =
        serde_json::from_slice(data).map_err(|_| AnswerError::Io)?;
    if rows.overflow {
        return Err(AnswerError::InputTooLarge);
    }
    let mut known = HashSet::new();
    let mut result = Vec::new();
    for row in rows.values {
        if !valid_id(&row.id, "que_")
            || !valid_id(&row.session, "ses_")
            || !known.insert(row.id.clone())
        {
            return Err(AnswerError::Changed);
        }
        if row.session != session {
            continue;
        }
        if row.questions.values.is_empty() || row.questions.overflow {
            return Err(AnswerError::InputTooLarge);
        }
        let mut total = row.id.len();
        let mut items = Vec::with_capacity(row.questions.values.len());
        for info in row.questions.values {
            if info.options.overflow {
                return Err(AnswerError::InputTooLarge);
            }
            let labels: Vec<_> = info.options.values.into_iter().map(|o| o.label).collect();
            let mut distinct = HashSet::new();
            if info.question.trim().is_empty()
                || (labels.is_empty() && !info.custom)
                || labels
                    .iter()
                    .any(|v| v.trim().is_empty() || !distinct.insert(v))
            {
                return Err(AnswerError::Changed);
            }
            for field in [&info.header, &info.question].into_iter().chain(&labels) {
                total += field.len();
                if field.len() > MAX_FIELD || total > MAX_QUESTION_BYTES {
                    return Err(AnswerError::InputTooLarge);
                }
                if field
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
                {
                    return Err(AnswerError::Changed);
                }
            }
            items.push(
                QuestionItem::new(info.header, info.question, labels)
                    .with_free_text(info.custom)
                    .with_multi_select(info.multiple),
            );
        }
        result.push(PendingQuestion::new(row.id, items));
    }
    Ok(result)
}

fn encode_answers(
    items: &[QuestionItem],
    answers: &[Answer],
) -> std::result::Result<Vec<u8>, AnswerError> {
    if answers.len() != items.len() {
        return Err(AnswerError::Unsupported);
    }
    let mut encoded = Vec::with_capacity(items.len());
    for (item, answer) in items.iter().zip(answers) {
        let label = |i: usize| {
            item.options
                .get(i)
                .map(String::as_str)
                .ok_or(AnswerError::Unsupported)
        };
        let values = match answer {
            Answer::Choice(i) => vec![label(*i)?],
            Answer::Choices(indices)
                if item.multi_select
                    && !indices.is_empty()
                    && indices.len() <= item.options.len() =>
            {
                let mut seen = HashSet::new();
                let mut labels = Vec::with_capacity(indices.len());
                for &i in indices {
                    if !seen.insert(i) {
                        return Err(AnswerError::Unsupported);
                    }
                    labels.push(label(i)?);
                }
                labels
            }
            Answer::Text(value) if item.free_text && !value.trim().is_empty() => {
                if value.len() > MAX_FIELD {
                    return Err(AnswerError::InputTooLarge);
                }
                vec![value.as_str()]
            }
            _ => return Err(AnswerError::Unsupported),
        };
        encoded.push(values);
    }
    #[derive(serde::Serialize)]
    struct Reply<'a> {
        answers: Vec<Vec<&'a str>>,
    }
    let bytes = serde_json::to_vec(&Reply { answers: encoded }).map_err(|_| AnswerError::Io)?;
    if bytes.len() > MAX_QUESTION_BYTES {
        return Err(AnswerError::InputTooLarge);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests;
