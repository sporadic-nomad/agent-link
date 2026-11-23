//! Change notification for pull receivers. A producer calls [`Wakeup::wake_all`] after
//! releasing its own locks whenever a receiver's observable state changed. Waiters
//! rerun their own check after every wakeup; the primitive carries no data. Its mutex
//! is a leaf lock: no other lock is taken and no host code runs while it is held.
//!
//! A blocking waiter reads the epoch before each check and an async waiter registers
//! before each check, so a change between a check and the wait is never missed.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    epoch: u64,
    #[cfg(test)]
    parked: usize,
    /// Holds the next waiter between its failed check and its wait.
    #[cfg(test)]
    between: Option<Box<dyn FnOnce() + Send>>,
}

#[derive(Default)]
pub(super) struct Wakeup {
    state: Mutex<State>,
    cv: Condvar,
    #[cfg(feature = "tokio")]
    tasks: tokio::sync::Notify,
}

impl Wakeup {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wake every blocked thread and waiting task. Never call it under a receiver lock.
    pub(super) fn wake_all(&self) {
        {
            let mut state = self.lock();
            state.epoch = state.epoch.wrapping_add(1);
        }
        self.cv.notify_all();
        #[cfg(feature = "tokio")]
        self.tasks.notify_waiters();
    }

    /// Run `check` until it returns a value, blocking between attempts until a wakeup.
    /// `None` once `timeout` elapses without one. The final attempt can take as long as
    /// `check` itself. A timeout too large for an `Instant` waits without a deadline.
    pub(super) fn wait_for<T>(
        &self,
        timeout: Duration,
        mut check: impl FnMut() -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            let seen = self.lock().epoch;
            if let Some(value) = check() {
                return Some(value);
            }
            #[cfg(test)]
            self.pause_between();
            if !self.wait_since(seen, deadline) {
                return None;
            }
        }
    }

    /// The async form of [`wait_for`](Self::wait_for) without a deadline. Holds no lock
    /// across an await and spawns nothing. Dropping the future between attempts is safe
    /// whenever `check` either returns a value or changes nothing.
    #[cfg(feature = "tokio")]
    pub(super) async fn until<T>(&self, mut check: impl FnMut() -> Option<T>) -> T {
        loop {
            // Created before the check: `notify_waiters` reaches every `Notified` that
            // exists at the call, polled or not.
            let changed = self.tasks.notified();
            if let Some(value) = check() {
                return value;
            }
            #[cfg(test)]
            self.pause_between();
            changed.await;
        }
    }

    fn wait_since(&self, seen: u64, deadline: Option<Instant>) -> bool {
        let mut state = self.lock();
        loop {
            let wait = match deadline {
                Some(deadline) => deadline.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(3600),
            };
            if wait.is_zero() {
                return false;
            }
            if state.epoch != seen {
                return true;
            }
            #[cfg(test)]
            {
                state.parked += 1;
            }
            state = self
                .cv
                .wait_timeout(state, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
            #[cfg(test)]
            {
                state.parked -= 1;
            }
        }
    }

    /// Threads currently blocked in [`wait_for`](Self::wait_for).
    #[cfg(test)]
    pub(super) fn parked(&self) -> usize {
        self.lock().parked
    }

    /// Run `pause` on the next waiter after its failed check, before it blocks or awaits.
    #[cfg(test)]
    pub(super) fn pause_next(&self, pause: impl FnOnce() + Send + 'static) {
        self.lock().between = Some(Box::new(pause));
    }

    #[cfg(test)]
    fn pause_between(&self) {
        let pause = self.lock().between.take();
        if let Some(pause) = pause {
            pause();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wakeup_during_an_empty_pull_cannot_extend_its_deadline() {
        let wake = Wakeup::default();
        let mut attempts = 0;
        let result = wake.wait_for(Duration::ZERO, || {
            attempts += 1;
            wake.wake_all();
            (attempts > 1).then_some(())
        });
        assert_eq!(result, None);
        assert_eq!(attempts, 1);
    }
}
