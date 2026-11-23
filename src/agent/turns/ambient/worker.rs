//! One ambient worker per attachment: samples the terminal, re-locates the transcript,
//! reads it with bounded passes (or polls a feed within its time budget) and publishes to
//! the receiver's queue. Source, parser and feed code runs here only, never on a
//! controller thread or under a lock.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crossbeam_channel::{Receiver, select_biased};

use super::feed::{self, Driver, FeedStatus};
use super::watch::{Files, Report, Seen};
use super::{
    AmbientClosed, AmbientContext, AmbientFeed, AmbientFeedBudget, AmbientOptions, AmbientSource,
    AmbientStatus, AmbientUnavailable, AmbientWaiting, Base, Shared, TurnBatch, paths,
};
use crate::SessionId;
use crate::agent::AgentState;
use crate::agent::state::{AgentProgram, Controller};

/// Bounded passes over one transcript per wake; more continue at once after the worker
/// has checked its receiver and terminal again.
const PASSES_PER_WAKE: usize = 64;

/// What an attachment reads.
#[derive(Clone)]
pub(in crate::agent) enum Source {
    /// A transcript file it tails.
    Transcript(Arc<dyn AmbientSource>),
    /// A feed it polls.
    Feed(Arc<dyn AmbientFeed>),
}

pub(super) struct Worker {
    // Retain launch metadata through terminal drop. Controller owns only a
    // TerminalHandle: retaining it never delays child teardown.
    ctl: Arc<Controller>,
    /// Disconnects when the terminal is torn down.
    closed: Receiver<()>,
    /// The receiver's stop.
    cancelled: Receiver<()>,
    base: Arc<Base>,
    shared: Arc<Shared>,
    source: Source,
    options: AmbientOptions,
    attached_at: SystemTime,
    id: SessionId,
    ctx: AmbientContext,
    generation: Generation,
    /// Lines skipped for their length since attach.
    oversized: u32,
}

/// The read state of one agent launch.
struct Generation {
    /// The controller's lifecycle revision it belongs to.
    lifecycle: u64,
    /// Launch time survives departure and the terminal-end lifecycle revision.
    launched_at: Option<SystemTime>,
    /// What is new: written, or ended, at or after this.
    since: SystemTime,
    /// A feed's reader for this launch, once opened.
    feed: Option<Driver>,
    files: Files,
    seen: Seen,
    /// The launch's agent was seen leaving: nothing is read until a launch or proven foreground return.
    departed: bool,
}

impl Generation {
    fn new(lifecycle: u64, since: SystemTime, launched_at: Option<SystemTime>) -> Self {
        Self {
            lifecycle,
            launched_at,
            since,
            feed: None,
            files: Files::new(since),
            seen: Seen::default(),
            departed: false,
        }
    }
}

fn current_launch(ctl: &Controller, lifecycle: u64) -> bool {
    ctl.read(|st| st.lifecycle_revision()) == lifecycle
}

/// The terminal as one wake finds it.
pub(super) struct Sample {
    pub(super) lifecycle: u64,
    /// An agent this terminal launched runs (or the direct child does), as the
    /// controller knows it.
    running: bool,
    /// The launch's identified program no longer owns the terminal: the shell or another
    /// program does (the controller notices that only at its next launch or delivery).
    left: bool,
    /// The same identified program positively owns the foreground again.
    returned: bool,
    program: Option<u32>,
    launched_at: Option<SystemTime>,
    pub(super) end: Option<AmbientClosed>,
}

impl Sample {
    /// Captures launch identity and lifecycle together, without child I/O.
    pub(super) fn controller(ctl: &Controller, base: &Base) -> Self {
        ctl.read(|st| {
            let state = st.agent_state();
            let end = if st.disposed || ctl.term().turns_ended() || state == AgentState::Exited {
                Some(AmbientClosed::TerminalEnded)
            } else if st.monitor_unavailable() {
                Some(AmbientClosed::Monitor)
            } else {
                None
            };
            let launched = matches!(
                state,
                AgentState::Starting | AgentState::TrustPrompt | AgentState::Ready
            );
            let (running, program) = if base.direct {
                (launched || ctl.cfg.profile.is_none(), base.child)
            } else {
                let program = match st.program {
                    AgentProgram::Known(id) => Some(id.0),
                    _ => None,
                };
                (launched, program)
            };
            Sample {
                lifecycle: st.lifecycle_revision(),
                running,
                left: false,
                returned: false,
                program,
                launched_at: st
                    .launched_at()
                    .or_else(|| base.direct.then_some(base.spawned_at)),
                end,
            }
        })
    }

    fn update_context(&self, ctx: &mut AmbientContext) {
        ctx.program = self.program;
        ctx.launched_at = self.launched_at.filter(|_| self.running && !self.left);
    }
}

impl Worker {
    pub(super) fn new(
        ctl: &Arc<Controller>,
        base: Arc<Base>,
        shared: Arc<Shared>,
        source: Source,
        options: AmbientOptions,
        cancelled: Receiver<()>,
        launch: Sample,
    ) -> Self {
        let attached_at = SystemTime::now();
        let mut ctx = AmbientContext::new(base.cwd.clone(), base.env.clone());
        launch.update_context(&mut ctx);
        Self {
            ctl: Arc::clone(ctl),
            closed: ctl.term().closed_signal(),
            cancelled,
            ctx,
            base,
            shared,
            source,
            options,
            attached_at,
            id: ctl.cfg.id,
            generation: Generation::new(launch.lifecycle, attached_at, launch.launched_at),
            oversized: 0,
        }
    }

    pub(super) fn run(mut self) {
        let shared = Arc::clone(&self.shared);
        let id = self.id;
        if catch_unwind(AssertUnwindSafe(|| self.observe())).is_err() {
            log::error!("[agent-link:{id}] ambient turn worker panicked");
            shared.close(AmbientClosed::WorkerPanic);
        }
        // Every return path closes first; this only guards against a missed one.
        shared.close(AmbientClosed::TerminalEnded);
    }

    fn observe(&mut self) {
        let ticks = crossbeam_channel::tick(self.options.poll);
        let mut terminal_closed = false;
        loop {
            if !self.shared.live() {
                return;
            }
            let mut sample = self.sample();
            if terminal_closed {
                sample.end = Some(AmbientClosed::TerminalEnded);
            }
            let terminal_end = sample.end == Some(AmbientClosed::TerminalEnded);
            // Session end itself changes the lifecycle revision. Only a different
            // launch time calls for a replacement reader in that final sample.
            let changed = sample.lifecycle != self.generation.lifecycle
                && (!terminal_end || sample.launched_at != self.generation.launched_at);
            let resumed = self.generation.departed && sample.returned;
            if sample.end.is_none() || terminal_end {
                if changed || (sample.left && !self.generation.departed) || resumed {
                    let replacement = changed && (sample.running || terminal_end);
                    let since = if resumed {
                        SystemTime::now()
                    } else {
                        sample
                            .launched_at
                            .map_or_else(SystemTime::now, |at| at.max(self.attached_at))
                    };
                    self.end_generation(replacement.then_some(since));
                    if !self.shared.live() {
                        return;
                    }
                    self.ctx.current_transcript = None;
                    self.generation = Generation::new(sample.lifecycle, since, sample.launched_at);
                    self.generation.departed = sample.left;
                    #[cfg(feature = "questions")]
                    self.shared.question.restart(sample.lifecycle);
                }
                if terminal_end {
                    // Preserve the departing context on ordinary exit. A replacement
                    // may have started and finished entirely between worker polls.
                    if changed {
                        self.ctx.program = sample.program;
                        self.ctx.launched_at = sample.launched_at;
                    }
                    self.end_generation(None);
                }
            }
            if let Some(end) = sample.end {
                self.shared.close(end);
                return;
            }
            sample.update_context(&mut self.ctx);
            let again = if sample.running && !self.generation.departed {
                self.poll()
            } else {
                #[cfg(feature = "questions")]
                self.shared.question.clear();
                self.shared.publish_batch(
                    Some(AmbientStatus::Waiting(AmbientWaiting::NoAgent)),
                    TurnBatch::default(),
                );
                false
            };
            if again {
                continue;
            }
            if !self.shared.live() {
                return;
            }
            select_biased! {
                recv(self.cancelled) -> _ => return,
                recv(self.closed) -> _ => {
                    terminal_closed = true;
                }
                recv(ticks) -> _ => {}
            }
        }
    }

    /// Reads the controller's view without waiting on the agent; then, outside its lock,
    /// who owns the terminal when the launch's program was identified, and whether the
    /// child exited (a bounded wait only while teardown reaps it).
    fn sample(&self) -> Sample {
        let ctl = &self.ctl;
        let mut sample = Sample::controller(ctl, &self.base);
        if let Some(program) = sample
            .program
            .filter(|_| !self.base.direct && sample.running && sample.end.is_none())
        {
            // The policy's trusted answer only: unknown ownership proves nothing.
            let front = ctl.read_front();
            sample.left = front.shell || front.program.is_some_and(|now| now.0 != program);
            sample.returned = !front.shell && front.program.is_some_and(|now| now.0 == program);
        }
        let term = ctl.term().clone();
        if sample.end.is_none() && term.try_wait().is_some() {
            sample.end = Some(AmbientClosed::TerminalEnded);
        }
        sample
    }

    /// Locates the transcript and reads it in bounded passes, or polls the feed. Returns
    /// `true` when the passes ran out with more to read.
    fn poll(&mut self) -> bool {
        let source = match &self.source {
            Source::Transcript(source) => Arc::clone(source),
            Source::Feed(_) => return self.poll_feed(None),
        };
        let located = source.locate(&self.ctx);
        // A source lookup can overlap a shell relaunch. Re-sample before opening the
        // returned path with the departing launch's context.
        if !current_launch(&self.ctl, self.generation.lifecycle) {
            return true;
        }
        let path = match located {
            Ok(path) => paths::anchored(path, &self.ctx.cwd),
            Err(reason) => {
                waiting(&self.shared, reason);
                return false;
            }
        };
        if self.ctx.current_transcript.as_ref() != Some(&path) {
            if let Some(left) = self.ctx.current_transcript.replace(path.clone()) {
                if !self.drain_path(&left, &*source, Drain::Watched) {
                    return false;
                }
            }
        }
        let Self {
            shared,
            options,
            generation,
            oversized,
            ctx,
            id,
            ..
        } = self;
        let source = &*source;
        let Generation {
            files,
            seen,
            lifecycle,
            ..
        } = generation;
        if missing_dir(&path) {
            waiting(shared, AmbientUnavailable::MissingDirectory);
            return false;
        }
        let watched = match files.watch(&path, source, shared.capacity.clone(), seen) {
            Ok(watched) => watched,
            Err(e) => {
                waiting(shared, AmbientUnavailable::Io(e.kind()));
                return false;
            }
        };
        for _ in 0..PASSES_PER_WAKE {
            let mut stale = false;
            let mut report = watched.read_through(source, seen, None, |file| {
                // The descriptor is now open. A file created by a replacement
                // could have appeared after locate or during parser construction.
                stale = !current_launch(&self.ctl, *lifecycle);
                if stale {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                source.accepts_file(ctx, file)
            });
            if stale {
                // Any records from earlier read-ahead still belong to this launch.
                return publish(shared, oversized, report, None, *id);
            }
            if !report.more {
                watched.quiet(options.quiet, false, &mut report);
            }
            let more = report.more;
            // What the transcript shows asked, once read to its end.
            #[cfg(feature = "questions")]
            shared.question.set(
                watched.asked(),
                !more && report.error.is_none() && watched.settled(),
            );
            let unrecognised = watched.unrecognised;
            if !publish(shared, oversized, report, Some(unrecognised), *id) || !more {
                return false;
            }
        }
        true
    }

    /// One poll of this launch's feed reader (opened at the launch's first wake). A final deadline means
    /// the launch ended — what arrived is read, its state not published. Returns `true`
    /// when the feed has more to tell at once.
    fn poll_feed(&mut self, final_deadline: Option<Instant>) -> bool {
        let last = final_deadline.is_some();
        let Source::Feed(source) = &self.source else {
            return false;
        };
        let generation = &mut self.generation;
        if generation.feed.is_none() {
            if last {
                // Never opened in this launch: nothing arrived.
                return false;
            }
            let reader = source.open(&self.ctx, self.base.endpoint.as_ref());
            generation.feed = Some(Driver::new(
                reader,
                generation.since,
                self.shared.capacity.clone(),
            ));
        }
        let Some(driver) = generation.feed.as_mut() else {
            return false;
        };
        let limits = &self.options.limits;
        let deadline = final_deadline.unwrap_or_else(|| Instant::now() + feed::POLL_BUDGET);
        let mut budget = AmbientFeedBudget::new(deadline, limits);
        budget.last = last;
        let polled = driver.poll(&self.ctx, &budget);
        self.oversized = self.oversized.saturating_add(polled.oversized);
        let status = match polled.status {
            FeedStatus::Ready { unrecognised } => AmbientStatus::Active {
                unrecognised,
                oversized_lines: self.oversized,
            },
            FeedStatus::Unavailable(reason) => {
                AmbientStatus::Waiting(AmbientWaiting::Unavailable(reason))
            }
            FeedStatus::IncompleteDrain => {
                self.shared.publish_batch(None, polled.batch);
                self.shared.close(AmbientClosed::IncompleteDrain);
                return false;
            }
        };
        self.shared
            .publish_batch((!last).then_some(status), polled.batch)
            && polled.more
    }

    /// The transcript followed so far is read a last time, its quiet forced; a feed is
    /// drained under one deadline and its reader dropped.
    fn end_generation(&mut self, replacement: Option<SystemTime>) {
        let source = match &self.source {
            Source::Transcript(source) => Arc::clone(source),
            Source::Feed(_) => {
                let deadline = Instant::now() + feed::POLL_BUDGET;
                while self.shared.live() {
                    if Instant::now() >= deadline {
                        self.shared.close(AmbientClosed::IncompleteDrain);
                        break;
                    }
                    if !self.poll_feed(Some(deadline)) {
                        break;
                    }
                }
                self.generation.feed = None;
                return;
            }
        };
        if let Some(path) = self.ctx.current_transcript.clone() {
            if !self.drain_path(
                &path,
                &*source,
                replacement.map_or(Drain::Located, Drain::Replaced),
            ) {
                return;
            }
        }
        if replacement.is_none()
            && !self.generation.departed
            && self.ctx.launched_at.is_some()
            && self.shared.live()
        {
            // A CLI can create or switch transcripts between the last poll and exit.
            match source.locate(&self.ctx) {
                Ok(path) => {
                    let path = paths::anchored(path, &self.ctx.cwd);
                    if Some(&path) != self.ctx.current_transcript.as_ref() {
                        self.drain_path(&path, &*source, Drain::Located);
                    }
                }
                Err(AmbientUnavailable::Io(_) | AmbientUnavailable::ReadBudgetExceeded) => {
                    self.shared.close(AmbientClosed::IncompleteDrain)
                }
                Err(_) => {}
            }
        }
    }

    /// Drain a captured file through its EOF in bounded passes and force its quiet.
    fn drain_path(&mut self, path: &Path, source: &dyn AmbientSource, mode: Drain) -> bool {
        let Self {
            shared,
            options,
            generation,
            oversized,
            ctx,
            id,
            ..
        } = self;
        let Generation {
            files,
            seen,
            lifecycle,
            ..
        } = generation;
        let watched = match mode {
            Drain::Located => match files.watch(path, source, shared.capacity.clone(), seen) {
                Ok(watched) => watched,
                Err(_) => {
                    shared.close(AmbientClosed::IncompleteDrain);
                    return false;
                }
            },
            Drain::Watched | Drain::Replaced(_) => {
                let Some(watched) = files.get_mut(path) else {
                    return true;
                };
                watched
            }
        };
        // Recheck after opening the file. A replacement can start during an old
        // lookup, including the final lookup after an observed return to the shell.
        let replacement = || match mode {
            Drain::Replaced(at) => Some(at),
            _ => {
                let now = Sample::controller(&self.ctl, &self.base);
                now.launched_at
                    .filter(|&at| now.lifecycle != *lifecycle && Some(at) != ctx.launched_at)
            }
        };
        #[cfg(feature = "questions")]
        shared.question.unsettle();
        let fence = match watched.final_read(replacement) {
            Ok(Some(fence)) => fence,
            Ok(None) => return true,
            Err(_) => {
                shared.close(AmbientClosed::IncompleteDrain);
                return false;
            }
        };
        let mut first_pass = true;
        while shared.live() {
            let mut report = watched.read_through(source, seen, Some(&fence), |file| {
                source.accepts_file(ctx, file)
            });
            let more = report.more;
            // The first check can discover a rewrite that predates departure.
            // A later rewrite invalidates the fence and everything in this pass.
            if report.rewritten && !first_pass {
                shared.close(AmbientClosed::IncompleteDrain);
                return false;
            }
            first_pass = false;
            let lost = report.error.is_some();
            if !more && !lost {
                watched.quiet(options.quiet, true, &mut report);
            }
            if !publish(shared, oversized, report, None, *id) {
                return false;
            }
            if lost {
                shared.close(AmbientClosed::IncompleteDrain);
                return false;
            }
            if !more {
                return true;
            }
        }
        false
    }
}

enum Drain {
    Watched,
    Located,
    Replaced(SystemTime),
}

fn waiting(shared: &Shared, reason: AmbientUnavailable) {
    // What the transcript shows asked is unknown until it is read again.
    #[cfg(feature = "questions")]
    shared.question.unsettle();
    shared.publish_batch(
        Some(AmbientStatus::Waiting(AmbientWaiting::Unavailable(reason))),
        TurnBatch::default(),
    );
}

/// Publishes a pass's turns, with the status it leaves when `unrecognised` is given (the
/// transcript followed now). Returns whether observation is still live.
fn publish(
    shared: &Shared,
    oversized: &mut u32,
    report: Report,
    unrecognised: Option<bool>,
    id: SessionId,
) -> bool {
    *oversized = oversized.saturating_add(report.oversized);
    if report.rebaselined {
        log::debug!(
            "[agent-link:{id}] ambient: using the rewritten transcript's EOF as the baseline"
        );
    }
    let status = unrecognised.map(|unrecognised| match report.error {
        Some(kind) => {
            AmbientStatus::Waiting(AmbientWaiting::Unavailable(AmbientUnavailable::Io(kind)))
        }
        None => AmbientStatus::Active {
            unrecognised,
            oversized_lines: *oversized,
        },
    });
    shared.publish_batch(status, report.batch)
}

/// The directory `path` is in, when it does not exist: no transcript can appear there.
fn missing_dir(path: &Path) -> bool {
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .is_some_and(|dir| !dir.is_dir())
}
