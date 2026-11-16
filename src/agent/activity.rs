//! [`Activity`] and the tracker that derives it from the window title and progress
//! reports. Informational only: it never gates a delivery.

use std::time::{Duration, Instant};

/// Whether the agent is working, as its terminal signals tell. Informational: an agent that
/// emits neither signal stays `Idle` or `Unknown`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Activity {
    /// A working signal is on: a Braille spinner glyph leads the window title, or an
    /// active progress report (OSC 9;4) is fresh.
    Working,
    /// Neither signal is on (after a short debounce), or the agent exited.
    Idle,
    /// Nothing seen since the agent was (re)launched.
    Unknown,
}

/// A title whose first non-whitespace character is in the Braille Patterns block
/// (U+2800–U+28FF): agent CLIs prefix their title with such a spinner while generating.
pub(crate) fn title_working(title: &str) -> bool {
    title
        .trim_start()
        .chars()
        .next()
        .is_some_and(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
}

/// Folds the two working sources (title spinner, progress) into one debounced
/// [`Activity`]: working applies at once, idle only after `idle_debounce` without either.
/// Pure and deadline-driven: the caller passes the time and calls
/// [`on_deadline`](Self::on_deadline) at [`next_deadline`](Self::next_deadline). Every
/// method returns the new state when it changed.
#[derive(Debug)]
pub(crate) struct ActivityTracker {
    idle_debounce: Duration,
    progress_stale: Duration,
    state: Activity,
    title_working: bool,
    progress_working: bool,
    /// The pending idle transition.
    idle_at: Option<Instant>,
    /// When an active progress report goes stale (no keep-alive, no clear).
    stale_at: Option<Instant>,
}

impl ActivityTracker {
    pub(crate) fn new(idle_debounce: Duration, progress_stale: Duration) -> Self {
        Self {
            idle_debounce,
            progress_stale,
            state: Activity::Unknown,
            title_working: false,
            progress_working: false,
            idle_at: None,
            stale_at: None,
        }
    }

    pub(crate) fn state(&self) -> Activity {
        self.state
    }

    /// A new window title (every OSC 0/2 set).
    pub(crate) fn push_title(&mut self, title: &str, now: Instant) -> Option<Activity> {
        self.title_working = title_working(title);
        self.recompute(now)
    }

    /// A progress report: `true` for any active state, `false` for a clear. An active one
    /// re-arms the staleness window, so a run that stops reporting without a clear (killed
    /// mid-turn) still drops back to idle.
    pub(crate) fn push_progress(&mut self, working: bool, now: Instant) -> Option<Activity> {
        self.progress_working = working;
        self.stale_at = working.then(|| now + self.progress_stale);
        self.recompute(now)
    }

    /// The agent exited or left the foreground: idle at once, nothing pending.
    pub(crate) fn mark_exited(&mut self) -> Option<Activity> {
        self.clear();
        self.set(Activity::Idle)
    }

    /// The agent is (re)launching: unknown, nothing pending.
    pub(crate) fn reset(&mut self) -> Option<Activity> {
        self.clear();
        self.set(Activity::Unknown)
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        match (self.idle_at, self.stale_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Fires every deadline due at `now`, earliest first, each at its own time.
    pub(crate) fn on_deadline(&mut self, now: Instant) -> Option<Activity> {
        let before = self.state;
        while let Some(at) = self.next_deadline().filter(|at| *at <= now) {
            if self.stale_at == Some(at) {
                self.stale_at = None;
                self.progress_working = false;
                self.recompute(at);
            } else {
                self.idle_at = None;
                self.set(Activity::Idle);
            }
        }
        (self.state != before).then_some(self.state)
    }

    fn clear(&mut self) {
        self.title_working = false;
        self.progress_working = false;
        self.idle_at = None;
        self.stale_at = None;
    }

    /// Working from either source applies at once and drops a pending idle; with neither,
    /// the single idle deadline is (re)started unless already idle.
    fn recompute(&mut self, now: Instant) -> Option<Activity> {
        if self.title_working || self.progress_working {
            self.idle_at = None;
            return self.set(Activity::Working);
        }
        if self.state != Activity::Idle {
            self.idle_at = Some(now + self.idle_debounce);
        }
        None
    }

    fn set(&mut self, next: Activity) -> Option<Activity> {
        (self.state != next).then(|| {
            self.state = next;
            next
        })
    }
}

#[cfg(test)]
mod tests {
    //! Ports of the embedded terminal's activity tracker (`createActivityTracker`) and its
    //! tests; each test names the function it mirrors.
    use super::*;

    const DEBOUNCE: Duration = Duration::from_millis(600);
    const STALE: Duration = Duration::from_millis(2500);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn tracker() -> (ActivityTracker, Instant) {
        (ActivityTracker::new(DEBOUNCE, STALE), Instant::now())
    }

    /// `classifyTitle`: a Braille glyph first (after whitespace) is working, anything else
    /// is no signal.
    #[test]
    fn a_braille_title_is_working() {
        assert!(title_working("⠋ fake"));
        assert!(title_working("  ⣿ x"));
        assert!(title_working("\u{2800}"));
        assert!(!title_working("✳ Claude Code"));
        assert!(!title_working("◐ Claude Code"));
        assert!(!title_working("fake ⠋"));
        assert!(!title_working(""));
        assert!(!title_working("   "));
    }

    /// `pushTitle`: working applies at once; idle follows only after the debounce, and
    /// another frame inside it cancels the idle.
    #[test]
    fn working_is_immediate_and_idle_is_debounced() {
        let (mut t, t0) = tracker();
        assert_eq!(t.state(), Activity::Unknown);
        assert_eq!(t.push_title("⠋ fake", t0), Some(Activity::Working));
        assert_eq!(t.push_title("⠙ fake", t0 + ms(100)), None);
        assert_eq!(t.push_title("fake", t0 + ms(200)), None);
        assert_eq!(t.next_deadline(), Some(t0 + ms(800)));
        assert_eq!(t.on_deadline(t0 + ms(799)), None);
        // A spinner frame inside the debounce keeps it working.
        assert_eq!(t.push_title("⠹ fake", t0 + ms(500)), None);
        assert_eq!(t.next_deadline(), None);
        assert_eq!(t.push_title("fake", t0 + ms(600)), None);
        assert_eq!(t.on_deadline(t0 + ms(1200)), Some(Activity::Idle));
        assert_eq!(t.state(), Activity::Idle);
        // Already idle: no timer, no event.
        assert_eq!(t.push_title("fake", t0 + ms(1300)), None);
        assert_eq!(t.next_deadline(), None);
    }

    /// From `unknown`, a title without a spinner still settles to idle after the debounce.
    #[test]
    fn an_unknown_tracker_settles_to_idle() {
        let (mut t, t0) = tracker();
        assert_eq!(t.push_title("π - repo", t0), None);
        assert_eq!(t.on_deadline(t0 + DEBOUNCE), Some(Activity::Idle));
    }

    /// `pushProgress`: an active report is working; a clear goes idle after the debounce.
    #[test]
    fn progress_is_a_second_working_source() {
        let (mut t, t0) = tracker();
        assert_eq!(t.push_progress(true, t0), Some(Activity::Working));
        // Title not working, progress still is: stays working.
        assert_eq!(t.push_title("fake", t0 + ms(10)), None);
        assert_eq!(t.state(), Activity::Working);
        assert_eq!(t.push_progress(false, t0 + ms(400)), None);
        assert_eq!(t.on_deadline(t0 + ms(1000)), Some(Activity::Idle));
    }

    /// A progress report that is never repeated nor cleared heals to idle: stale after
    /// `progress_stale`, then the debounce.
    #[test]
    fn stale_progress_self_heals() {
        let (mut t, t0) = tracker();
        t.push_progress(true, t0);
        // A keep-alive re-arms the window.
        t.push_progress(true, t0 + ms(1000));
        assert_eq!(t.on_deadline(t0 + ms(2600)), None);
        assert_eq!(t.next_deadline(), Some(t0 + ms(3500)));
        // Stale at 3.5 s, idle 600 ms later — both fire on one late wake-up, in order.
        assert_eq!(t.on_deadline(t0 + ms(5000)), Some(Activity::Idle));
        assert_eq!(t.next_deadline(), None);
    }

    /// `markExited`: idle at once, pending timers dropped.
    #[test]
    fn mark_exited_goes_idle_at_once() {
        let (mut t, t0) = tracker();
        t.push_progress(true, t0);
        t.push_title("⠋ x", t0);
        assert_eq!(t.mark_exited(), Some(Activity::Idle));
        assert_eq!(t.next_deadline(), None);
        assert_eq!(t.mark_exited(), None);
        // A later spinner is working again.
        assert_eq!(t.push_title("⠋ x", t0 + ms(5)), Some(Activity::Working));
    }

    /// `reset`: unknown, pending timers dropped, signals forgotten.
    #[test]
    fn reset_goes_unknown() {
        let (mut t, t0) = tracker();
        t.push_progress(true, t0);
        assert_eq!(t.reset(), Some(Activity::Unknown));
        assert_eq!(t.next_deadline(), None);
        assert_eq!(t.reset(), None);
        // The forgotten progress does not hold "working" against a plain title.
        assert_eq!(t.push_title("x", t0 + ms(1)), None);
        assert_eq!(t.on_deadline(t0 + ms(601)), Some(Activity::Idle));
    }

    /// `onChange` fires only on a real change: every method reports `Some` only then.
    #[test]
    fn changes_are_reported_once() {
        let (mut t, t0) = tracker();
        let seen: Vec<Activity> = [
            t.push_title("⠋", t0),
            t.push_title("⠙", t0 + ms(100)),
            t.push_progress(true, t0 + ms(150)),
            t.push_title("x", t0 + ms(200)),
            t.push_progress(false, t0 + ms(300)),
            t.on_deadline(t0 + ms(2000)),
        ]
        .into_iter()
        .flatten()
        .collect();
        assert_eq!(seen, [Activity::Working, Activity::Idle]);
    }
}
