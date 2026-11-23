//! Blocking and async waits on an ambient receiver, driven through the same `publish`,
//! `close` and `stop` paths the worker and the host use. Waiter threads report through
//! channels, so every check has a deadline even if a wakeup were lost, and the
//! parked-waiter count proves a waiter was blocked before the event that must wake it.

use std::sync::Arc;
use std::thread::Scope;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};

use super::{active, receiver, turn};
use crate::agent::turns::TurnOptions;
use crate::agent::turns::ambient::{
    AmbientClosed, AmbientReceiver, AmbientRecvError, AmbientStatus, AmbientTurn,
    AmbientUnavailable, AmbientWaiting,
};

/// Every assertion's deadline.
const WAIT: Duration = Duration::from_secs(6);
/// Longer than every assertion deadline: a lost wakeup fails the assertion first.
const LONG: Duration = Duration::from_secs(30);

fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

/// Run `wait` on a scoped thread; its result arrives on the returned channel.
fn waiter<'scope, T: Send + 'scope>(
    scope: &'scope Scope<'scope, '_>,
    wait: impl FnOnce() -> T + Send + 'scope,
) -> Receiver<T> {
    let (done, result) = bounded(1);
    scope.spawn(move || {
        let _ = done.send(wait());
    });
    result
}

/// Run `wait` on a detached thread, so that a lost wakeup of a wait without a deadline
/// fails the caller's deadline instead of hanging the test.
fn detached<T: Send + 'static>(
    rx: &Arc<AmbientReceiver>,
    wait: impl FnOnce(&AmbientReceiver) -> T + Send + 'static,
) -> Receiver<T> {
    let (done, result) = bounded(1);
    let rx = rx.clone();
    std::thread::spawn(move || {
        let _ = done.send(wait(&rx));
    });
    result
}

fn woken<T>(result: &Receiver<T>) -> T {
    result
        .recv_timeout(WAIT)
        .expect("waiter woken before the deadline")
}

fn parked(rx: &AmbientReceiver, waiters: usize) {
    until("waiters blocked on the receiver", || rx.parked() == waiters);
}

fn text(turn: AmbientTurn) -> String {
    turn.reply.expect("a reply").text
}

fn unavailable(reason: AmbientUnavailable) -> AmbientStatus {
    AmbientStatus::Waiting(AmbientWaiting::Unavailable(reason))
}

fn oversized(lines: u32) -> AmbientStatus {
    AmbientStatus::Active {
        unrecognised: false,
        oversized_lines: lines,
    }
}

/// Holds the receiver's next waiter between its failed check and its wait (or await)
/// until released.
struct Barrier {
    entered: Receiver<()>,
    release: Sender<()>,
}

impl Barrier {
    fn install(rx: &AmbientReceiver) -> Self {
        let (enter, entered) = bounded(1);
        let (release, resume) = bounded::<()>(1);
        rx.pause_next_wait(move || {
            let _ = enter.send(());
            // A barrier dropped by a failed assertion also releases the waiter.
            let _ = resume.recv_timeout(WAIT * 2);
        });
        Self { entered, release }
    }

    fn wait(&self) {
        self.entered
            .recv_timeout(WAIT)
            .expect("waiter reached the pause");
    }

    fn release(self) {
        let _ = self.release.send(());
    }
}

#[test]
fn blocked_waiters_wake_on_status_changes_and_publication_and_time_out_when_idle() {
    let (rx, _cancelled) = receiver(TurnOptions::default());
    let starting = rx.status();
    assert_eq!(
        rx.recv_timeout(Duration::ZERO).unwrap_err(),
        AmbientRecvError::Timeout
    );
    let idle = Duration::from_millis(20);
    let start = Instant::now();
    assert_eq!(rx.wait_status_change(starting, idle), None);
    assert!(start.elapsed() >= idle);

    let not_found = unavailable(AmbientUnavailable::NotFound);
    let missing = unavailable(AmbientUnavailable::MissingDirectory);
    std::thread::scope(|s| {
        let status = waiter(s, || rx.wait_status_change(starting, LONG));
        let reply = waiter(s, || rx.recv_timeout(LONG));
        parked(&rx, 2);
        assert!(rx.shared.publish(Some(not_found), Vec::new()));
        assert_eq!(woken(&status), Some(not_found));
        // The status change woke the receive too. It found nothing and blocked again:
        // waiting, whatever the reason, is not final.
        parked(&rx, 1);
        // A change of waiting reason is a change.
        let status = waiter(s, || rx.wait_status_change(not_found, LONG));
        parked(&rx, 2);
        assert!(rx.shared.publish(Some(missing), Vec::new()));
        assert_eq!(woken(&status), Some(missing));
        parked(&rx, 1);
        // As is a change of the `Active` counters.
        assert!(rx.shared.publish(active(), Vec::new()));
        parked(&rx, 1);
        let status = waiter(s, || rx.wait_status_change(active().unwrap(), LONG));
        parked(&rx, 2);
        assert!(rx.shared.publish(Some(oversized(1)), Vec::new()));
        assert_eq!(woken(&status), Some(oversized(1)));
        parked(&rx, 1);
        assert!(rx.shared.publish(None, vec![turn("woken")]));
        assert_eq!(text(woken(&reply).unwrap()), "woken");
    });

    let start = Instant::now();
    assert_eq!(
        rx.recv_timeout(idle).unwrap_err(),
        AmbientRecvError::Timeout
    );
    assert!(start.elapsed() >= idle);
    assert_eq!(rx.status(), oversized(1));
}

#[test]
fn a_close_wakes_every_waiter_and_is_final() {
    let (rx, _cancelled) = receiver(TurnOptions::default());
    assert!(rx.shared.publish(active(), Vec::new()));
    let seen = rx.status();
    std::thread::scope(|s| {
        let reply = waiter(s, || rx.recv_timeout(LONG));
        let status = waiter(s, || rx.wait_status_change(seen, LONG));
        parked(&rx, 2);
        rx.shared.close(AmbientClosed::TerminalEnded);
        let ended = AmbientClosed::TerminalEnded;
        assert_eq!(woken(&reply).unwrap_err(), AmbientRecvError::Closed(ended));
        assert_eq!(woken(&status), Some(AmbientStatus::Closed(ended)));
    });
    // Later waits return at once, the status even when it equals `seen`.
    let start = Instant::now();
    let closed = AmbientStatus::Closed(AmbientClosed::TerminalEnded);
    assert_eq!(
        rx.recv_timeout(LONG).unwrap_err(),
        AmbientRecvError::Closed(AmbientClosed::TerminalEnded)
    );
    assert_eq!(rx.wait_status_change(closed, LONG), Some(closed));
    assert!(start.elapsed() < WAIT);
}

/// Ambient closes keep what was queued, unlike the controlled tier's gaps: the blocked
/// receive gets the first turn, later receives the rest, and only then the close.
#[test]
fn an_overflow_wakes_waiters_and_the_turns_queued_before_it_come_first() {
    let limits = TurnOptions {
        queue_items: 2,
        ..TurnOptions::default()
    };
    let (rx, _cancelled) = receiver(limits);
    assert!(rx.shared.publish(active(), Vec::new()));
    let seen = rx.status();
    let overflow = AmbientClosed::Overflow;
    std::thread::scope(|s| {
        let reply = waiter(s, || rx.recv_timeout(LONG));
        let status = waiter(s, || rx.wait_status_change(seen, LONG));
        parked(&rx, 2);
        assert!(
            !rx.shared
                .publish(None, vec![turn("a"), turn("b"), turn("c")])
        );
        assert_eq!(woken(&status), Some(AmbientStatus::Closed(overflow)));
        assert_eq!(text(woken(&reply).unwrap()), "a");
    });
    let start = Instant::now();
    assert_eq!(text(rx.recv_timeout(LONG).unwrap()), "b");
    assert_eq!(
        rx.recv_timeout(LONG).unwrap_err(),
        AmbientRecvError::Closed(overflow)
    );
    assert!(rx.try_recv().is_none());
    assert!(start.elapsed() < WAIT);

    // `stop` after a close keeps its reason but discards what was left.
    let (rx, _cancelled) = receiver(TurnOptions::default());
    assert!(rx.shared.publish(active(), vec![turn("dropped")]));
    rx.shared.close(AmbientClosed::TerminalEnded);
    rx.stop();
    assert_eq!(
        rx.recv_timeout(Duration::ZERO).unwrap_err(),
        AmbientRecvError::Closed(AmbientClosed::TerminalEnded)
    );
}

#[test]
fn stop_on_another_thread_wakes_every_waiter_including_ones_without_a_deadline() {
    let (rx, cancelled) = receiver(TurnOptions::default());
    assert!(rx.shared.publish(active(), Vec::new()));
    let rx = Arc::new(rx);
    let seen = rx.status();
    let replies = [
        detached(&rx, |rx| rx.recv_timeout(LONG)),
        detached(&rx, |rx| rx.recv_timeout(Duration::MAX)),
    ];
    let status = detached(&rx, move |rx| rx.wait_status_change(seen, Duration::MAX));
    parked(&rx, 3);
    woken(&detached(&rx, |rx| rx.stop()));
    let stopped = AmbientClosed::Stopped;
    for reply in &replies {
        assert_eq!(woken(reply).unwrap_err(), AmbientRecvError::Closed(stopped));
    }
    assert_eq!(woken(&status), Some(AmbientStatus::Closed(stopped)));
    assert!(cancelled.try_recv().is_ok(), "the worker is woken too");
    assert_eq!(
        rx.recv_timeout(Duration::ZERO).unwrap_err(),
        AmbientRecvError::Closed(stopped)
    );
}

#[test]
fn each_turn_reaches_exactly_one_of_several_waiters() {
    let (rx, _cancelled) = receiver(TurnOptions::default());
    std::thread::scope(|s| {
        let (done, results) = bounded(3);
        for _ in 0..3 {
            let done = done.clone();
            let rx = &rx;
            s.spawn(move || done.send(rx.recv_timeout(LONG)).unwrap());
        }
        parked(&rx, 3);
        assert!(rx.shared.publish(active(), vec![turn("one"), turn("two")]));
        let mut texts = [woken(&results), woken(&results)].map(|r| text(r.unwrap()));
        texts.sort();
        assert_eq!(texts, ["one", "two"]);
        // The third waiter found an empty queue and blocked again.
        parked(&rx, 1);
        rx.stop();
        assert_eq!(
            woken(&results).unwrap_err(),
            AmbientRecvError::Closed(AmbientClosed::Stopped)
        );
    });
}

/// Publication before the waiters start, after both blocked, and racing their first check.
#[test]
fn repeated_publications_reach_waiters_in_every_order() {
    let (rx, _cancelled) = receiver(TurnOptions::default());
    for cycle in 0..24 {
        let text_sent = format!("cycle-{cycle}");
        let seen = rx.status();
        let now = oversized(cycle + 1);
        let publish = || assert!(rx.shared.publish(Some(now), vec![turn(&text_sent)]));
        std::thread::scope(|s| {
            if cycle % 3 == 0 {
                publish();
            }
            let reply = waiter(s, || rx.recv_timeout(LONG));
            let status = waiter(s, || rx.wait_status_change(seen, LONG));
            if cycle % 3 == 1 {
                parked(&rx, 2);
            }
            if cycle % 3 != 0 {
                publish();
            }
            assert_eq!(text(woken(&reply).unwrap()), text_sent);
            assert_eq!(woken(&status), Some(now));
        });
    }
}

/// The waiter has checked and found nothing, but has not blocked yet, when the worker
/// publishes or closes. That wakeup must still end its wait.
#[test]
fn a_change_between_a_check_and_the_wait_is_not_missed() {
    let (rx, _cancelled) = receiver(TurnOptions::default());
    assert!(rx.shared.publish(active(), Vec::new()));
    std::thread::scope(|s| {
        let barrier = Barrier::install(&rx);
        let reply = waiter(s, || rx.recv_timeout(LONG));
        barrier.wait();
        assert!(rx.shared.publish(None, vec![turn("between")]));
        barrier.release();
        assert_eq!(text(woken(&reply).unwrap()), "between");

        let barrier = Barrier::install(&rx);
        let (rx, seen) = (&rx, rx.status());
        let status = waiter(s, move || rx.wait_status_change(seen, LONG));
        barrier.wait();
        rx.shared.close(AmbientClosed::Monitor);
        barrier.release();
        assert_eq!(
            woken(&status),
            Some(AmbientStatus::Closed(AmbientClosed::Monitor))
        );
    });
}

/// `recv` and `status_change` (feature `tokio`) on a current-thread runtime.
#[cfg(feature = "tokio")]
mod tasks {
    use tokio::task::yield_now;
    use tokio::time::timeout;

    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    async fn within<T>(task: tokio::task::JoinHandle<T>) -> T {
        timeout(WAIT, task)
            .await
            .expect("task woken before the deadline")
            .unwrap()
    }

    fn recv(rx: &Arc<AmbientReceiver>) -> tokio::task::JoinHandle<AmbientResult> {
        let rx = rx.clone();
        tokio::spawn(async move { rx.recv().await })
    }

    fn status_change(
        rx: &Arc<AmbientReceiver>,
        seen: AmbientStatus,
    ) -> tokio::task::JoinHandle<AmbientStatus> {
        let rx = rx.clone();
        tokio::spawn(async move { rx.status_change(seen).await })
    }

    type AmbientResult = Result<AmbientTurn, AmbientRecvError>;

    #[test]
    fn receivers_are_shareable_and_their_futures_send() {
        fn shareable<T: Send + Sync>() {}
        fn send<T: Send>(_: &T) {}
        shareable::<AmbientReceiver>();
        let (rx, _cancelled) = receiver(TurnOptions::default());
        send(&rx.recv());
        send(&rx.status_change(rx.status()));
    }

    #[test]
    fn tasks_wake_on_status_publication_and_close_and_cancellation_loses_nothing() {
        let limits = TurnOptions {
            queue_items: 2,
            ..TurnOptions::default()
        };
        let (rx, _cancelled) = receiver(limits);
        let rx = Arc::new(rx);
        runtime().block_on(async {
            let changed = status_change(&rx, rx.status());
            let replies = [recv(&rx), recv(&rx)];
            // Current-thread runtime: the tasks run and register before the publication.
            yield_now().await;
            let not_found = unavailable(AmbientUnavailable::NotFound);
            assert!(rx.shared.publish(Some(not_found), Vec::new()));
            assert_eq!(within(changed).await, not_found);

            assert!(rx.shared.publish(active(), vec![turn("one"), turn("two")]));
            let mut texts = Vec::new();
            for reply in replies {
                texts.push(text(within(reply).await.unwrap()));
            }
            texts.sort();
            assert_eq!(texts, ["one", "two"]);

            // Neither an aborted task nor an elapsed timeout removes a turn.
            let aborted = recv(&rx);
            yield_now().await;
            aborted.abort();
            assert!(aborted.await.unwrap_err().is_cancelled());
            assert!(timeout(Duration::from_millis(20), rx.recv()).await.is_err());
            assert!(rx.shared.publish(None, vec![turn("kept")]));
            let kept = timeout(WAIT, rx.recv()).await.unwrap().unwrap();
            assert_eq!(text(kept), "kept");

            // An overflow wakes both; queued turns come before the close.
            let reply = recv(&rx);
            let closed = status_change(&rx, rx.status());
            yield_now().await;
            let batch = vec![turn("a"), turn("b"), turn("c")];
            assert!(!rx.shared.publish(None, batch));
            let overflow = AmbientClosed::Overflow;
            assert_eq!(within(closed).await, AmbientStatus::Closed(overflow));
            assert_eq!(text(within(reply).await.unwrap()), "a");
            assert_eq!(text(rx.recv().await.unwrap()), "b");
            assert_eq!(
                rx.recv().await.unwrap_err(),
                AmbientRecvError::Closed(overflow)
            );
        });
    }

    #[test]
    fn stop_on_another_thread_wakes_every_task() {
        let (rx, _cancelled) = receiver(TurnOptions::default());
        let rx = Arc::new(rx);
        runtime().block_on(async {
            let replies = [recv(&rx), recv(&rx)];
            let status = status_change(&rx, rx.status());
            yield_now().await;
            let stopper = {
                let rx = rx.clone();
                std::thread::spawn(move || rx.stop())
            };
            let stopped = AmbientClosed::Stopped;
            for reply in replies {
                assert_eq!(
                    within(reply).await.unwrap_err(),
                    AmbientRecvError::Closed(stopped)
                );
            }
            assert_eq!(within(status).await, AmbientStatus::Closed(stopped));
            stopper.join().unwrap();
        });
    }

    /// The task has checked and found nothing, but has not awaited yet, when the worker
    /// publishes on another thread.
    #[test]
    fn a_publication_between_a_check_and_the_await_is_not_missed() {
        let (rx, _cancelled) = receiver(TurnOptions::default());
        let barrier = Barrier::install(&rx);
        std::thread::scope(|s| {
            s.spawn(|| {
                barrier.wait();
                assert!(rx.shared.publish(active(), vec![turn("between")]));
                barrier.release();
            });
            let reply = runtime().block_on(async { timeout(WAIT, rx.recv()).await });
            let reply = reply.expect("task woken before the deadline");
            assert_eq!(text(reply.unwrap()), "between");
        });
    }
}
