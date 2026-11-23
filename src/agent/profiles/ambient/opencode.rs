//! OpenCode's ambient turn feed: its TUI's own HTTP server. OpenCode keeps its sessions in
//! a database, not in a file to tail; its TUI serves them when launched with a port.
//! Routes, event names and message shapes follow OpenCode 1.18.21 as the predecessor crate
//! used them (verified there by hand, with no automated live test); nothing here was
//! checked against a running OpenCode, and OpenCode 2.x is not covered.
//!
//! - **Launch** ([`launch_server`], only through
//!   [`AgentTerminal::spawn_with_ambient_feed`](crate::agent::AgentTerminal::spawn_with_ambient_feed)):
//!   `--port <a 127.0.0.1 port free a moment before> --hostname 127.0.0.1`, and
//!   `OPENCODE_SERVER_USERNAME=opencode` with `OPENCODE_SERVER_PASSWORD=<16 bytes of OS
//!   randomness as 32 hex digits>` (over the host's values for those keys).
//! - **Whose server?** Before any request carries the password, the connection's server
//!   end must be held by the launched agent's process (its process group on Unix, it or a
//!   process below it on Windows; [`owner`](crate::agent::loopback::owner)). A connection
//!   served by another process is never sent anything and ends the feed for this launch
//!   ([`AmbientFeedUnavailable::OwnerMismatch`]); one whose owner cannot be seen yet is
//!   retried with backoff ([`AmbientFeedUnavailable::OwnerUnverified`]).
//! - **Protected?** On a verified connection `GET /session` without the password must be
//!   refused (`401`): a server that answers it does not enforce the launch's password
//!   ([`AmbientFeedUnavailable::Unprotected`], final). With the password, `GET /session`
//!   must answer `200` and a list (its sessions' `parentID`s are learnt); `401`/`403` is
//!   final ([`AmbientFeedUnavailable::CredentialsRefused`]). Then `GET /event`, on its own
//!   verified connection, opens the event stream: [`AmbientFeedState::Ready`].
//! - **Events**: one JSON `{type, properties}` per `data:` line. `session.idle`, or
//!   `session.status` with `status.type` `idle`, queues its session (an id of letters,
//!   digits, `_` and `-`, at most 128 bytes: it goes into a request path).
//!   `session.created` / `session.updated` teach a session's `parentID`, `session.deleted`
//!   forgets it. A stream that ends is opened again after a backoff (250 ms doubling to
//!   8 s; a stream open 10 s resets it). OpenCode does not replay what it sent meanwhile.
//! - **Turn end**: a queued session that is no subagent's (the `task` tool's sessions end
//!   their own turns inside the user's; one neither the list nor an event named is looked
//!   up with `GET /session/{id}`) → `GET /session/{id}/message`. The reader visits new turns in message-list
//!   order, resuming after the last handled message ID for that session. Assistant
//!   `parentID` changes separate prompts; a queued user message alone proves no end.
//!   Auto-compaction and its synthetic continuation keep the prompt open. Only
//!   its last non-summary assistant message supplies a turn end, with `time.completed`
//!   or a nonempty `finish`. A finished message uses `time.created` when its completion
//!   time is missing. `tool-calls`, `unknown`, and tool parts needing a model follow-up
//!   cannot end a turn, even with `finish: "stop"`. Provider-executed tools and
//!   cleanup-marked interrupted errors need no follow-up. The trailing assistant must
//!   answer the latest prompt; `GET /session/status` must show idle and
//!   a subsequent `?limit=1` lookup must still show the same last message ID.
//!   Only when it `finish`ed `stop` is there a
//!   reply: its `text` parts that are neither `synthetic` nor `ignored`, blank ones left
//!   out, joined by a blank line. A lookup or fetch that fails is tried at the next polls,
//!   three times in all. Final polls use only connections verified during the launch.
//!   Pending work that requires a new connection or exhausts its retries ends observation with
//!   [`IncompleteDrain`](crate::agent::turns::ambient::AmbientClosed::IncompleteDrain).
//! - **Shape check**: a `data:` line that is not JSON, an event without a `type`, a known
//!   turn event without its fields and an answer of another shape are
//!   [`AmbientFeedEvent::Unrecognised`]; reading goes on.
//!
//! Questions (OpenCode's `question` tool: `question.asked` / `question.replied` /
//! `question.rejected`, `GET /question`, `POST /question/{id}/reply`) are not read here;
//! the event match in [`Reader::event`] and the verified request connection are where a
//! follow-up would hook them in.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::net::{Ipv4Addr, TcpListener};
use std::time::{Duration, Instant, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

use crate::agent::LaunchPlan;
use crate::agent::loopback::owner::{self, Owner};
use crate::agent::loopback::{self, Connection, DrainLimits, EventStream, Exchange, Opened};
use crate::agent::turns::TurnReply;
use crate::agent::turns::ambient::{
    AmbientContext, AmbientEndpoint, AmbientFeed, AmbientFeedBudget, AmbientFeedEvent,
    AmbientFeedPoll, AmbientFeedReader, AmbientFeedState, AmbientFeedUnavailable,
    AmbientUnavailable,
};

/// The host OpenCode's server is told to listen on: loopback only.
const HOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
/// The Basic-auth user OpenCode pairs with its password (its default, set anyway).
pub(in crate::agent) const USERNAME: &str = "opencode";
pub(in crate::agent) const USERNAME_ENV: &str = "OPENCODE_SERVER_USERNAME";
pub(in crate::agent) const PASSWORD_ENV: &str = "OPENCODE_SERVER_PASSWORD";

/// Adds OpenCode's server to `plan`: `--port <a 127.0.0.1 port free a moment before>
/// --hostname 127.0.0.1` and the username and a fresh password in its env, replacing any
/// value the plan had for those keys. The port is bound, read and released here: another
/// process can take it before OpenCode does (the feed then finds that process serving it
/// and ends with [`AmbientFeedUnavailable::OwnerMismatch`]).
pub(in crate::agent) fn launch_server(plan: &mut LaunchPlan) -> crate::Result<AmbientEndpoint> {
    if plan.args.iter().any(|arg| arg == "--") {
        return Err(crate::Error::InvalidInput(
            "cannot append OpenCode server flags after an end-of-options marker".into(),
        ));
    }
    let taken = |flag: &str| {
        plan.args
            .iter()
            .any(|arg| arg == flag || arg.starts_with(&format!("{flag}=")))
    };
    if taken("--port") || taken("--hostname") {
        return Err(crate::Error::InvalidInput(
            "the OpenCode launch already sets --port or --hostname".into(),
        ));
    }
    // The TUI's boolean option accepts an explicit value and the last setting wins.
    let mini = plan
        .args
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, arg)| match arg.as_str() {
            "--mini" => Some(plan.args.get(i + 1).is_none_or(|next| next != "false")),
            "--no-mini" => Some(false),
            arg => arg
                .strip_prefix("--mini=")
                .map(|value| matches!(value, "true" | "'true'" | r#""true""#)),
        });
    if mini == Some(true) {
        return Err(crate::Error::InvalidInput(
            "OpenCode --mini cannot serve an ambient feed".into(),
        ));
    }
    let addr = TcpListener::bind((HOST, 0))
        .and_then(|listener| listener.local_addr())
        .map_err(|source| crate::Error::Io {
            context: "pick a loopback port for the OpenCode server".into(),
            source,
        })?;
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| crate::Error::Io {
        context: "OS randomness for the OpenCode server password".into(),
        source: std::io::Error::other(e.to_string()),
    })?;
    let password: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    plan.args.extend([
        "--port".to_string(),
        addr.port().to_string(),
        "--hostname".to_string(),
        HOST.to_string(),
    ]);
    let ours = |key: &str| {
        key.eq_ignore_ascii_case(USERNAME_ENV) || key.eq_ignore_ascii_case(PASSWORD_ENV)
    };
    plan.env.retain(|(key, _)| !ours(key));
    plan.env_defaults.retain(|(key, _)| !ours(key));
    plan.env.push((USERNAME_ENV.into(), USERNAME.into()));
    plan.env.push((PASSWORD_ENV.into(), password.clone()));
    AmbientEndpoint::new(addr, USERNAME, password).ok_or_else(|| {
        crate::Error::InvalidInput("the OpenCode server endpoint is not loopback".into())
    })
}

/// OpenCode's [`AmbientFeed`]: its TUI's server at the launch's endpoint.
pub(in crate::agent) struct OpencodeServer;

impl AmbientFeed for OpencodeServer {
    fn open(
        &self,
        _ctx: &AmbientContext,
        endpoint: Option<&AmbientEndpoint>,
    ) -> Box<dyn AmbientFeedReader> {
        Box::new(Reader::new(endpoint.cloned()))
    }
}

/// One exchange at most this long (connect, owner check and request).
const EXCHANGE: Duration = Duration::from_secs(1);
/// How long the server has to accept a connection before its owner is looked for again.
const ACCEPT_WAIT: Duration = Duration::from_millis(500);
/// A drain of the event stream ends once nothing came this long, at most after
/// [`DRAIN_TIME`] (leaving the rest of the poll for reply fetches).
const QUIET: Duration = Duration::from_millis(20);
const DRAIN_TIME: Duration = Duration::from_millis(500);
/// Body bytes of the event stream read per poll.
const DRAIN_BYTES: usize = 4 << 20;
/// Bodies read at most: a session's, the session list's, a message list's (a
/// long session's messages hold every tool output: a longer list's turn is skipped and
/// counted as oversized).
const SESSION_BODY: usize = 64 << 10;
const LIST_BODY: usize = 4 << 20;
const MESSAGES_BODY: usize = 8 << 20;
/// The first wait before trying the server again after a failure; doubled per failure in
/// a row, up to [`RETRY_MAX`].
const RETRY_FIRST: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(8);
/// An event stream that stayed open this long was healthy: the next drop starts the
/// backoff over.
const STABLE: Duration = Duration::from_secs(10);
/// Sessions remembered (subagent or not); past it they are learnt again.
const SESSIONS_KEPT: usize = 4096;
/// Idle sessions queued at most; past it the oldest is dropped (a missed turn).
const IDLE_KEPT: usize = 64;
/// Polls a session's reply is tried in before it is given up.
const READ_ATTEMPTS: u32 = 3;
/// A reply fetch is not started with less than this left of the poll.
const FETCH_MARGIN: Duration = Duration::from_millis(100);

fn feed(reason: AmbientFeedUnavailable) -> AmbientUnavailable {
    AmbientUnavailable::Feed(reason)
}

/// An open event stream.
struct Connected {
    events: EventStream,
    since: Instant,
}

/// What an authenticated request got.
enum Got {
    Answer(u16, Vec<u8>),
    /// The body exceeded its cap.
    TooLarge,
}

/// Why a request got no usable answer.
enum Failed {
    /// Final for the launch.
    Ended(AmbientUnavailable),
    /// Worth another try.
    Again,
}

/// One launch's reader. Holds the password (as the encoded header value) only in memory;
/// its `Debug` shows neither.
struct Reader {
    endpoint: Option<AmbientEndpoint>,
    basic: String,
    /// The program the connections were verified for; another one drops them.
    program: Option<u32>,
    /// A verified connection for requests, kept open between them.
    request: Option<Connection>,
    stream: Option<Connected>,
    /// The server answered since the launch (a refused connection is no longer "still
    /// starting").
    answered: bool,
    /// The state reported until the next attempt.
    reported: AmbientFeedState,
    /// Failed attempts (and short-lived streams) in a row, and when to try again.
    failures: u32,
    retry_at: Option<Instant>,
    /// Classification and last handled turn for each known session.
    sessions: HashMap<String, Session>,
    /// Sessions that went idle and are not handled yet, oldest first.
    idle: Vec<String>,
    /// Session id → failed reads of its reply so far.
    attempts: HashMap<String, u32>,
}

#[derive(Clone)]
struct Session {
    subagent: bool,
    cursor: Option<MessageCursor>,
}

#[derive(Clone)]
struct MessageCursor {
    id: String,
    completed: u64,
}

impl fmt::Debug for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader")
            .field("endpoint", &self.endpoint)
            .field("connected", &self.stream.is_some())
            .finish_non_exhaustive()
    }
}

impl AmbientFeedReader for Reader {
    fn poll(&mut self, ctx: &AmbientContext, budget: &AmbientFeedBudget) -> AmbientFeedPoll {
        if self.endpoint.is_none() {
            return AmbientFeedPoll::new(AmbientFeedState::Ended(feed(
                AmbientFeedUnavailable::NotServed,
            )));
        }
        let mut poll = AmbientFeedPoll::new(AmbientFeedState::Ready);
        if ctx.program != self.program {
            // Verified for another program (or none): nothing carries over.
            self.request = None;
            self.stream = None;
            self.sessions.clear();
            self.idle.clear();
            self.attempts.clear();
            self.program = ctx.program;
        }
        let Some(program) = ctx.program else {
            poll.state = AmbientFeedState::Unavailable(AmbientUnavailable::AgentUnknown);
            return poll;
        };
        self.drain(budget, &mut poll);
        if let Err(state) = self.turn_ends(program, budget, &mut poll) {
            poll.state = state;
            return poll;
        }
        let due = self.retry_at.is_none_or(|at| Instant::now() >= at);
        if self.stream.is_none() && due && !budget.last {
            self.reported = self.connect(program, budget.deadline);
        }
        poll.state = if self.stream.is_some() {
            AmbientFeedState::Ready
        } else {
            self.reported
        };
        poll
    }
}

impl Reader {
    fn new(endpoint: Option<AmbientEndpoint>) -> Self {
        let basic = endpoint.as_ref().map_or_else(String::new, |e| {
            STANDARD.encode(format!("{}:{}", e.username(), e.password()))
        });
        Self {
            endpoint,
            basic,
            program: None,
            request: None,
            stream: None,
            answered: false,
            reported: AmbientFeedState::Unavailable(feed(AmbientFeedUnavailable::NotListening)),
            failures: 0,
            retry_at: None,
            sessions: HashMap::new(),
            idle: Vec::new(),
            attempts: HashMap::new(),
        }
    }

    /// Reads what the event stream buffered.
    fn drain(&mut self, budget: &AmbientFeedBudget, poll: &mut AmbientFeedPoll) {
        let Some(connected) = self.stream.as_mut() else {
            return;
        };
        let limits = DrainLimits {
            until: budget.deadline.min(Instant::now() + DRAIN_TIME),
            quiet: QUIET,
            line_bytes: budget.line_bytes,
            lines: budget.events,
            bytes: DRAIN_BYTES,
        };
        let drained = connected.events.drain(&limits);
        poll.oversized = poll.oversized.saturating_add(drained.oversized);
        poll.more |= drained.cut;
        for data in &drained.data {
            self.event(data, &mut poll.events);
        }
        if drained.ended.is_some() {
            self.dropped();
        }
    }

    /// Opens the server: is it the agent's, is it protected, does it take the
    /// credentials, its events.
    fn connect(&mut self, program: u32, deadline: Instant) -> AmbientFeedState {
        let probe = match self.verified(program, deadline) {
            Ok(connection) => connection,
            Err(state) => return state,
        };
        match probe.probe("/session", exchange_until(deadline)) {
            Ok(401) => {}
            Ok(200..=299) => {
                return AmbientFeedState::Ended(feed(AmbientFeedUnavailable::Unprotected));
            }
            Ok(code) => return self.failed(AmbientFeedUnavailable::Status(code)),
            Err(_) => return self.failed(AmbientFeedUnavailable::Protocol),
        }
        let mut request = match self.verified(program, deadline) {
            Ok(connection) => connection,
            Err(state) => return state,
        };
        let list = request.request(&Exchange {
            path: "/session",
            post: None,
            auth: true,
            cap: LIST_BODY,
            until: exchange_until(deadline),
        });
        match list {
            Ok((200, body)) => match self.read_sessions(&body) {
                Ok(()) => {}
                Err(_) => return self.failed(AmbientFeedUnavailable::Protocol),
            },
            Ok((401 | 403, _)) => {
                return AmbientFeedState::Ended(feed(AmbientFeedUnavailable::CredentialsRefused));
            }
            Ok((code, _)) => return self.failed(AmbientFeedUnavailable::Status(code)),
            Err(_) => return self.failed(AmbientFeedUnavailable::Protocol),
        }
        self.answered = true;
        let events = match self.verified(program, deadline) {
            Ok(connection) => connection,
            Err(state) => return state,
        };
        match events.into_events("/event", exchange_until(deadline)) {
            Ok(Opened::Events(events)) => {
                self.stream = Some(Connected {
                    events,
                    since: Instant::now(),
                });
                if !request.is_closed() {
                    self.request = Some(request);
                }
                self.retry_at = None;
                AmbientFeedState::Ready
            }
            Ok(Opened::Status(401 | 403)) => {
                AmbientFeedState::Ended(feed(AmbientFeedUnavailable::CredentialsRefused))
            }
            Ok(Opened::Status(code)) => self.failed(AmbientFeedUnavailable::Status(code)),
            Err(_) => self.failed(AmbientFeedUnavailable::Protocol),
        }
    }

    /// A new connection to the endpoint whose server end the launched agent holds;
    /// nothing is written on it before that is established.
    fn verified(
        &mut self,
        program: u32,
        deadline: Instant,
    ) -> Result<Connection, AmbientFeedState> {
        let Some(addr) = self.endpoint.as_ref().map(AmbientEndpoint::addr) else {
            return Err(AmbientFeedState::Ended(feed(
                AmbientFeedUnavailable::NotServed,
            )));
        };
        let socket = match loopback::connect(addr, exchange_until(deadline)) {
            Ok(socket) => socket,
            // Not up yet since the launch: asked again at every wake.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused && !self.answered => {
                self.retry_at = None;
                return Err(AmbientFeedState::Unavailable(feed(
                    AmbientFeedUnavailable::NotListening,
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                return Err(self.failed(AmbientFeedUnavailable::NotListening));
            }
            Err(_) => return Err(self.failed(AmbientFeedUnavailable::Protocol)),
        };
        let accept = deadline.min(Instant::now() + ACCEPT_WAIT);
        match owner::server_of(&socket, program, accept) {
            Owner::Agent => Ok(Connection::new(socket, addr, self.basic.clone())),
            Owner::Other => Err(AmbientFeedState::Ended(feed(
                AmbientFeedUnavailable::OwnerMismatch,
            ))),
            Owner::Unseen => Err(self.failed(AmbientFeedUnavailable::OwnerUnverified)),
            Owner::Unsupported => Err(AmbientFeedState::Ended(AmbientUnavailable::Unsupported)),
        }
    }

    /// An authenticated GET reuses the verified socket. Only a live poll may open another.
    fn get(
        &mut self,
        program: u32,
        path: &str,
        cap: usize,
        budget: &AmbientFeedBudget,
    ) -> Result<Got, Failed> {
        let mut connection = match self.request.take().filter(|c| !c.is_closed()) {
            Some(connection) => connection,
            None if budget.last => {
                return Err(Failed::Ended(AmbientUnavailable::Io(
                    std::io::ErrorKind::NotConnected,
                )));
            }
            None => self
                .verified(program, budget.deadline)
                .map_err(|state| match state {
                    AmbientFeedState::Ended(reason) => Failed::Ended(reason),
                    AmbientFeedState::Unavailable(_) | AmbientFeedState::Ready => Failed::Again,
                })?,
        };
        let answer = connection.request(&Exchange {
            path,
            post: None,
            auth: true,
            cap,
            until: exchange_until(budget.deadline),
        });
        if !connection.is_closed() {
            self.request = Some(connection);
        }
        match answer {
            Ok((401 | 403, _)) => Err(Failed::Ended(feed(
                AmbientFeedUnavailable::CredentialsRefused,
            ))),
            Ok((code, body)) => Ok(Got::Answer(code, body)),
            Err(e) if loopback::too_large(&e) => Ok(Got::TooLarge),
            Err(e) if budget.last && self.request.is_none() => {
                Err(Failed::Ended(AmbientUnavailable::Io(e.kind())))
            }
            Err(_) => Err(Failed::Again),
        }
    }

    /// An attempt failed: `reason`, and the next one after the backoff.
    fn failed(&mut self, reason: AmbientFeedUnavailable) -> AmbientFeedState {
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(Instant::now() + backoff(self.failures));
        AmbientFeedState::Unavailable(feed(reason))
    }

    /// The event stream ended: opened again after the backoff (at once after a healthy
    /// stream's first wait).
    fn dropped(&mut self) {
        let Some(connected) = self.stream.take() else {
            return;
        };
        if connected.since.elapsed() >= STABLE {
            self.failures = 0;
        }
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(Instant::now() + backoff(self.failures));
        self.reported = AmbientFeedState::Unavailable(feed(AmbientFeedUnavailable::NotListening));
    }

    /// One `data:` payload.
    fn event(&mut self, data: &[u8], events: &mut Vec<AmbientFeedEvent>) {
        let Ok(envelope) = serde_json::from_slice::<Envelope<'_>>(data) else {
            events.push(AmbientFeedEvent::Unrecognised);
            return;
        };
        let Some(kind) = envelope.kind else {
            events.push(AmbientFeedEvent::Unrecognised);
            return;
        };
        match kind.as_ref() {
            "session.idle" | "session.status" => {
                let Ok(event) = serde_json::from_slice::<StatusEvent<'_>>(data) else {
                    events.push(AmbientFeedEvent::Unrecognised);
                    return;
                };
                let properties = event.properties.unwrap_or_default();
                let idle = kind == "session.idle"
                    || properties
                        .status
                        .as_ref()
                        .and_then(|status| status.kind.as_deref())
                        .is_some_and(|kind| kind == "idle");
                let known = kind == "session.idle"
                    || properties.status.as_ref().is_some_and(|s| s.kind.is_some());
                if !known {
                    events.push(AmbientFeedEvent::Unrecognised);
                } else if idle {
                    self.went_idle(properties.session.as_deref(), events);
                }
            }
            "session.created" | "session.updated" | "session.deleted" => {
                let info = serde_json::from_slice::<InfoEvent<'_>>(data)
                    .ok()
                    .and_then(|event| event.properties)
                    .and_then(|properties| properties.info);
                match info.and_then(|info| {
                    let subagent = info.subagent();
                    let id = info.id.filter(|id| valid_id(id))?;
                    Some((id, subagent))
                }) {
                    Some((id, _)) if kind == "session.deleted" => {
                        self.sessions.remove(id.as_ref());
                    }
                    Some((id, subagent)) => self.learn(&id, subagent),
                    None => events.push(AmbientFeedEvent::Unrecognised),
                }
            }
            // Question events (`question.asked`, `question.replied`, `question.rejected`)
            // would be matched here by a follow-up; every other event is not a turn's.
            _ => {}
        }
    }

    fn went_idle(&mut self, session: Option<&str>, events: &mut Vec<AmbientFeedEvent>) {
        match session.filter(|id| valid_id(id)) {
            Some(id) => {
                if !self.idle.iter().any(|known| known == id) {
                    if self.idle.len() >= IDLE_KEPT {
                        let evicted = self.idle.remove(0);
                        self.attempts.remove(&evicted);
                        events.push(AmbientFeedEvent::Unrecognised);
                    }
                    self.idle.push(id.to_string());
                }
            }
            None => events.push(AmbientFeedEvent::Unrecognised),
        }
    }

    fn learn(&mut self, session: &str, subagent: bool) {
        learn_session(&mut self.sessions, session, subagent);
    }

    fn read_sessions(&mut self, body: &[u8]) -> serde_json::Result<()> {
        // A malformed list must leave existing classifications and turn cursors intact.
        let mut sessions = self.sessions.clone();
        let mut decoder = serde_json::Deserializer::from_slice(body);
        (&mut decoder).deserialize_seq(Sessions(&mut sessions))?;
        decoder.end()?;
        self.sessions = sessions;
        Ok(())
    }

    /// The turn ends of the sessions that went idle, while the poll has time (the rest
    /// wait for the next poll, which then comes at once). A session whose lookup or reply
    /// read failed is tried again at the next polls, [`READ_ATTEMPTS`] in all. `Err`: the
    /// feed ended for this launch.
    fn turn_ends(
        &mut self,
        program: u32,
        budget: &AmbientFeedBudget,
        poll: &mut AmbientFeedPoll,
    ) -> Result<(), AmbientFeedState> {
        let mut again = Vec::new();
        while !self.idle.is_empty() {
            let left = budget.deadline.saturating_duration_since(Instant::now());
            if left < FETCH_MARGIN || poll.events.len() >= budget.events {
                poll.more = true;
                break;
            }
            let session = self.idle.remove(0);
            match self.turn_end_of(program, &session, budget, poll) {
                Ok(told) => {
                    self.attempts.remove(&session);
                    match told {
                        Told::Turns(page) => {
                            if let Some(cursor) = page.cursor {
                                if let Some(known) = self.sessions.get_mut(&session) {
                                    known.cursor = Some(cursor);
                                }
                            }
                            poll.events.extend(page.events);
                            if page.more {
                                self.idle.insert(0, session);
                                poll.more = true;
                                break;
                            }
                        }
                        Told::Nothing => {}
                        Told::Unrecognised => poll.events.push(AmbientFeedEvent::Unrecognised),
                        Told::Oversized => poll.oversized = poll.oversized.saturating_add(1),
                    }
                }
                Err(Failed::Ended(reason)) => {
                    poll.more = true;
                    self.idle.insert(0, session);
                    return Err(AmbientFeedState::Ended(reason));
                }
                Err(Failed::Again) => {
                    let tried = self.attempts.entry(session.clone()).or_default();
                    *tried += 1;
                    if *tried < READ_ATTEMPTS {
                        again.push(session);
                    } else if budget.last {
                        poll.more = true;
                        return Err(AmbientFeedState::Ended(feed(
                            AmbientFeedUnavailable::Protocol,
                        )));
                    } else {
                        self.attempts.remove(&session);
                        poll.events.push(AmbientFeedEvent::Unrecognised);
                    }
                }
            }
        }
        poll.more |= budget.last && !again.is_empty();
        for session in again {
            if !self.idle.contains(&session) {
                self.idle.push(session);
            }
        }
        Ok(())
    }

    /// What `session` going idle told.
    fn turn_end_of(
        &mut self,
        program: u32,
        session: &str,
        budget: &AmbientFeedBudget,
        poll: &mut AmbientFeedPoll,
    ) -> Result<Told, Failed> {
        let room = budget.events - poll.events.len();
        if self.subagent(program, session, budget)? {
            return Ok(Told::Nothing);
        }
        let path = format!("/session/{session}/message");
        match self.get(program, &path, MESSAGES_BODY, budget)? {
            Got::TooLarge => Ok(Told::Oversized),
            Got::Answer(200, body) => {
                let since = budget
                    .since
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let cursor = self.sessions.get(session).and_then(|s| s.cursor.as_ref());
                let Ok(mut page) = turn_page(&body, cursor, since, room, budget.reply_bytes) else {
                    return Ok(Told::Unrecognised);
                };
                if let Some((last_id, turn)) = page.trailing.take() {
                    // An idle event names a session, not a prompt. Require the same last
                    // message on both sides of a current idle observation.
                    match self.confirm_idle_tail(program, session, &path, &last_id, budget) {
                        Ok(()) => page.push(turn),
                        // Publish the completed prefix and retry its suffix on the next page.
                        Err(Failed::Again) if !page.events.is_empty() => page.more = true,
                        Err(failed) => {
                            poll.events.extend(page.events);
                            return Err(failed);
                        }
                    }
                }
                Ok(Told::Turns(page))
            }
            Got::Answer(..) => Err(Failed::Again),
        }
    }

    /// OpenCode publishes idle before removing the busy entry. Retry from the
    /// message list until status and the latest message ID confirm the candidate.
    fn confirm_idle_tail(
        &mut self,
        program: u32,
        session: &str,
        messages: &str,
        last_id: &str,
        budget: &AmbientFeedBudget,
    ) -> Result<(), Failed> {
        let Got::Answer(200, body) = self.get(program, "/session/status", LIST_BODY, budget)?
        else {
            return Err(Failed::Again);
        };
        let mut decoder = serde_json::Deserializer::from_slice(&body);
        let idle = (&mut decoder)
            .deserialize_map(SessionIdle(session))
            .map_err(|_| Failed::Again)?;
        decoder.end().map_err(|_| Failed::Again)?;
        if !idle {
            return Err(Failed::Again);
        }
        let path = format!("{messages}?limit=1");
        let Got::Answer(200, body) = self.get(program, &path, MESSAGES_BODY, budget)? else {
            return Err(Failed::Again);
        };
        #[derive(Deserialize)]
        struct Row {
            info: Id,
        }
        #[derive(Deserialize)]
        struct Id {
            id: String,
        }
        let [row]: [Row; 1] = serde_json::from_slice(&body).map_err(|_| Failed::Again)?;
        if row.info.id == last_id {
            Ok(())
        } else {
            Err(Failed::Again)
        }
    }

    /// Whether `session` is a subagent's, looked up when no list or event named it.
    fn subagent(
        &mut self,
        program: u32,
        session: &str,
        budget: &AmbientFeedBudget,
    ) -> Result<bool, Failed> {
        if let Some(subagent) = self.sessions.get(session) {
            return Ok(subagent.subagent);
        }
        let path = format!("/session/{session}");
        match self.get(program, &path, SESSION_BODY, budget)? {
            Got::Answer(200, body) => match serde_json::from_slice::<SessionInfo<'_>>(&body) {
                Ok(info) if info.id.as_deref() == Some(session) => {
                    let subagent = info.subagent();
                    self.learn(session, subagent);
                    Ok(subagent)
                }
                _ => Err(Failed::Again),
            },
            Got::Answer(..) => Err(Failed::Again),
            Got::TooLarge => Err(Failed::Again),
        }
    }
}

/// What an idle session's messages told.
enum Told {
    Turns(TurnPage),
    /// A subagent session.
    Nothing,
    Unrecognised,
    /// Its message list exceeded [`MESSAGES_BODY`].
    Oversized,
}

fn exchange_until(deadline: Instant) -> Instant {
    deadline.min(Instant::now() + EXCHANGE)
}

/// The wait before attempt `failures + 1`.
fn backoff(failures: u32) -> Duration {
    let doubled = RETRY_FIRST.saturating_mul(1 << failures.saturating_sub(1).min(16));
    doubled.min(RETRY_MAX)
}

/// An id OpenCode gives (`ses_…`): letters, digits, `_`, `-` — it goes into a URL path.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct StatusEvent<'a> {
    #[serde(borrow, default)]
    properties: Option<StatusProperties<'a>>,
}

#[derive(Default, Deserialize)]
struct StatusProperties<'a> {
    #[serde(rename = "sessionID", borrow, default)]
    session: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    status: Option<Status<'a>>,
}

#[derive(Deserialize)]
struct Status<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
}

struct SessionIdle<'a>(&'a str);

impl<'de> Visitor<'de> for SessionIdle<'_> {
    type Value = bool;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a map of session statuses")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
        let mut idle = true;
        while let Some((id, status)) = map.next_entry::<&str, Status<'de>>()? {
            if id == self.0 {
                idle = status.kind.as_deref() == Some("idle");
            }
        }
        Ok(idle)
    }
}

#[derive(Deserialize)]
struct InfoEvent<'a> {
    #[serde(borrow, default)]
    properties: Option<InfoProperties<'a>>,
}

#[derive(Deserialize)]
struct InfoProperties<'a> {
    #[serde(borrow, default)]
    info: Option<SessionInfo<'a>>,
}

#[derive(Deserialize)]
struct SessionInfo<'a> {
    #[serde(borrow, default)]
    id: Option<Cow<'a, str>>,
    #[serde(rename = "parentID", borrow, default)]
    parent: Option<Word<'a>>,
}

impl SessionInfo<'_> {
    /// A subagent's session: it names a parent.
    fn subagent(&self) -> bool {
        self.parent
            .as_ref()
            .is_some_and(|parent| parent.0.is_some())
    }
}

fn learn_session(sessions: &mut HashMap<String, Session>, id: &str, subagent: bool) {
    if let Some(known) = sessions.get_mut(id) {
        known.subagent = subagent;
    } else {
        if sessions.len() >= SESSIONS_KEPT {
            sessions.clear();
        }
        sessions.insert(
            id.to_owned(),
            Session {
                subagent,
                cursor: None,
            },
        );
    }
}

struct Sessions<'a>(&'a mut HashMap<String, Session>);

impl<'de> Visitor<'de> for Sessions<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a list of sessions")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(session) = seq.next_element::<SessionInfo<'de>>()? {
            if let Some(id) = session.id.as_deref().filter(|id| valid_id(id)) {
                learn_session(self.0, id, session.subagent());
            }
        }
        Ok(())
    }
}

/// A string, or `None` for any other JSON value (skipped without being kept).
struct Word<'a>(Option<Cow<'a, str>>);

impl<'de: 'a, 'a> Deserialize<'de> for Word<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Word(d.deserialize_any(Lenient::<WordKind>(PhantomData))?))
    }
}

/// `true` for the JSON boolean `true`, `false` for any other value (an OpenCode user
/// message's `summary` is an object; an assistant's is a boolean).
struct IsTrue(bool);

impl<'de> Deserialize<'de> for IsTrue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(IsTrue(d.deserialize_any(Lenient::<TrueKind>(PhantomData))?))
    }
}

struct WordKind;
struct TrueKind;

/// What a lenient visitor keeps of a string or a boolean; anything else is skipped.
trait Keep<'de> {
    type Value;
    fn other() -> Self::Value;
    fn string(_: &str) -> Self::Value {
        Self::other()
    }
    fn borrowed_string(s: &'de str) -> Self::Value {
        Self::string(s)
    }
    fn boolean(_: bool) -> Self::Value {
        Self::other()
    }
}

impl<'de> Keep<'de> for WordKind {
    type Value = Option<Cow<'de, str>>;
    fn other() -> Self::Value {
        None
    }
    fn string(s: &str) -> Self::Value {
        Some(Cow::Owned(s.to_string()))
    }
    fn borrowed_string(s: &'de str) -> Self::Value {
        Some(Cow::Borrowed(s))
    }
}

impl<'de> Keep<'de> for TrueKind {
    type Value = bool;
    fn other() -> Self::Value {
        false
    }
    fn boolean(b: bool) -> Self::Value {
        b
    }
}

struct Lenient<K>(PhantomData<K>);

impl<'de, K: Keep<'de>> Visitor<'de> for Lenient<K> {
    type Value = K::Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(K::boolean(v))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(K::string(v))
    }
    fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
        Ok(K::borrowed_string(v))
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(K::other())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(K::other())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(K::other())
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(K::other())
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(K::other())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(K::other())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(K::other())
    }
}

struct Message<'a> {
    info: MessageInfo<'a>,
    parts: Option<Parts>,
}

/// Pass the attachment's reply limit into each message, regardless of field order.
struct ReadMessage(usize);

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "lowercase")]
enum MessageField {
    Info,
    Parts,
    #[serde(other)]
    Other,
}

impl<'de> DeserializeSeed<'de> for ReadMessage {
    type Value = Message<'de>;

    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<Self::Value, D::Error> {
        decoder.deserialize_struct("Message", &["info", "parts"], self)
    }
}

impl<'de> Visitor<'de> for ReadMessage {
    type Value = Message<'de>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a message with info and optional parts")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut info = None;
        let mut parts = None;
        while let Some(field) = map.next_key()? {
            match field {
                MessageField::Info if info.is_none() => info = Some(map.next_value()?),
                MessageField::Parts if parts.is_none() => {
                    parts = Some(map.next_value_seed(ReadParts(self.0))?);
                }
                MessageField::Info => return Err(de::Error::duplicate_field("info")),
                MessageField::Parts => return Err(de::Error::duplicate_field("parts")),
                MessageField::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(Message {
            info: info.ok_or_else(|| de::Error::missing_field("info"))?,
            parts: parts.flatten(),
        })
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let info = seq
            .next_element()?
            .ok_or_else(|| de::Error::invalid_length(0, &self))?;
        let parts = seq.next_element_seed(ReadParts(self.0))?.flatten();
        Ok(Message { info, parts })
    }
}

#[derive(Deserialize)]
struct MessageInfo<'a> {
    #[serde(rename = "parentID", borrow, default)]
    parent: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    role: Option<Word<'a>>,
    #[serde(default)]
    time: Option<Times>,
    #[serde(borrow, default)]
    finish: Option<Word<'a>>,
    #[serde(default)]
    summary: Option<IsTrue>,
}

#[derive(Deserialize)]
struct Times {
    #[serde(default)]
    created: Option<u64>,
    #[serde(default)]
    completed: Option<u64>,
}

#[derive(Deserialize)]
struct Part<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Word<'a>>,
    #[serde(borrow, default)]
    text: Option<Cow<'a, str>>,
    #[serde(default)]
    synthetic: Option<IsTrue>,
    #[serde(default)]
    auto: Option<IsTrue>,
    #[serde(default)]
    metadata: Option<PartMetadata>,
    #[serde(borrow, default)]
    state: Option<ToolState<'a>>,
    #[serde(default)]
    ignored: Option<IsTrue>,
}

#[derive(Deserialize)]
struct PartMetadata {
    compaction_continue: Option<IsTrue>,
    #[serde(rename = "providerExecuted")]
    provider_executed: Option<IsTrue>,
    interrupted: Option<IsTrue>,
}

#[derive(Deserialize)]
struct ToolState<'a> {
    #[serde(borrow, default)]
    status: Option<Word<'a>>,
    #[serde(default)]
    metadata: Option<PartMetadata>,
}

impl Part<'_> {
    fn needs_tool_followup(&self) -> bool {
        self.is_kind("tool")
            && !self
                .metadata
                .as_ref()
                .and_then(|m| m.provider_executed.as_ref())
                .is_some_and(|v| v.0)
            && !self.state.as_ref().is_some_and(|state| {
                state.status.as_ref().and_then(|s| s.0.as_deref()) == Some("error")
                    && state
                        .metadata
                        .as_ref()
                        .and_then(|m| m.interrupted.as_ref())
                        .is_some_and(|v| v.0)
            })
    }

    fn is_kind(&self, kind: &str) -> bool {
        self.kind.as_ref().and_then(|k| k.0.as_deref()) == Some(kind)
    }

    fn is_compaction_continuation(&self) -> bool {
        self.is_kind("text")
            && self.synthetic.as_ref().is_some_and(|s| s.0)
            && self
                .metadata
                .as_ref()
                .and_then(|m| m.compaction_continue.as_ref())
                .is_some_and(|c| c.0)
    }

    fn reply_text(&self) -> Option<&str> {
        self.text.as_deref().filter(|_| {
            self.is_kind("text")
                && !self.synthetic.as_ref().is_some_and(|s| s.0)
                && !self.ignored.as_ref().is_some_and(|i| i.0)
        })
    }
}

enum UserRow {
    Prompt,
    AutoCompaction,
    Internal,
}

struct Parts {
    needs_tool_followup: bool,
    user_row: UserRow,
    reply: Option<TurnReply>,
}

struct ReadParts(usize);

impl<'de> DeserializeSeed<'de> for ReadParts {
    type Value = Option<Parts>;

    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<Self::Value, D::Error> {
        decoder.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for ReadParts {
    type Value = Option<Parts>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a list of message parts or null")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut reply = TurnReply::default();
        let mut user_row = UserRow::Internal;
        let mut compaction = None;
        let mut needs_tool_followup = false;
        while let Some(part) = seq.next_element::<Part<'de>>()? {
            needs_tool_followup |= part.needs_tool_followup();
            if compaction.is_none() && part.is_kind("compaction") {
                compaction = Some(if part.auto.as_ref().is_some_and(|a| a.0) {
                    UserRow::AutoCompaction
                } else {
                    UserRow::Internal
                });
            }
            // An empty row is still being written. A synthetic continuation counts
            // only if it is the sole part, unless a compaction marker takes precedence.
            user_row = match user_row {
                UserRow::Internal if part.is_compaction_continuation() => UserRow::AutoCompaction,
                _ => UserRow::Prompt,
            };
            if let Some(text) = part.reply_text().filter(|_| !reply.truncated) {
                super::append(&mut reply, text, self.0);
            }
        }
        Ok(Some(Parts {
            needs_tool_followup,
            user_row: compaction.unwrap_or(user_row),
            reply: (!reply.text.is_empty() || reply.truncated).then_some(reply),
        }))
    }
}

/// At most one poll's remaining capacity. The cursor advances only through returned
/// events, so a later fetch can continue even if several idle notifications coalesced.
#[derive(Default)]
struct TurnPage {
    events: Vec<AmbientFeedEvent>,
    cursor: Option<MessageCursor>,
    more: bool,
    /// A trailing completion and the last row (possibly a compaction summary) in
    /// its snapshot. Move the completion to the page only after confirming idle.
    trailing: Option<(String, Completion)>,
}

struct Completion {
    cursor: MessageCursor,
    event: AmbientFeedEvent,
}

impl TurnPage {
    fn push(&mut self, turn: Completion) {
        self.cursor = Some(turn.cursor);
        self.events.push(turn.event);
    }
}

struct Messages<'a> {
    cursor: Option<&'a MessageCursor>,
    since: u128,
    room: usize,
    reply_bytes: usize,
}

impl<'de> Visitor<'de> for Messages<'_> {
    type Value = TurnPage;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a list of messages")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut page = TurnPage::default();
        let mut previous = self.cursor;
        let mut last: Option<Message<'_>> = None;
        let mut prompt = None;
        let mut last_id = None;
        while let Some(message) = seq.next_element_seed(ReadMessage(self.reply_bytes))? {
            last_id = message
                .info
                .id
                .as_ref()
                .filter(|id| !id.is_empty() && id.len() <= 256)
                .cloned();
            match message
                .info
                .role
                .as_ref()
                .and_then(|role| role.0.as_deref())
            {
                Some("user") => {
                    match message
                        .parts
                        .map_or(UserRow::Internal, |parts| parts.user_row)
                    {
                        // A replay of the original prompt can follow without a
                        // synthetic marker. Its previous tool step is superseded.
                        UserRow::AutoCompaction => {
                            prompt = last_id.clone();
                            last = None;
                            continue;
                        }
                        UserRow::Internal => continue,
                        UserRow::Prompt => {}
                    }
                    // A prompt may be saved while the previous model step is still
                    // running. It cannot by itself prove that step ended a turn.
                    prompt = last_id.clone();
                }
                Some("assistant") if !message.info.summary.as_ref().is_some_and(|s| s.0) => {
                    if let Some(prior) = last.take() {
                        if prior.info.parent.is_some()
                            && message.info.parent.is_some()
                            && prior.info.parent != message.info.parent
                        {
                            if let Some(turn) = self
                                .completion(prior, &mut previous, &mut page)
                                .map_err(de::Error::custom)?
                            {
                                page.push(turn);
                            }
                        }
                    }
                    last = Some(message);
                }
                Some(_) => {}
                None => return Err(de::Error::custom("a message without a role")),
            }
        }
        if let Some(last) = last.filter(|last| {
            // Idle belongs to the session. An assistant for an older prompt is not
            // the final answer to a newer prompt already saved by that session.
            prompt.is_none() || last.info.parent == prompt
        }) {
            let turn = self
                .completion(last, &mut previous, &mut page)
                .map_err(de::Error::custom)?;
            page.trailing = last_id.zip(turn).map(|(id, turn)| (id.into_owned(), turn));
        }
        Ok(page)
    }
}

impl Messages<'_> {
    /// Only the last non-summary assistant message of each prompt can end its turn.
    fn completion(
        &self,
        message: Message<'_>,
        previous: &mut Option<&MessageCursor>,
        page: &mut TurnPage,
    ) -> Result<Option<Completion>, &'static str> {
        let info = &message.info;
        if !info
            .parent
            .as_deref()
            .is_some_and(|id| !id.is_empty() && id.len() <= 256)
        {
            return Err("an assistant message without its parent id");
        }
        let times = info.time.as_ref();
        let (Some(id), Some(created)) = (
            info.id
                .as_deref()
                .filter(|id| !id.is_empty() && id.len() <= 256),
            times.and_then(|t| t.created),
        ) else {
            return Err("an assistant message without its id or time");
        };
        let finish = info
            .finish
            .as_ref()
            .and_then(|finish| finish.0.as_deref())
            .filter(|s| !s.trim().is_empty());
        if matches!(finish, Some("tool-calls" | "unknown"))
            || message
                .parts
                .as_ref()
                .is_some_and(|parts| parts.needs_tool_followup)
        {
            return Ok(None);
        }
        let ended = match times.and_then(|t| t.completed) {
            Some(at) => at,
            None if finish.is_some() => created,
            // Another turn can start between the idle event and this fetch.
            None => return Ok(None),
        };
        if let Some(cursor) = previous {
            if id == cursor.id {
                // The exact anchor wins over timestamps, including ties. Discard any
                // provisional suffix from before it if the server clock ran backwards.
                *previous = None;
                *page = TurnPage::default();
                return Ok(None);
            }
            // If the server pruned the anchor, only strictly later completions are new.
            if ended <= cursor.completed {
                return Ok(None);
            }
        }
        if u128::from(ended) < self.since {
            return Ok(None);
        }
        if page.events.len() == self.room {
            page.more = true;
            return Ok(None);
        }
        let reply = if finish == Some("stop") {
            message
                .parts
                .ok_or("a stopped assistant message without parts")?
                .reply
        } else {
            None
        };
        let cursor = MessageCursor {
            id: id.to_string(),
            completed: ended,
        };
        let event = AmbientFeedEvent::Ended {
            key: id.to_string(),
            ended: UNIX_EPOCH
                .checked_add(Duration::from_millis(ended))
                .ok_or("invalid completion time")?,
            reply,
        };
        Ok(Some(Completion { cursor, event }))
    }
}

fn turn_page(
    body: &[u8],
    cursor: Option<&MessageCursor>,
    since: u128,
    room: usize,
    reply_bytes: usize,
) -> serde_json::Result<TurnPage> {
    let mut decoder = serde_json::Deserializer::from_slice(body);
    let page = (&mut decoder).deserialize_seq(Messages {
        cursor,
        since,
        room,
        reply_bytes,
    })?;
    decoder.end()?;
    Ok(page)
}

#[cfg(test)]
mod tests;
