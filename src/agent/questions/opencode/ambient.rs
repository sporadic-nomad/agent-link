//! OpenCode questions from the TUI's own server, best effort: the server a terminal
//! spawned with [`spawn_with_ambient_feed`](crate::agent::AgentTerminal::spawn_with_ambient_feed)
//! launched, at the same endpoint and with the same credentials as the ambient OpenCode
//! turn feed, over the same bounded loopback client and with the same check of who serves
//! each connection.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;

use super::{encode_answers, parse_pending, question_deadline, valid_id};
use crate::agent::AgentTerminal;
use crate::agent::loopback::owner::{self, Owner};
use crate::agent::loopback::{self, Connection};
use crate::agent::questions::{Answer, AnswerError, PendingQuestion, QuestionFeed};
use crate::agent::state::AgentProgram;
use crate::agent::turns::ambient::AmbientEndpoint;

/// A new connection is made (and its server's owner found) within this.
const CONNECT: Duration = Duration::from_secs(1);
/// How long the server has to accept a connection before its owner counts as unseen.
const ACCEPT_WAIT: Duration = Duration::from_millis(500);
/// Deadline for the unauthenticated probe's response head.
const PROBE_TIME: Duration = Duration::from_secs(1);

/// The launched agent's program: its Unix process group (a direct child's own), or on
/// Windows its process; `None` while it is not known.
type Program = dyn Fn() -> Option<u32> + Send + Sync;

/// OpenCode 1.x questions from the TUI's own server, for a terminal spawned with
/// [`AgentTerminal::spawn_with_ambient_feed`] and the OpenCode profile. Pass it to
/// [`AgentTerminal::observe_feed_questions`] while the launched agent runs; like every
/// feed it is released when the agent relaunches, leaves or the terminal ends, so make a
/// new one per launch. Construction performs no I/O.
///
/// The host supplies the root session ID displayed by the TUI and keeps it on that
/// conversation while observing. Call [`AgentTerminal::stop_questions`] before switching
/// conversations, then attach a new feed for the selected session.
///
/// - **Whose server?** Before anything is written on a connection — the list's, a session
///   lookup's and the one carrying an answer — the process holding its server end must be
///   the launched agent's (as for the ambient turn feed). Another process ends the feed
///   ([`AnswerError::NotOpen`]) with nothing sent; one not seen yet is tried again at the
///   next poll. Once per launch, `GET /question` without credentials must be refused
///   (`401`); a server answering it does not enforce the launch's password and ends the
///   feed, as do refused credentials (`401`/`403`), a redirect, `404` or `410`.
/// - **Which conversation?** Both polling and answer revalidation filter `GET /question`
///   by the supplied session ID. A matching request also requires a root session
///   (no `parentID`), checked with `GET /session/{id}` and cached for the program.
/// - **Answer.** The pending request is read again, its exact questions compared, the
///   terminal's foreground checked (`current`), then the selected labels or text are
///   posted to `/question/{id}/reply` on a connection verified as above. `true` is
///   success, `404` means the request is gone ([`AnswerError::NotOpen`]), and anything
///   else — another status or body, a failed or cut exchange — may have been taken:
///   [`AnswerError::Unconfirmed`], and the connection is dropped.
///
/// Requests go without a `directory` query: the TUI's server serves its own project.
/// The list and its session lookup share a three-second I/O deadline. Posting an
/// answer has a separate three-second I/O deadline. Each response is capped at 256 KiB.
/// One connection is kept between calls and serializes them. Routes and shapes follow
/// OpenCode 1.18.21 and are tested against a synthetic server only. `Debug` never shows
/// the password.
pub struct OpenCodeAmbientQuestions {
    endpoint: AmbientEndpoint,
    session: String,
    basic: String,
    program: Box<Program>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The program the connection and the probe were for.
    program: Option<u32>,
    connection: Option<Connection>,
    /// The server refused a request without credentials.
    probed: bool,
    /// Whether the bound session belongs to a subagent or has been deleted.
    subagent: Option<bool>,
    /// The server must not be used again.
    ended: bool,
}

impl OpenCodeAmbientQuestions {
    /// Questions for the TUI's host-attested `session` on the OpenCode server at the endpoint
    /// [`spawn_with_ambient_feed`](AgentTerminal::spawn_with_ambient_feed) gave it.
    /// Returns [`Error::InvalidInput`](crate::Error::InvalidInput) for a terminal spawned
    /// otherwise or an invalid OpenCode session ID. Performs no I/O.
    pub fn new(term: &AgentTerminal, session: &str) -> crate::Result<Self> {
        if !valid_id(session, "ses_") {
            return Err(crate::Error::InvalidInput(
                "invalid OpenCode session ID".into(),
            ));
        }
        let base = Arc::clone(&term.ambient);
        let endpoint = base.endpoint.clone().ok_or_else(|| {
            crate::Error::InvalidInput(
                "the terminal was not spawned with spawn_with_ambient_feed".into(),
            )
        })?;
        let ctl = Arc::downgrade(&term.ctl);
        let program = move || {
            let ctl = ctl.upgrade()?;
            if base.direct {
                return base.child;
            }
            ctl.read(|st| match st.program {
                AgentProgram::Known(id) => Some(id.0),
                _ => None,
            })
        };
        Ok(Self::with_program(endpoint, session, Box::new(program)))
    }

    fn with_program(endpoint: AmbientEndpoint, session: &str, program: Box<Program>) -> Self {
        let basic = STANDARD.encode(format!("{}:{}", endpoint.username(), endpoint.password()));
        Self {
            endpoint,
            session: session.to_owned(),
            basic,
            program,
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The kept connection, or a new one whose server end `program` holds; nothing is
    /// written on a new one before that is found. The probe runs once per program.
    fn connect(
        &self,
        state: &mut State,
        program: u32,
        until: Instant,
    ) -> Result<Connection, AnswerError> {
        if state.program != Some(program) {
            state.program = Some(program);
            state.connection = None;
            state.probed = false;
            state.subagent = None;
        }
        if let Some(connection) = state.connection.take().filter(|c| !c.is_closed()) {
            return Ok(connection);
        }
        if !state.probed {
            let probe = self.verified(state, program, until)?;
            match probe.probe("/question", until.min(Instant::now() + PROBE_TIME)) {
                Ok(401) => state.probed = true,
                Ok(200..=399 | 404 | 410) => {
                    state.ended = true;
                    return Err(AnswerError::NotOpen);
                }
                _ => return Err(AnswerError::Io),
            }
        }
        self.verified(state, program, until)
    }

    fn verified(
        &self,
        state: &mut State,
        program: u32,
        until: Instant,
    ) -> Result<Connection, AnswerError> {
        let addr = self.endpoint.addr();
        let socket = loopback::connect(addr, until.min(Instant::now() + CONNECT))
            .map_err(|_| AnswerError::Io)?;
        match owner::server_of(&socket, program, until.min(Instant::now() + ACCEPT_WAIT)) {
            Owner::Agent => Ok(Connection::new(socket, addr, self.basic.clone())),
            Owner::Unseen => Err(AnswerError::Io),
            Owner::Other | Owner::Unsupported => {
                state.ended = true;
                Err(AnswerError::NotOpen)
            }
        }
    }

    /// An authenticated GET on a verified connection, kept when it stays open.
    fn get(
        &self,
        state: &mut State,
        program: u32,
        path: &str,
        until: Instant,
    ) -> Result<(u16, Vec<u8>), AnswerError> {
        let mut connection = self.connect(state, program, until)?;
        let answer = connection
            .exchange(path, None, until)
            .map_err(|_| AnswerError::Io)?;
        if !connection.is_closed() {
            state.connection = Some(connection);
        }
        if matches!(answer.0, 401 | 403) {
            state.ended = true;
            return Err(AnswerError::NotOpen);
        }
        Ok(answer)
    }

    /// The pending requests of the terminal's conversation (see the type's docs).
    fn list(&self, state: &mut State) -> Result<Vec<PendingQuestion>, AnswerError> {
        if state.ended {
            return Err(AnswerError::NotOpen);
        }
        let until = question_deadline();
        let program = (self.program)().ok_or(AnswerError::Io)?;
        let (status, body) = self.get(state, program, "/question", until)?;
        match status {
            200 => {}
            300..=399 | 404 | 410 => {
                state.ended = true;
                return Err(AnswerError::NotOpen);
            }
            _ => return Err(AnswerError::Io),
        }
        let questions = parse_pending(&body, &self.session)?;
        if !questions.is_empty() && self.subagent(state, program, until)? {
            return Ok(Vec::new());
        }
        Ok(questions)
    }

    /// Whether to exclude the bound session because it is a subagent's or was deleted.
    fn subagent(
        &self,
        state: &mut State,
        program: u32,
        until: Instant,
    ) -> Result<bool, AnswerError> {
        if let Some(known) = state.subagent {
            return Ok(known);
        }
        let path = format!("/session/{}", self.session);
        let (status, body) = self.get(state, program, &path, until)?;
        let subagent = match status {
            200 => {
                let info: SessionInfo =
                    serde_json::from_slice(&body).map_err(|_| AnswerError::Io)?;
                if info.id != self.session {
                    return Err(AnswerError::Io);
                }
                info.parent.is_some_and(|parent| !parent.is_empty())
            }
            404 => true,
            _ => return Err(AnswerError::Io),
        };
        state.subagent = Some(subagent);
        Ok(subagent)
    }
}

impl QuestionFeed for OpenCodeAmbientQuestions {
    fn pending(&self) -> Result<Vec<PendingQuestion>, AnswerError> {
        self.list(&mut self.lock())
    }

    fn answer(
        &self,
        pending: &PendingQuestion,
        answers: &[Answer],
        current: &dyn Fn() -> bool,
    ) -> Result<(), AnswerError> {
        if !valid_id(&pending.id, "que_") {
            return Err(AnswerError::NotOpen);
        }
        let mut state = self.lock();
        let visible = self
            .list(&mut state)?
            .into_iter()
            .find(|q| q.id == pending.id)
            .ok_or(AnswerError::NotOpen)?;
        if visible.questions != pending.questions {
            return Err(AnswerError::Changed);
        }
        let payload = encode_answers(&visible.questions, answers)?;
        let program = (self.program)()
            .filter(|id| Some(*id) == state.program)
            .ok_or(AnswerError::NotOpen)?;
        // The list's connection, or a new one whose server end the agent holds.
        let until = question_deadline();
        let mut connection = self.connect(&mut state, program, until)?;
        if !current() {
            state.connection = Some(connection);
            return Err(AnswerError::NotOpen);
        }
        let path = format!("/question/{}/reply", pending.id);
        match connection.exchange(&path, Some(&payload), until) {
            Ok((200, body)) if matches!(serde_json::from_slice::<bool>(&body), Ok(true)) => {
                if !connection.is_closed() {
                    state.connection = Some(connection);
                }
                Ok(())
            }
            Ok((404, _)) => {
                if !connection.is_closed() {
                    state.connection = Some(connection);
                }
                Err(AnswerError::NotOpen)
            }
            _ => Err(AnswerError::Unconfirmed),
        }
    }
}

impl fmt::Debug for OpenCodeAmbientQuestions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("OpenCodeAmbientQuestions")
            .field("endpoint", &self.endpoint)
            .field("connected", &state.connection.is_some())
            .field("ended", &state.ended)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct SessionInfo {
    id: String,
    #[serde(rename = "parentID", default)]
    parent: Option<String>,
}

#[cfg(test)]
mod tests;
