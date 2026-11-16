//! The startup gate: after a launch, holds programmatic input until the agent is safe to
//! type into, and answers its startup dialogs. A pure state machine — the caller passes
//! the time, the screen text and the profile, and performs the returned effects (keys to
//! write, lines to log) outside its locks.
//!
//! Readiness, strongest first:
//! 1. the ready composer on the **current screen** (evidence) — ready at once;
//! 2. a dialog answered with Enter, then the ready screen, output quiet or the hard cap;
//! 3. neither: output quiet / the hard cap only lift `blocking` (the host's overlay); input
//!    waits until the trust window ends.
//!
//! Never ready while the last screen read shows a dialog: one the gate does not answer
//! (no key, steering off, budgets used up) parks it in `TrustPrompt` — overlay lifted, so
//! the user can answer — until the dialog leaves the screen.
//!
//! Dialogs are read from the current screen, or — while the window is open — from a raw
//! output buffer (a dialog drawn with cursor moves the screen read missed; never used for
//! readiness). Keys are bounded: Enter and arrow budgets with cooldowns, reset when the
//! dialog leaves the screen; the first key for a dialog waits until it settled. For a
//! short while after readiness the screen is still checked for a dialog (codex draws a
//! ready-looking composer before its dialogs).
//!
//! Profile code is host code: a panic in it is caught, reported once, and the profile is
//! not called again (screens read as unknown; readiness falls back to the timers).

#[cfg(test)]
mod replay_tests;
mod rolling;
#[cfg(test)]
mod tests;

use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use rolling::{RollingText, Utf8Stream};

use super::{AgentProfile, AgentState, Dialog, DialogKind, ScreenState, Timing, TrustKey};
use crate::screen::catch_panic;

/// A re-evaluation running this much later than due means the monitor was stalled (a full
/// event receiver): what was drawn meanwhile may still be unread.
const LATE_WAKE: Duration = Duration::from_millis(250);

/// What the caller does for the gate, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GateEffect {
    /// Write this key to the PTY.
    Key {
        /// The dialog it answers.
        kind: DialogKind,
        /// The key.
        key: TrustKey,
    },
    /// A diagnostic line (trace level).
    Note(String),
    /// The trust window closed and no screen of this launch was recognised (neither a
    /// dialog nor the ready composer): log the screen so a reworded CLI can be triaged.
    MarkersUnrecognised {
        /// Since the launch.
        elapsed: Duration,
    },
    /// The profile panicked (the message); it is not called again.
    ProfilePanicked(String),
}

/// The deadlines the gate keeps, in firing order for equal times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Due {
    Quiet,
    Max,
    Window,
    Reeval,
}

/// Port of the embedded terminal panel's startup gate (`armAgentStartup`, `noteOutput`,
/// `evaluateStartupScreen`, `approveTrustPrompt`, `markAgentReady`, `resetSessionGates`).
#[derive(Clone, Debug)]
pub(crate) struct StartupGate {
    timing: Timing,
    steer_dialogs: bool,
    /// An agent was launched in this session and has not been seen leaving.
    running: bool,
    /// Safe for programmatic input.
    ready: bool,
    /// The host should mask the terminal (startup not settled, or a dialog being steered).
    blocking: bool,
    /// Output went quiet (or the cap passed) since the last arm or Enter.
    settled: bool,
    /// A dialog was answered with Enter since the last arm.
    trust_confirmed: bool,
    /// The ready composer was on screen.
    ready_evidence: bool,
    /// The session ended.
    ended: bool,
    /// Dialogs are answered until then; the window's own deadline too.
    trust_watch_until: Option<Instant>,
    quiet_deadline: Option<Instant>,
    max_deadline: Option<Instant>,
    /// Evaluate the screen again then (after a key's cooldown).
    reeval_deadline: Option<Instant>,
    /// After readiness: until then a dialog on screen still un-readies the gate.
    dialog_watch_until: Option<Instant>,
    decoder: Utf8Stream,
    output: RollingText,
    /// The current screen's last classification.
    last_screen: ScreenState,
    approve_attempts: u32,
    /// When the gate first saw the dialog the budgets are for: its first key waits until
    /// `dialog_settle` after it.
    dialog_seen_at: Option<Instant>,
    last_approve_at: Option<Instant>,
    /// Hash of the text the last Enter was chosen from: another Enter waits until the text
    /// changed, or until `enter_redraw_until` (then the Enter counts as lost).
    enter_source: Option<u64>,
    enter_redraw_until: Option<Instant>,
    nav_presses: u32,
    last_nav_at: Option<Instant>,
    /// Hash of the text the last arrow key was chosen from: the next arrow waits until the
    /// text changed (the selection moved), or until `nav_redraw_until`.
    nav_source: Option<u64>,
    /// The last arrow key counts as lost (sent again from an unchanged screen) from then on.
    nav_redraw_until: Option<Instant>,
    /// The profile panicked once: it is not called again.
    profile_failed: bool,
    /// A "left to the user" / budget note was logged for the dialog on screen.
    noted: bool,
    // Per launch, for the marker report.
    armed_at: Option<Instant>,
    saw_dialog: bool,
    saw_ready: bool,
    marker_reported: bool,
    out: Vec<GateEffect>,
}

impl StartupGate {
    pub(crate) fn new(timing: Timing, steer_dialogs: bool) -> Self {
        let cap = timing.startup_buffer_chars;
        Self {
            timing,
            steer_dialogs,
            running: false,
            ready: false,
            blocking: false,
            settled: false,
            trust_confirmed: false,
            ready_evidence: false,
            ended: false,
            trust_watch_until: None,
            quiet_deadline: None,
            max_deadline: None,
            reeval_deadline: None,
            dialog_watch_until: None,
            decoder: Utf8Stream::default(),
            output: RollingText::new(cap),
            last_screen: ScreenState::Unknown,
            approve_attempts: 0,
            dialog_seen_at: None,
            last_approve_at: None,
            enter_source: None,
            enter_redraw_until: None,
            nav_presses: 0,
            last_nav_at: None,
            nav_source: None,
            nav_redraw_until: None,
            profile_failed: false,
            noted: false,
            armed_at: None,
            saw_dialog: false,
            saw_ready: false,
            marker_reported: false,
            out: Vec::new(),
        }
    }

    /// An agent was launched and has not been seen leaving (the delivery jobs wait while
    /// it runs and is not ready).
    pub(crate) fn running(&self) -> bool {
        self.running
    }

    pub(crate) fn ready(&self) -> bool {
        self.ready
    }

    pub(crate) fn blocking(&self) -> bool {
        self.blocking
    }

    pub(crate) fn ended(&self) -> bool {
        self.ended
    }

    /// `Exited` once the session ended; `ShellOnly` while no agent runs; `Ready`; a dialog
    /// on the last screen read ⇒ `TrustPrompt`; else `Starting`.
    pub(crate) fn state(&self) -> AgentState {
        if self.ended {
            AgentState::Exited
        } else if !self.running {
            AgentState::ShellOnly
        } else if self.ready {
            AgentState::Ready
        } else if matches!(self.last_screen, ScreenState::Dialog(_)) {
            AgentState::TrustPrompt
        } else {
            AgentState::Starting
        }
    }

    /// Whether [`evaluate`](Self::evaluate) would read a screen now (callers skip the
    /// screen read otherwise).
    pub(crate) fn wants_screen(&self, now: Instant) -> bool {
        self.running && !self.profile_failed && (!self.ready || self.watching_after_ready(now))
    }

    /// The agent was launched (`launchAgent`): it runs from now on, and the gate is armed.
    pub(crate) fn launch(&mut self, now: Instant) {
        self.running = true;
        self.arm(now);
    }

    /// `armAgentStartup`: hold input until the agent's startup settles; open the trust
    /// window; start the hard cap.
    fn arm(&mut self, now: Instant) {
        self.reset_gate();
        self.blocking = true;
        self.ready = false;
        self.open_window(now);
        self.quiet_deadline = None;
        self.max_deadline = Some(now + self.timing.startup_max);
        self.dialog_watch_until = None;
        self.armed_at = Some(now);
        self.saw_dialog = false;
        self.saw_ready = false;
        self.marker_reported = false;
    }

    /// `resetStartupGate`.
    fn reset_gate(&mut self) {
        self.blocking = false;
        self.settled = false;
        self.trust_confirmed = false;
        self.ready_evidence = false;
        self.decoder.reset();
        self.output.clear();
        self.last_screen = ScreenState::Unknown;
        self.fresh_budgets();
    }

    /// Fresh Enter and arrow budgets (`clearStartupNav` plus the attempt counters).
    fn fresh_budgets(&mut self) {
        self.approve_attempts = 0;
        self.dialog_seen_at = None;
        self.last_approve_at = None;
        self.enter_source = None;
        self.enter_redraw_until = None;
        self.nav_presses = 0;
        self.last_nav_at = None;
        self.nav_source = None;
        self.nav_redraw_until = None;
        self.reeval_deadline = None;
        self.noted = false;
    }

    /// The shell is in front again: the agent is gone (the panel clears `agentRunning`).
    /// Not ready, nothing to mask, no timers — until the next [`launch`](Self::launch).
    pub(crate) fn leave(&mut self) {
        self.running = false;
        self.ready = false;
        self.quiet_deadline = None;
        self.max_deadline = None;
        self.trust_watch_until = None;
        self.dialog_watch_until = None;
        self.reset_gate();
    }

    /// `resetSessionGates`: the session is gone.
    pub(crate) fn end_session(&mut self) {
        self.running = false;
        self.ready = false;
        self.ended = true;
        self.quiet_deadline = None;
        self.max_deadline = None;
        self.trust_watch_until = None;
        self.dialog_watch_until = None;
        self.reset_gate();
    }

    fn window_open(&self, now: Instant) -> bool {
        self.trust_watch_until.is_some_and(|until| now < until)
    }

    /// Opens the trust window. The raw buffer starts over: output is decoded only while
    /// the window is open, so the decoder may hold a stale partial sequence.
    fn open_window(&mut self, now: Instant) {
        self.trust_watch_until = Some(now + self.timing.trust_watch);
        self.decoder.reset();
        self.output.clear();
    }

    /// The dialog the last screen read showed.
    fn dialog_on_screen(&self) -> Option<Dialog> {
        match self.last_screen {
            ScreenState::Dialog(dialog) => Some(dialog),
            _ => None,
        }
    }

    fn watching_after_ready(&self, now: Instant) -> bool {
        self.dialog_watch_until.is_some_and(|until| now < until)
    }

    /// `noteOutput`, raw bookkeeping per chunk: the rolling buffer for the whole trust
    /// window — independent of readiness, which can come during the pause between the
    /// shell's echo of the launch line and the agent's dialog — and, until ready, the
    /// quiet countdown. Output is decoded only for the buffer (the window reopens with a
    /// fresh decoder).
    pub(crate) fn on_output(&mut self, bytes: &[u8], now: Instant) {
        if !self.running {
            return;
        }
        if self.window_open(now) {
            let text = self.decoder.decode(bytes);
            self.output.push(&text);
        }
        if self.ready {
            return;
        }
        self.quiet_deadline = Some(now + self.timing.startup_quiet);
    }

    /// `evaluateStartupScreen` on `screen` (the current screen's rows joined by `\n`):
    /// a dialog is steered; leaving a dialog restores the key budgets; the ready composer
    /// readies the gate; otherwise the raw buffer may still show a dialog.
    pub(crate) fn evaluate(
        &mut self,
        screen: &str,
        profile: &dyn AgentProfile,
        now: Instant,
    ) -> Vec<GateEffect> {
        self.evaluate_at(screen, profile, now);
        std::mem::take(&mut self.out)
    }

    fn evaluate_at(&mut self, screen: &str, profile: &dyn AgentProfile, now: Instant) {
        if !self.running {
            return;
        }
        if self.ready {
            self.watch_after_ready(screen, profile, now);
            return;
        }
        let state = guard(&mut self.profile_failed, &mut self.out, || {
            profile.classify(screen)
        })
        .unwrap_or(ScreenState::Unknown);
        let prev = std::mem::replace(&mut self.last_screen, state);
        if let ScreenState::Dialog(dialog) = state {
            if matches!(prev, ScreenState::Dialog(p) if p.kind != dialog.kind) {
                // Another dialog drawn straight over the first (claude: trust, then the
                // bypass warning): it gets its own budgets.
                self.fresh_budgets();
            }
            self.saw_dialog = true;
            self.approve(dialog, screen, now);
            return;
        }
        if matches!(prev, ScreenState::Dialog(_)) {
            // The dialog left the screen: a second, distinct one gets fresh budgets, while
            // a stuck redraw loop stays bounded by them.
            self.fresh_budgets();
        }
        if state == ScreenState::InputReady {
            self.saw_ready = true;
            self.ready_evidence = true;
            self.mark_ready(now);
            return;
        }
        // Unknown: the raw buffer may hold a dialog the screen read missed (detection only —
        // stale buffer text never holds readiness back).
        if self.window_open(now) {
            let found = guard(&mut self.profile_failed, &mut self.out, || {
                profile.dialog(self.output.text())
            });
            if let Some(dialog) = found.flatten() {
                self.saw_dialog = true;
                let source = self.output.text().to_owned();
                self.approve(dialog, &source, now);
            }
        }
    }

    /// Not in the panel's gate: while the post-ready watch runs, a dialog on the current
    /// screen (never the buffer — after readiness it holds transcript text) wins over
    /// readiness and is steered with fresh budgets and a reopened window.
    fn watch_after_ready(&mut self, screen: &str, profile: &dyn AgentProfile, now: Instant) {
        if !self.watching_after_ready(now) {
            return;
        }
        let found = guard(&mut self.profile_failed, &mut self.out, || {
            profile.dialog(screen)
        });
        let Some(dialog) = found.flatten() else {
            return;
        };
        self.ready = false;
        self.blocking = true;
        self.last_screen = ScreenState::Dialog(dialog);
        self.dialog_watch_until = None;
        self.fresh_budgets();
        self.open_window(now);
        self.max_deadline = Some(now + self.timing.startup_max);
        self.saw_dialog = true;
        self.note(format!(
            "{:?} dialog after ready: not ready until it is answered",
            dialog.kind
        ));
        self.approve(dialog, screen, now);
    }

    /// `approveTrustPrompt`: one bounded step toward the profile's answer — an arrow key
    /// toward the wanted option, or Enter once it is selected. `source` is the text the
    /// dialog was read from.
    fn approve(&mut self, dialog: Dialog, source: &str, now: Instant) {
        if !self.running {
            return;
        }
        if !self.window_open(now) {
            self.note_once(format!(
                "{:?} dialog after the trust window: left to the user",
                dialog.kind
            ));
            return;
        }
        let Some(key) = dialog.key else {
            self.note_once(format!(
                "{:?} dialog: selection unreadable or not answered by the profile, left to the user",
                dialog.kind
            ));
            return;
        };
        if !self.steer_dialogs {
            self.note_once(format!(
                "{:?} dialog: steering is off, left to the user",
                dialog.kind
            ));
            return;
        }
        // Not in the panel's gate: the first key waits until the dialog settled (claude
        // resets its trust dialog's selection when a key comes right after its first draw).
        let seen = *self.dialog_seen_at.get_or_insert(now);
        let settled = seen + self.timing.dialog_settle;
        if self.approve_attempts == 0 && self.nav_presses == 0 && now < settled {
            // Masked from now on, as while a key's redraw is awaited.
            self.blocking = true;
            self.quiet_deadline = None;
            // A dialog seen inside the window still gets its first key: the window stays
            // open past the settle (by a cooldown, for a late wake-up).
            let keep_open = settled + self.timing.approve_cooldown;
            if let Some(until) = self.trust_watch_until.filter(|until| *until < keep_open) {
                self.note(format!(
                    "trust window kept open {} ms for the dialog's first key",
                    (keep_open - until).as_millis()
                ));
                self.trust_watch_until = Some(keep_open);
            }
            self.reeval_at(settled);
            return;
        }
        let cooldown = self.timing.approve_cooldown;
        if key != TrustKey::Enter {
            self.navigate(dialog.kind, key, source, now);
            return;
        }
        if self.approve_attempts >= self.timing.approve_attempts {
            self.note_once(format!("{:?} dialog: Enter budget used up", dialog.kind));
            return;
        }
        if let Some(last) = self.last_approve_at.filter(|last| now < *last + cooldown) {
            // Not in the panel's gate: the skipped Enter is retried after the cooldown even
            // when no more output comes (a second dialog right after the first).
            self.reeval_at(last + cooldown + self.timing.nav_reeval_extra);
            return;
        }
        // Not in the panel's gate: like an arrow, another Enter waits until the screen
        // changed — the answered dialog may still be drawn, and an Enter on whatever comes
        // next could pick its default (claude's bypass warning: "No, exit") — or until
        // `nav_redraw_wait`, after which the first Enter counts as lost and is retried.
        let source = text_hash(source);
        if let Some(until) = self.enter_redraw_until {
            if self.enter_source == Some(source) && now < until {
                self.reeval_at(until);
                return;
            }
        }
        self.approve_attempts += 1;
        self.last_approve_at = Some(now);
        self.enter_source = Some(source);
        self.enter_redraw_until = Some(now + self.timing.nav_redraw_wait);
        // Retry a lost Enter on a dialog that does not redraw (no output to re-evaluate on).
        self.reeval_at(now + self.timing.nav_redraw_wait);
        self.output.clear();
        self.settled = false;
        self.trust_confirmed = true;
        // A dialog on screen means not ready, even if a premature settle lifted the
        // overlay: block again and hold input until the post-confirm redraw reads ready
        // (the cap is only the failsafe).
        self.blocking = true;
        self.ready = false;
        self.quiet_deadline = None;
        self.max_deadline = Some(now + self.timing.startup_max);
        self.out.push(GateEffect::Key {
            kind: dialog.kind,
            key,
        });
    }

    /// The arrow path of `approveTrustPrompt`, with its own budget and cooldown; a
    /// re-evaluation after the cooldown sends the next key or the Enter even if the redraw
    /// was the last output. The next arrow also waits for the screen to change — a key
    /// sent but not yet drawn must not be sent again (overshoot) — or for
    /// `nav_redraw_wait`, after which the key counts as lost.
    fn navigate(&mut self, kind: DialogKind, key: TrustKey, source: &str, now: Instant) {
        if self.nav_presses >= self.timing.nav_presses {
            self.note_once(format!("{kind:?} dialog: arrow budget used up"));
            return;
        }
        let source = text_hash(source);
        if let Some(last) = self.last_nav_at {
            let cooled = last + self.timing.approve_cooldown;
            if now < cooled {
                self.reeval_at(cooled + self.timing.nav_reeval_extra);
                return;
            }
        }
        if let Some(until) = self.nav_redraw_until {
            if self.nav_source == Some(source) && now < until {
                self.reeval_at(until);
                return;
            }
        }
        self.nav_presses += 1;
        self.last_nav_at = Some(now);
        self.nav_source = Some(source);
        self.nav_redraw_until = Some(now + self.timing.nav_redraw_wait);
        // Keep the dialog masked while it is steered: no quiet settle while the redraw is
        // awaited (the next output re-arms it; the cap stays the failsafe).
        self.blocking = true;
        self.quiet_deadline = None;
        self.max_deadline = Some(now + self.timing.startup_max);
        self.reeval_deadline =
            Some(now + self.timing.approve_cooldown + self.timing.nav_reeval_extra);
        self.out.push(GateEffect::Key { kind, key });
    }

    /// Schedules a re-evaluation, keeping an earlier one.
    fn reeval_at(&mut self, at: Instant) {
        self.reeval_deadline = Some(self.reeval_deadline.map_or(at, |r| r.min(at)));
    }

    /// `markAgentReady`: the startup UI settled. The overlay lifts; input waits until a
    /// dialog was answered, the ready screen was seen, or the trust window is over — and,
    /// unlike the panel's gate, never while the last screen read shows a dialog (parked in
    /// `TrustPrompt` until it leaves the screen; the next output re-runs the evaluation).
    fn mark_ready(&mut self, now: Instant) {
        self.quiet_deadline = None;
        self.max_deadline = None;
        self.settled = true;
        self.blocking = false;
        if self.dialog_on_screen().is_some() {
            return;
        }
        if !self.trust_confirmed && !self.ready_evidence && self.window_open(now) {
            return;
        }
        let why = if self.ready_evidence {
            "ready composer on screen"
        } else if self.trust_confirmed {
            "dialog answered, startup settled"
        } else {
            "trust window over"
        };
        self.trust_watch_until = None;
        self.reset_gate();
        self.ready = true;
        self.dialog_watch_until = Some(now + self.timing.ready_dialog_watch);
        self.note(format!("ready ({why})"));
    }

    /// The earliest deadline.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.deadlines().map(|(at, _)| at).min()
    }

    fn deadlines(&self) -> impl Iterator<Item = (Instant, Due)> + use<> {
        [
            (self.quiet_deadline, Due::Quiet),
            (self.max_deadline, Due::Max),
            (self.trust_watch_until, Due::Window),
            (self.reeval_deadline, Due::Reeval),
        ]
        .into_iter()
        .filter_map(|(at, due)| at.map(|at| (at, due)))
    }

    /// Fires every deadline due at `now`. The timers fire earliest first, each at its own
    /// time (as the panel's timers would have): quiet and cap settle the gate, the window's
    /// end reports unrecognised markers and readies a settled gate. A due re-evaluation
    /// then reads `screen` (the current screen) once, at `now` — a late wake-up never
    /// replays several keys against one screen; one later than [`LATE_WAKE`] restarts the
    /// last arrow's redraw wait (its redraw may still be unread) instead of counting the
    /// key as lost.
    pub(crate) fn on_deadline(
        &mut self,
        now: Instant,
        screen: &str,
        profile: &dyn AgentProfile,
    ) -> Vec<GateEffect> {
        let mut reeval_due = None;
        while let Some((at, due)) = self
            .deadlines()
            .filter(|(at, _)| *at <= now)
            .min_by_key(|(at, _)| *at)
        {
            match due {
                Due::Quiet => {
                    self.quiet_deadline = None;
                    self.mark_ready(at);
                }
                Due::Max => {
                    self.max_deadline = None;
                    self.mark_ready(at);
                }
                Due::Window => {
                    self.trust_watch_until = None;
                    self.report_markers(at);
                    if self.settled && !self.ready {
                        self.mark_ready(at);
                        if let Some(dialog) = self.dialog_on_screen() {
                            self.note(format!(
                                "trust window over with a {:?} dialog on screen: not ready until it leaves the screen",
                                dialog.kind
                            ));
                        }
                    }
                }
                Due::Reeval => {
                    self.reeval_deadline = None;
                    reeval_due = Some(at);
                }
            }
        }
        if let Some(due) = reeval_due {
            if now.saturating_duration_since(due) > LATE_WAKE {
                for until in [&mut self.nav_redraw_until, &mut self.enter_redraw_until]
                    .into_iter()
                    .flatten()
                {
                    *until = (*until).max(now + self.timing.nav_redraw_wait);
                }
            }
            self.evaluate_at(screen, profile, now);
        }
        std::mem::take(&mut self.out)
    }

    /// Once per launch, when the window closes and nothing of this launch was recognised.
    fn report_markers(&mut self, at: Instant) {
        if self.marker_reported || self.saw_dialog || self.saw_ready {
            return;
        }
        let Some(armed) = self.armed_at else {
            return;
        };
        self.marker_reported = true;
        self.out.push(GateEffect::MarkersUnrecognised {
            elapsed: at.saturating_duration_since(armed),
        });
    }

    fn note(&mut self, line: String) {
        self.out.push(GateEffect::Note(line));
    }

    /// One note per dialog on screen (evaluations repeat on every output chunk).
    fn note_once(&mut self, line: String) {
        if !self.noted {
            self.noted = true;
            self.note(line);
        }
    }
}

/// Calls host profile code. A panic marks the profile failed (one
/// [`GateEffect::ProfilePanicked`]) and gives `None`, as does any call after that.
fn guard<T>(failed: &mut bool, out: &mut Vec<GateEffect>, call: impl FnOnce() -> T) -> Option<T> {
    if *failed {
        return None;
    }
    match catch_panic(call) {
        Ok(value) => Some(value),
        Err(message) => {
            *failed = true;
            out.push(GateEffect::ProfilePanicked(message));
            None
        }
    }
}

/// Identifies a text to tell whether it changed (within one process).
fn text_hash(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}
