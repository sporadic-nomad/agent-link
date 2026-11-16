//! The delivery waits' decisions, as pure functions of the output view, the screen's
//! classification and the time — the job thread loops on them (`controller`, `verify`), the
//! unit tests drive them on a fake clock.
//!
//! - [`lull_ready`]: before a paste (and after a reset), wait for a lull in the agent's
//!   output — the ready composer on screen, a settled frame or progress report, or plain
//!   output quiet, capped.
//! - [`paste_landed`]: after a paste, the submit Enter waits for output **after** the paste
//!   that then went quiet (byte quiet only: claude aggregates stdin ~100 ms after a paste
//!   and absorbs an Enter that lands inside that window, so no early "landed" from a chip
//!   on screen or a frame end), and never decides from a screen the view has not counted.
//! - [`SubmitCheck`]: after an Enter, only a screen quiet long enough — and fully counted
//!   by the view — may testify whether the text left the composer; and only once something
//!   came back after the paste (an agent that echoed nothing may not be reading).
//! - [`interrupt_due`]: a launch line typed after a Ctrl-C waits until the shell has
//!   handled it.
//!
//! *Unseen* output: the session's reader has read more chunks than the
//! monitor counted (an exact per-chunk count: `TerminalHandle::output_chunks`, read after
//! the screen, against `OutputView::output_serial`). This includes output waiting to render
//! behind controller publication. It comes from a backlog — for as long
//! as a host receiver stays full, the session and its monitor stall. No wait reads quiet
//! off such a screen. The lull and the paste wait stay bounded by their caps; submit
//! verification has none — it waits out the stall (back-pressure by design: drain every
//! receiver).
//!
//! Fences (the paste's, [`Fence`]) are that read count, sampled right after the write was
//! queued: a chunk read before it — the backlog from before the paste, counted late — never
//! reads as output after it.

use std::time::{Duration, Instant};

use super::ScreenState;

/// Evidence that a redraw finished needs this much silence after it before a lull counts
/// on it (a frame end or a cleared progress report, with no output since).
pub(crate) const EVIDENCE_QUIET: Duration = Duration::from_millis(50);

/// Waits poll no tighter than this.
const MIN_POLL: Duration = Duration::from_millis(10);

/// Before the paste's echo, [`paste_landed`] looks again this often.
const ECHO_POLL: Duration = Duration::from_millis(30);

/// A launch line typed after a Ctrl-C waits for output after the Ctrl-C (the shell's `^C`
/// and new prompt) and then this much quiet: input that arrives while a shell handles the
/// interrupt can be lost — bash 5.3 dropped the launch line's first characters in about one
/// try in three when it came in the Ctrl-C's own write (one in twenty right after it), none
/// with 10 ms between them.
const INTERRUPT_QUIET: Duration = Duration::from_millis(150);

/// ... or this long at most (a shell that prints nothing for a Ctrl-C).
const INTERRUPT_MAX: Duration = Duration::from_millis(1000);

/// A point in the output stream: everything after it is "fresh".
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fence {
    /// The chunks the reader had fed at that point (output after it, once counted, has a
    /// higher serial).
    pub(crate) serial: u64,
    pub(crate) at: Instant,
}

/// A redraw-finished signal: when it was seen and the output serial then (the chunk that
/// carried it).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Evidence {
    pub(crate) at: Instant,
    pub(crate) serial: u64,
}

/// The output bookkeeping a decision reads (copied out of the controller's state).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OutputView {
    /// +1 per output chunk counted (the `n`-th chunk the reader fed is serial `n`).
    pub(crate) output_serial: u64,
    pub(crate) last_output_at: Option<Instant>,
    /// The last DEC 2026 frame end — only one after which no frame was open (a chunk that
    /// ends a frame and begins the next leaves the screen mid-redraw).
    pub(crate) last_frame_end: Option<Evidence>,
    /// The last progress report that cleared (OSC 9;4;0).
    pub(crate) last_progress_clear: Option<Evidence>,
}

impl OutputView {
    /// Time since the last output; `Duration::MAX` when there was none.
    fn since_output(&self, now: Instant) -> Duration {
        self.last_output_at
            .map_or(Duration::MAX, |at| now.saturating_duration_since(at))
    }

    /// The reader has fed `fed` chunks (read after the screen): output this view has not
    /// counted yet is on that screen.
    pub(crate) fn unseen(&self, fed: u64) -> bool {
        fed > self.output_serial
    }

    /// For the lull, when [`unseen`](Self::unseen): the chunks up to `fed` as arriving now
    /// — no quiet and no rung is read off them (the cap still ends the wait); fresh when
    /// fed after the fence. Submit verification takes the flag instead ([`SubmitCheck`]).
    pub(crate) fn with_unseen_output(mut self, fed: u64, now: Instant) -> Self {
        self.output_serial = self.output_serial.max(fed);
        self.last_output_at = Some(now);
        self
    }
}

/// A wait's verdict: done (and why), or look again at the given time.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Settle<T> {
    Done(T),
    Wait(Instant),
}

/// Why a lull counts as ready.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Lull {
    /// The ready composer is on the current screen.
    InputReady,
    /// A synchronized-output frame ended and nothing followed.
    FrameSettled,
    /// A progress report cleared and nothing followed.
    ProgressCleared,
    /// Output (after the fence, if any) went quiet.
    Quiet,
    /// No output after the fence, quiet since the fence (not with `require_fresh`).
    BaselineQuiet,
    /// The cap.
    Max,
}

/// [`lull_ready`]'s parameters.
#[derive(Copy, Clone, Debug)]
pub(crate) struct LullParams {
    pub(crate) quiet: Duration,
    pub(crate) max: Duration,
    /// Only output after this point counts as fresh; `None`: all output does.
    pub(crate) after: Option<Fence>,
    /// Silence alone never counts — only fresh output that settles (or the cap): after a
    /// reset, the stale screen and a silent reset still in flight must not read ready.
    pub(crate) require_fresh: bool,
    /// When the wait began.
    pub(crate) start: Instant,
}

/// Port of the panel's input-lull wait (`waitForInputReadyOrQuiet`), plus the settle
/// ladder: a frame end or a cleared progress report with no output for
/// [`EVIDENCE_QUIET`] since counts before the byte-quiet rule — only when it is fresh and
/// newer than the last output.
pub(crate) fn lull_ready(
    view: &OutputView,
    screen: ScreenState,
    now: Instant,
    p: &LullParams,
) -> Settle<Lull> {
    let elapsed = now.saturating_duration_since(p.start);
    if elapsed >= p.max {
        return Settle::Done(Lull::Max);
    }
    let fresh = |serial: u64| p.after.is_none_or(|fence| serial > fence.serial);
    let saw_fresh = fresh(view.output_serial);
    let baseline_quiet = now.saturating_duration_since(p.after.map_or(p.start, |f| f.at));
    let since_output = view.since_output(now);
    // The current screen is the state authority. Under `require_fresh` a ready composer
    // alone may be the stale screen from before a reset: only fresh output vouches for it.
    if screen == ScreenState::InputReady
        && (saw_fresh || (!p.require_fresh && baseline_quiet >= p.quiet))
    {
        return Settle::Done(Lull::InputReady);
    }
    let mut next = p.max - elapsed;
    if saw_fresh {
        for (evidence, why) in [
            (view.last_frame_end, Lull::FrameSettled),
            (view.last_progress_clear, Lull::ProgressCleared),
        ] {
            let Some(ev) = evidence else { continue };
            if ev.serial != view.output_serial || !fresh(ev.serial) {
                continue;
            }
            let since = now.saturating_duration_since(ev.at);
            if since >= EVIDENCE_QUIET {
                return Settle::Done(why);
            }
            next = next.min(EVIDENCE_QUIET - since);
        }
        if since_output >= p.quiet {
            return Settle::Done(Lull::Quiet);
        }
        next = next.min(p.quiet - since_output);
    } else {
        let remaining = p.quiet.saturating_sub(baseline_quiet);
        if !p.require_fresh && remaining.is_zero() {
            return Settle::Done(Lull::BaselineQuiet);
        }
        // Waiting for fresh output: look again soon (the panel polled every 10 ms).
        next = next.min(remaining);
    }
    Settle::Wait(now + next.max(MIN_POLL))
}

/// Port of the panel's paste-landed wait (`waitForPasteLanded`): output strictly after
/// the paste fence (`after_serial` is the reader's fed count immediately before the
/// paste was queued: earlier chunks are backlog, even when counted later) and
/// then `quiet` without output; capped at `max`. The submit path separately requires
/// visible pasted text before its Enter; this timing wait alone proves no delivery.
///
/// `unseen` (the reader fed chunks the view has not counted): not landed yet — the view's
/// last output time is stale; only the counted view, caught up, decides.
pub(crate) fn paste_landed(
    view: &OutputView,
    unseen: bool,
    now: Instant,
    after_serial: u64,
    quiet: Duration,
    max: Duration,
    start: Instant,
) -> Settle<()> {
    let elapsed = now.saturating_duration_since(start);
    if elapsed >= max {
        return Settle::Done(());
    }
    let echoed = view.output_serial > after_serial;
    let since_output = view.since_output(now);
    if echoed && !unseen && since_output >= quiet {
        return Settle::Done(());
    }
    // Before the echo, or while the count lags: poll. After it: wait out the quiet.
    let next = if echoed && !unseen {
        (quiet - since_output).min(max - elapsed)
    } else {
        ECHO_POLL.min(max - elapsed)
    };
    Settle::Wait(now + next.max(MIN_POLL))
}

/// When the launch line held back after a Ctrl-C (queued when the reader had fed `fence`
/// chunks, at `at`) may be typed: [`INTERRUPT_QUIET`] after the last output once the shell
/// answered (output counted after the fence) — later output moves it — and
/// [`INTERRUPT_MAX`] after `at` at the latest.
pub(crate) fn interrupt_due(view: &OutputView, fence: u64, at: Instant) -> Instant {
    let cap = at + INTERRUPT_MAX;
    match view.last_output_at {
        Some(last) if view.output_serial > fence => cap.min(last + INTERRUPT_QUIET),
        _ => cap,
    }
}

/// [`SubmitCheck`]'s verdict.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Submit {
    /// A readable composer no longer holds the text that was observed before Enter.
    Submitted,
    /// A readable composer still holds the text; eligible for an explicitly enabled retry.
    Pending,
    /// No stable, readable composer proves either result. Never retry blindly.
    Unverified,
}

/// The submit-verification timings [`SubmitCheck`] reads.
#[derive(Copy, Clone, Debug)]
pub(crate) struct VerifyTiming {
    pub(crate) quiet: Duration,
    pub(crate) window: Duration,
    pub(crate) poll: Duration,
}

/// Verifies one Enter after the pasted text was observed in the composer. Output must
/// arrive after this Enter's fence. A readable composer then proves that the text left
/// (`Some(false)`) or remains (`Some(true)`). An unreadable composer, silence, or output
/// volume alone proves neither and ends `Unverified` at the window's deadline.
///
/// A quiet screen testifies immediately. During continuing output the same definite
/// composer verdict must be observed throughout `quiet` before it can testify: title
/// updates or a streaming transcript do not prevent verification of a readable composer.
/// A changed or unknown verdict restarts that observation interval. Pending input permits
/// an opt-in retry only after the full window; uncertain input never permits one. A stale
/// composer may still show accepted input, so retrying can submit twice.
///
/// A screen ahead of the monitor's output count never testifies. Back-pressure stalls
/// verification and restarts its window, so an observed pre-Enter snapshot cannot serve
/// as post-Enter evidence.
#[derive(Copy, Clone, Debug)]
pub(crate) struct SubmitCheck {
    window_start: Instant,
    enter_fence: u64,
    observed: Option<(bool, Instant)>,
}

impl SubmitCheck {
    pub(crate) fn new(start: Instant, enter_fence: u64) -> Self {
        Self {
            window_start: start,
            enter_fence,
            observed: None,
        }
    }

    /// The verdict for the screen read now (`pending`, `unseen`) and the view read after it.
    pub(crate) fn poll(
        &mut self,
        view: &OutputView,
        pending: Option<bool>,
        unseen: bool,
        now: Instant,
        t: &VerifyTiming,
    ) -> Settle<Submit> {
        let next = Settle::Wait(now + t.poll.max(MIN_POLL));
        if unseen {
            self.window_start = now;
            self.observed = None;
            return next;
        }
        let elapsed = now.saturating_duration_since(self.window_start);
        let pending = pending.filter(|_| view.output_serial > self.enter_fence);
        self.observed = match (self.observed, pending) {
            (Some((previous, since)), Some(current)) if previous == current => {
                Some((current, since))
            }
            (_, Some(current)) => Some((current, now)),
            (_, None) => None,
        };
        let stable = view.since_output(now) >= t.quiet
            || self
                .observed
                .is_some_and(|(_, since)| now.duration_since(since) >= t.quiet);
        match (pending, stable) {
            (Some(false), true) => Settle::Done(Submit::Submitted),
            (Some(true), true) if elapsed >= t.window => Settle::Done(Submit::Pending),
            _ if elapsed >= t.window => Settle::Done(Submit::Unverified),
            _ => next,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A fake clock: instants as milliseconds after a fixed base.
    struct Clock(Instant);

    impl Clock {
        fn new() -> Self {
            Self(Instant::now())
        }

        fn at(&self, n: u64) -> Instant {
            self.0 + ms(n)
        }
    }

    fn view(c: &Clock, serial: u64, last_output: Option<u64>) -> OutputView {
        OutputView {
            output_serial: serial,
            last_output_at: last_output.map(|n| c.at(n)),
            ..OutputView::default()
        }
    }

    fn evidence(c: &Clock, n: u64, serial: u64) -> Option<Evidence> {
        Some(Evidence {
            at: c.at(n),
            serial,
        })
    }

    fn pre_paste(c: &Clock) -> LullParams {
        LullParams {
            quiet: ms(120),
            max: ms(2000),
            after: None,
            require_fresh: false,
            start: c.at(1000),
        }
    }

    fn post_reset(c: &Clock, fence_at: u64, fence_serial: u64) -> LullParams {
        LullParams {
            quiet: ms(450),
            max: ms(3000),
            after: Some(Fence {
                serial: fence_serial,
                at: c.at(fence_at),
            }),
            require_fresh: true,
            start: c.at(fence_at),
        }
    }

    use ScreenState::{InputReady, Unknown};

    #[test]
    fn the_lull_waits_for_output_quiet() {
        let c = Clock::new();
        let p = pre_paste(&c);
        // Output 50 ms before the start: 70 ms more to wait.
        let v = view(&c, 5, Some(950));
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1000), &p),
            Settle::Wait(c.at(1070))
        );
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1070), &p),
            Settle::Done(Lull::Quiet)
        );
        // Never any output: quiet at once.
        let v = view(&c, 0, None);
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1000), &p),
            Settle::Done(Lull::Quiet)
        );
    }

    #[test]
    fn the_lull_takes_the_ready_screen_at_once() {
        let c = Clock::new();
        let v = view(&c, 5, Some(999));
        let p = pre_paste(&c);
        assert_eq!(
            lull_ready(&v, InputReady, c.at(1000), &p),
            Settle::Done(Lull::InputReady)
        );
    }

    #[test]
    fn the_lull_is_capped() {
        let c = Clock::new();
        let p = pre_paste(&c);
        // Output keeps coming: the cap ends it.
        let v = view(&c, 90, Some(2990));
        assert_eq!(
            lull_ready(&v, Unknown, c.at(2999), &p),
            Settle::Wait(c.at(3009))
        );
        assert_eq!(
            lull_ready(&v, Unknown, c.at(3000), &p),
            Settle::Done(Lull::Max)
        );
        // The next look is at the cap (at least 10 ms ahead).
        let v = view(&c, 90, Some(2995));
        assert_eq!(
            lull_ready(&v, Unknown, c.at(2996), &p),
            Settle::Wait(c.at(3006))
        );
    }

    #[test]
    fn without_fresh_output_the_baseline_quiet_counts_unless_fresh_output_is_required() {
        let c = Clock::new();
        // Fence at 1000 (serial 7); no output since.
        let v = view(&c, 7, Some(900));
        let mut p = post_reset(&c, 1000, 7);
        p.require_fresh = false;
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1200), &p),
            Settle::Wait(c.at(1450))
        );
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1450), &p),
            Settle::Done(Lull::BaselineQuiet)
        );
        // A ready screen counts after the baseline quiet too.
        assert_eq!(
            lull_ready(&v, InputReady, c.at(1450), &p),
            Settle::Done(Lull::InputReady)
        );
        // Required: silence and the stale ready screen never count — only the cap. Past
        // the quiet it looks for fresh output every 10 ms.
        p.require_fresh = true;
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1200), &p),
            Settle::Wait(c.at(1450))
        );
        assert_eq!(
            lull_ready(&v, Unknown, c.at(2000), &p),
            Settle::Wait(c.at(2010))
        );
        assert_eq!(
            lull_ready(&v, InputReady, c.at(3999), &p),
            Settle::Wait(c.at(4009))
        );
        assert_eq!(
            lull_ready(&v, InputReady, c.at(4000), &p),
            Settle::Done(Lull::Max)
        );
    }

    #[test]
    fn fresh_output_then_quiet_ends_a_post_reset_wait() {
        let c = Clock::new();
        let p = post_reset(&c, 1000, 7);
        // Fresh output at 1300: the ready screen counts at once, quiet after 450 ms.
        let v = view(&c, 8, Some(1300));
        assert_eq!(
            lull_ready(&v, InputReady, c.at(1301), &p),
            Settle::Done(Lull::InputReady)
        );
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1301), &p),
            Settle::Wait(c.at(1750))
        );
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1750), &p),
            Settle::Done(Lull::Quiet)
        );
    }

    #[test]
    fn a_settled_frame_ends_the_lull_early() {
        let c = Clock::new();
        let p = pre_paste(&c);
        let mut v = view(&c, 5, Some(990));
        v.last_frame_end = evidence(&c, 990, 5);
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1000), &p),
            Settle::Wait(c.at(1040))
        );
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1040), &p),
            Settle::Done(Lull::FrameSettled)
        );
    }

    #[test]
    fn a_frame_end_followed_by_output_is_ignored() {
        let c = Clock::new();
        let p = pre_paste(&c);
        // The frame ended with chunk 4; chunk 5 came after it.
        let mut v = view(&c, 5, Some(995));
        v.last_frame_end = evidence(&c, 980, 4);
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1040), &p),
            Settle::Wait(c.at(1115))
        );
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1115), &p),
            Settle::Done(Lull::Quiet)
        );
    }

    #[test]
    fn a_cleared_progress_report_ends_the_lull_early() {
        let c = Clock::new();
        let p = pre_paste(&c);
        let mut v = view(&c, 5, Some(1000));
        v.last_progress_clear = evidence(&c, 1000, 5);
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1020), &p),
            Settle::Wait(c.at(1050))
        );
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1050), &p),
            Settle::Done(Lull::ProgressCleared)
        );
    }

    #[test]
    fn evidence_from_before_the_fence_is_not_fresh() {
        let c = Clock::new();
        let p = post_reset(&c, 1000, 7);
        // The frame ended at serial 7 (the fence): not evidence of the fresh screen.
        let mut v = view(&c, 7, Some(990));
        v.last_frame_end = evidence(&c, 990, 7);
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1100), &p),
            Settle::Wait(c.at(1450))
        );
        // A fresh frame end (serial 8) counts.
        let mut v = view(&c, 8, Some(1200));
        v.last_frame_end = evidence(&c, 1200, 8);
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1250), &p),
            Settle::Done(Lull::FrameSettled)
        );
    }

    #[test]
    fn unseen_output_counts_as_output_now() {
        let c = Clock::new();
        let p = pre_paste(&c);
        let mut v = view(&c, 5, Some(800));
        v.last_frame_end = evidence(&c, 800, 5);
        assert!(!v.unseen(5) && v.unseen(7));
        let v = v.with_unseen_output(7, c.at(1000));
        assert_eq!(v.output_serial, 7);
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1000), &p),
            Settle::Wait(c.at(1120))
        );
    }

    /// Unseen chunks are fresh only when fed after the fence: the backlog from before it is
    /// not, but still no quiet is read off it.
    #[test]
    fn unseen_output_is_fresh_only_after_the_fence() {
        let c = Clock::new();
        let mut p = post_reset(&c, 1000, 7);
        p.require_fresh = false;
        // Counted up to 5 (at 900); the reader had fed 7 at the fence, 7 now: backlog.
        let v = view(&c, 5, Some(900)).with_unseen_output(7, c.at(1500));
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1500), &p),
            Settle::Done(Lull::BaselineQuiet)
        );
        // Fed 8 now: fresh, and arriving now.
        let v = view(&c, 5, Some(900)).with_unseen_output(8, c.at(1500));
        assert_eq!(
            lull_ready(&v, InputReady, c.at(1500), &p),
            Settle::Done(Lull::InputReady)
        );
        assert_eq!(
            lull_ready(&v, Unknown, c.at(1500), &p),
            Settle::Wait(c.at(1950))
        );
    }

    #[test]
    fn a_paste_lands_after_its_echo_went_quiet() {
        let c = Clock::new();
        let (quiet, max, start) = (ms(250), ms(2000), c.at(1000));
        // Output before the paste (serial 4 = the serial right after the write): no echo.
        let v = view(&c, 4, Some(900));
        assert_eq!(
            paste_landed(&v, false, c.at(1000), 4, quiet, max, start),
            Settle::Wait(c.at(1030))
        );
        // The echo at 1040; a chip on screen does not shorten the quiet.
        let v = view(&c, 5, Some(1040));
        assert_eq!(
            paste_landed(&v, false, c.at(1050), 4, quiet, max, start),
            Settle::Wait(c.at(1290))
        );
        assert_eq!(
            paste_landed(&v, false, c.at(1290), 4, quiet, max, start),
            Settle::Done(())
        );
    }

    /// While the reader is ahead of the count nothing is decided — the counted view decides
    /// once it caught up; and the backlog fed before the paste (the fence is the fed count)
    /// never reads as its echo, even counted after it.
    #[test]
    fn no_paste_lands_while_the_count_lags() {
        let c = Clock::new();
        let (quiet, max, start) = (ms(250), ms(2000), c.at(1000));
        // The reader had fed 6 chunks when the paste was queued; 4 were counted then. The
        // backlog (chunks 5, 6) is counted at 1010 and 1300: no echo.
        let v = view(&c, 5, Some(1010));
        assert_eq!(
            paste_landed(&v, true, c.at(1050), 6, quiet, max, start),
            Settle::Wait(c.at(1080))
        );
        let v = view(&c, 6, Some(1300));
        assert_eq!(
            paste_landed(&v, false, c.at(1600), 6, quiet, max, start),
            Settle::Wait(c.at(1630)),
            "quiet, but no output after the paste yet"
        );
        // The echo (chunk 7) is fed; counted at 1700: quiet from there.
        assert_eq!(
            paste_landed(&v, true, c.at(1650), 6, quiet, max, start),
            Settle::Wait(c.at(1680))
        );
        let v = view(&c, 7, Some(1700));
        assert_eq!(
            paste_landed(&v, false, c.at(1700), 6, quiet, max, start),
            Settle::Wait(c.at(1950))
        );
        assert_eq!(
            paste_landed(&v, false, c.at(1950), 6, quiet, max, start),
            Settle::Done(())
        );
        // The cap still ends a lag that never clears.
        assert_eq!(
            paste_landed(&v, true, c.at(2990), 6, quiet, max, start),
            Settle::Wait(c.at(3000))
        );
        assert_eq!(
            paste_landed(&v, true, c.at(3000), 6, quiet, max, start),
            Settle::Done(())
        );
    }

    #[test]
    fn a_paste_without_echo_lands_at_the_cap() {
        let c = Clock::new();
        let (quiet, max, start) = (ms(250), ms(2000), c.at(1000));
        let v = view(&c, 4, Some(100));
        assert_eq!(
            paste_landed(&v, false, c.at(2990), 4, quiet, max, start),
            Settle::Wait(c.at(3000))
        );
        assert_eq!(
            paste_landed(&v, false, c.at(3000), 4, quiet, max, start),
            Settle::Done(())
        );
        // An echo that never goes quiet: also the cap.
        let v = view(&c, 9, Some(2999));
        assert_eq!(
            paste_landed(&v, false, c.at(3000), 4, quiet, max, start),
            Settle::Done(())
        );
    }

    fn verify() -> VerifyTiming {
        VerifyTiming {
            quiet: ms(250),
            window: ms(700),
            poll: ms(100),
        }
    }

    /// Polls `check` like the job does: the verdict, or the next poll time.
    fn poll(
        check: &mut SubmitCheck,
        v: &OutputView,
        pending: Option<bool>,
        unseen: bool,
        now: Instant,
    ) -> Settle<Submit> {
        check.poll(v, pending, unseen, now, &verify())
    }

    #[test]
    fn only_a_quiet_screen_testifies() {
        let c = Clock::new();
        let mut check = SubmitCheck::new(c.at(1000), 0);
        // Output 100 ms ago: the composer is not read, whatever it says.
        let v = view(&c, 9, Some(1100));
        for pending in [Some(true), Some(false), None] {
            assert_eq!(
                poll(&mut check, &v, pending, false, c.at(1200)),
                Settle::Wait(c.at(1300))
            );
        }
        // Quiet for 250 ms: only a readable empty composer proves submission.
        assert_eq!(
            poll(&mut check, &v, Some(false), false, c.at(1350)),
            Settle::Done(Submit::Submitted)
        );
        assert_eq!(
            poll(&mut check, &v, None, false, c.at(1350)),
            Settle::Wait(c.at(1450))
        );
        // Not from a screen holding output the view has not counted.
        assert_eq!(
            poll(&mut check, &v, Some(false), true, c.at(1350)),
            Settle::Wait(c.at(1450))
        );
    }

    #[test]
    fn text_left_in_the_composer_is_pending_after_the_window() {
        let c = Clock::new();
        let mut check = SubmitCheck::new(c.at(1000), 0);
        let v = view(&c, 9, Some(1050));
        assert_eq!(
            poll(&mut check, &v, Some(true), false, c.at(1400)),
            Settle::Wait(c.at(1500))
        );
        assert_eq!(
            poll(&mut check, &v, Some(true), false, c.at(1700)),
            Settle::Done(Submit::Pending)
        );
    }

    #[test]
    fn output_alone_proves_nothing() {
        let c = Clock::new();
        let v = view(&c, 30, Some(1690));
        for pending in [Some(true), Some(false), None] {
            let mut check = SubmitCheck::new(c.at(1000), 0);
            assert_eq!(
                poll(&mut check, &v, pending, false, c.at(1700)),
                Settle::Done(Submit::Unverified)
            );
        }
    }

    #[test]
    fn a_stable_composer_testifies_during_background_output() {
        let c = Clock::new();
        for (pending, expected) in [(true, Submit::Pending), (false, Submit::Submitted)] {
            let mut check = SubmitCheck::new(c.at(1000), 0);
            let v = view(&c, 20, Some(1290));
            assert_eq!(
                poll(&mut check, &v, Some(pending), false, c.at(1300)),
                Settle::Wait(c.at(1400))
            );
            let v = view(&c, 30, Some(1690));
            assert_eq!(
                poll(&mut check, &v, Some(pending), false, c.at(1700)),
                Settle::Done(expected)
            );
        }
    }

    #[test]
    fn unknown_and_changed_composer_verdicts_cancel_stability() {
        let c = Clock::new();
        for changed in [Some(true), None] {
            let mut check = SubmitCheck::new(c.at(1000), 0);
            let v = view(&c, 20, Some(1290));
            assert_eq!(
                poll(&mut check, &v, Some(false), false, c.at(1300)),
                Settle::Wait(c.at(1400))
            );
            let v = view(&c, 25, Some(1490));
            assert_eq!(
                poll(&mut check, &v, changed, false, c.at(1500)),
                Settle::Wait(c.at(1600))
            );
            let v = view(&c, 30, Some(1690));
            assert_eq!(
                poll(&mut check, &v, Some(false), false, c.at(1700)),
                Settle::Done(Submit::Unverified)
            );
        }
    }

    /// A host receiver stays full for longer than the window: the model holds a chunk the
    /// monitor never counted. The swallowed Enter (text still in the composer) must not
    /// read as a running turn — then or when the backlog is counted, late, as output now.
    #[test]
    fn a_stall_is_no_running_turn() {
        let c = Clock::new();
        let mut check = SubmitCheck::new(c.at(1000), 0);
        // The echo of the Enter was counted at 1050; the next chunk stalls from 1100.
        let v = view(&c, 9, Some(1050));
        let mut at = 1100;
        while at < 2500 {
            assert_eq!(
                poll(&mut check, &v, Some(true), true, c.at(at)),
                Settle::Wait(c.at(at + 100)),
                "at {at}"
            );
            at += 100;
        }
        // The stall ends: the backlog is counted at 2500.
        let v = view(&c, 10, Some(2500));
        for at in [2500, 2600, 2700, 2800, 3000] {
            assert_eq!(
                poll(&mut check, &v, Some(true), false, c.at(at)),
                Settle::Wait(c.at(at + 100)),
                "at {at}"
            );
        }
        // A full window after the last unseen look (2400), the quiet composer testifies.
        assert_eq!(
            poll(&mut check, &v, Some(true), false, c.at(3100)),
            Settle::Done(Submit::Pending)
        );
    }

    /// No fresh output after Enter (the agent is hung or not reading): the
    /// unchanged screen proves no submit or pending retry. The window ends `Unverified`
    /// without an Enter retry. Once fresh output arrives, readable composer evidence
    /// can testify again.
    #[test]
    fn an_unchanged_screen_after_a_silent_enter_proves_nothing() {
        let c = Clock::new();
        // Enter was fenced at 9; the last output (chunk 9) came before it.
        let v = view(&c, 9, Some(100));
        for pending in [Some(false), None] {
            let mut check = SubmitCheck::new(c.at(1000), 9);
            assert_eq!(
                poll(&mut check, &v, pending, false, c.at(1300)),
                Settle::Wait(c.at(1400)),
                "{pending:?}"
            );
            assert_eq!(
                poll(&mut check, &v, pending, false, c.at(1700)),
                Settle::Done(Submit::Unverified),
                "{pending:?}"
            );
        }
        // Backlog from before Enter, counted late, is no receipt either.
        let v = view(&c, 9, Some(1690));
        let mut check = SubmitCheck::new(c.at(1000), 9);
        assert_eq!(
            poll(&mut check, &v, Some(false), false, c.at(1700)),
            Settle::Done(Submit::Unverified)
        );
        // Output fed but not counted yet (a stall) may be the receipt: restart.
        let mut check = SubmitCheck::new(c.at(1000), 9);
        assert_eq!(
            poll(&mut check, &v, Some(false), true, c.at(1700)),
            Settle::Wait(c.at(1800))
        );
        assert_eq!(
            poll(&mut check, &v, Some(false), false, c.at(2300)),
            Settle::Wait(c.at(2400))
        );
        assert_eq!(
            poll(&mut check, &v, Some(false), false, c.at(2400)),
            Settle::Done(Submit::Unverified)
        );
        // The agent catches up: output after Enter, then quiet — the empty composer
        // testifies again.
        let v = view(&c, 10, Some(1500));
        let mut check = SubmitCheck::new(c.at(1000), 9);
        assert_eq!(
            poll(&mut check, &v, Some(false), false, c.at(1600)),
            Settle::Wait(c.at(1700))
        );
        assert_eq!(
            poll(&mut check, &v, Some(false), false, c.at(1750)),
            Settle::Done(Submit::Submitted)
        );
    }

    /// A launch line after a Ctrl-C waits for the shell's answer and then a short quiet —
    /// or the cap, for a shell that prints nothing.
    #[test]
    fn a_launch_line_waits_for_the_shell_to_take_its_ctrl_c() {
        let c = Clock::new();
        let at = c.at(1000);
        // Nothing after the Ctrl-C (queued with 4 chunks fed): the cap.
        assert_eq!(interrupt_due(&view(&c, 4, Some(900)), 4, at), c.at(2000));
        assert_eq!(interrupt_due(&view(&c, 4, None), 4, at), c.at(2000));
        // The shell's `^C` and prompt at 1020: 150 ms of quiet after it; more output
        // moves it.
        assert_eq!(interrupt_due(&view(&c, 6, Some(1020)), 4, at), c.at(1170));
        assert_eq!(interrupt_due(&view(&c, 7, Some(1100)), 4, at), c.at(1250));
        // Output that never stops: the cap still types the line.
        assert_eq!(interrupt_due(&view(&c, 90, Some(1985)), 4, at), c.at(2000));
    }
}
