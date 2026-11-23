//! What an ambient worker keeps per transcript: [`Watched`] — one file's reader, parser
//! and the line it handled last — and [`Files`], the transcripts located in the current
//! agent launch, which decides where a newly located one is read from. Ported from the
//! predecessor observer with byte, record, queue and memory bounds added.
//!
//! - **Where a file is first read from.** One kept in [`Files`] goes on where it was. One
//!   seen before (its directory was listed when a transcript there was first located, or
//!   it dropped out of the kept ones) is read from that size on: what was written to it
//!   since — a resumed older session — is new, nothing before; unless the bytes saved there
//!   have changed (rewritten since), then from its end. One created at or after the reference
//!   time (the attachment, or a later launch) is read from its start until an eviction
//!   cannot save its offset. Other existing files start at their end, including any
//!   unknown path after that first lost offset. Missing files start at zero.
//! - **After a rewrite** the file is read again from its first line, and turn ends are
//!   held until the line handled last (the *anchor*: the last turn end, else the last line
//!   there was when reading began) is found again. A match for the anchor or its
//!   predecessor at the original byte position takes priority over content alone. After a
//!   predecessor match, the first completion can still be the old end marker displaced by
//!   inserted lines. Its marker text and bounded reply must both match. Without
//!   a positioned match, the reader looks for both lines together, then the predecessor,
//!   then the anchor text alone. A rewrite during the initial baseline scan has no
//!   reliable anchor and resumes at the rewritten EOF. A file still
//!   shorter than what was read before may be a rewrite caught part-way: while it is empty
//!   or grew since the last read, it is looked for again on the next. Once its size held
//!   for a read, or it outgrew what was read, without the anchor: the file is read on from
//!   its end (`rebaselined`). Held turn ends are bounded by the queue limits; more than
//!   fit closes the attachment with an overflow once they would be published.
//! - **Copied history** (a fork copies the conversation into a new file). [`Seen`]
//!   remembers digests of conversation prefixes across transcripts. A new file skips
//!   the prefix it shares with another file, ignoring a source-identified session
//!   header. Once the records diverge, all later records belong to the new file.
//!   Once history is incomplete, newly discovered files start at their observed EOF.
//! - **Quiet** ([`Watched::quiet`]): once the file has not grown for the window (or cannot
//!   grow any more), the parser is asked whether that ended a turn. Its event counts as
//!   the last non-metadata line's: not when that line is copied history or the turn end handled last,
//!   nor after a failed read or while a rewrite is still being placed.
//! - **An end shown by the next line** ([`AmbientRecord::EndedBefore`]) counts as the
//!   preceding non-metadata line's, wherever it is read: dropped when that line is copied history or the
//!   anchor.
//! - **Questions** (feature `questions`): what the parser tells of a question counts on a
//!   new line of the file's own; a copied line, a line read again after a rewrite (until
//!   the anchor is placed), a line over the budget and the rewrite itself withdraw it.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use super::batch::{Capacity, TurnBatch};
use super::tail::{Budget, Feed, FinalRead, LineKey, Rewrite, SavedOffset, Tail};
use super::{AmbientEvidence, AmbientParser, AmbientRecord, AmbientSource, AmbientTurn};
use crate::agent::turns::TurnReply;

/// Transcripts kept with their read state; the least recently located one beyond is
/// dropped (where it was left is kept while [`Files::known`] has room).
pub(super) const MAX_WATCHED: usize = 4;
/// Directory entries examined across listings, and file offsets retained, per launch.
const MAX_LISTED: usize = 4096;
/// Distinct conversation prefix digests retained for copy detection.
const MAX_SEEN: usize = 8192;

/// Pin the known history during fork scans. Once a line cannot be remembered,
/// a subsequent new file needs an EOF baseline instead of a digest comparison.
#[derive(Debug, Default)]
pub(super) struct Seen {
    /// The sole transcript this key occurred in, or None if several contained it.
    owner: HashMap<u64, Option<u32>>,
    incomplete: bool,
}

impl Seen {
    /// Record this entire prefix and report whether another transcript contained it.
    fn record(&mut self, digest: u64, owner: u32) -> bool {
        if let Some(prior) = self.owner.get_mut(&digest) {
            if *prior != Some(owner) {
                *prior = None;
                return true;
            }
        } else if self.owner.len() < MAX_SEEN {
            self.owner.insert(digest, Some(owner));
        } else {
            self.incomplete = true;
        }
        false
    }
}

/// What one [`Watched::read`] found.
#[derive(Debug, Default)]
pub(super) struct Report {
    /// Turns to publish, in order.
    pub(super) batch: TurnBatch,
    /// The file was rewritten: it is read again from its first line.
    pub(super) rewritten: bool,
    /// The line handled last was found again in the rewritten file: its index.
    pub(super) found_again: Option<u64>,
    /// The rewritten file's EOF is the new baseline.
    pub(super) rebaselined: bool,
    /// The pass stopped at this error (what came before it is reported).
    pub(super) error: Option<std::io::ErrorKind>,
    /// A budget ran out before the end of the file: another pass is due at once.
    pub(super) more: bool,
    /// Lines skipped for exceeding the line budget in this pass.
    pub(super) oversized: u32,
}

/// A turn end found in a line (or by quiet), before it is known to be new.
enum Event {
    Ended {
        reply: Option<TurnReply>,
        evidence: AmbientEvidence,
    },
    SuppressedEnd,
    Unrecognised,
}

/// A reported end can move away from its predecessor when a writer inserts metadata.
/// Keep its line and a digest of its bounded parser result, without retaining reply text.
#[derive(Clone, Copy)]
struct Anchor {
    line: LineKey,
    // The original marker remains here when only its predecessor has been placed.
    completion: Option<(LineKey, u64)>,
}

impl Anchor {
    fn line(line: LineKey) -> Self {
        Self {
            line,
            completion: None,
        }
    }

    fn placed(mut self, line: LineKey, how: Match) -> Self {
        self.line = line;
        if !matches!(how, Match::Before | Match::PositionedBefore) {
            if let Some((marker, _)) = &mut self.completion {
                *marker = line;
            }
        }
        self
    }

    /// When placed at the predecessor, consume the first subsequent end as history
    /// only if it reproduces the displaced marker and its reply. An intervening turn
    /// boundary retires this possibility, even when the next turn has identical text.
    fn replayed(&mut self, key: LineKey, event: &Event, limit: usize) -> bool {
        let Some((marker, digest)) = self.completion else {
            return false;
        };
        if self.line == marker || matches!(event, Event::Unrecognised) {
            return false;
        }
        if key.same_text(&marker) && event.reply_digest(limit) == Some(digest) {
            self.line = key;
            self.completion = Some((key, digest));
            true
        } else {
            self.completion = None;
            false
        }
    }
}

impl Event {
    fn reply_digest(&self, limit: usize) -> Option<u64> {
        let Self::Ended { reply, .. } = self else {
            return None;
        };
        let mut hash = DefaultHasher::new();
        reply
            .as_ref()
            .map(|r| {
                let bytes = r.text.as_bytes();
                (
                    &bytes[..bytes.len().min(limit)],
                    r.truncated || bytes.len() > limit,
                )
            })
            .hash(&mut hash);
        Some(hash.finish())
    }
}

/// The suffix after a possible rewrite anchor. Status-only records
/// coalesce, and discarding this suffix releases its reply reservations.
#[derive(Default)]
struct Held {
    batch: TurnBatch,
    anchor: Option<Anchor>,
    unrecognised: Option<bool>,
}

impl Held {
    fn push(&mut self, key: LineKey, event: Event, capacity: &Capacity) {
        if let Some(status) = apply(event, key, &mut self.anchor, &mut self.batch, capacity) {
            self.unrecognised = Some(status);
        }
    }

    fn commit(self, anchor: &mut Option<Anchor>, unrecognised: &mut bool, batch: &mut TurnBatch) {
        if let Some(status) = self.unrecognised {
            *unrecognised = status;
        }
        if self.anchor.is_some() {
            *anchor = self.anchor;
        }
        batch.append(self.batch);
    }
}

/// One transcript being read.
pub(super) struct Watched {
    pub(super) path: PathBuf,
    /// Tells this transcript's lines in [`Seen`].
    id: u32,
    tail: Tail,
    parser: Box<dyn AmbientParser>,
    capacity: Capacity,
    /// The line handled last: the last turn end read, else the last line there was when
    /// reading began.
    anchor: Option<Anchor>,
    /// Reading a rewritten file again, until the anchor is placed in it.
    reread: Option<Reread>,
    /// A record of a known kind lacked its fields since the last turn end.
    pub(super) unrecognised: bool,
    /// The file was read from its start and has not diverged from possible copied history.
    may_copy: bool,
    /// Hashes each record in order. Absent when the baseline scan skipped the start.
    prefix: Option<DefaultHasher>,
    /// The line to which a quiet or next-record end belongs, skipping unchanged metadata.
    inference_line: Option<LastLine>,
    /// When a read last found the file grown (or rewritten); when it was opened before.
    grew_at: Instant,
    /// The question the file's own lines show asked (feature `questions`).
    #[cfg(feature = "questions")]
    asking: super::question::Asking,
}

/// A line read: where it is, its key, whether it was copied history.
#[derive(Clone, Copy)]
struct LastLine {
    index: u64,
    key: LineKey,
    copied: bool,
}

/// A rewritten file read again.
struct Reread {
    read_before: u64,
    last_size: Option<u64>,
    found: Option<(Anchor, u64, Match)>,
    held: Held,
}

/// How the anchor was found again; a later variant outranks an earlier one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Match {
    /// The line's text only (the line before it changed).
    Text,
    /// The line before it: the anchor itself was dropped.
    Before,
    /// The line and the one before it.
    Exact,
    /// The predecessor at its original position.
    PositionedBefore,
    /// The same pair with position evidence.
    PositionedExact,
}

impl Reread {
    fn new(read_before: u64) -> Self {
        Self {
            read_before,
            last_size: None,
            found: None,
            held: Held::default(),
        }
    }

    /// One line of the rewritten file. A match never gives way to one of a lower rank.
    fn see(
        &mut self,
        anchor: Anchor,
        (index, key): (u64, LineKey),
        event: Option<Event>,
        capacity: &Capacity,
    ) {
        if event
            .as_ref()
            .is_some_and(|event| self.replayed((index, key), event, capacity))
        {
            return;
        }
        let matched = if key.same_occurrence(anchor.line) {
            Some(Match::PositionedExact)
        } else if key.at_predecessor(anchor.line) {
            Some(Match::PositionedBefore)
        } else if key.pair() == anchor.line.pair() {
            Some(Match::Exact)
        } else if key.preceded(&anchor.line) {
            Some(Match::Before)
        } else if key.same_text(&anchor.line) {
            Some(Match::Text)
        } else {
            None
        };
        let matched = matched.filter(|how| self.found.is_none_or(|(.., best)| *how >= best));
        if let Some(how) = matched {
            // This line and what came before it were handled already.
            self.found = Some((anchor.placed(key, how), index, how));
            self.held = Held::default();
        } else if let (Some(_), Some(event)) = (self.found, event) {
            self.held.push(key, event, capacity);
        }
    }

    /// Upgrade a predecessor match when the parser identifies its displaced completion.
    fn replayed(
        &mut self,
        (index, key): (u64, LineKey),
        event: &Event,
        capacity: &Capacity,
    ) -> bool {
        let Some((found, at, how)) = &mut self.found else {
            return false;
        };
        if index < *at || !found.replayed(key, event, capacity.limits.reply_bytes) {
            return false;
        }
        *at = index;
        *how = if *how == Match::PositionedBefore {
            Match::PositionedExact
        } else {
            Match::Exact
        };
        self.held = Held::default();
        true
    }

    /// A next-record end belongs to its earlier line. It can confirm the anchor,
    /// end the turn at the predecessor, or add a new turn to the suffix.
    fn place(&mut self, line: (u64, LineKey), event: Option<Event>, capacity: &Capacity) {
        if let (Some((_, at, _)), Some(event)) = (self.found, event) {
            if !self.replayed(line, &event, capacity) && line.0 > at {
                self.held.push(line.1, event, capacity);
            }
        }
    }
}

impl Watched {
    /// `may_copy`: `tail` is at the file's start.
    fn new(
        id: u32,
        path: PathBuf,
        tail: Tail,
        parser: Box<dyn AmbientParser>,
        capacity: Capacity,
        may_copy: bool,
    ) -> Self {
        Self {
            path,
            id,
            anchor: tail.last_key().map(Anchor::line),
            prefix: (!tail.skipped_baseline_prefix()).then(DefaultHasher::new),
            tail,
            parser,
            capacity,
            reread: None,
            unrecognised: false,
            may_copy,
            inference_line: None,
            grew_at: Instant::now(),
            #[cfg(feature = "questions")]
            asking: Default::default(),
        }
    }

    /// The question this file's own lines show asked, as of the last read.
    #[cfg(feature = "questions")]
    pub(super) fn asked(&self) -> Option<std::sync::Arc<crate::agent::questions::PendingQuestion>> {
        self.asking.asked()
    }

    /// The last read ended on a complete line past the baseline, with no rewrite being
    /// placed: what the file's lines show is its current state.
    pub(super) fn settled(&self) -> bool {
        self.reread.is_none()
            && self.tail.past_baseline()
            && self.tail.offset() == self.tail.complete()
    }

    /// On relaunch, leave files created from the replacement's launch time to its
    /// reader. Without a birth time, only the file previously read here is eligible.
    pub(super) fn final_read(
        &self,
        replacement: impl FnOnce() -> Option<SystemTime>,
    ) -> std::io::Result<Option<FinalRead>> {
        match super::paths::open_regular(&self.path) {
            Ok((file, _)) => {
                if let Some(at) = replacement() {
                    let belongs_to_old = match file.metadata()?.created() {
                        Ok(born) => born < at,
                        Err(_) => self
                            .tail
                            .last_file_id()
                            .is_some_and(|id| super::paths::file_id(&file) == Some(id)),
                    };
                    if !belongs_to_old {
                        return Ok(None);
                    }
                }
                FinalRead::capture(file).map(Some)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !self.tail.observed => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn budget(&self) -> Budget {
        Budget {
            bytes: self.capacity.limits.read_bytes,
            records: self.capacity.limits.records,
            line_bytes: self.capacity.limits.line_bytes,
        }
    }

    /// One bounded pass over what was written since the last one, reporting the turns it
    /// ended; the lines read go to `seen`.
    #[cfg(test)]
    pub(super) fn read(&mut self, source: &dyn AmbientSource, seen: &mut Seen) -> Report {
        self.read_through(source, seen, None, |_| Ok(true))
    }

    /// With a departure fence, appends beyond that byte offset belong to a later read.
    pub(super) fn read_through(
        &mut self,
        source: &dyn AmbientSource,
        seen: &mut Seen,
        final_read: Option<&FinalRead>,
        accepts: impl FnOnce(&std::fs::File) -> std::io::Result<bool>,
    ) -> Report {
        let mut report = Report::default();
        let read_to = self.tail.offset();
        let budget = self.budget();
        let Self {
            id,
            tail,
            parser,
            capacity,
            anchor,
            reread,
            unrecognised,
            may_copy,
            prefix,
            inference_line,
            grew_at,
            #[cfg(feature = "questions")]
            asking,
            ..
        } = self;
        let reply_bytes = capacity.limits.reply_bytes;
        seen.incomplete |= tail.skipped_baseline_prefix();
        let mut on_record = |feed: Feed<'_>| {
            let mut record_prefix = |key: LineKey, bytes: Option<&[u8]>| {
                let Some(prefix) = prefix.as_mut() else {
                    return false;
                };
                if key.pair().0.is_none()
                    && bytes.is_some_and(|line| source.is_session_header(line))
                {
                    return false;
                }
                key.hash.hash(prefix);
                let elsewhere = seen.record(prefix.finish(), *id);
                *may_copy &= elsewhere;
                *may_copy
            };
            let (line, record, copied) = match feed {
                Feed::Rewritten(rewrite) => {
                    *parser = source.parser();
                    report.rewritten = true;
                    *inference_line = None;
                    #[cfg(feature = "questions")]
                    {
                        asking.clear();
                        asking.replaying = matches!(rewrite, Rewrite::Replay { .. });
                    }
                    let (read, unfinished_baseline) = match rewrite {
                        Rewrite::Replay {
                            read,
                            unfinished_baseline,
                        } => (read, unfinished_baseline),
                        Rewrite::Rebaseline { skipped_prefix } => {
                            *prefix = (!skipped_prefix).then(DefaultHasher::new);
                            *may_copy = false;
                            *anchor = None;
                            *reread = None;
                            report.rebaselined = true;
                            return;
                        }
                    };
                    *prefix = Some(DefaultHasher::new());
                    if unfinished_baseline {
                        // An earlier baseline line cannot identify the end of history.
                        *anchor = None;
                    } else if let Some(anchor) = anchor {
                        if let Some((marker, _)) = anchor.completion {
                            // Another rewrite can restore a marker still sought after its predecessor.
                            anchor.line = marker;
                        }
                    }
                    match reread {
                        // Rewritten again before the anchor was placed: still looked for.
                        Some(r) => {
                            r.read_before = r.read_before.max(read);
                            r.last_size = None;
                            r.found = None;
                            r.held = Held::default();
                        }
                        None if unfinished_baseline || anchor.is_some() => {
                            *reread = Some(Reread::new(read))
                        }
                        None => {}
                    }
                    return;
                }
                Feed::Baseline { key, bytes } => {
                    record_prefix(key, bytes);
                    *may_copy = false;
                    *anchor = Some(Anchor::line(key));
                    return;
                }
                Feed::Line { index, key, bytes } => {
                    let copied = record_prefix(key, Some(bytes));
                    let record = parser.record(bytes, reply_bytes);
                    // A question counts on the file's own lines only.
                    #[cfg(feature = "questions")]
                    asking.line(&mut **parser, bytes, reread.is_none() && !copied);
                    ((index, key), record, copied)
                }
                Feed::Oversized { index, key } => {
                    // The skipped record may have ended a turn or invalidated its text.
                    *parser = source.parser();
                    report.oversized = report.oversized.saturating_add(1);
                    #[cfg(feature = "questions")]
                    asking.clear();
                    ((index, key), None, record_prefix(key, None))
                }
            };
            // Metadata can follow the candidate without changing its origin.
            let unchanged = matches!(record, Some(AmbientRecord::Unchanged));
            let (at, at_copied, event) = match record {
                Some(AmbientRecord::EndedBefore(reply)) => match *inference_line {
                    Some(before) => (
                        (before.index, before.key),
                        before.copied,
                        Some(Event::Ended {
                            reply,
                            evidence: AmbientEvidence::InferredFromNextRecord,
                        }),
                    ),
                    None => (line, copied, None),
                },
                Some(AmbientRecord::Ended(reply)) => (
                    line,
                    copied,
                    Some(Event::Ended {
                        reply,
                        evidence: AmbientEvidence::ExplicitEnd,
                    }),
                ),
                Some(AmbientRecord::SuppressedEnd) => (line, copied, Some(Event::SuppressedEnd)),
                Some(AmbientRecord::Unrecognised) => (line, copied, Some(Event::Unrecognised)),
                Some(AmbientRecord::Unchanged) | None => (line, copied, None),
            };
            if unchanged {
                // Copied metadata also establishes that earlier records were history.
                if let Some(origin) = inference_line {
                    origin.copied |= copied;
                }
            } else {
                *inference_line = Some(LastLine {
                    index: line.0,
                    key: line.1,
                    copied,
                });
            }
            // A next-record end still belongs to its original line during a rewrite.
            let event = event.filter(|_| !at_copied);
            match (reread.as_mut(), *anchor) {
                (Some(r), Some(anchor)) if copied => {
                    r.held = Held::default();
                    r.see(anchor, line, None, capacity);
                }
                (Some(_), None) => {} // No reliable anchor: rebaseline at EOF.
                (Some(r), Some(anchor)) if at == line => r.see(anchor, line, event, capacity),
                (Some(r), Some(anchor)) => {
                    r.place(at, event, capacity);
                    r.see(anchor, line, None, capacity);
                }
                // A rewrite before the fork's first own turn must still place its anchor.
                _ if copied => {
                    *anchor = Some(Anchor::line(line.1));
                }
                _ => {
                    let Some(event) = event else { return };
                    if let Some(anchor) = anchor {
                        // A replay can reach EOF before the displaced marker arrives.
                        if anchor.replayed(at.1, &event, reply_bytes)
                            || (at != line && anchor.line == at.1)
                        {
                            return;
                        }
                    }
                    if let Some(status) = apply(event, at.1, anchor, &mut report.batch, capacity) {
                        *unrecognised = status;
                    }
                }
            }
        };
        let result = tail.read_checked(&budget, final_read, accepts, &mut on_record);
        if report.rewritten || tail.offset() != read_to {
            *grew_at = Instant::now();
        }
        match result {
            Err(e) => {
                // A re-read goes on with the next pass.
                report.error = Some(e.kind());
                return report;
            }
            Ok(more) => report.more = more,
        }
        if !report.more {
            self.settle(source, &mut report, final_read.is_some());
            #[cfg(feature = "questions")]
            {
                self.asking.replaying = false;
            }
        }
        report
    }

    /// After a pass that reached the file's end: once the file has not grown for `window`
    /// — or at once when `force`d (it cannot grow any more) — asks the parser what that
    /// ended, and adds the turn to `report` (see the module docs).
    pub(super) fn quiet(&mut self, window: Duration, force: bool, report: &mut Report) {
        let waiting = !(force || self.grew_at.elapsed() >= window);
        if waiting || report.error.is_some() || report.more || !self.settled() {
            return;
        }
        let Some(record) = self.parser.quiet(self.capacity.limits.reply_bytes) else {
            return;
        };
        let evidence = if force {
            AmbientEvidence::InferredAtTranscriptEnd
        } else {
            AmbientEvidence::InferredAfterQuiet
        };
        let event = match record {
            AmbientRecord::Ended(reply) | AmbientRecord::EndedBefore(reply) => {
                Event::Ended { reply, evidence }
            }
            AmbientRecord::SuppressedEnd => Event::SuppressedEnd,
            AmbientRecord::Unrecognised => Event::Unrecognised,
            AmbientRecord::Unchanged => return,
        };
        // Quiet belongs to the candidate, including across intervening metadata.
        let Some(last) = self.inference_line else {
            return;
        };
        if last.copied
            || self.anchor.as_mut().is_some_and(|anchor| {
                anchor.replayed(last.key, &event, self.capacity.limits.reply_bytes)
                    || anchor.line == last.key
            })
        {
            return;
        }
        if matches!(event, Event::Ended { .. } | Event::SuppressedEnd) {
            self.may_copy = false;
        }
        if let Some(status) = apply(
            event,
            last.key,
            &mut self.anchor,
            &mut report.batch,
            &self.capacity,
        ) {
            self.unrecognised = status;
        }
    }

    /// After a pass that reached the file's end: places the anchor of a re-read when it
    /// can (see the module docs).
    fn settle(&mut self, source: &dyn AmbientSource, report: &mut Report, final_read: bool) {
        let Some(r) = self.reread.as_mut() else {
            return;
        };
        let size = self.tail.offset();
        // Shorter than before, and empty or grown since the last read: maybe part-way.
        let in_progress = size < r.read_before && (size == 0 || r.last_size != Some(size));
        let done = match r.found {
            Some((.., Match::Exact | Match::PositionedExact)) => true,
            _ => !in_progress,
        };
        if !done && !final_read {
            r.last_size = Some(size);
            return;
        }
        let Some(r) = self.reread.take() else {
            return;
        };
        match r.found {
            Some((at, index, _)) => {
                if self.inference_line.is_some_and(|line| line.index < index) {
                    self.inference_line = None;
                }
                self.anchor = Some(at);
                report.found_again = Some(index);
                if r.held.anchor.is_some() {
                    self.may_copy = false;
                }
                r.held
                    .commit(&mut self.anchor, &mut self.unrecognised, &mut report.batch);
            }
            None => {
                // Every complete record replayed so far is behind the new baseline.
                self.parser = source.parser();
                self.inference_line = None;
                self.anchor = self.tail.last_key().map(Anchor::line);
                report.rebaselined = true;
            }
        }
    }
}

/// Apply a turn event and return its change to the unrecognised status, if any.
fn apply(
    event: Event,
    key: LineKey,
    anchor: &mut Option<Anchor>,
    batch: &mut TurnBatch,
    capacity: &Capacity,
) -> Option<bool> {
    let completion = event
        .reply_digest(capacity.limits.reply_bytes)
        .map(|hash| (key, hash));
    match event {
        Event::Ended { reply, evidence } => {
            *anchor = Some(Anchor {
                line: key,
                completion,
            });
            batch.push(
                AmbientTurn {
                    reply: reply.and_then(|reply| bounded(reply, capacity.limits.reply_bytes)),
                    evidence,
                },
                capacity,
            );
            Some(false)
        }
        Event::SuppressedEnd => {
            *anchor = Some(Anchor::line(key));
            None
        }
        Event::Unrecognised => Some(true),
    }
}

/// The reply as the host gets it: cut to `limit` bytes at a UTF-8 boundary (flagged
/// truncated); `None` when blank and complete.
pub(super) fn bounded(mut reply: TurnReply, limit: usize) -> Option<TurnReply> {
    if reply.text.len() > limit {
        let mut cut = TurnReply::default();
        cut.push(&reply.text, limit);
        reply = cut;
    }
    (reply.truncated || !reply.text.trim().is_empty()).then_some(reply)
}

/// The transcripts located in one agent launch, and where to read one not kept.
pub(super) struct Files {
    /// Files created at or after this time are new.
    since: SystemTime,
    next_id: u32,
    /// The most recently located last.
    watched: Vec<Watched>,
    /// Where a file not in `watched` is read from once located: its size when its
    /// directory was listed (it existed before `since`), or where it was left when it
    /// dropped out of `watched`.
    known: HashMap<PathBuf, SavedOffset>,
    /// An eviction could not save its offset. An unknown file may have been read before.
    lost_offset: bool,
    /// Directory entries left to examine in this launch, including non-files and errors.
    entries_left: usize,
    /// The directories listed.
    listed: HashSet<PathBuf>,
}

impl Files {
    pub(super) fn new(since: SystemTime) -> Self {
        Self {
            since,
            next_id: 0,
            watched: Vec::new(),
            known: HashMap::new(),
            lost_offset: false,
            entries_left: MAX_LISTED,
            listed: HashSet::new(),
        }
    }

    /// The transcript at `path` when it is kept.
    pub(super) fn get_mut(&mut self, path: &Path) -> Option<&mut Watched> {
        self.watched.iter_mut().find(|w| w.path == path)
    }

    /// The transcript at `path`, now the most recently located: the one kept, else opened
    /// (see the module docs). `Err`: it cannot be read.
    pub(super) fn watch(
        &mut self,
        path: &Path,
        source: &dyn AmbientSource,
        capacity: Capacity,
        seen: &Seen,
    ) -> std::io::Result<&mut Watched> {
        let at = match self.watched.iter().position(|w| w.path == path) {
            Some(at) => at,
            None => {
                let watched = self.open(path, source, capacity, seen)?;
                if self.watched.len() >= MAX_WATCHED {
                    let gone = self.watched.remove(0);
                    if self.known.len() < MAX_LISTED {
                        self.known.insert(gone.path, gone.tail.into_saved_offset());
                    } else {
                        self.lost_offset = true;
                    }
                }
                self.watched.push(watched);
                self.watched.len() - 1
            }
        };
        self.watched[at..].rotate_left(1);
        let last = self.watched.len() - 1;
        Ok(&mut self.watched[last])
    }

    fn open(
        &mut self,
        path: &Path,
        source: &dyn AmbientSource,
        capacity: Capacity,
        seen: &Seen,
    ) -> std::io::Result<Watched> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            self.list(dir);
        }
        let meta = match std::fs::metadata(path) {
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let baseline = match (&meta, self.known.get(path)) {
            // Missing: everything it will hold is new.
            (None, _) => 0,
            // A listed offset can fall inside a line that has since been completed.
            (Some(meta), Some(saved)) => {
                if saved.still_matches(path)? {
                    saved.at
                } else {
                    meta.len()
                }
            }
            (Some(meta), None)
                if !seen.incomplete && !self.lost_offset && created_since(meta, self.since) =>
            {
                0
            }
            (Some(meta), None) => meta.len(),
        };
        let mut tail = Tail::from(path.to_path_buf(), baseline)?;
        tail.observed = meta.is_some();
        self.known.remove(path);
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        Ok(Watched::new(
            id,
            path.to_path_buf(),
            tail,
            source.parser(),
            capacity,
            baseline == 0,
        ))
    }

    /// Baselines older files once per directory, within the launch's scan and offset limits.
    fn list(&mut self, dir: &Path) {
        if self.entries_left == 0
            || self.listed.len() >= MAX_LISTED
            || !self.listed.insert(dir.to_path_buf())
            || self.known.len() >= MAX_LISTED
        {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.take(self.entries_left) {
            self.entries_left -= 1;
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.is_file() && !created_since(&meta, self.since) {
                let Ok(saved) = SavedOffset::listed(&path, meta.len()) else {
                    continue;
                };
                self.known.entry(path).or_insert(saved);
                if self.known.len() == MAX_LISTED {
                    break;
                }
            }
        }
    }
}

/// The file was created at or after `since`: all it holds is new. Without a creation time
/// (some file systems) it counts as older: nothing written before is ever reported, at
/// the cost of a new file's first lines.
fn created_since(meta: &std::fs::Metadata, since: SystemTime) -> bool {
    meta.created().is_ok_and(|at| at >= since)
}

#[cfg(test)]
mod tests;
