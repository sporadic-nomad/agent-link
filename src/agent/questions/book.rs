use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{
    AnswerError, AnswerTicket, ClosedBy, MAX_FEED_QUESTIONS, PendingQuestion, QuestionFeed,
    QuestionId,
};
use crate::agent::{
    AgentTerminal,
    controller::{Owner, Runner, job_owner},
    state::{Controller, CtlState},
};
use crate::{Event, SessionId};

const POLL: Duration = Duration::from_millis(100);
const ABSENT_SETTLE: Duration = Duration::from_millis(300);
// Reserve a close as soon as an open is published. Includes the event in a blocked fanout.
const EVENT_BUDGET: usize = 8;

#[derive(Clone)]
pub(in crate::agent) enum Source {
    Screen,
    #[cfg(unix)]
    Transcript(Arc<crate::agent::turns::TurnReceiver>),
    /// A best-effort ambient attachment's transcript, corroborated on screen like
    /// [`Transcript`](Self::Transcript).
    Ambient(crate::agent::turns::ambient::Questions),
    Feed(Arc<dyn QuestionFeed>),
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Screen => "Screen",
            #[cfg(unix)]
            Self::Transcript(_) => "Transcript",
            Self::Ambient(_) => "Ambient",
            Self::Feed(_) => "Feed",
        })
    }
}

impl Source {
    /// The question a transcript source shows asked now; `None` for the screen and feeds.
    pub(super) fn transcript_question(
        &self,
    ) -> Option<Result<Option<Arc<PendingQuestion>>, AnswerError>> {
        match self {
            #[cfg(unix)]
            Self::Transcript(reader) => Some(reader.pending_question()),
            Self::Ambient(source) => Some(source.pending_question()),
            Self::Screen | Self::Feed(_) => None,
        }
    }
}

pub(super) struct Open {
    pub(super) id: QuestionId,
    pub(super) pending: Arc<PendingQuestion>,
    pub(super) attempted: bool,
    pub(super) accepted: bool,
}

pub(in crate::agent) struct Book {
    session: SessionId,
    pub(super) source: Option<Source>,
    pub(super) epoch: u64,
    worker: bool,
    next: u64,
    baseline: bool,
    passed: Option<Arc<PendingQuestion>>,
    missing: Option<Instant>,
    pub(super) open: Option<Open>,
    pub(super) admission: Option<(usize, AnswerTicket)>,
    events: usize,
    feed_done: std::collections::VecDeque<String>,
}
impl Book {
    pub(in crate::agent) fn new(session: SessionId) -> Self {
        Self {
            session,
            source: None,
            epoch: 0,
            worker: false,
            next: 0,
            baseline: true,
            passed: None,
            missing: None,
            open: None,
            admission: None,
            events: 0,
            feed_done: Default::default(),
        }
    }
    pub(in crate::agent) fn event_published(&mut self) {
        self.events -= 1;
    }
    pub(in crate::agent) fn admitted_bytes(&self) -> Option<usize> {
        self.admission.as_ref().map(|(bytes, _)| *bytes)
    }
    pub(in crate::agent) fn take_ticket(&mut self) -> Option<AnswerTicket> {
        self.admission.take().map(|(_, t)| t)
    }

    fn close(&mut self, by: ClosedBy, observed: bool) -> Option<Event> {
        let open = self.open.take()?;
        if matches!(self.source, Some(Source::Feed(_)))
            && !self.feed_done.contains(&open.pending.id)
        {
            if self.feed_done.len() == 256 {
                self.feed_done.pop_front();
            }
            self.feed_done.push_back(open.pending.id.clone());
        }
        let by = if !observed {
            ClosedBy::Gone
        } else if open.accepted {
            ClosedBy::Host
        } else if open.attempted {
            ClosedBy::Gone
        } else {
            by
        };
        Some(Event::QuestionClosed {
            id: self.session,
            qid: open.id,
            by,
        })
    }
}

pub(super) enum Seen {
    Found(Arc<PendingQuestion>),
    Absent,
    Torn,
    Gone,
}

impl CtlState {
    /// Provider credentials and transcript authority are scoped to the launch that
    /// supplied them. Release host-owned objects after the controller lock is released.
    pub(in crate::agent) fn invalidate_questions(&mut self) {
        self.question_gone();
        if !matches!(self.questions.source, Some(Source::Screen)) {
            if let Some(source) = self.questions.source.take() {
                self.actions
                    .push(crate::agent::state::Action::ReleaseQuestions(source));
            }
            self.questions.feed_done.clear();
        }
    }

    fn question_gone(&mut self) {
        self.questions.epoch = self.questions.epoch.wrapping_add(1);
        self.questions.baseline = true;
        self.questions.passed = None;
        self.questions.missing = None;
        self.close_question(ClosedBy::Gone, false);
    }

    fn close_question(&mut self, by: ClosedBy, observed: bool) {
        if let (Some(open), Some((_, ticket))) = (&self.questions.open, &self.questions.admission) {
            if open.id == ticket.id {
                self.actions.push(crate::agent::state::Action::EndAnswer(
                    ticket.clone(),
                    if open.attempted {
                        AnswerError::Unconfirmed
                    } else {
                        AnswerError::NotOpen
                    },
                ));
            }
        }
        if let Some(event) = self.questions.close(by, observed) {
            self.publish(event);
        }
    }

    fn question_look(&mut self, seen: Seen, now: Instant) {
        if matches!(seen, Seen::Gone) {
            if self.questions.open.is_some()
                || !matches!(self.questions.source, Some(Source::Screen))
            {
                self.invalidate_questions();
            }
            return;
        }
        if matches!(seen, Seen::Torn) {
            self.questions.missing = None;
            return;
        }
        if self.questions.baseline {
            self.questions.baseline = false;
            // An already drawn screen may be historical. A provider ID comes from a
            // current server snapshot or a transcript record read after attachment.
            if matches!(self.questions.source, Some(Source::Screen)) {
                if let Seen::Found(pending) = &seen {
                    self.questions.passed = Some(pending.clone());
                }
                return;
            }
        }
        match seen {
            Seen::Absent => {
                let since = self.questions.missing.get_or_insert(now);
                if now.duration_since(*since) < ABSENT_SETTLE {
                    return;
                }
                self.questions.passed = None;
                let by = if matches!(self.questions.source, Some(Source::Feed(_))) {
                    ClosedBy::Gone
                } else {
                    ClosedBy::Desk
                };
                self.close_question(by, true);
            }
            Seen::Found(question) => {
                self.questions.missing = None;
                if self.questions.passed.as_deref() == Some(&question)
                    || self
                        .questions
                        .open
                        .as_ref()
                        .is_some_and(|old| old.pending == question)
                {
                    return;
                }
                self.questions.passed = None;
                let by = if matches!(self.questions.source, Some(Source::Feed(_))) {
                    ClosedBy::Gone
                } else {
                    ClosedBy::Desk
                };
                self.close_question(by, true);
                if self.questions.events > EVENT_BUDGET - 2
                    || (matches!(self.questions.source, Some(Source::Feed(_)))
                        && self.questions.feed_done.contains(&question.id))
                {
                    return;
                }
                let Some(serial) = self.questions.next.checked_add(1) else {
                    return;
                };
                self.questions.next = serial;
                let id = QuestionId {
                    session: self.questions.session,
                    serial,
                };
                self.questions.events += 2;
                let event = Event::Question {
                    id: id.session,
                    qid: id,
                    questions: question.questions.clone().into(),
                };
                self.questions.open = Some(Open {
                    id,
                    pending: question,
                    attempted: false,
                    accepted: false,
                });
                self.publish(event);
            }
            Seen::Gone | Seen::Torn => {}
        }
    }
}

impl Source {
    pub(super) fn feed_question(
        feed: &dyn QuestionFeed,
        preferred: Option<&PendingQuestion>,
        done: &std::collections::VecDeque<String>,
    ) -> Result<Option<PendingQuestion>, AnswerError> {
        let mut all = feed.pending()?;
        if all.len() > MAX_FEED_QUESTIONS {
            return Err(AnswerError::InputTooLarge);
        }
        let mut ids = std::collections::HashSet::new();
        if all
            .iter()
            .any(|q| q.id.is_empty() || !q.valid() || !ids.insert(q.id.as_str()))
        {
            return Err(AnswerError::Changed);
        }
        // A changing provider order must not displace a pending first question.
        all.retain(|q| !done.contains(&q.id));
        let selected = preferred
            .and_then(|p| all.iter().position(|q| q.id == p.id))
            .unwrap_or(0);
        Ok(all.into_iter().nth(selected))
    }

    fn look(
        &self,
        runner: &mut Runner,
        preferred: Option<&PendingQuestion>,
        done: &std::collections::VecDeque<String>,
    ) -> (Seen, Option<crate::agent::controller::Observation>) {
        let front = runner.ctl.read_front();
        let owner = runner
            .ctl
            .read(|s| job_owner(front.shell, front.program, s.program));
        if owner == Owner::Shell || runner.ctl.term().try_wait().is_some() {
            return (Seen::Gone, None);
        }
        if owner == Owner::Unproven {
            return (Seen::Torn, None);
        }
        if let Self::Feed(feed) = self {
            return (
                match Self::feed_question(&**feed, preferred, done) {
                    Ok(Some(q)) => Seen::Found(Arc::new(q.clone())),
                    Ok(None) => Seen::Absent,
                    Err(AnswerError::NotOpen) => Seen::Gone,
                    Err(_) => Seen::Torn,
                },
                None,
            );
        }
        let pending = match self.transcript_question() {
            None => None,
            Some(Ok(Some(pending))) => Some(pending),
            Some(Ok(None)) => return (Seen::Absent, None),
            Some(Err(AnswerError::NotOpen)) => return (Seen::Gone, None),
            Some(Err(_)) => return (Seen::Torn, None),
        };
        let obs = runner.observation();
        let seen = runner.read_observation(obs.as_ref(), None, |p, screen, leftover| {
            let screen = leftover.fresh(screen);
            if screen.lines().all(|row| {
                row.chars().all(|c| {
                    c.is_whitespace() || matches!(c, '─' | '│' | '━' | '╭' | '╰' | '╮' | '╯')
                })
            }) {
                return Seen::Torn;
            }
            if !p.question_dialog(screen) {
                return Seen::Absent;
            }
            if let Some(q) = pending {
                if p.question_shown(screen, &q) {
                    Seen::Found(q)
                } else {
                    Seen::Torn
                }
            } else {
                p.question_on_screen(screen)
                    .filter(PendingQuestion::valid)
                    .map_or(Seen::Torn, |q| Seen::Found(Arc::new(q.clone())))
            }
        });
        (seen.ok().and_then(|r| r.value).unwrap_or(Seen::Torn), obs)
    }
}

fn work(ctl: Arc<Controller>) {
    let mut runner = Runner::new(ctl.clone());
    loop {
        let next = ctl.wait_for(|s| {
            if s.disposed
                || s.monitor_unavailable()
                || s.agent_state() == crate::agent::AgentState::Exited
            {
                return Some(None);
            }
            s.questions.source.clone().map(|source| {
                Some((
                    s.questions.epoch,
                    s.lifecycle_revision(),
                    source,
                    s.questions.open.as_ref().map(|o| o.pending.clone()),
                    s.questions.feed_done.clone(),
                ))
            })
        });
        let Some((epoch, lifecycle, source, preferred, done)) = next else {
            break;
        };
        let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            source.look(&mut runner, preferred.as_deref(), &done)
        }));
        let disabled = runner.profile_failed && !matches!(source, Source::Feed(_));
        if observation.is_err() || disabled {
            let released = ctl.update(|s, _| {
                if s.questions.epoch != epoch {
                    return None;
                }
                s.question_gone();
                s.questions.source.take()
            });
            drop(released);
            if observation.is_err() {
                log::warn!("question observer callback panicked");
            }
            runner = Runner::new(ctl.clone());
            continue;
        }
        let Ok((seen, obs)) = observation else {
            continue;
        };
        let front = ctl.read_front();
        let exited = ctl.term().try_wait().is_some();
        ctl.update(|s, now| {
            if s.cancel_reason().is_some()
                || s.questions.epoch != epoch
                || s.lifecycle_revision() != lifecycle
                || obs
                    .as_ref()
                    .is_some_and(|o| !runner.observation_current(o, s))
            {
                return;
            }
            if exited
                || matches!(seen, Seen::Gone)
                || job_owner(front.shell, front.program, s.program) != Owner::Agent
            {
                if exited || front.shell || matches!(seen, Seen::Gone) || s.questions.open.is_some()
                {
                    s.question_look(Seen::Gone, now);
                }
                return;
            }
            // Keep an answering job's question until it records the result. An answer
            // revalidates the live source independently before each mutation.
            if s.questions.open.as_ref().is_some_and(|o| {
                o.attempted
                    && s.questions
                        .admission
                        .as_ref()
                        .is_some_and(|(_, t)| t.id == o.id)
            }) {
                return;
            }
            s.question_look(seen, now);
        });
        ctl.wait_until(Instant::now() + POLL, |s| {
            (s.disposed || s.questions.epoch != epoch).then_some(())
        });
    }
    ctl.update(|s, _| s.invalidate_questions());
}

impl AgentTerminal {
    /// Start reporting newly visible, complete questions through ordinary PTY events.
    /// A question already present on the first observed screen establishes a baseline.
    /// Replacing the source invalidates outstanding question IDs.
    pub fn observe_questions(&self) -> crate::Result<()> {
        self.start_questions(Source::Screen)
    }

    /// Observe a host-authenticated feed for this PTY. The host and feed must bind
    /// the provider session to this terminal and keep that binding valid while enabled.
    /// The first complete list can contain a question already pending on the server.
    pub fn observe_feed_questions(&self, feed: Arc<dyn QuestionFeed>) -> crate::Result<()> {
        self.start_questions(Source::Feed(feed))
    }

    /// Corroborate fresh transcript tool calls against this same terminal's question screen.
    /// The caller retains access to final replies through the shared receiver. Gaps and
    /// closure invalidate pending calls. Requires a receiver from this terminal.
    pub fn observe_transcript_questions(
        &self,
        receiver: Arc<crate::agent::turns::TurnReceiver>,
    ) -> crate::Result<()> {
        #[cfg(unix)]
        {
            if !receiver.belongs_to(&self.ctl) {
                return Err(crate::Error::InvalidInput(
                    "turn receiver belongs to a different or closed terminal".into(),
                ));
            }
            self.start_questions(Source::Transcript(receiver))
        }
        #[cfg(not(unix))]
        {
            let _ = receiver;
            Err(crate::Error::Unsupported(
                "no supported transcript question source".into(),
            ))
        }
    }

    /// Corroborate the questions a best-effort ambient attachment's transcript shows asked
    /// against this terminal's screen, on every platform. The built-in Claude
    /// (`AskUserQuestion`) and Codex (Plan-mode `request_user_input`) sources read them;
    /// see [`AmbientParser::question`](crate::agent::turns::ambient::AmbientParser::question).
    /// Keep polling the receiver for replies: stopping or dropping it ends its authority to
    /// answer a question.
    ///
    /// The same rules as for [`observe_transcript_questions`](Self::observe_transcript_questions)
    /// apply to what is relayed and answered: a call must follow a prompt the attachment
    /// read (written after it started reading), the currently displayed question must agree with
    /// the recorded prompt and all visible option labels (checked again before every
    /// answer key), the agent's program must own the terminal, and a launch change, screen
    /// change or reattachment fences the source. **Weaker evidence:** the transcript is the
    /// user's own, of whatever CLI version is installed, with no writer exclusion or
    /// attribution; another process can write a matching record. The on-screen dialog is
    /// what makes a relayed question this terminal's, and the keys go only to that dialog.
    /// The question is the one the attachment's worker last read (every
    /// [`AmbientOptions::poll`](crate::agent::turns::ambient::AmbientOptions::poll)); no file
    /// is read again before an answer key. Like every non-screen source, this one ends with
    /// the agent's launch: observe again after a relaunch.
    ///
    /// Returns [`Error::InvalidInput`](crate::Error::InvalidInput) for a receiver of
    /// another terminal or one that closed, and [`Error::Unsupported`](crate::Error::Unsupported)
    /// for a feed attachment (OpenCode: use
    /// [`OpenCodeAmbientQuestions`](super::opencode::OpenCodeAmbientQuestions)) or a
    /// terminal without a profile.
    pub fn observe_ambient_transcript_questions(
        &self,
        receiver: &crate::agent::turns::ambient::AmbientReceiver,
    ) -> crate::Result<()> {
        let source = receiver.questions();
        if !source.belongs_to(&self.ctl) {
            return Err(crate::Error::InvalidInput(
                "ambient receiver belongs to a different or closed terminal".into(),
            ));
        }
        if !source.reads_transcript() {
            return Err(crate::Error::Unsupported(
                "ambient transcript questions require a transcript attachment".into(),
            ));
        }
        self.start_questions(Source::Ambient(source))
    }

    /// Corroborate a Codex Plan question using this terminal's isolated transcript
    /// and live screen. Keep polling the receiver for final replies. Dropping or
    /// stopping it ends its authority to answer a question.
    ///
    /// Returns an error for a different terminal, a closed reader or a provider
    /// without a supported transcript question format.
    pub fn observe_provider_questions(
        &self,
        receiver: &crate::agent::turns::TranscriptReceiver,
    ) -> crate::Result<()> {
        if receiver.source != crate::agent::turns::TranscriptSource::Codex {
            return Err(crate::Error::Unsupported(
                "isolated transcript questions require a Codex reader".into(),
            ));
        }
        self.observe_transcript_questions(receiver.receiver.clone())
    }

    fn start_questions(&self, source: Source) -> crate::Result<()> {
        if self.ctl.cfg.profile.is_none() {
            return Err(crate::Error::Unsupported(
                "question observation requires an agent profile".into(),
            ));
        }
        let _order = self.queueing.lock().unwrap_or_else(|e| e.into_inner());
        let epoch = self
            .ctl
            .read(|s| s.cancel_reason().is_none().then_some(s.questions.epoch))
            .ok_or(crate::Error::Closed)?;
        if !self.ctl.read(|s| s.questions.worker) {
            let ctl = self.ctl.clone();
            std::thread::Builder::new()
                .name(format!("agent-link-questions-{}", ctl.cfg.id))
                .spawn(move || work(ctl))
                .map_err(|e| crate::Error::Spawn(format!("spawn question observer: {e}")))?;
        }
        self.ctl.update(|s, _| s.questions.worker = true);
        drop(_order);
        let previous = self.ctl.update(|s, _| {
            if s.cancel_reason().is_some() || s.questions.epoch != epoch {
                return Err(source);
            }
            let same_feed = match (&s.questions.source, &source) {
                (Some(Source::Feed(old)), Source::Feed(new)) => Arc::ptr_eq(old, new),
                _ => false,
            };
            s.question_gone();
            if !same_feed {
                s.questions.feed_done.clear();
            }
            Ok(s.questions.source.replace(source))
        });
        previous.map(drop).map_err(|_| crate::Error::Closed)
    }

    /// Stop question observation and close its open ID. An outstanding callback may
    /// finish on its worker, but its stale result cannot publish a question.
    pub fn stop_questions(&self) {
        let previous = self.ctl.update(|s, _| {
            s.question_gone();
            s.questions.source.take()
        });
        drop(previous);
    }
}
