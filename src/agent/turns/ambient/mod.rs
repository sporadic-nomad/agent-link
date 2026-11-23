//! Best-effort ("ambient") turn observation: what an agent CLI's own JSON-lines
//! transcript says ended a turn, read from any [`AgentTerminal`] however it was spawned.
//!
//! **This tier provides no writer exclusion and no attribution guarantee.** It needs no
//! lease, no isolated configuration root, no pinned CLI version and no direct launch, and
//! it runs on Unix and Windows (plain `std` file reads). In exchange, another process of
//! the same user — a second CLI session, a resumed or forked conversation, an editor
//! extension, any program — can write a transcript the source locates or append records
//! to it, and those records are reported like the agent's own. A reply is "what the CLI's
//! transcript says ended a turn", suitable for convenience relaying (a desktop host that
//! relays agent replies to chat channels), **never for authorisation decisions**,
//! billing, or as proof that this terminal's agent produced it. For verified
//! observations use the controlled tier ([`TurnReceiver`](super::TurnReceiver),
//! [`TranscriptReceiver`](super::TranscriptReceiver)).
//!
//! # Model
//!
//! - An [`AmbientSource`] knows one CLI: where its transcript for the agent in a terminal
//!   is ([`locate`](AmbientSource::locate), from an [`AmbientContext`], called on every
//!   wake) and how its lines read ([`AmbientParser`], fed one complete line at a time,
//!   plus a "the file has been quiet" call). A profile offers its source through
//!   [`AgentProfile::ambient_turns`](crate::agent::AgentProfile::ambient_turns) — the
//!   built-in Claude, Codex, Pi and Cursor profiles do, for the user's own configuration
//!   directory and any installed version — and hosts can bring their own with
//!   [`AgentTerminal::attach_ambient_turns_with`].
//! - [`AgentTerminal::attach_ambient_turns`] starts one worker thread
//!   (`agent-link-ambient-<id>`) per attachment and returns an [`AmbientReceiver`]. It
//!   is a pull receiver like [`TurnReceiver`](super::TurnReceiver):
//!   [`try_recv`](AmbientReceiver::try_recv) and [`status`](AmbientReceiver::status)
//!   never block, while [`recv_timeout`](AmbientReceiver::recv_timeout) and
//!   [`wait_status_change`](AmbientReceiver::wait_status_change) (with `tokio`, the
//!   awaitable `recv` and `status_change`) wait for the worker's next change instead of
//!   polling. Nothing goes through the PTY [`Event`](crate::Event) stream or the
//!   controller's monitor queue, and none of these calls performs I/O.
//! - Each [`AmbientTurn`] says how its end was established ([`AmbientEvidence`]): an
//!   explicit end record, the record after it, a quiet window, or the agent leaving the
//!   transcript.
//! - A CLI that writes no transcript to tail but serves its turns (OpenCode's TUI server)
//!   has an [`AmbientFeed`] instead: the worker polls one [`AmbientFeedReader`] per agent
//!   launch, with a time budget per poll, and keeps the same rules (from now, once per
//!   turn, bounds, status). A profile offers it through
//!   [`AgentProfile::ambient_feed`](crate::agent::AgentProfile::ambient_feed), used by
//!   [`AgentTerminal::attach_ambient_turns`] when the profile has no transcript source;
//!   hosts can bring their own with [`AgentTerminal::attach_ambient_feed_with`]. A feed
//!   that reads the agent's server needs the agent launched to serve one:
//!   [`AgentTerminal::spawn_with_ambient_feed`] (see
//!   [`AgentProfile::ambient_feed_launch`](crate::agent::AgentProfile::ambient_feed_launch)).
//!
//! # Lifecycle
//!
//! An attachment follows the terminal, not one agent process, so hosts attach once per
//! terminal. While no agent launched by this terminal runs (before
//! [`launch`](AgentTerminal::launch), or after the agent returned to the shell) it waits
//! with [`AmbientWaiting::NoAgent`] and reads nothing; an agent the user starts by hand in
//! the shell is not one the terminal launched. A direct-spawned terminal's child is its
//! agent. Every launch and relaunch the controller records (its lifecycle revision), and
//! every departure, starts a new *generation*: the transcript followed so far is read a
//! last time. At a complete line boundary a candidate final record can end its turn with
//! [`AmbientEvidence::InferredAtTranscriptEnd`]. Positions are then dropped. Each
//! generation establishes fresh [starting positions](#from-now) for its transcripts.
//! A suspended launch resumes from fresh starting positions when the same identified
//! process group is positively observed in the foreground, without needing a relaunch.
//! The worker re-locates the transcript on every wake
//! with the current [`AmbientContext::program`] and [`AmbientContext::launched_at`].
//! On a path switch, child exit or terminal drop, the worker drains the old transcript
//! through the byte length observed at departure. Each pass verifies the opened file’s
//! identity and the bytes at the captured EOF. At the same length it also requires the
//! captured file revision. When the departing launch time is known and no replacement launch has been
//! observed, it checks for a new path and drains that one too. On relaunch, the old
//! reader leaves a newly created file to the replacement. Without a creation time it
//! can drain only a file it previously identified. A failed final read or a
//! rewrite detected during the drain closes observation with [`AmbientClosed::IncompleteDrain`].
//! Before the terminal drain, the worker reconciles the latest recorded launch, even
//! if that replacement started and exited between polls. The final lookup uses that
//! launch's context and freshness time. After a successful terminal drain the receiver closes with [`AmbientClosed::TerminalEnded`].
//!
//! For a feed, a generation is one reader: opened at the launch's first wake and drained on departure.
//! Final polls ([`AmbientFeedBudget::last`]) share a two-second deadline. A feed with
//! pages outstanding at expiry closes observation with [`AmbientClosed::IncompleteDrain`]. A turn the
//! feed says ended before the generation started is not reported; a reader that reports
//! [`AmbientFeedState::Ended`] is not polled again until the next launch.
//!
//! A departure is noticed on a wake when the terminal's foreground, as the terminal's
//! [`ForegroundPolicy`](crate::agent::ForegroundPolicy) trusts it, shows the shell or
//! another program instead of the launch's identified program. Where that program was
//! never identified (Windows under the default exact-only policy, or after the host
//! submitted a line to the shell) a departure is noticed only at the controller's next
//! launch or delivery, and reading continues meanwhile.
//!
//! # From now
//!
//! For a transcript that predates attachment or launch, reading starts at its size when
//! listed or first located. A line still incomplete there is new once it ends. Files
//! located before they exist start at byte zero. Files created during the generation
//! also start at zero unless an evicted transcript's offset was lost. From then on,
//! existing files with unknown positions start at EOF to avoid replaying old turns.
//! A file that shrinks, changes its first line or
//! changes its last bytes read was rewritten or replaced: it is read again from its start
//! and only what follows the line handled last (found again by content) is new; when that
//! line is gone or the rewrite interrupted the initial baseline scan, reading resumes at EOF. A fork skips the conversation prefix
//! it shares with a previously read transcript. A source may identify a per-session header
//! to exclude from that comparison. Once the files diverge, subsequent turns are fresh.
//! Once the digest capacity or baseline scan window has omitted history,
//! a newly encountered transcript with an unknown position starts at its observed EOF. Reported turns are not
//! reported again after a rewrite.
//!
//! # Bounds
//!
//! [`AmbientOptions::limits`] reuses [`TurnOptions`] and its
//! ceilings: each pass reads at most `read_bytes` new bytes and hands at most `records`
//! lines to the parser; a line longer than `line_bytes` is never buffered (it is hashed
//! for continuity, skipped and counted in [`AmbientStatus::Active`]); a reply keeps at
//! most `reply_bytes` (cut at a UTF-8 boundary, [`TurnReply::truncated`] set).
//! Staged and queued turns together use at most `queue_items` slots and `queue_bytes` reply bytes. A turn that does not
//! fit closes the attachment with [`AmbientClosed::Overflow`] (as the controlled reader
//! ends with [`TurnGap::Overflow`](super::TurnGap::Overflow)); turns queued before it stay
//! available. At most [`ambient_worker_limit`] workers run per process (default
//! [`DEFAULT_AMBIENT_WORKERS`]), counted independently of the controlled tier's four.
//! Each worker keeps at most four transcripts' read state, each with a partial-line buffer
//! under `line_bytes` plus `read_bytes`, and at most 8,192 prefix digests to recognise
//! copied history. For starting positions it examines at most 4,096 directory entries
//! per launch and remembers at most 4,096 inactive transcript offsets.
//!
//! # Threads and shutdown
//!
//! [`stop`](AmbientReceiver::stop) and drop never wait: they mark the receiver closed,
//! discard queued turns and wake the worker and every waiting thread or task (see
//! [waiting](AmbientReceiver#waiting) for what else wakes them, and when). Nothing is
//! published after that, even if the worker is inside a slow file read or a source
//! callback; it then exits after that call returns, holding its worker slot until it does. Terminal teardown never waits for a
//! worker. Source and parser code runs only on the worker; a panic there closes the
//! attachment with [`AmbientClosed::WorkerPanic`].
//!
//! # Questions
//!
//! With the `questions` feature a parser can also tell what its transcript shows of a
//! question the agent asks (`AmbientParser::question`; the built-in Claude and Codex
//! sources do). `AgentTerminal::observe_ambient_transcript_questions` relays such a
//! question through the ordinary question events only while the terminal's screen shows
//! the same question, and answers it with the profile's keys. The worker keeps what the
//! transcript it follows shows asked; copied history, a rewritten file and a line over the
//! line budget withdraw it, and so does a new launch. The same best-effort terms apply:
//! the on-screen dialog, not the transcript, is what makes a relayed question this
//! terminal's.
//!
//! ```no_run
//! use std::path::PathBuf;
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! use agent_link::agent::AgentTerminal;
//! use agent_link::agent::turns::TurnReply;
//! use agent_link::agent::turns::ambient::{
//!     AmbientContext, AmbientOptions, AmbientParser, AmbientRecord, AmbientRecvError,
//!     AmbientSource, AmbientUnavailable,
//! };
//!
//! /// A CLI that writes `<config>/last.jsonl`, a `{"reply":…}` line per answer and a
//! /// `{"done":true}` line at each turn's end (a toy format).
//! struct Toy;
//!
//! impl AmbientSource for Toy {
//!     fn locate(&self, ctx: &AmbientContext) -> Result<PathBuf, AmbientUnavailable> {
//!         let home = ctx.home().ok_or(AmbientUnavailable::NoConfigRoot)?;
//!         Ok(home.join(".toy").join("last.jsonl"))
//!     }
//!
//!     fn parser(&self) -> Box<dyn AmbientParser> {
//!         Box::new(ToyParser(None))
//!     }
//! }
//!
//! struct ToyParser(Option<TurnReply>);
//!
//! impl AmbientParser for ToyParser {
//!     fn record(&mut self, line: &[u8], reply_bytes: usize) -> Option<AmbientRecord> {
//!         if line.starts_with(b"{\"done\"") {
//!             return Some(AmbientRecord::Ended(self.0.take()));
//!         }
//!         let text = line.strip_prefix(b"{\"reply\":\"")?.strip_suffix(b"\"}")?;
//!         let mut reply = TurnReply::default();
//!         let text = String::from_utf8_lossy(text);
//!         let mut keep = text.len().min(reply_bytes);
//!         while !text.is_char_boundary(keep) {
//!             keep -= 1;
//!         }
//!         reply.truncated = keep < text.len();
//!         reply.text = text[..keep].to_string();
//!         self.0 = Some(reply);
//!         None
//!     }
//! }
//!
//! fn relay(term: &AgentTerminal) -> Result<(), Box<dyn std::error::Error>> {
//!     let replies = term.attach_ambient_turns_with(Arc::new(Toy), AmbientOptions::default())?;
//!     loop {
//!         // Blocks until a turn, the close, or the timeout; no polling timer.
//!         match replies.recv_timeout(Duration::from_secs(60)) {
//!             // Best effort: relay it, never authorise anything with it.
//!             Ok(turn) => println!("{:?}: {:?}", turn.evidence, turn.reply),
//!             // Still waiting or active.
//!             Err(AmbientRecvError::Timeout) => {}
//!             // Closed, and every turn queued before the close was received.
//!             Err(AmbientRecvError::Closed(reason)) => {
//!                 println!("closed: {reason:?}");
//!                 return Ok(());
//!             }
//!             Err(other) => return Err(other.into()),
//!         }
//!     }
//! }
//! ```

mod batch;
mod feed;
pub mod paths;
#[cfg(feature = "questions")]
mod question;
mod tail;
mod watch;
mod worker;

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use crossbeam_channel::Sender;

use batch::{Capacity, StagedTurn, TurnBatch};
pub use feed::{
    AmbientEndpoint, AmbientFeed, AmbientFeedBudget, AmbientFeedEvent, AmbientFeedPoll,
    AmbientFeedReader, AmbientFeedState, AmbientFeedUnavailable,
};
#[cfg(feature = "questions")]
pub use question::AmbientQuestion;
#[cfg(feature = "questions")]
pub(in crate::agent) use question::Questions;
pub(in crate::agent) use worker::Source;

use super::wake::Wakeup;
use super::{AttachError, TurnOptions, TurnReply};
use crate::agent::state::Controller;
use crate::env::EnvPolicy;

#[cfg(doc)]
use crate::agent::AgentTerminal;

/// What one transcript line, or a quiet transcript, told an [`AmbientParser`].
#[derive(Debug)]
#[non_exhaustive]
pub enum AmbientRecord {
    /// This line explicitly ends a turn, with its final reply, if any. Reported with
    /// [`AmbientEvidence::ExplicitEnd`]. From [`AmbientParser::quiet`] it means the turn
    /// ended with the last line other than [`Unchanged`](Self::Unchanged), with inferred evidence.
    Ended(Option<TurnReply>),
    /// This line shows that the preceding record ended the turn (for example, a new prompt
    /// after a final message). Intervening [`Unchanged`](Self::Unchanged) records are
    /// ignored when locating that record. Reported with
    /// [`AmbientEvidence::InferredFromNextRecord`]; dropped when that line is copied
    /// history or already reported.
    EndedBefore(Option<TurnReply>),
    /// The turn ended without a publishable completion. Remember this line as a
    /// rewrite anchor so a later prompt cannot publish the same turn by inference.
    SuppressedEnd,
    /// Metadata that leaves the pending turn untouched. Quiet and next-record inference
    /// still belong to the preceding record that supplied the candidate.
    Unchanged,
    /// A record of a kind the parser knows lacked the fields it expects: the CLI's format
    /// may have changed and turns may be missed. Reported as
    /// [`AmbientStatus::Active`]`{ unrecognised: true, .. }` until a turn ends cleanly.
    Unrecognised,
}

/// Reads one transcript's lines in file order. A fresh parser is made for every file the
/// source is located at and again after every rewrite of it (the lines are then fed again
/// from the first, and the observer drops what it reported already).
///
/// Runs on the attachment's worker only. Retain at most one bounded reply candidate
/// (`reply_bytes`, the host's [`TurnOptions::reply_bytes`](super::TurnOptions::reply_bytes))
/// plus bounded metadata. Longer reply text is cut by the observer and flagged truncated.
/// A panic closes the attachment ([`AmbientClosed::WorkerPanic`]).
pub trait AmbientParser: Send {
    /// One complete, nonblank line: without its newline, trimmed of surrounding ASCII
    /// whitespace, at most [`TurnOptions::line_bytes`](super::TurnOptions::line_bytes)
    /// long, not validated as UTF-8 or JSON. The initial baseline is skipped. After a
    /// rewrite, records may be replayed to locate the last observed turn. An oversized
    /// line discards this parser and its pending candidate.
    fn record(&mut self, line: &[u8], reply_bytes: usize) -> Option<AmbientRecord>;

    /// The transcript has not grown for [`AmbientOptions::quiet`] since the last line
    /// passed, or cannot grow any more for this agent (it left the transcript, returned to
    /// the shell, relaunched or exited): what that ended, if anything. Both cases require
    /// a complete line boundary. Repeated quiet passes should report a turn only once. A returned
    /// [`Ended`](AmbientRecord::Ended) or [`EndedBefore`](AmbientRecord::EndedBefore)
    /// counts as the last line's (ignoring [`AmbientRecord::Unchanged`]), reported with
    /// [`AmbientEvidence::InferredAfterQuiet`] or
    /// [`AmbientEvidence::InferredAtTranscriptEnd`]. Default: nothing.
    fn quiet(&mut self, reply_bytes: usize) -> Option<AmbientRecord> {
        let _ = reply_bytes;
        None
    }

    /// What `line` — the line just passed to [`record`](Self::record) — tells of a question
    /// the agent asks the user and waits on (feature `questions`): a question tool's call
    /// written before its result ([`AmbientQuestion::Asked`]), that result
    /// ([`AmbientQuestion::Answered`]), or anything after which no question stands — a
    /// prompt, the turn's end, a question record that cannot be read
    /// ([`AmbientQuestion::Withdrawn`]). `None` leaves the question unchanged.
    /// Called after `record` for fresh lines. Replayed and copied lines call
    /// [`reset_question_context`](Self::reset_question_context). What a transcript asks
    /// is relayed only while the terminal's screen shows the same question
    /// ([`AgentTerminal::observe_ambient_transcript_questions`]). Default: `None` (the
    /// source reads no questions); the built-in Claude and Codex sources read them.
    #[cfg(feature = "questions")]
    fn question(&mut self, line: &[u8]) -> Option<AmbientQuestion> {
        let _ = line;
        None
    }

    /// Forget prompt and question evidence after `record` processed copied or replayed
    /// history. Stateful question parsers must clear any state that could authorize a
    /// later question. Stateless parsers can use the default.
    #[cfg(feature = "questions")]
    fn reset_question_context(&mut self) {}
}

/// One agent CLI's best-effort transcript knowledge: where it writes for the agent in a
/// terminal, and how its lines read. Returned by
/// [`AgentProfile::ambient_turns`](crate::agent::AgentProfile::ambient_turns) or passed to
/// [`AgentTerminal::attach_ambient_turns_with`]. Shared by attachments; called on their
/// workers only.
pub trait AmbientSource: Send + Sync + 'static {
    /// The transcript the agent in `ctx` writes now. Called on every wake while an agent
    /// runs (about every [`AmbientOptions::poll`]), so keep it cheap and bounded: read only
    /// small local files and bounded directory listings, never the network. A relative
    /// path is taken against [`AmbientContext::cwd`]. A path that differs from the last
    /// one is followed from then on. The file may not exist yet (it reads as empty until it
    /// does). Its directory must exist, and the opened file must be regular.
    /// During the launch, an error gives [`AmbientWaiting::Unavailable`] until the next
    /// lookup. On departure, after draining any known path, the worker checks for a newly selected
    /// transcript using the departing launch's context unless it observed a replacement launch.
    /// [`AmbientUnavailable::Io`] or [`AmbientUnavailable::ReadBudgetExceeded`] on this
    /// final lookup gives [`AmbientClosed::IncompleteDrain`].
    fn locate(&self, ctx: &AmbientContext) -> Result<PathBuf, AmbientUnavailable>;

    /// Checks the opened regular file before reading its records, including on departure.
    /// The descriptor starts at byte zero and the worker rewinds it after the check.
    /// Return false to skip a file whose header no longer identifies the selected project
    /// or session type. I/O errors stop the pass and fail a final drain. The default
    /// accepts every file at the located path.
    fn accepts_file(&self, _ctx: &AmbientContext, _file: &std::fs::File) -> std::io::Result<bool> {
        Ok(true)
    }

    /// Whether the first nonblank record is a session header that a fork can replace.
    /// Such a header contains no turn or question. The worker omits it when comparing
    /// conversation prefixes, including in the initial baseline. The default compares
    /// every record. Called on the worker with a complete line within the line budget.
    fn is_session_header(&self, _line: &[u8]) -> bool {
        false
    }

    /// A fresh parser for one transcript file.
    fn parser(&self) -> Box<dyn AmbientParser>;
}

/// What an [`AmbientSource`] is told about the terminal it locates a transcript for.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AmbientContext {
    /// The terminal's working directory, as spawned (made absolute). The agent may have
    /// started elsewhere if the shell changed directory first.
    pub cwd: PathBuf,
    /// The PTY child's environment policy, the profile launch plan's variables merged in.
    /// Read variables through [`var`](Self::var). A login shell's startup files can still
    /// change what the agent sees.
    pub env: EnvPolicy,
    /// The agent's program once known. Shell launches: Unix its foreground process group
    /// (the agent's pid when it leads its job); Windows the one child of the shell it runs
    /// under. Direct spawns: the child's pid. `None` while not known.
    pub program: Option<u32>,
    /// Wall-clock time just before direct child creation, or when the shell launch line
    /// was issued. `None` before launch.
    pub launched_at: Option<SystemTime>,
    /// Last path selected by this attachment during the current launch.
    pub(in crate::agent) current_transcript: Option<PathBuf>,
}

impl AmbientContext {
    /// A context for `cwd` and `env`, with no program and no launch time (for testing a
    /// source).
    pub fn new(cwd: impl Into<PathBuf>, env: EnvPolicy) -> Self {
        Self {
            cwd: cwd.into(),
            env,
            program: None,
            launched_at: None,
            current_transcript: None,
        }
    }

    /// The value `key` has in the agent's environment: the policy's own value, else — when
    /// the policy inherits and does not remove `key` — this process's. Terminal identity
    /// defaults and `PATH` prepends are not applied (use
    /// [`EnvPolicy::effective_path`] for `PATH`).
    pub fn var(&self, key: impl AsRef<OsStr>) -> Option<OsString> {
        self.env.child_var(key.as_ref())
    }

    /// The agent's home directory: `HOME` (Windows: `USERPROFILE`) from
    /// [`var`](Self::var); `None` when unset or empty.
    pub fn home(&self) -> Option<PathBuf> {
        let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        self.var(key).filter(|v| !v.is_empty()).map(PathBuf::from)
    }
}

/// Why no transcript is read now: a source's [`locate`](AmbientSource::locate) failure, or
/// the observer's own check of the located path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmbientUnavailable {
    /// The agent's program or launch, which the lookup needs, is not known yet.
    AgentUnknown,
    /// Neither the CLI's configuration override nor a home directory is set in the
    /// agent's environment.
    NoConfigRoot,
    /// No transcript for this agent was found (yet).
    NotFound,
    /// The lookup exhausted its byte budget with candidates still to check. Retried
    /// on the next wake. The Claude fallback starts a fresh scan on each lookup.
    ReadBudgetExceeded,
    /// Several candidates matched and the source declined to guess.
    Ambiguous,
    /// The located transcript's directory does not exist.
    MissingDirectory,
    /// The installed CLI's layout, its recorded state or this platform is not supported.
    Unsupported,
    /// Local I/O failed while locating or reading; retried on the next wake.
    Io(std::io::ErrorKind),
    /// A feed's server cannot be read ([`AmbientFeed`]); the reason says whether it is
    /// retried.
    Feed(AmbientFeedUnavailable),
}

/// How an [`AmbientTurn`]'s end was established. None of these attributes the turn to
/// this terminal's agent (see the [module docs](self)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AmbientEvidence {
    /// The source recognised an explicit end-of-turn record in the transcript, or a feed
    /// told an explicit turn end ([`AmbientFeedEvent::Ended`]).
    ExplicitEnd,
    /// A later record (typically the next prompt) showed that the record before it ended
    /// the turn.
    InferredFromNextRecord,
    /// The transcript stayed unchanged for [`AmbientOptions::quiet`] after a record the
    /// parser holds as a final reply. A later record can still contradict it.
    InferredAfterQuiet,
    /// The agent stopped writing the transcript — it moved to another one, returned to the
    /// shell, relaunched or exited — after a record the parser holds as a final reply.
    InferredAtTranscriptEnd,
}

impl AmbientEvidence {
    /// Whether the transcript held an explicit end record.
    pub fn is_explicit(self) -> bool {
        self == Self::ExplicitEnd
    }
}

/// One turn end a transcript (or a feed) showed after observation started. `Debug` reports the reply's
/// byte count and truncation flag, never its text.
#[derive(Debug)]
#[non_exhaustive]
pub struct AmbientTurn {
    /// The final reply, at most [`TurnOptions::reply_bytes`](super::TurnOptions::reply_bytes)
    /// (cut at a UTF-8 boundary and flagged truncated); `None` when the turn ended without
    /// text (aborted, failed, tool calls only) or with blank text. A truncated reply
    /// can have empty text when its first character exceeds the budget.
    pub reply: Option<TurnReply>,
    /// How the end was established.
    pub evidence: AmbientEvidence,
}

/// Why an [`AmbientReceiver`] waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmbientWaiting {
    /// The worker has not finished its first look yet.
    Starting,
    /// No agent launched by this terminal runs: before its launch, or after it returned to
    /// the shell.
    NoAgent,
    /// The transcript cannot be read now; retried on the next wake.
    Unavailable(AmbientUnavailable),
}

/// Why an [`AmbientReceiver`] closed. Closed is final; turns queued before it stay
/// available to [`try_recv`](AmbientReceiver::try_recv) and
/// [`recv_timeout`](AmbientReceiver::recv_timeout) until [`stop`](AmbientReceiver::stop) or
/// drop, and a receive returns [`AmbientRecvError::Closed`] once they are gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmbientClosed {
    /// The host called [`stop`](AmbientReceiver::stop) or dropped the receiver.
    Stopped,
    /// The terminal's child exited, its session ended, or the terminal was dropped.
    TerminalEnded,
    /// The controller lost its internal PTY monitor: agent launches can no longer be
    /// followed.
    Monitor,
    /// A turn did not fit the queue (`queue_items` or `queue_bytes`); it and everything
    /// after it are lost.
    Overflow,
    /// A previously observed transcript disappeared, the final read failed, or the
    /// transcript changed during the drain, or the final lookup exhausted its byte budget.
    /// A feed also reports this if it ends with pages outstanding during a final poll or
    /// has more to read at its final deadline. Queued turns stay available.
    IncompleteDrain,
    /// The worker, its source or its parser panicked.
    WorkerPanic,
}

/// Where an [`AmbientReceiver`] stands. Status is published by the worker; reading it
/// performs no I/O.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AmbientStatus {
    /// Not reading a transcript now.
    Waiting(AmbientWaiting),
    /// Reading the located transcript (it may not exist yet: it reads as empty).
    Active {
        /// A record of a kind the source knows lacked expected fields since the last turn
        /// end in this transcript: the CLI's format may have changed.
        unrecognised: bool,
        /// Saturating count of lines skipped since attach for exceeding
        /// [`TurnOptions::line_bytes`](super::TurnOptions::line_bytes); a turn end among
        /// them was missed.
        oversized_lines: u32,
    },
    /// Observation ended.
    Closed(AmbientClosed),
}

impl AmbientStatus {
    /// Whether observation ended.
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Closed(_))
    }
}

/// Per-attachment limits and timing, [`validate`](Self::validate)d at attach. Start from
/// [`AmbientOptions::default`] and change fields.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct AmbientOptions {
    /// Byte, record, reply and queue bounds, with the controlled tier's ceilings
    /// (defaults are the maximums). `reply_bytes` must fit `queue_bytes`.
    pub limits: TurnOptions,
    /// How often the worker wakes to re-locate and read (default 500 ms; 10 ms to 60 s).
    /// Passes that hit a byte or record budget continue without waiting.
    pub poll: Duration,
    /// How long a transcript must stay unchanged before the parser is asked whether that
    /// ended a turn (default 3 s; 10 ms to 1 h).
    pub quiet: Duration,
}

impl AmbientOptions {
    const POLL: (Duration, Duration) = (Duration::from_millis(10), Duration::from_secs(60));
    const QUIET: (Duration, Duration) = (Duration::from_millis(10), Duration::from_secs(3600));

    /// Checks every limit against its range ([`AttachError::InvalidLimits`] otherwise).
    pub fn validate(&self) -> Result<(), AttachError> {
        self.limits.validate()?;
        let within =
            |value: Duration, (min, max): (Duration, Duration)| (min..=max).contains(&value);
        if !within(self.poll, Self::POLL) || !within(self.quiet, Self::QUIET) {
            return Err(AttachError::InvalidLimits);
        }
        Ok(())
    }
}

impl Default for AmbientOptions {
    fn default() -> Self {
        Self {
            limits: TurnOptions::default(),
            poll: Duration::from_millis(500),
            quiet: Duration::from_secs(3),
        }
    }
}

/// Process-wide ambient workers allowed by default.
pub const DEFAULT_AMBIENT_WORKERS: usize = 64;
/// The highest [`set_ambient_worker_limit`] accepts.
pub const MAX_AMBIENT_WORKERS: usize = 256;

static WORKER_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_AMBIENT_WORKERS);
static WORKERS: AtomicUsize = AtomicUsize::new(0);

/// Sets how many ambient workers (active or retiring) may run in this process, 1 to
/// [`MAX_AMBIENT_WORKERS`]; [`AttachError::InvalidLimits`] otherwise. Independent of the
/// controlled tier's budget. Lowering it stops no running worker; attaches beyond it
/// return [`AttachError::Busy`].
pub fn set_ambient_worker_limit(limit: usize) -> Result<(), AttachError> {
    if !(1..=MAX_AMBIENT_WORKERS).contains(&limit) {
        return Err(AttachError::InvalidLimits);
    }
    WORKER_LIMIT.store(limit, Ordering::Release);
    Ok(())
}

/// The current process-wide ambient worker limit.
pub fn ambient_worker_limit() -> usize {
    WORKER_LIMIT.load(Ordering::Acquire)
}

/// Ambient workers running now, including those retiring after a stop.
pub fn ambient_workers() -> usize {
    WORKERS.load(Ordering::Acquire)
}

/// One slot of the process-wide worker budget, released when the worker thread ends.
struct Permit(());

impl Permit {
    fn acquire() -> Result<Self, AttachError> {
        #[allow(deprecated, reason = "try_update is unavailable on the Rust 1.85 MSRV")]
        WORKERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < WORKER_LIMIT.load(Ordering::Acquire)).then_some(n + 1)
            })
            .map_err(|_| AttachError::Busy)?;
        Ok(Self(()))
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What a terminal records at spawn for its ambient attachments.
pub(in crate::agent) struct Base {
    pub(in crate::agent) cwd: PathBuf,
    pub(in crate::agent) env: EnvPolicy,
    /// The child is the agent.
    pub(in crate::agent) direct: bool,
    pub(in crate::agent) child: Option<u32>,
    pub(in crate::agent) spawned_at: SystemTime,
    /// Where the launch serves an ambient feed
    /// ([`AgentTerminal::spawn_with_ambient_feed`]); `None` otherwise.
    pub(in crate::agent) endpoint: Option<AmbientEndpoint>,
}

/// Why a blocking or async receive on an [`AmbientReceiver`] returned without a turn.
///
/// Ambient observation has no gap: it either goes on (waiting or active, whatever the
/// waiting reason) or is closed for good, and turns queued before a close other than
/// [`stop`](AmbientReceiver::stop) are still delivered first. So, unlike the controlled
/// tier's [`TurnRecvError`](super::TurnRecvError), a closed receive carries the
/// [`AmbientClosed`] reason and means "closed, and nothing is left to receive".
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AmbientRecvError {
    /// The timeout elapsed while observation was still waiting (any [`AmbientWaiting`],
    /// [`Unavailable`](AmbientWaiting::Unavailable) included) or active and no turn was
    /// queued. Only blocking receives time out.
    #[error("no ambient turn within the timeout")]
    Timeout,
    /// Observation closed ([`AmbientStatus::Closed`]) with this reason, and every turn
    /// queued before it was received (after [`stop`](AmbientReceiver::stop), discarded).
    /// Final: every later receive returns it at once.
    #[error("ambient turn observation closed: {0:?}")]
    Closed(AmbientClosed),
}

struct Queue {
    status: AmbientStatus,
    turns: VecDeque<StagedTurn>,
}

impl Queue {
    fn pop(&mut self) -> Option<AmbientTurn> {
        Some(self.turns.pop_front()?.turn)
    }
}

/// The receiver's and the worker's shared state. Lock order: never held while taking the
/// controller lock or doing I/O. The worker, including a feed's polls, changes status and
/// queue only through [`publish_batch`](Self::publish_batch) and [`close`](Self::close), and the
/// receiver only through its pulls and [`stop`](AmbientReceiver::stop): each of these
/// wakes waiters after the queue lock is released whenever the status changed or a turn
/// was queued (stop always). A pull wakes nobody: it only removes a turn.
pub(super) struct Shared {
    queue: Mutex<Queue>,
    cancel: Sender<()>,
    capacity: Capacity,
    wakeup: Wakeup,
    /// The question the followed transcript shows asked (its own lock; never held with
    /// the queue's or the controller's).
    #[cfg(feature = "questions")]
    question: question::Slot,
}

impl Shared {
    fn new(limits: TurnOptions, cancel: Sender<()>) -> Self {
        Self {
            queue: Mutex::new(Queue {
                status: AmbientStatus::Waiting(AmbientWaiting::Starting),
                turns: VecDeque::new(),
            }),
            cancel,
            capacity: Capacity::new(limits),
            wakeup: Wakeup::default(),
            #[cfg(feature = "questions")]
            question: question::Slot::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn live(&self) -> bool {
        !self.lock().status.is_closed()
    }

    /// Publishes `status` (when given) and then `turns` while observation is live: a turn
    /// never reaches the host under a stale status. A turn that does not fit closes
    /// observation with an overflow. Returns whether it is still live. Wakes waiters when
    /// the status changed or a turn was queued; republishing an unchanged status (every
    /// wake) does not.
    fn publish_batch(&self, status: Option<AmbientStatus>, batch: TurnBatch) -> bool {
        let (live, changed) = {
            let mut q = self.lock();
            let before = q.status;
            let mut live = !q.status.is_closed();
            let mut queued = false;
            if live {
                if let Some(status) = status {
                    q.status = status;
                }
                queued = !batch.turns.is_empty();
                q.turns.extend(batch.turns);
                if batch.overflow {
                    q.status = AmbientStatus::Closed(AmbientClosed::Overflow);
                    live = false;
                }
            }
            (live, queued || q.status != before)
        };
        if changed {
            self.wakeup.wake_all();
        }
        live
    }

    #[cfg(test)]
    fn publish(&self, status: Option<AmbientStatus>, turns: Vec<AmbientTurn>) -> bool {
        let mut batch = TurnBatch::default();
        for turn in turns {
            batch.push(turn, &self.capacity);
        }
        self.publish_batch(status, batch)
    }

    /// Closes observation with `reason` unless it is closed already, waking waiters if so.
    fn close(&self, reason: AmbientClosed) {
        let closed = {
            let mut q = self.lock();
            let open = !q.status.is_closed();
            if open {
                q.status = AmbientStatus::Closed(reason);
            }
            open
        };
        if closed {
            self.wakeup.wake_all();
        }
    }
}

/// One best-effort observer of a terminal's agent transcripts (see the
/// [module docs](self)). Pull-based: [`status`](Self::status) and
/// [`try_recv`](Self::try_recv) never perform I/O or wait for the worker;
/// [`recv_timeout`](Self::recv_timeout) and [`wait_status_change`](Self::wait_status_change)
/// (with `tokio` also `recv` and `status_change`)
/// wait for the worker's next change instead of polling. Dropping it stops observation.
///
/// # Waiting
///
/// Waits block (or await) between attempts until the receiver changes; they spawn
/// nothing and hold no lock while blocked. Any number of threads and tasks may wait on
/// one receiver (share it by reference or in an [`Arc`]); each turn reaches one of them.
///
/// - **At once:** a turn's publication; every status the worker publishes that differs
///   from the last one — `Waiting` to `Active` and back, a change of waiting reason
///   (including a feed's [`AmbientFeedState`]), a change of the `Active` counters; any
///   close, the overflow included; [`stop`](Self::stop) or drop, from any thread; and
///   terminal teardown, which ends the worker's wait at once (it then reads the
///   transcript or feed through its final drain and closes).
/// - **At the worker's next wake** ([`AmbientOptions::poll`], default 500 ms): what the
///   worker learns by sampling — a launch, relaunch or departure, the child's exit,
///   monitor loss, a new transcript path or newly written lines, a quiet window that
///   ended, a feed's new events or state. Each is then published and wakes waiters.
///   While the worker is inside a slow file read, source lookup or feed poll (a feed has
///   two seconds per poll), everything but `stop` waits for that call to return.
///
/// Unlike [`TurnReceiver`](super::TurnReceiver), [`status`](Self::status) samples nothing
/// itself, so a wait that reaches its deadline reports what the worker last published.
#[must_use = "dropping the receiver stops observation"]
pub struct AmbientReceiver {
    shared: Arc<Shared>,
}

impl AmbientReceiver {
    /// Where observation stands, as the worker last published it.
    pub fn status(&self) -> AmbientStatus {
        self.shared.lock().status
    }

    /// The next turn, oldest first; `None` when none is queued. Turns queued before the
    /// receiver closed (other than by [`stop`](Self::stop)) are still returned. Never
    /// blocks; wait with [`recv_timeout`](Self::recv_timeout) instead of polling.
    pub fn try_recv(&self) -> Option<AmbientTurn> {
        self.shared.lock().pop()
    }

    /// Waits up to `timeout` for the next turn instead of polling
    /// [`try_recv`](Self::try_recv). A zero timeout makes one attempt; a timeout too large
    /// for the clock waits without a deadline.
    ///
    /// - `Ok(turn)`: the oldest queued turn, removed from the queue. After a close other
    ///   than [`stop`](Self::stop) — an [`Overflow`](AmbientClosed::Overflow) or the
    ///   terminal's end, say — the turns queued before it still come first.
    /// - [`AmbientRecvError::Closed`]: observation closed with that reason and no turn is
    ///   left. Final; later calls return it at once.
    /// - [`AmbientRecvError::Timeout`]: no turn arrived and observation is still waiting
    ///   or active. [`AmbientWaiting::Unavailable`] is not final: the worker retries.
    ///
    /// Blocks the calling thread, spawns nothing and performs no I/O. See
    /// [Waiting](Self#waiting) for what wakes it at once and what only at the worker's
    /// next wake.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<AmbientTurn, AmbientRecvError> {
        self.shared
            .wakeup
            .wait_for(timeout, || self.attempt())
            .or_else(|| self.attempt())
            .unwrap_or(Err(AmbientRecvError::Timeout))
    }

    /// Waits up to `timeout` until [`status`](Self::status) differs from `seen`, for
    /// example from [`AmbientWaiting::Starting`] to `Active`, from one waiting reason to
    /// another, or to `Closed`. Returns the current status at once when it differs or is
    /// closed (final, even when equal to `seen`); `None` when it stayed `seen` for the
    /// whole timeout. A change and its reversal between two of its checks can go unseen.
    /// Blocks the calling thread and performs no I/O; wakeups as for
    /// [`recv_timeout`](Self::recv_timeout).
    pub fn wait_status_change(
        &self,
        seen: AmbientStatus,
        timeout: Duration,
    ) -> Option<AmbientStatus> {
        let changed = |now: AmbientStatus| (now != seen || now.is_closed()).then_some(now);
        self.shared
            .wakeup
            .wait_for(timeout, || changed(self.status()))
            .or_else(|| changed(self.status()))
    }

    /// Awaits the next turn (feature `tokio`): [`recv_timeout`](Self::recv_timeout)
    /// without a deadline, so it never returns [`AmbientRecvError::Timeout`]; wrap it in a
    /// timeout of your own. Occupies no thread, spawns nothing and holds no lock across an
    /// await; every waiting task is woken. Cancel-safe: a turn is removed only in the step
    /// that returns it, so dropping the future never loses one.
    #[cfg(feature = "tokio")]
    pub async fn recv(&self) -> Result<AmbientTurn, AmbientRecvError> {
        self.shared.wakeup.until(|| self.attempt()).await
    }

    /// Awaits a status other than `seen`, or a closed one (feature `tokio`):
    /// [`wait_status_change`](Self::wait_status_change) without a deadline. Cancel-safe.
    #[cfg(feature = "tokio")]
    pub async fn status_change(&self, seen: AmbientStatus) -> AmbientStatus {
        self.shared
            .wakeup
            .until(|| {
                let now = self.status();
                (now != seen || now.is_closed()).then_some(now)
            })
            .await
    }

    /// One pull: the oldest turn, else the close once nothing is left; `None` while
    /// observation goes on without a turn. One lock, so no turn queued before a close is
    /// skipped.
    fn attempt(&self) -> Option<Result<AmbientTurn, AmbientRecvError>> {
        let mut q = self.shared.lock();
        if let Some(turn) = q.pop() {
            return Some(Ok(turn));
        }
        match q.status {
            AmbientStatus::Closed(reason) => Some(Err(AmbientRecvError::Closed(reason))),
            AmbientStatus::Waiting(_) | AmbientStatus::Active { .. } => None,
        }
    }

    /// Ends observation ([`AmbientClosed::Stopped`] unless already closed), discards queued
    /// turns and wakes the worker and every waiter without waiting for the worker.
    /// Repeated calls are harmless. The terminal keeps running.
    pub fn stop(&self) {
        let discarded = {
            let mut q = self.shared.lock();
            if !q.status.is_closed() {
                q.status = AmbientStatus::Closed(AmbientClosed::Stopped);
            }
            std::mem::take(&mut q.turns)
        };
        drop(discarded);
        let _ = self.shared.cancel.try_send(());
        self.shared.wakeup.wake_all();
    }

    /// Threads blocked in a wait on this receiver.
    #[cfg(test)]
    fn parked(&self) -> usize {
        self.shared.wakeup.parked()
    }

    /// Runs `pause` on the next waiter after its failed check, before it blocks or awaits.
    #[cfg(test)]
    fn pause_next_wait(&self, pause: impl FnOnce() + Send + 'static) {
        self.shared.wakeup.pause_next(pause);
    }
}

impl Drop for AmbientReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}

impl fmt::Debug for AmbientReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let q = self.shared.lock();
        f.debug_struct("AmbientReceiver")
            .field("status", &q.status)
            .field("queued", &q.turns.len())
            .finish()
    }
}

/// Starts a worker for `source` on the terminal `ctl` controls.
pub(in crate::agent) fn attach(
    ctl: &Arc<Controller>,
    base: &Arc<Base>,
    source: Source,
    options: AmbientOptions,
) -> Result<AmbientReceiver, AttachError> {
    options.validate()?;
    let launch = worker::Sample::controller(ctl, base);
    if launch.end.is_some() {
        return Err(AttachError::NoLaunch);
    }
    let permit = Permit::acquire()?;
    let (cancel, cancelled) = crossbeam_channel::bounded(1);
    let shared = Arc::new(Shared::new(options.limits, cancel));
    #[cfg(feature = "questions")]
    shared.question.bind(
        ctl,
        launch.lifecycle,
        matches!(source, Source::Transcript(_)),
    );
    let id = ctl.cfg.id;
    let worker = worker::Worker::new(
        ctl,
        Arc::clone(base),
        Arc::clone(&shared),
        source,
        options,
        cancelled,
        launch,
    );
    std::thread::Builder::new()
        .name(format!("agent-link-ambient-{id}"))
        .spawn(move || {
            let _permit = permit;
            worker.run();
        })
        .map_err(|_| AttachError::Spawn)?;
    Ok(AmbientReceiver { shared })
}

#[cfg(test)]
mod tests;
