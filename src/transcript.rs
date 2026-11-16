//! Bounded, synchronous presentation projection (`transcript` feature).
//!
//! Screen text is an incomplete heuristic view, never delivery, readiness, idle,
//! completion or resume evidence. Hosts supply fenced text and explicit segment
//! boundaries; no parser, formatting policy, callbacks or background work runs here.
//! Cloned snapshots/updates are host-owned memory. Debug redacts message content.

use crate::{
    SessionId,
    portable::{EventContext, EventKind, EventV1, TextUpdate},
};
use std::collections::VecDeque;

/// Retention limits, counting UTF-8 text and native origin strings, not allocator RSS.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum retained messages, including the active segment.
    pub messages: usize,
    /// Maximum total retained payload bytes, including active accumulation.
    pub bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            messages: 128,
            bytes: 1024 * 1024,
        }
    }
}
/// Invalid limits, exhausted local counters, or reversed source range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Both limits must be nonzero.
    #[error("transcript limits must be nonzero")]
    InvalidLimits,
    /// Local monotonic IDs/revisions exhausted; create a new projector.
    #[error("transcript counter exhausted")]
    Exhausted,
    /// A source range has its end before its start.
    #[error("invalid transcript source range")]
    InvalidRange,
    /// Duplicate or older sequence for a retained native item; state is unchanged.
    #[error("duplicate or stale native transcript mutation")]
    StaleNative,
}
/// Half-open raw byte span and inclusive output event sequence span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawSpan {
    /// Process-local PTY generation.
    pub generation: SessionId,
    /// First contributing raw byte offset.
    pub start: u64,
    /// Exclusive last contributing raw byte offset.
    pub end: u64,
    /// First contributing output event.
    pub first_seq: u64,
    /// Last contributing output event.
    pub last_seq: u64,
}
/// Source evidence; screen spans describe inputs, never exact text-to-byte mappings.
#[derive(Clone, PartialEq, Eq)]
pub enum Origin {
    /// Latest complete host-supplied screen view's fence; earlier revisions are replaced.
    Screen(RawSpan),
    /// Exact native origin of the latest mutation; native IDs stay optional.
    Native(EventContext),
}
/// Strength of identity/text interpretation, never a delivery confidence score.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    /// Whole screen text may include chrome, quoted prompts, wrapping and missing history.
    HeuristicScreen,
    /// Text associated using an explicit native item identity.
    NativeItem,
    /// Native text without item identity: distinct local observation only.
    ProjectionOnly,
}
/// Retained presentation message. Local IDs are process-local, never provider IDs.
#[derive(Clone, PartialEq, Eq)]
pub struct Message {
    /// Monotonic local identity, unchanged by replacement.
    pub id: u64,
    /// Monotonic projector update revision of this message.
    pub revision: u64,
    /// UTF-8 prefix retained under the byte budget.
    pub text: String,
    /// Latest source observation.
    pub origin: Origin,
    /// Explicit interpretation strength.
    pub confidence: Confidence,
    /// Some text was omitted; subsequent deltas cannot repair that loss.
    pub truncated: bool,
}
impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Message")
            .field("id", &self.id)
            .field("revision", &self.revision)
            .field("truncated", &self.truncated)
            .finish_non_exhaustive()
    }
}
/// Bounded result of one mutation. Evicted IDs are exact and oldest first.
#[derive(Clone, Debug)]
pub struct Update {
    /// Current global revision; transport adapters can detect missed updates with this.
    pub revision: u64,
    /// Changed message, absent for non-text input or an origin too large to retain.
    pub message: Option<Message>,
    /// Exact evicted local IDs, bounded by the configured message count.
    pub evicted: Vec<u64>,
    /// Input could not be retained because its native origin exceeded the byte budget.
    pub gap: bool,
}
/// Current bounded view, not a complete conversation or provider history.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Last update revision.
    pub revision: u64,
    /// Oldest-first retained messages.
    pub messages: Vec<Message>,
    /// Charged text and native identity bytes.
    pub retained_bytes: usize,
    /// Sticky indication of eviction, truncation or unretained input.
    pub incomplete: bool,
}
/// Single-owner projection; methods never block on I/O or consumers.
pub struct Projector {
    limits: Limits,
    messages: VecDeque<Message>,
    revision: u64,
    next_id: u64,
    screen_id: Option<u64>,
    incomplete: bool,
}
impl Projector {
    /// Validate budgets before allocating retained state.
    pub fn new(limits: Limits) -> Result<Self, Error> {
        if limits.messages == 0 || limits.bytes == 0 {
            return Err(Error::InvalidLimits);
        }
        Ok(Self {
            limits,
            messages: VecDeque::new(),
            revision: 0,
            next_id: 1,
            screen_id: None,
            incomplete: false,
        })
    }
    /// Start a distinct host-defined screen segment, even if its text is identical.
    /// Call after scrolling, resize, alternate-screen transitions or interruption when
    /// continuity is unknown. This does not assert a new provider message or turn.
    pub fn begin_segment(&mut self) {
        self.screen_id = None;
    }
    /// Copy the bounded current view. No historical revisions are retained internally.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            revision: self.revision,
            messages: self.messages.iter().cloned().collect(),
            retained_bytes: self.bytes(),
            incomplete: self.incomplete,
        }
    }
    /// Replace the current screen segment wholesale, preserving whitespace and Unicode.
    /// Repeated/quoted lines are never removed. Supply the view's actual output fence.
    /// Even an unchanged screen creates a revision; stability has no semantic meaning.
    pub fn screen(&mut self, text: &str, span: RawSpan) -> Result<Update, Error> {
        if span.end < span.start || span.last_seq < span.first_seq {
            return Err(Error::InvalidRange);
        }
        let id = self.screen_id.filter(|id| {
            self.messages.iter().any(|m| {
                m.id == *id
                    && matches!(&m.origin, Origin::Screen(old) if old.generation == span.generation)
            })
        });
        let update = self.apply(
            id,
            text,
            false,
            Origin::Screen(span),
            Confidence::HeuristicScreen,
        )?;
        self.screen_id = update.message.as_ref().map(|m| m.id);
        Ok(update)
    }
    /// Project native text. Only matching explicit item IDs within the same provider,
    /// generation, session and turn are merged. Missing item IDs create separate local
    /// observations, including deltas; no hidden grouping invents native identity.
    /// Duplicate/stale sequences for a retained native item return `Error::StaleNative`
    /// without mutation. Evicted/unknown items have no deduplication history: hosts
    /// must supply ordered, non-replayed input or explicitly record a gap.
    /// Non-text events are ignored and never become inferred lifecycle state.
    pub fn native(&mut self, event: &EventV1) -> Result<Update, Error> {
        let EventKind::Text { text, update, .. } = &event.kind else {
            return Ok(Update {
                revision: self.revision,
                message: None,
                evicted: Vec::new(),
                gap: false,
            });
        };
        let ctx = &event.context;
        if context_bytes(ctx) > self.limits.bytes {
            return self.record_gap();
        }
        let id = ctx.item_id.as_ref().and_then(|_| self.messages.iter().find(|m| matches!(&m.origin, Origin::Native(old) if old.provider == ctx.provider && old.generation == ctx.generation && old.session_id == ctx.session_id && old.turn_id == ctx.turn_id && old.item_id == ctx.item_id)).map(|m| m.id));
        if let Some(id) = id {
            if self.messages.iter().any(|m| {
                m.id == id && matches!(&m.origin, Origin::Native(old) if ctx.seq <= old.seq)
            }) {
                return Err(Error::StaleNative);
            }
        }
        self.apply(
            id,
            text,
            *update == TextUpdate::Delta,
            Origin::Native(ctx.clone()),
            if ctx.item_id.is_some() {
                Confidence::NativeItem
            } else {
                Confidence::ProjectionOnly
            },
        )
    }
    /// Record missing/discontinuous source input, clearing potentially stale text.
    /// Hosts call this on replay loss, interrupted decoding or unknown continuity.
    /// No provider or process outcome is implied.
    pub fn record_gap(&mut self) -> Result<Update, Error> {
        self.revision = self.revision.checked_add(1).ok_or(Error::Exhausted)?;
        let evicted = self.messages.iter().map(|m| m.id).collect();
        self.messages.clear();
        self.screen_id = None;
        self.incomplete = true;
        Ok(Update {
            revision: self.revision,
            message: None,
            evicted,
            gap: true,
        })
    }
    fn bytes(&self) -> usize {
        self.messages.iter().map(charge).sum()
    }
    fn apply(
        &mut self,
        id: Option<u64>,
        text: &str,
        append: bool,
        origin: Origin,
        confidence: Confidence,
    ) -> Result<Update, Error> {
        let revision = self.revision.checked_add(1).ok_or(Error::Exhausted)?;
        let next_id = if id.is_none() {
            self.next_id.checked_add(1).ok_or(Error::Exhausted)?
        } else {
            self.next_id
        };
        self.revision = revision;
        let overhead = origin_bytes(&origin);
        let position = id.and_then(|id| self.messages.iter().position(|m| m.id == id));
        let old = position.and_then(|i| self.messages.remove(i));
        let local_id = old.as_ref().map_or(self.next_id, |m| m.id);
        self.next_id = next_id;
        let budget = self.limits.bytes - overhead;
        let mut value = String::new();
        let mut truncated = false;
        if append {
            if let Some(old) = &old {
                truncated = push_prefix(&mut value, &old.text, budget) || old.truncated;
            }
        }
        if !truncated {
            truncated |= push_prefix(&mut value, text, budget);
        }
        let message = Message {
            id: local_id,
            revision,
            text: value,
            origin,
            confidence,
            truncated,
        };
        let mut evicted = Vec::new();
        while self.messages.len() >= self.limits.messages
            || self.bytes() > self.limits.bytes - charge(&message)
        {
            if let Some(old) = self.messages.pop_front() {
                evicted.push(old.id);
            } else {
                break;
            }
        }
        // Preserve chronological local IDs even when updating an earlier native item.
        let pos = self
            .messages
            .iter()
            .position(|m| m.id > local_id)
            .unwrap_or(self.messages.len());
        self.messages.insert(pos, message.clone());
        self.incomplete |= truncated || !evicted.is_empty();
        Ok(Update {
            revision,
            message: Some(message),
            evicted,
            gap: false,
        })
    }
}
fn origin_bytes(origin: &Origin) -> usize {
    match origin {
        Origin::Screen(_) => 0,
        Origin::Native(ctx) => context_bytes(ctx),
    }
}
fn charge(message: &Message) -> usize {
    message.text.len() + origin_bytes(&message.origin)
}
fn push_prefix(value: &mut String, text: &str, budget: usize) -> bool {
    let mut end = text.len().min(budget - value.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    value.push_str(&text[..end]);
    end < text.len()
}

fn context_bytes(ctx: &EventContext) -> usize {
    ctx.session_id
        .as_ref()
        .map_or(0, String::len)
        .saturating_add(ctx.turn_id.as_ref().map_or(0, String::len))
        .saturating_add(ctx.item_id.as_ref().map_or(0, String::len))
        .saturating_add(match &ctx.request_id {
            Some(crate::portable::RequestId::String(s)) => s.len(),
            _ => 0,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::{Provider, TextPhase};
    fn span() -> RawSpan {
        RawSpan {
            generation: SessionId(1),
            start: 0,
            end: 10,
            first_seq: 0,
            last_seq: 1,
        }
    }
    fn event(item: Option<&str>, text: &str, update: TextUpdate) -> EventV1 {
        EventV1 {
            context: EventContext {
                provider: Provider::Acp,
                generation: SessionId(2),
                seq: {
                    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                },
                session_id: None,
                turn_id: None,
                item_id: item.map(str::to_owned),
                request_id: None,
            },
            native_method: "text".into(),
            kind: EventKind::Text {
                text: text.into(),
                update,
                phase: TextPhase::Unknown,
            },
        }
    }
    #[test]
    fn whole_screen_revisions_preserve_repetition_quotes_and_wrapping() {
        let mut p = Projector::new(Limits::default()).unwrap();
        let text = "prompt\n回答\nquoted prompt\n回答";
        let first = p.screen(text, span()).unwrap().message.unwrap();
        let second = p.screen(text, span()).unwrap().message.unwrap();
        assert_eq!(first.id, second.id);
        assert!(second.revision > first.revision);
        assert_eq!(second.text, text);
        p.begin_segment();
        let third = p.screen(text, span()).unwrap().message.unwrap();
        assert_ne!(third.id, first.id);
        // A scrolled/resized/alternate-screen snapshot replaces, never line-deduplicates.
        let replacement = p
            .screen("回答\nwrap\nped", span())
            .unwrap()
            .message
            .unwrap();
        assert_eq!(replacement.id, third.id);
        assert_eq!(replacement.text, "回答\nwrap\nped");
        assert_eq!(replacement.confidence, Confidence::HeuristicScreen);
    }
    #[test]
    fn bounded_unicode_active_accumulation_and_explicit_loss() {
        let mut p = Projector::new(Limits {
            messages: 2,
            bytes: 5,
        })
        .unwrap();
        let m = p
            .native(&event(Some("a"), "ééé", TextUpdate::Delta))
            .unwrap()
            .message
            .unwrap();
        assert_eq!(m.text, "éé");
        assert!(m.truncated);
        let m = p
            .native(&event(Some("a"), "x", TextUpdate::Delta))
            .unwrap()
            .message
            .unwrap();
        assert_eq!(m.text, "éé");
        assert!(m.truncated);
        let m = p
            .native(&event(Some("a"), "ok", TextUpdate::Replace))
            .unwrap()
            .message
            .unwrap();
        assert_eq!(m.text, "ok");
        assert!(!m.truncated);
        p.begin_segment();
        let update = p.screen("12345", span()).unwrap();
        assert!(!update.evicted.is_empty());
        assert!(p.snapshot().retained_bytes <= 5);
        let gap = p.record_gap().unwrap();
        assert!(gap.gap);
        assert!(!gap.evicted.is_empty());
        assert!(p.snapshot().messages.is_empty());
        assert!(p.snapshot().incomplete);
    }
    #[test]
    fn native_identity_and_missing_identity_never_merge_by_text() {
        let mut p = Projector::new(Limits::default()).unwrap();
        let a = p
            .native(&event(Some("a"), "same", TextUpdate::Delta))
            .unwrap()
            .message
            .unwrap();
        let b = p
            .native(&event(Some("b"), "same", TextUpdate::Delta))
            .unwrap()
            .message
            .unwrap();
        assert_ne!(a.id, b.id);
        let a2 = p
            .native(&event(Some("a"), "!", TextUpdate::Delta))
            .unwrap()
            .message
            .unwrap();
        assert_eq!(a.id, a2.id);
        assert_eq!(a2.text, "same!");
        let unknown = p
            .native(&event(None, "same", TextUpdate::Delta))
            .unwrap()
            .message
            .unwrap();
        let unknown2 = p
            .native(&event(None, "same", TextUpdate::Delta))
            .unwrap()
            .message
            .unwrap();
        assert_ne!(unknown.id, unknown2.id);
        assert_eq!(unknown.confidence, Confidence::ProjectionOnly);
        let mut other = event(Some("a"), "new", TextUpdate::Delta);
        other.context.generation = SessionId(3);
        assert_ne!(a.id, p.native(&other).unwrap().message.unwrap().id);
    }
    #[test]
    fn item_budget_and_nontext_never_invent_lifecycle_state() {
        let mut p = Projector::new(Limits {
            messages: 2,
            bytes: 64,
        })
        .unwrap();
        let a = p
            .native(&event(None, "same", TextUpdate::Delta))
            .unwrap()
            .message
            .unwrap();
        p.native(&event(None, "same", TextUpdate::Delta)).unwrap();
        let update = p.native(&event(None, "same", TextUpdate::Delta)).unwrap();
        assert_eq!(update.evicted, vec![a.id]);
        assert_eq!(p.snapshot().messages.len(), 2);
        let revision = p.snapshot().revision;
        let mut completion = event(None, "", TextUpdate::Delta);
        completion.kind = EventKind::TurnCompleted {
            status: crate::portable::CompletionStatus::Completed,
        };
        let update = p.native(&completion).unwrap();
        assert_eq!(update.revision, revision);
        assert!(update.message.is_none());
        assert_eq!(p.snapshot().messages.len(), 2);
        p.begin_segment();
        let old = p.screen("screen", span()).unwrap().message.unwrap();
        let mut other = span();
        other.generation = SessionId(99);
        let new = p.screen("screen", other).unwrap().message.unwrap();
        assert_ne!(old.id, new.id);
    }
    #[test]
    fn consumer_reconstructs_middle_item_growth_and_stale_input_is_refused() {
        let mut p = Projector::new(Limits {
            messages: 3,
            bytes: 12,
        })
        .unwrap();
        let mut consumer = std::collections::BTreeMap::new();
        for item in ["a", "b", "c"] {
            let update = p
                .native(&event(Some(item), "xx", TextUpdate::Delta))
                .unwrap();
            for id in update.evicted {
                consumer.remove(&id);
            }
            if let Some(m) = update.message {
                consumer.insert(m.id, m);
            }
        }
        let growth = event(Some("b"), "12345678901", TextUpdate::Replace);
        let update = p.native(&growth).unwrap();
        assert_eq!(update.evicted, vec![1, 3]);
        for id in update.evicted {
            consumer.remove(&id);
        }
        if let Some(m) = update.message {
            consumer.insert(m.id, m);
        }
        assert_eq!(
            consumer.into_values().collect::<Vec<_>>(),
            p.snapshot().messages
        );
        let snapshot = p.snapshot();
        assert_eq!(p.native(&growth).unwrap_err(), Error::StaleNative);
        let mut stale = growth.clone();
        stale.context.seq -= 1;
        assert_eq!(p.native(&stale).unwrap_err(), Error::StaleNative);
        assert_eq!(snapshot.messages, p.snapshot().messages);
        assert_eq!(snapshot.revision, p.snapshot().revision);
    }
    #[test]
    fn oversized_origin_and_invalid_input_are_explicit() {
        assert!(
            Projector::new(Limits {
                messages: 0,
                bytes: 1
            })
            .is_err()
        );
        let mut p = Projector::new(Limits {
            messages: 1,
            bytes: 2,
        })
        .unwrap();
        p.screen("ok", span()).unwrap();
        let result = p
            .native(&event(Some("too big"), "x", TextUpdate::Delta))
            .unwrap();
        assert!(result.gap);
        assert!(result.message.is_none());
        assert!(p.snapshot().messages.is_empty());
        let mut bad = span();
        bad.start = 11;
        let rev = p.snapshot().revision;
        assert_eq!(p.screen("x", bad).unwrap_err(), Error::InvalidRange);
        assert_eq!(p.snapshot().revision, rev);
    }
}
