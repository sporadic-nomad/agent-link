use std::sync::Arc;
use std::time::{Duration, Instant};

use super::book::Source;
use super::{
    Answer, AnswerError, AnswerTicket, KeyStep, MAX_OPTIONS, MAX_QUESTIONS, MAX_STEPS,
    PendingQuestion, QuestionId,
};
use crate::agent::{
    AgentTerminal,
    controller::{Job, Owner, Runner, job_owner, paste_text},
    state::CtlState,
};

pub(in crate::agent) struct AnswerJob {
    ticket: AnswerTicket,
    pending: Arc<PendingQuestion>,
    answers: Box<[Answer]>,
    source: Source,
}

fn answer_size(answers: &[Answer], limit: usize) -> Result<usize, AnswerError> {
    if answers.len() > MAX_QUESTIONS {
        return Err(AnswerError::InputTooLarge);
    }
    let mut bytes = 0usize;
    for answer in answers {
        let n = match answer {
            Answer::Text(t) if t.len() > limit => return Err(AnswerError::InputTooLarge),
            Answer::Text(t) => t.len(),
            Answer::Choices(indices) if indices.len() > MAX_OPTIONS => {
                return Err(AnswerError::InputTooLarge);
            }
            Answer::Choices(indices) => indices.len() * std::mem::size_of::<usize>(),
            Answer::Choice(_) => std::mem::size_of::<usize>(),
        };
        bytes = bytes.checked_add(n).ok_or(AnswerError::InputTooLarge)?;
    }
    Ok(bytes)
}
pub(super) fn validate(pending: &PendingQuestion, answers: &[Answer]) -> Result<(), AnswerError> {
    if answers.len() != pending.questions.len() {
        return Err(AnswerError::Unsupported);
    }
    for (question, answer) in pending.questions.iter().zip(answers) {
        let valid = match answer {
            Answer::Choice(i) => *i < question.options.len(),
            Answer::Choices(choices) => {
                question.multi_select
                    && !choices.is_empty()
                    && choices.len() <= MAX_OPTIONS
                    && choices
                        .iter()
                        .enumerate()
                        .all(|(n, i)| *i < question.options.len() && !choices[..n].contains(i))
            }
            Answer::Text(text) => question.free_text && !text.trim().is_empty(),
        };
        if !valid {
            return Err(AnswerError::Unsupported);
        }
    }
    Ok(())
}

impl AgentTerminal {
    /// Queue an explicit answer to exactly this question appearance, in order with
    /// message delivery. The call performs no child or network I/O. Refusals resolve
    /// immediately and publish no event. Never retry an unconfirmed answer automatically.
    pub fn answer_question(&self, id: QuestionId, answers: Vec<Answer>) -> AnswerTicket {
        let ticket = AnswerTicket::new(id);
        let source = self.ctl.read(|s| {
            let open = s.questions.open.as_ref()?;
            (open.id == id && s.cancel_reason().is_none())
                .then_some(matches!(s.questions.source, Some(Source::Feed(_))))
        });
        let Some(feed) = source else {
            ticket.finish(Err(AnswerError::NotOpen));
            return ticket;
        };
        let bytes = match answer_size(&answers, self.ctl.cfg.limits.max_message_bytes) {
            Ok(bytes) if bytes <= self.ctl.cfg.limits.max_pending_bytes => bytes,
            _ => {
                ticket.finish(Err(AnswerError::InputTooLarge));
                return ticket;
            }
        };
        // Deep-clone only the bounded contents. Caller-owned Vec/String capacities
        // can be much larger than their visible lengths.
        let answers = answers
            .iter()
            .map(|a| match a {
                Answer::Text(text) if !feed => Answer::Text(paste_text(text).0),
                a => a.clone(),
            })
            .collect::<Box<[_]>>();
        let order = self.queueing.lock().unwrap_or_else(|e| e.into_inner());
        let result = self.ctl.update(|st, _| {
            let book = &st.questions;
            if st.cancel_reason().is_some() {
                return Err(AnswerError::NotOpen);
            }
            let open = book
                .open
                .as_ref()
                .filter(|o| o.id == id)
                .ok_or(AnswerError::NotOpen)?;
            let source = book.source.clone().ok_or(AnswerError::NotOpen)?;
            if book
                .admission
                .as_ref()
                .is_some_and(|(_, t)| t.try_result().is_none())
            {
                return Err(AnswerError::QueueFull);
            }
            if open.accepted {
                return Err(AnswerError::NotOpen);
            }
            if open.attempted {
                return Err(AnswerError::Unconfirmed);
            }
            if book.admission.is_some() || !st.answer_room(bytes) {
                return Err(AnswerError::QueueFull);
            }
            validate(&open.pending, &answers)?;
            let job = AnswerJob {
                ticket: ticket.clone(),
                pending: open.pending.clone(),
                answers,
                source,
            };
            st.questions.admission = Some((bytes, ticket.clone()));
            st.jobs.push_back(Job::Answer(job));
            Ok(())
        });
        drop(order);
        if let Err(err) = result {
            ticket.finish(Err(err));
        }
        ticket
    }
}

impl CtlState {
    fn answer_current(&self, id: QuestionId) -> bool {
        self.cancel_reason().is_none()
            && self.questions.open.as_ref().is_some_and(|o| o.id == id)
            && self
                .questions
                .admission
                .as_ref()
                .is_some_and(|(_, t)| t.id == id)
    }
}

impl Runner {
    pub(in crate::agent) fn run_answer(&mut self, job: AnswerJob) {
        let mut sent = false;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.dispatch_answer(&job, &mut sent)
        }))
        .unwrap_or(Err(AnswerError::Unconfirmed));
        let result = if sent && result.is_err() {
            Err(AnswerError::Unconfirmed)
        } else {
            result
        };
        let result = self.ctl.update(|st, _| {
            let current = st.answer_current(job.ticket.id);
            if let Some(open) = st.questions.open.as_mut().filter(|o| o.id == job.ticket.id) {
                open.accepted = result.is_ok();
                open.attempted = sent || result == Err(AnswerError::Unconfirmed) || result.is_ok();
            }
            if !current && result.is_ok() {
                Err(AnswerError::Unconfirmed)
            } else {
                result
            }
        });
        job.ticket.finish(result);
        self.ctl.update(|st, _| {
            st.questions.take_ticket();
        });
    }

    fn answer_owner(&self, job: &AnswerJob) -> Result<(), AnswerError> {
        if self.ctl.term().try_wait().is_some() {
            return Err(AnswerError::NotOpen);
        }
        let front = self.ctl.read_front();
        self.ctl.read(|st| {
            if st.answer_current(job.ticket.id)
                && job_owner(front.shell, front.program, st.program) == Owner::Agent
            {
                Ok(())
            } else {
                Err(AnswerError::NotOpen)
            }
        })
    }

    fn answer_authority(&self, job: &AnswerJob) -> Result<(), AnswerError> {
        self.answer_owner(job)?;
        match &job.source {
            Source::Screen => Ok(()),
            #[cfg(unix)]
            Source::Transcript(reader) => {
                let current = reader.pending_question()?.ok_or(AnswerError::NotOpen)?;
                if current == job.pending {
                    Ok(())
                } else {
                    Err(AnswerError::Changed)
                }
            }
            Source::Feed(feed) => {
                let current =
                    Source::feed_question(&**feed, Some(&job.pending), &Default::default())?
                        .ok_or(AnswerError::NotOpen)?;
                if current == *job.pending {
                    Ok(())
                } else {
                    Err(AnswerError::Changed)
                }
            }
        }
    }

    fn mark_attempt(
        &self,
        job: &AnswerJob,
        obs: Option<&crate::agent::controller::Observation>,
    ) -> Result<(), AnswerError> {
        self.ctl.update(|s, _| {
            if !s.answer_current(job.ticket.id) {
                return Err(AnswerError::NotOpen);
            }
            if obs.is_some_and(|obs| !self.observation_current(obs, s)) {
                return Err(AnswerError::Changed);
            }
            if let Some(open) = &mut s.questions.open {
                open.attempted = true;
            }
            Ok(())
        })
    }

    fn dispatch_answer(&mut self, job: &AnswerJob, sent: &mut bool) -> Result<(), AnswerError> {
        self.answer_authority(job)?;
        if let Source::Feed(feed) = &job.source {
            let authorized = std::cell::Cell::new(false);
            let result = feed.answer(&job.pending, &job.answers, &|| {
                let current =
                    self.answer_owner(job).is_ok() && self.mark_attempt(job, None).is_ok();
                authorized.set(current);
                current
            });
            return if result.is_ok() && !authorized.get() {
                Err(AnswerError::Unconfirmed)
            } else {
                result
            };
        }
        let timing = self.ctl.cfg.timing.clone();
        let start = Instant::now();
        let lull = self
            .wait_lull(
                crate::agent::settle::LullParams {
                    quiet: timing.pre_paste_quiet,
                    max: timing.pre_paste_max,
                    after: None,
                    require_fresh: false,
                    start,
                },
                None,
            )
            .map_err(|_| AnswerError::NotOpen)?;
        if matches!(lull, crate::agent::settle::Lull::Max) {
            return Err(AnswerError::Changed);
        }
        self.answer_authority(job)?;
        let read = self
            .profile_read(None, |profile, screen, leftover| {
                let fresh = leftover.fresh(screen);
                if !profile.question_dialog(fresh) || !profile.question_shown(fresh, &job.pending) {
                    return Err(AnswerError::Changed);
                }
                profile.answer_keys(fresh, &job.pending, &job.answers)
            })
            .map_err(|_| AnswerError::NotOpen)?;
        let steps = read.value.ok_or(AnswerError::Changed)??;
        let key_bytes = self.validate_steps(&steps)?;
        self.ctl.update(|s, _| {
            if !s.answer_current(job.ticket.id) {
                return Err(AnswerError::NotOpen);
            }
            let reserved = s.questions.admitted_bytes().ok_or(AnswerError::NotOpen)?;
            let total = reserved
                .checked_add(key_bytes)
                .ok_or(AnswerError::InputTooLarge)?;
            if !s.answer_room(total) {
                return Err(AnswerError::QueueFull);
            }
            if let Some((bytes, _)) = &mut s.questions.admission {
                *bytes = total;
            }
            Ok(())
        })?;
        let mut after = None;
        for (n, step) in steps.iter().enumerate() {
            let deadline = Instant::now() + timing.submit_verify_window;
            loop {
                self.answer_authority(job)?;
                let obs = self.observation().ok_or(AnswerError::Changed)?;
                let check = self
                    .read_observation(Some(&obs), None, |p, screen, leftover| {
                        let fresh = leftover.fresh(screen);
                        p.question_dialog(fresh)
                            && p.question_shown(fresh, &job.pending)
                            && step.guard.as_ref().is_none_or(|guard| guard(fresh))
                    })
                    .map_err(|_| AnswerError::NotOpen)?;
                if check.value == Some(true) && after.is_none_or(|serial| obs.serial > serial) {
                    self.answer_authority(job)?;
                    if self.mark_attempt(job, Some(&obs)).is_ok() {
                        self.ctl.term().write(&step.bytes).map_err(|e| match e {
                            crate::Error::QueueFull => AnswerError::QueueFull,
                            crate::Error::InputTooLarge { .. } => AnswerError::InputTooLarge,
                            _ => AnswerError::Io,
                        })?;
                        *sent = true;
                        after = Some(obs.serial);
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    return Err(AnswerError::Changed);
                }
                self.ctl
                    .sleep_until(
                        (Instant::now() + timing.submit_verify_poll.max(Duration::from_millis(1)))
                            .min(deadline),
                    )
                    .map_err(|_| AnswerError::NotOpen)?;
            }
            if n + 1 < steps.len() && !step.settle.is_zero() {
                self.ctl
                    .sleep_until(Instant::now() + step.settle)
                    .map_err(|_| AnswerError::NotOpen)?;
            }
        }
        Ok(())
    }

    fn validate_steps(&self, steps: &[KeyStep]) -> Result<usize, AnswerError> {
        if steps.is_empty() {
            return Err(AnswerError::Unsupported);
        }
        let max = self.ctl.term().max_write_bytes();
        let mut bytes = 0usize;
        if steps.len() > MAX_STEPS
            || steps.iter().any(|s| {
                bytes = bytes.saturating_add(s.bytes.len());
                s.bytes.is_empty()
                    || s.bytes.len() > max
                    || bytes > self.ctl.cfg.limits.max_pending_bytes
                    || s.settle > Duration::from_secs(5)
            })
        {
            return Err(AnswerError::InputTooLarge);
        }
        Ok(bytes)
    }
}
