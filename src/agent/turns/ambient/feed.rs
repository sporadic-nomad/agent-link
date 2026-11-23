//! Feed sources: an agent whose turns the worker does not read from a file but polls — the
//! agent's own local server (OpenCode's TUI). The same receiver, status, evidence, limits,
//! worker budget, stop semantics and lifecycle apply; a feed is the second way a source
//! can tell turn ends.
//!
//! [`Driver`] keeps the worker's rules for one generation (agent launch): a turn that
//! ended before the generation started is not reported (from now), a key at most once,
//! a reply cut to `reply_bytes`, an unrecognised record standing until a turn ends
//! cleanly, and a reader that reports [`AmbientFeedState::Ended`] is dropped and not
//! polled again until the next launch.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::hash::{BuildHasher, RandomState};
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime};

use super::batch::{Capacity, TurnBatch};
use super::watch::bounded;
use super::{AmbientContext, AmbientEvidence, AmbientTurn, AmbientUnavailable};
use crate::agent::turns::{TurnOptions, TurnReply};

/// How long one [`AmbientFeedReader::poll`] may take ([`AmbientFeedBudget::deadline`]).
pub(super) const POLL_BUDGET: Duration = Duration::from_secs(2);

/// Turn keys remembered (the newest, as digests); a turn told again later than that would
/// be reported again.
const KEYS_KEPT: usize = 4096;
/// The longest key taken; a longer one is an unrecognised record.
const MAX_KEY: usize = 256;

/// Where a launched agent serves its ambient feed: a loopback socket address and the
/// Basic-auth credentials its launch configured
/// ([`AgentProfile::ambient_feed_launch`](crate::agent::AgentProfile::ambient_feed_launch)).
/// Loopback only by construction: [`new`](Self::new) refuses any other address, and no
/// host name is ever resolved. `Debug` never shows the password.
#[derive(Clone, PartialEq, Eq)]
pub struct AmbientEndpoint {
    addr: SocketAddr,
    username: String,
    password: String,
}

impl AmbientEndpoint {
    /// The server at `addr`, reached as `username` with `password`. `None` unless `addr`
    /// is a loopback address (`127.0.0.0/8`, `::1`) with a nonzero port, `username` is 1
    /// to 128 bytes without `:` and `password` 1 to 1,024 bytes, neither with a control
    /// character.
    pub fn new(
        addr: SocketAddr,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Option<Self> {
        let (username, password) = (username.into(), password.into());
        let plain = |s: &str, max: usize| {
            !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
        };
        (addr.ip().is_loopback()
            && addr.port() != 0
            && plain(&username, 128)
            && !username.contains(':')
            && plain(&password, 1024))
        .then_some(Self {
            addr,
            username,
            password,
        })
    }

    /// The server's socket address (loopback).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The Basic-auth user.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// The Basic-auth password. Never log or print it.
    pub fn password(&self) -> &str {
        &self.password
    }
}

impl fmt::Debug for AmbientEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AmbientEndpoint")
            .field("addr", &self.addr)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// A turn source the ambient worker polls instead of a transcript it tails: an agent CLI's
/// own local server. Returned by
/// [`AgentProfile::ambient_feed`](crate::agent::AgentProfile::ambient_feed) or passed to
/// [`AgentTerminal::attach_ambient_feed_with`](crate::agent::AgentTerminal::attach_ambient_feed_with).
/// Shared by attachments; called on their workers only. The same best-effort terms as for
/// an [`AmbientSource`](super::AmbientSource) apply: a reply is what the source said ended
/// a turn, with no attribution guarantee beyond what the source itself checks.
pub trait AmbientFeed: Send + Sync + 'static {
    /// A reader for one agent launch (generation), opened at the first wake an agent of
    /// the terminal runs and dropped when that launch ends (relaunch, departure, exit,
    /// stop). `endpoint` is where the launch was configured to serve, when the terminal was
    /// spawned with
    /// [`spawn_with_ambient_feed`](crate::agent::AgentTerminal::spawn_with_ambient_feed);
    /// `None` otherwise. Keep it cheap: connect in [`AmbientFeedReader::poll`].
    fn open(
        &self,
        ctx: &AmbientContext,
        endpoint: Option<&AmbientEndpoint>,
    ) -> Box<dyn AmbientFeedReader>;
}

/// One launch's connection to an [`AmbientFeed`].
pub trait AmbientFeedReader: Send {
    /// What the source told since the last poll, with `ctx` as the terminal stands now.
    /// Called on every wake (about every [`AmbientOptions::poll`](super::AmbientOptions::poll))
    /// while the launch's agent runs, and with [`AmbientFeedBudget::last`] when
    /// the launch ends. Final polls continue while [`AmbientFeedPoll::more`] is set and share
    /// one deadline. Return by [`AmbientFeedBudget::deadline`]: read what arrived, never
    /// wait for more, and bound every exchange by that deadline. A late return delays only
    /// this attachment's worker; [`stop`](super::AmbientReceiver::stop) never waits for it.
    fn poll(&mut self, ctx: &AmbientContext, budget: &AmbientFeedBudget) -> AmbientFeedPoll;
}

/// What one [`AmbientFeedReader::poll`] may spend.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct AmbientFeedBudget {
    /// Earliest completion the driver will accept for this attachment and launch.
    pub(in crate::agent) since: SystemTime,
    /// Return by then. Final polls share the two seconds available for departure.
    pub deadline: Instant,
    /// The attachment's [`TurnOptions::line_bytes`]: the longest event or record kept; a
    /// longer one is skipped and counted in [`AmbientFeedPoll::oversized`].
    pub line_bytes: usize,
    /// The attachment's [`TurnOptions::reply_bytes`]: keep at most this much reply text
    /// (the worker cuts longer text and flags it truncated).
    pub reply_bytes: usize,
    /// Maximum events to return. The worker reduces [`TurnOptions::records`] to fit
    /// the remaining item and byte capacity at maximum reply size, with a minimum
    /// of one candidate. Set [`AmbientFeedPoll::more`] when more are waiting.
    pub events: usize,
    /// The launch ended (relaunch, departure, exit or stop): tell what already arrived;
    /// connect nothing new. Its state is not published.
    pub last: bool,
}

impl AmbientFeedBudget {
    /// A budget ending at `deadline` with `limits` (for testing a feed).
    pub fn new(deadline: Instant, limits: &TurnOptions) -> Self {
        Self {
            since: SystemTime::UNIX_EPOCH,
            deadline,
            line_bytes: limits.line_bytes,
            reply_bytes: limits.reply_bytes,
            events: limits.records,
            last: false,
        }
    }
}

/// What one [`AmbientFeedReader::poll`] found.
#[derive(Debug)]
#[non_exhaustive]
pub struct AmbientFeedPoll {
    /// Where the source stands now.
    pub state: AmbientFeedState,
    /// What it told since the last poll, in order.
    pub events: Vec<AmbientFeedEvent>,
    /// Events or records skipped for exceeding their bound since the last poll (added to
    /// [`AmbientStatus::Active`](super::AmbientStatus::Active)'s `oversized_lines`); a turn
    /// end among them was missed.
    pub oversized: u32,
    /// The source had more to tell than this poll read: the worker polls again without
    /// waiting for its next wake. On a final poll, returning `more` with
    /// [`AmbientFeedState::Ended`] delivers the completed events and closes observation
    /// with [`AmbientClosed::IncompleteDrain`](super::AmbientClosed::IncompleteDrain).
    pub more: bool,
}

impl AmbientFeedPoll {
    /// `state`, no events, nothing skipped, nothing more.
    pub fn new(state: AmbientFeedState) -> Self {
        Self {
            state,
            events: Vec::new(),
            oversized: 0,
            more: false,
        }
    }
}

/// Where a feed's source stands, as [`AmbientFeedPoll::state`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmbientFeedState {
    /// It is read: [`AmbientStatus::Active`](super::AmbientStatus::Active).
    Ready,
    /// It cannot be read now: [`AmbientWaiting::Unavailable`](super::AmbientWaiting::Unavailable)
    /// with the reason; polled again on the next wake.
    Unavailable(AmbientUnavailable),
    /// It must not be read again during this launch (a server that is not the agent's, or
    /// that refused or did not need the launch's credentials): the reader is dropped,
    /// status stays [`AmbientWaiting::Unavailable`](super::AmbientWaiting::Unavailable)
    /// with the reason, and the next launch opens a new reader.
    Ended(AmbientUnavailable),
}

/// What a feed told, in [`AmbientFeedPoll::events`].
#[derive(Debug)]
#[non_exhaustive]
pub enum AmbientFeedEvent {
    /// A turn ended, as the source states explicitly (reported with
    /// [`AmbientEvidence::ExplicitEnd`]). Reported at most once per `key` (the newest 4,096
    /// keys are remembered), and not at all when it `ended` before the launch's observation
    /// started.
    Ended {
        /// Tells the turn apart: the same turn told again (a reconnect, a repeated
        /// notification) carries the same key (OpenCode: its last message's id). 1 to 256
        /// bytes; another key counts as [`Unrecognised`](Self::Unrecognised).
        key: String,
        /// When it ended, by this machine's clock.
        ended: SystemTime,
        /// Its final reply (`None`: aborted, failed, tool calls only). Text over
        /// [`AmbientFeedBudget::reply_bytes`] is cut and flagged truncated; blank text is
        /// `None`.
        reply: Option<TurnReply>,
    },
    /// Something the source told of its turns could not be used (an event of a known kind
    /// without its fields, an answer of another shape):
    /// [`AmbientStatus::Active`](super::AmbientStatus::Active)`{ unrecognised: true, .. }`
    /// until a turn ends cleanly.
    Unrecognised,
}

/// Why a feed cannot be read, in [`AmbientUnavailable::Feed`]. Nothing carries the
/// endpoint's password.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmbientFeedUnavailable {
    /// The launch was not configured to serve a feed: the terminal was not spawned with
    /// [`spawn_with_ambient_feed`](crate::agent::AgentTerminal::spawn_with_ambient_feed).
    NotServed,
    /// Nothing accepts connections at the endpoint (yet): the agent's server may still be
    /// starting. Retried on every wake.
    NotListening,
    /// The connection's owner is still unverified. The server may still be accepting,
    /// the owner may be inaccessible, or a Windows parent link may be unverifiable.
    /// The feed retries with backoff and withholds credentials.
    OwnerUnverified,
    /// A process that is not the launched agent serves the endpoint (another program holds
    /// the port). No credentials were sent. Final for this launch.
    OwnerMismatch,
    /// The agent's server answered a request without credentials: it does not enforce the
    /// launch's password. Final for this launch.
    Unprotected,
    /// The agent's server refused the launch's credentials. Final for this launch.
    CredentialsRefused,
    /// The server answered with this unexpected HTTP status; retried with backoff.
    Status(u16),
    /// The server's answer could not be read in time or within its size bound, or has a
    /// shape the source does not know; retried with backoff.
    Protocol,
}

/// The turn keys reported, as digests under per-driver random keys (a server cannot pick
/// colliding keys), the newest [`KEYS_KEPT`].
struct Keys {
    hasher: RandomState,
    set: HashSet<u64>,
    order: VecDeque<u64>,
}

impl Keys {
    fn new() -> Self {
        Self {
            hasher: RandomState::new(),
            set: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// Records `key`; `false` when it was reported already.
    fn remember(&mut self, key: &str) -> bool {
        let digest = self.hasher.hash_one(key);
        if !self.set.insert(digest) {
            return false;
        }
        self.order.push_back(digest);
        if self.order.len() > KEYS_KEPT {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        true
    }
}

/// Where a generation's feed stands after a poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FeedStatus {
    Ready { unrecognised: bool },
    Unavailable(AmbientUnavailable),
    IncompleteDrain,
}

/// What one poll gives the worker to publish.
pub(super) struct Polled {
    pub(super) status: FeedStatus,
    pub(super) batch: TurnBatch,
    pub(super) oversized: u32,
    pub(super) more: bool,
}

/// One generation's reader and the worker's rules over what it tells.
pub(super) struct Driver {
    capacity: Capacity,
    /// `None` once the reader ended for this launch.
    reader: Option<Box<dyn AmbientFeedReader>>,
    /// A turn that ended before this is not reported.
    since: SystemTime,
    keys: Keys,
    /// An unrecognised record since the last clean turn end.
    unrecognised: bool,
    /// Why the reader ended.
    ended: Option<AmbientUnavailable>,
}

impl Driver {
    pub(super) fn new(
        reader: Box<dyn AmbientFeedReader>,
        since: SystemTime,
        capacity: Capacity,
    ) -> Self {
        Self {
            reader: Some(reader),
            capacity,
            since,
            keys: Keys::new(),
            unrecognised: false,
            ended: None,
        }
    }

    pub(super) fn poll(&mut self, ctx: &AmbientContext, budget: &AmbientFeedBudget) -> Polled {
        let Some(reader) = self.reader.as_mut() else {
            return Polled {
                status: FeedStatus::Unavailable(
                    self.ended
                        .unwrap_or(AmbientUnavailable::Feed(AmbientFeedUnavailable::NotServed)),
                ),
                batch: TurnBatch::default(),
                oversized: 0,
                more: false,
            };
        };
        let mut budget = *budget;
        budget.since = self.since;
        budget.events = budget.events.min(self.capacity.feed_events());
        let poll = reader.poll(ctx, &budget);
        let mut batch = TurnBatch::default();
        for event in poll.events {
            match event {
                AmbientFeedEvent::Ended { key, ended, reply } => {
                    if key.is_empty() || key.len() > MAX_KEY {
                        self.unrecognised = true;
                        continue;
                    }
                    // From now, and once.
                    if ended < self.since || !self.keys.remember(&key) {
                        continue;
                    }
                    self.unrecognised = false;
                    batch.push(
                        AmbientTurn {
                            reply: reply.and_then(|reply| bounded(reply, budget.reply_bytes)),
                            evidence: AmbientEvidence::ExplicitEnd,
                        },
                        &self.capacity,
                    );
                    if batch.overflow {
                        break;
                    }
                }
                AmbientFeedEvent::Unrecognised => self.unrecognised = true,
            }
        }
        let status = match poll.state {
            AmbientFeedState::Ready => FeedStatus::Ready {
                unrecognised: self.unrecognised,
            },
            AmbientFeedState::Unavailable(reason) => FeedStatus::Unavailable(reason),
            AmbientFeedState::Ended(reason) => {
                self.reader = None;
                self.ended = Some(reason);
                if budget.last && poll.more {
                    FeedStatus::IncompleteDrain
                } else {
                    FeedStatus::Unavailable(reason)
                }
            }
        };
        Polled {
            status,
            batch,
            oversized: poll.oversized,
            more: poll.more && self.reader.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};

    use super::*;
    use crate::env::EnvPolicy;

    /// A reader that returns the polls a test queued (then `Ready` with nothing), counting
    /// its polls.
    #[derive(Clone, Default)]
    struct Scripted {
        polls: Arc<Mutex<VecDeque<AmbientFeedPoll>>>,
        calls: Arc<Mutex<usize>>,
    }

    impl Scripted {
        fn push(&self, state: AmbientFeedState, events: Vec<AmbientFeedEvent>) {
            let mut poll = AmbientFeedPoll::new(state);
            poll.events = events;
            self.polls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_back(poll);
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl AmbientFeedReader for Scripted {
        fn poll(&mut self, _: &AmbientContext, _: &AmbientFeedBudget) -> AmbientFeedPoll {
            *self.calls.lock().unwrap() += 1;
            self.polls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
                .unwrap_or_else(|| AmbientFeedPoll::new(AmbientFeedState::Ready))
        }
    }

    fn ctx() -> AmbientContext {
        AmbientContext::new("/x", EnvPolicy::default())
    }

    fn budget(reply_bytes: usize) -> AmbientFeedBudget {
        let limits = TurnOptions {
            reply_bytes,
            ..TurnOptions::default()
        };
        AmbientFeedBudget::new(Instant::now() + POLL_BUDGET, &limits)
    }

    fn driver(since: SystemTime) -> (Driver, Scripted) {
        let script = Scripted::default();
        (
            Driver::new(
                Box::new(script.clone()),
                since,
                Capacity::new(TurnOptions::default()),
            ),
            script,
        )
    }

    fn reply(text: &str) -> Option<TurnReply> {
        Some(TurnReply {
            text: text.into(),
            ..TurnReply::default()
        })
    }

    fn ended(key: &str, at: SystemTime, text: Option<&str>) -> AmbientFeedEvent {
        AmbientFeedEvent::Ended {
            key: key.into(),
            ended: at,
            reply: text.and_then(reply),
        }
    }

    fn texts(polled: &Polled) -> Vec<Option<&str>> {
        polled
            .batch
            .turns
            .iter()
            .map(|staged| staged.turn.reply.as_ref().map(|r| r.text.as_str()))
            .collect()
    }

    #[test]
    fn a_turn_that_ended_before_the_generation_started_is_not_reported() {
        let since = SystemTime::now();
        let (mut d, script) = driver(since);
        script.push(
            AmbientFeedState::Ready,
            vec![
                ended("a", since - Duration::from_secs(1), Some("old")),
                ended("b", since + Duration::from_millis(1), Some("new")),
            ],
        );
        let polled = d.poll(&ctx(), &budget(1024));
        assert_eq!(texts(&polled), [Some("new")]);
        assert_eq!(
            polled.batch.turns[0].turn.evidence,
            AmbientEvidence::ExplicitEnd
        );
        assert_eq!(
            polled.status,
            FeedStatus::Ready {
                unrecognised: false
            }
        );
    }

    #[test]
    fn a_turn_is_reported_once_and_a_blank_reply_is_none() {
        let since = SystemTime::now();
        let (mut d, script) = driver(since);
        let at = since + Duration::from_secs(1);
        script.push(
            AmbientFeedState::Ready,
            vec![
                ended("a", at, Some("one")),
                ended("a", at, Some("one")),
                ended("b", at, Some(" \n")),
                ended("c", at, None),
            ],
        );
        script.push(AmbientFeedState::Ready, vec![ended("a", at, Some("one"))]);
        assert_eq!(
            texts(&d.poll(&ctx(), &budget(1024))),
            [Some("one"), None, None]
        );
        assert!(
            d.poll(&ctx(), &budget(1024)).batch.turns.is_empty(),
            "told again"
        );
    }

    #[test]
    fn feed_admission_stops_at_the_first_turn_beyond_either_budget() {
        for (items, bytes) in [(1, 16), (4, 3)] {
            let at = SystemTime::now();
            let script = Scripted::default();
            let limits = TurnOptions {
                queue_items: items,
                queue_bytes: bytes,
                reply_bytes: 3,
                ..TurnOptions::default()
            };
            let mut driver = Driver::new(Box::new(script.clone()), at, Capacity::new(limits));
            // A custom reader may ignore its poll budget. Only the prefix that fits
            // is retained, even when a later, smaller turn would fit.
            script.push(
                AmbientFeedState::Ready,
                vec![
                    ended("a", at, Some("ab")),
                    ended("b", at, Some("cd")),
                    ended("c", at, None),
                ],
            );
            let poll = driver.poll(
                &ctx(),
                &AmbientFeedBudget::new(Instant::now() + POLL_BUDGET, &limits),
            );
            assert!(poll.batch.overflow);
            assert_eq!(texts(&poll), [Some("ab")]);
        }
    }

    #[test]
    fn a_long_reply_is_cut_at_a_char_boundary_and_flagged() {
        let since = SystemTime::now();
        let (mut d, script) = driver(since);
        script.push(
            AmbientFeedState::Ready,
            vec![ended("a", since + Duration::from_secs(1), Some("aé€x"))],
        );
        let polled = d.poll(&ctx(), &budget(4));
        let reply = polled.batch.turns[0].turn.reply.as_ref().unwrap();
        assert_eq!(reply.text, "aé");
        assert!(reply.truncated);
    }

    #[test]
    fn an_unrecognised_record_stands_until_a_turn_ends_cleanly() {
        let since = SystemTime::now();
        let (mut d, script) = driver(since);
        let at = since + Duration::from_secs(1);
        let unrecognised = |d: &mut Driver| match d.poll(&ctx(), &budget(64)).status {
            FeedStatus::Ready { unrecognised } => unrecognised,
            other => panic!("{other:?}"),
        };
        script.push(
            AmbientFeedState::Ready,
            vec![AmbientFeedEvent::Unrecognised],
        );
        assert!(unrecognised(&mut d));
        assert!(unrecognised(&mut d), "stands");
        // A key over the bound is no clean end, and neither is a turn told again.
        script.push(
            AmbientFeedState::Ready,
            vec![ended(&"k".repeat(MAX_KEY + 1), at, Some("x"))],
        );
        assert!(unrecognised(&mut d));
        script.push(AmbientFeedState::Ready, vec![ended("a", at, Some("x"))]);
        assert!(!unrecognised(&mut d));
        script.push(
            AmbientFeedState::Ready,
            vec![AmbientFeedEvent::Unrecognised, ended("a", at, None)],
        );
        assert!(unrecognised(&mut d), "told again");
    }

    /// An ended reader is dropped: never polled again in this generation, and its reason
    /// stands.
    #[test]
    fn an_ended_reader_is_not_polled_again() {
        let (mut d, script) = driver(SystemTime::now());
        let mismatch = AmbientUnavailable::Feed(AmbientFeedUnavailable::OwnerMismatch);
        script.push(AmbientFeedState::Ended(mismatch), vec![]);
        for _ in 0..3 {
            let polled = d.poll(&ctx(), &budget(64));
            assert_eq!(polled.status, FeedStatus::Unavailable(mismatch));
            assert!(!polled.more);
        }
        assert_eq!(script.calls(), 1);
    }

    #[test]
    fn only_the_newest_keys_are_kept() {
        let mut keys = Keys::new();
        for n in 0..=KEYS_KEPT {
            assert!(keys.remember(&n.to_string()));
        }
        assert_eq!(keys.set.len(), KEYS_KEPT);
        assert!(!keys.remember(&KEYS_KEPT.to_string()));
        assert!(keys.remember("0"), "the oldest was let go");
    }

    #[test]
    fn an_endpoint_is_loopback_only_and_never_shows_its_password() {
        let local: SocketAddr = "127.0.0.1:4096".parse().unwrap();
        let endpoint = AmbientEndpoint::new(local, "opencode", "s3cret").unwrap();
        assert_eq!(endpoint.addr(), local);
        assert_eq!(
            (endpoint.username(), endpoint.password()),
            ("opencode", "s3cret")
        );
        let shown = format!("{endpoint:?}");
        assert!(
            !shown.contains("s3cret") && shown.contains("redacted"),
            "{shown}"
        );
        assert!(AmbientEndpoint::new("[::1]:1".parse().unwrap(), "u", "p").is_some());
        for (addr, user, password) in [
            ("10.0.0.1:4096", "u", "p"),
            ("0.0.0.0:4096", "u", "p"),
            ("127.0.0.1:0", "u", "p"),
            ("127.0.0.1:1", "", "p"),
            ("127.0.0.1:1", "a:b", "p"),
            ("127.0.0.1:1", "u", ""),
            ("127.0.0.1:1", "u", "p\n"),
        ] {
            let addr: SocketAddr = addr.parse().unwrap();
            assert!(
                AmbientEndpoint::new(addr, user, password).is_none(),
                "{addr} {user:?} {password:?}"
            );
        }
        assert!(AmbientEndpoint::new(local, "u", "p".repeat(1025)).is_none());
    }
}
