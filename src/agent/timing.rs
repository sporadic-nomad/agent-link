//! [`Timing`]: every wait, window and budget of the agent controller, with the reason for
//! each default.

use std::time::Duration;

/// The controller's waits, windows and budgets. [`Default`] gives values tuned against the
/// real CLIs (each field gives its reason); tests shorten them.
///
/// Startup gate (after a launch):
/// - Readiness comes from the ready composer on screen, an answered dialog, or the
///   fallbacks: [`startup_quiet`](Self::startup_quiet) of output quiet or the
///   [`startup_max`](Self::startup_max) cap lift [`blocking`](super::AgentTerminal::blocking);
///   without a ready screen or an answered dialog, programmatic input then still waits for
///   the [`trust_watch`](Self::trust_watch) window to end.
/// - Dialogs are answered within that window, bounded by key budgets and cooldowns, the
///   first key after [`dialog_settle`](Self::dialog_settle) (a dialog seen inside the window
///   keeps it open until then). A dialog left unanswered keeps
///   the agent not ready (past the window) until it leaves the screen.
///
/// Every duration is capped at one day when a terminal is spawned (a larger value, e.g.
/// `Duration::MAX` meant as "never", would overflow `Instant` arithmetic).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timing {
    /// Output quiet that counts as "the startup UI settled" — the fallback for an agent
    /// whose ready screen is not recognised. Default 450 ms.
    pub startup_quiet: Duration,
    /// Hard cap on the startup settle; restarted by every dialog key, so a confirmed dialog
    /// that never redraws a ready screen still settles. Default 3 s.
    pub startup_max: Duration,
    /// How long after a launch dialogs are watched for and answered, independent of (and
    /// longer than) the settle: the shell echoes the launch line at once — which starts the
    /// quiet countdown — while a cold agent can take seconds before it draws its trust
    /// dialog. Default 12 s.
    pub trust_watch: Duration,
    /// Enter presses per dialog; a dialog that leaves the screen gives the next one a fresh
    /// budget, a redraw loop stays bounded. Default 2.
    pub approve_attempts: u32,
    /// The first key for a dialog waits until this long after the gate first saw it.
    /// verified: claude 2.1.295 — a key within ~140 ms of its trust dialog's first draw
    /// makes claude redraw the dialog ~160 ms later with "No, exit" selected again,
    /// dropping the keys in between; an Enter that lands after that redraw quits claude.
    /// Keys from 139 ms on were never affected (11 runs). Default 300 ms.
    pub dialog_settle: Duration,
    /// Minimum gap between two Enters, and between two arrow keys. Default 350 ms.
    pub approve_cooldown: Duration,
    /// Arrow keys per dialog, to move its selection onto the wanted option (claude lists
    /// "No, exit" first and selected). Default 4.
    pub nav_presses: u32,
    /// The screen is evaluated again this long after the cooldown that follows a key, so
    /// the next key or the Enter lands even when the redraw was the last output. Default
    /// 50 ms.
    pub nav_reeval_extra: Duration,
    /// After an arrow key the next one waits until the screen changed (the selection moved),
    /// at most this long — then the key counts as lost and is sent again. Keeps a slow
    /// redraw from turning two presses into an overshoot. Default 1 s.
    pub nav_redraw_wait: Duration,
    /// Characters of raw output kept for dialog detection (a dialog drawn with cursor moves
    /// the screen read missed). Never used for readiness. Default 16 000.
    pub startup_buffer_chars: usize,
    /// Before a paste: wait for this much output quiet… Default 120 ms.
    pub pre_paste_quiet: Duration,
    /// …but no longer than this. Default 2 s.
    pub pre_paste_max: Duration,
    /// After a paste, the submit Enter waits until output after the paste was quiet this
    /// long. Above claude's ~100 ms stdin aggregation, so the first Enter normally lands
    /// outside it instead of being absorbed into the paste. Default 250 ms.
    pub paste_echo_quiet: Duration,
    /// Cap on the paste-settle wait. Submission still requires visible pasted text;
    /// reaching this cap alone never authorizes Enter. Default 2 s.
    pub paste_echo_max: Duration,
    /// Submit verification requires this much output quiet or a readable composer verdict
    /// that stays stable across polls during continuing output. Default 250 ms.
    pub submit_verify_quiet: Duration,
    /// Time to obtain a current dialog check before input, to observe the paste, or to
    /// verify each Enter. Default 700 ms per check. A pending composer permits an
    /// opted-in retry only after the Enter's window. Known reset commands allow at
    /// least `startup_max` for their Enter receipt and are never retried.
    pub submit_verify_window: Duration,
    /// How often submit verification polls. Default 100 ms.
    pub submit_verify_poll: Duration,
    /// Additional Enter attempts while the profile still sees the sent text in the
    /// composer. Default 0: an uncertain submit ends `SubmitUnverified` after one Enter.
    /// Opting into retries can submit twice if the agent accepted the first Enter but
    /// has not redrawn its composer yet. Screen evidence cannot guarantee exactly-once
    /// delivery; enable retries only when the integration accepts that risk. Known reset
    /// commands never retry, regardless of this value.
    pub submit_retry_max: u32,
    /// Activity: "idle" is applied only after the working signals stayed off this long, so
    /// spinner frames and brief title clears do not flicker. Default 600 ms.
    pub idle_debounce: Duration,
    /// Activity: a progress report that is not repeated within this window counts as
    /// cleared — agents re-send it about every second, and a killed turn never clears it.
    /// Default 2.5 s.
    pub progress_stale: Duration,
    /// Per-command timeout of an install or sign-in probe. Default 8 s.
    pub probe_timeout: Duration,
    /// A timeout for a commit-point handshake
    /// ([`Revalidate::Handshake`](super::Revalidate::Handshake)) for hosts with none of
    /// their own; the crate waits for the one the handshake carries. Default 10 s.
    pub revalidate_timeout: Duration,
    /// After readiness the current screen is still checked for a startup dialog this long:
    /// codex draws a ready-looking composer for up to ~375 ms before its trust dialog or
    /// update prompt. A dialog in that window un-readies the gate and is steered; after it a
    /// dialog is left alone (the agent's own transcript may contain dialog-like words).
    /// Default 1 s (≥ 2.5× the longest measured composer-to-dialog gap).
    pub ready_dialog_watch: Duration,
}

impl Timing {
    /// The longest duration the controller schedules.
    pub(crate) const MAX_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

    /// Every duration capped at [`MAX_WAIT`](Self::MAX_WAIT).
    pub(crate) fn clamped(mut self) -> Self {
        for d in [
            &mut self.startup_quiet,
            &mut self.startup_max,
            &mut self.trust_watch,
            &mut self.dialog_settle,
            &mut self.approve_cooldown,
            &mut self.nav_reeval_extra,
            &mut self.nav_redraw_wait,
            &mut self.pre_paste_quiet,
            &mut self.pre_paste_max,
            &mut self.paste_echo_quiet,
            &mut self.paste_echo_max,
            &mut self.submit_verify_quiet,
            &mut self.submit_verify_window,
            &mut self.submit_verify_poll,
            &mut self.idle_debounce,
            &mut self.progress_stale,
            &mut self.probe_timeout,
            &mut self.revalidate_timeout,
            &mut self.ready_dialog_watch,
        ] {
            *d = (*d).min(Self::MAX_WAIT);
        }
        self
    }
}

impl Default for Timing {
    fn default() -> Self {
        let ms = Duration::from_millis;
        Self {
            startup_quiet: ms(450),
            startup_max: ms(3_000),
            trust_watch: ms(12_000),
            approve_attempts: 2,
            dialog_settle: ms(300),
            approve_cooldown: ms(350),
            nav_presses: 4,
            nav_reeval_extra: ms(50),
            nav_redraw_wait: ms(1_000),
            startup_buffer_chars: 16_000,
            pre_paste_quiet: ms(120),
            pre_paste_max: ms(2_000),
            paste_echo_quiet: ms(250),
            paste_echo_max: ms(2_000),
            submit_verify_quiet: ms(250),
            submit_verify_window: ms(700),
            submit_verify_poll: ms(100),
            submit_retry_max: 0,
            idle_debounce: ms(600),
            progress_stale: ms(2_500),
            probe_timeout: ms(8_000),
            revalidate_timeout: ms(10_000),
            ready_dialog_watch: ms(1_000),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamping_caps_every_duration_and_keeps_the_rest() {
        let huge = Timing {
            startup_quiet: Duration::MAX,
            trust_watch: Duration::MAX,
            ready_dialog_watch: Duration::MAX,
            ..Timing::default()
        }
        .clamped();
        assert_eq!(huge.startup_quiet, Timing::MAX_WAIT);
        assert_eq!(huge.trust_watch, Timing::MAX_WAIT);
        assert_eq!(huge.ready_dialog_watch, Timing::MAX_WAIT);
        let _ = std::time::Instant::now() + huge.startup_quiet;
        assert_eq!(Timing::default().clamped(), Timing::default());
    }

    #[test]
    fn defaults_use_measured_timings_and_disable_submit_retries() {
        let t = Timing::default();
        let ms = |d: Duration| d.as_millis();
        assert_eq!(
            [
                ms(t.startup_quiet),
                ms(t.startup_max),
                ms(t.trust_watch),
                ms(t.approve_cooldown),
                ms(t.nav_reeval_extra),
                ms(t.pre_paste_quiet),
                ms(t.pre_paste_max),
                ms(t.paste_echo_quiet),
                ms(t.paste_echo_max),
                ms(t.submit_verify_quiet),
                ms(t.submit_verify_window),
                ms(t.submit_verify_poll),
                ms(t.idle_debounce),
                ms(t.progress_stale),
            ],
            [
                450, 3000, 12000, 350, 50, 120, 2000, 250, 2000, 250, 700, 100, 600, 2500
            ]
        );
        assert_eq!(
            (t.approve_attempts, t.nav_presses, t.submit_retry_max),
            (2, 4, 0)
        );
        assert_eq!(t.startup_buffer_chars, 16_000);
        assert_eq!(
            [
                ms(t.probe_timeout),
                ms(t.revalidate_timeout),
                ms(t.ready_dialog_watch),
                ms(t.nav_redraw_wait),
                ms(t.dialog_settle)
            ],
            [8000, 10000, 1000, 1000, 300]
        );
    }
}
