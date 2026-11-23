//! Blocking and async waits on a turn receiver. Waiter threads report through channels,
//! so every check has a deadline even if a wakeup were lost, and the parked-waiter count
//! proves a waiter was blocked before the event that must wake it.

use std::fs;
use std::sync::Arc;
use std::thread::Scope;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};

use super::super::*;
use super::HISTORY;
use super::peer::*;
use crate::Event;

/// Longer than every assertion deadline: a lost wakeup fails the assertion first.
const LONG: Duration = Duration::from_secs(30);

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

fn woken<T>(result: &Receiver<T>) -> T {
    result
        .recv_timeout(WAIT)
        .expect("waiter woken before the deadline")
}

fn parked(rx: &TurnReceiver, waiters: usize) {
    until("waiters blocked on the receiver", || rx.parked() == waiters);
}

/// A receiver on a stopped clock, bound at the transcript's current EOF.
fn bound(peer: &Peer) -> (TurnReceiver, Sender<Instant>) {
    let mut attached = None;
    until("previous worker released its permit", || {
        let (clock, options) = clock();
        match peer
            .term
            .attach_turns(peer.source(), JsonParser::default(), options)
        {
            Ok(rx) => attached = Some((rx, clock)),
            Err(AttachError::Busy) => {}
            Err(err) => panic!("attach refused: {err:?}"),
        }
        attached.is_some()
    });
    let (rx, clock) = attached.unwrap();
    observed(&rx, &peer.file);
    (rx, clock)
}

/// The direct child exits and its `Exit` is published. No receiver access follows.
fn exited(peer: &Peer) {
    peer.input("exit-peer");
    let deadline = Instant::now() + WAIT;
    loop {
        match peer.events.recv_deadline(deadline) {
            Ok(Event::Exit { .. }) => return,
            Ok(_) => {}
            Err(err) => panic!("no Exit before the deadline: {err}"),
        }
    }
}

#[test]
fn blocked_waiters_wake_on_binding_and_publication_and_time_out_when_idle() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "publish", false, None);
    fs::remove_file(&peer.file).unwrap();
    let (clock, options) = clock();
    let rx = peer
        .term
        .attach_turns(peer.source(), JsonParser::default(), options)
        .unwrap();
    // Rendezvous: the first lookup completed without a file.
    tick(&clock);
    assert_eq!(rx.status(), Status::Waiting);
    assert_eq!(
        rx.recv_timeout(Duration::ZERO).unwrap_err(),
        TurnRecvError::Timeout
    );
    let start = Instant::now();
    let idle = Duration::from_millis(20);
    assert_eq!(rx.wait_status_change(Status::Waiting, idle), None);
    assert!(start.elapsed() >= idle);

    std::thread::scope(|s| {
        let active = waiter(s, || rx.wait_status_change(Status::Waiting, LONG));
        let reply = waiter(s, || rx.recv_timeout(LONG));
        parked(&rx, 2);
        let late = home.path().join("late");
        fs::write(&late, HISTORY).unwrap();
        fs::rename(late, &peer.file).unwrap();
        tick(&clock);
        let eof = HISTORY.len() as u64;
        assert_eq!(
            woken(&active),
            Some(Status::Active {
                start: eof,
                offset: eof,
                incomplete_turns: 0
            })
        );
        // The binding woke the receive too. It found nothing and blocked again.
        parked(&rx, 1);
        turn(&peer.file, "woken");
        tick(&clock);
        assert_eq!(woken(&reply).unwrap().text, "woken");
    });

    let start = Instant::now();
    assert_eq!(rx.recv_timeout(idle).unwrap_err(), TurnRecvError::Timeout);
    assert!(start.elapsed() >= idle);
    assert!(matches!(rx.status(), Status::Active { .. }));
}

#[test]
fn a_gap_wakes_every_waiter_and_a_queued_pull_still_rechecks_and_discards() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "gap", false, None);
    let (rx, clock) = bound(&peer);
    let seen = rx.status();
    std::thread::scope(|s| {
        let reply = waiter(s, || rx.recv_timeout(LONG));
        let status = waiter(s, || rx.wait_status_change(seen, LONG));
        parked(&rx, 2);
        append(&peer.file, b"malformed\n");
        tick(&clock);
        assert_eq!(woken(&reply).unwrap_err(), TurnRecvError::Gap(Gap::Format));
        assert_eq!(woken(&status), Some(Status::Gap(Gap::Format)));
    });
    // A gap is final. Later waits return it at once.
    let start = Instant::now();
    assert_eq!(
        rx.recv_timeout(LONG).unwrap_err(),
        TurnRecvError::Gap(Gap::Format)
    );
    let gap = Status::Gap(Gap::Format);
    assert_eq!(rx.wait_status_change(gap, LONG), Some(gap));
    assert!(start.elapsed() < WAIT);
    drop(rx);

    // The worker waits for its clock, so only the receive's own pull can see that
    // the queued reply's transcript was replaced.
    let (rx, clock) = bound(&peer);
    turn(&peer.file, "discarded");
    tick(&clock);
    observed(&rx, &peer.file);
    let replacement = peer.file.with_extension("new");
    fs::copy(&peer.file, &replacement).unwrap();
    fs::rename(replacement, &peer.file).unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::ZERO).unwrap_err(),
        TurnRecvError::Gap(Gap::Replaced)
    );
    assert!(rx.try_recv().is_none());
}

#[test]
fn child_exit_is_sampled_by_the_worker_or_at_the_deadline_and_teardown_wakes_at_once() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "exit", false, None);
    let (rx, clock) = bound(&peer);
    std::thread::scope(|s| {
        let reply = waiter(s, || rx.recv_timeout(LONG));
        parked(&rx, 1);
        exited(&peer);
        // Exit has no wakeup of its own. The worker's next pass records it.
        tick(&clock);
        assert_eq!(woken(&reply).unwrap_err(), TurnRecvError::Closed);
    });

    // Without a worker pass, the waiter samples the exit when its timeout elapses.
    let peer = Peer::start(home.path(), "deadline", false, None);
    let (rx, _clock) = bound(&peer);
    std::thread::scope(|s| {
        let reply = waiter(s, || rx.recv_timeout(WAIT / 3));
        parked(&rx, 1);
        exited(&peer);
        assert_eq!(woken(&reply).unwrap_err(), TurnRecvError::Closed);
    });

    let peer = Peer::start(home.path(), "teardown", false, None);
    let (rx, _clock) = bound(&peer);
    let seen = rx.status();
    let term = peer.term;
    std::thread::scope(|s| {
        let reply = waiter(s, || rx.recv_timeout(LONG));
        let status = waiter(s, || rx.wait_status_change(seen, LONG));
        parked(&rx, 2);
        // Teardown closes the PTY event bus. The worker leaves without another tick.
        drop(term);
        assert_eq!(woken(&reply).unwrap_err(), TurnRecvError::Closed);
        assert_eq!(woken(&status), Some(Status::Closed));
    });
}

/// Run `wait` on a detached thread, so that a lost wakeup of a wait without a deadline
/// fails the caller's deadline instead of hanging the test.
fn detached<T: Send + 'static>(
    rx: &Arc<TurnReceiver>,
    wait: impl FnOnce(&TurnReceiver) -> T + Send + 'static,
) -> Receiver<T> {
    let (done, result) = bounded(1);
    let rx = rx.clone();
    std::thread::spawn(move || {
        let _ = done.send(wait(&rx));
    });
    result
}

#[test]
fn stop_on_another_thread_wakes_every_waiter_including_ones_without_a_deadline() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "stop", false, None);
    let (rx, _clock) = bound(&peer);
    let rx = Arc::new(rx);
    let seen = rx.status();
    let replies = [
        detached(&rx, |rx| rx.recv_timeout(LONG)),
        detached(&rx, |rx| rx.recv_timeout(Duration::MAX)),
    ];
    let status = detached(&rx, move |rx| rx.wait_status_change(seen, Duration::MAX));
    parked(&rx, 3);
    woken(&detached(&rx, |rx| rx.stop()));
    for reply in &replies {
        assert_eq!(woken(reply).unwrap_err(), TurnRecvError::Closed);
    }
    assert_eq!(woken(&status), Some(Status::Closed));
    assert_eq!(
        rx.recv_timeout(Duration::ZERO).unwrap_err(),
        TurnRecvError::Closed
    );
}

#[test]
fn each_reply_reaches_exactly_one_of_several_waiters() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "many", false, None);
    let (rx, clock) = bound(&peer);
    std::thread::scope(|s| {
        let (done, results) = bounded(3);
        for _ in 0..3 {
            let done = done.clone();
            let rx = &rx;
            s.spawn(move || done.send(rx.recv_timeout(LONG)).unwrap());
        }
        parked(&rx, 3);
        turn(&peer.file, "one");
        turn(&peer.file, "two");
        tick(&clock);
        let mut texts = [woken(&results), woken(&results)].map(|r| r.unwrap().text);
        texts.sort();
        assert_eq!(texts, ["one", "two"]);
        // The third waiter found an empty queue and blocked again.
        parked(&rx, 1);
        rx.stop();
        assert_eq!(woken(&results).unwrap_err(), TurnRecvError::Closed);
    });
}

/// Publication before the waiters start, after both blocked, and racing their first check.
#[test]
fn repeated_publications_reach_waiters_in_every_order() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "orders", false, None);
    let (rx, clock) = bound(&peer);
    for cycle in 0..24 {
        let text = format!("cycle-{cycle}");
        let seen = rx.status();
        let publish = || {
            turn(&peer.file, &text);
            tick(&clock);
        };
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
            assert_eq!(woken(&reply).unwrap().text, text);
            assert!(matches!(woken(&status), Some(Status::Active { .. })));
        });
    }
}

/// The waiter has checked and found nothing, but has not blocked yet, when the worker
/// publishes or ends observation. That wakeup must still end its wait.
#[test]
fn a_change_between_a_check_and_the_wait_is_not_missed() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "between", false, None);
    let (rx, clock) = bound(&peer);
    std::thread::scope(|s| {
        let (barrier, pause) = Barrier::new();
        rx.pause_next_wait(pause);
        let reply = waiter(s, || rx.recv_timeout(LONG));
        barrier.wait();
        turn(&peer.file, "between");
        tick(&clock);
        observed(&rx, &peer.file);
        barrier.release();
        assert_eq!(woken(&reply).unwrap().text, "between");

        let (barrier, pause) = Barrier::new();
        rx.pause_next_wait(pause);
        let (rx, seen) = (&rx, rx.status());
        let status = waiter(s, move || rx.wait_status_change(seen, LONG));
        barrier.wait();
        append(&peer.file, b"malformed\n");
        tick(&clock);
        until("worker recorded the gap", || {
            rx.status() == Status::Gap(Gap::Format)
        });
        barrier.release();
        assert_eq!(woken(&status), Some(Status::Gap(Gap::Format)));
    });
}

/// Cursor-style rewrites: data rows followed by end markers, in an image rewritten in place.
#[cfg(feature = "turns")]
struct Rewrites(Writer);

#[cfg(feature = "turns")]
impl Source for Rewrites {
    fn witness(&self) -> std::sync::Arc<dyn Witness> {
        self.0.witness()
    }

    fn current(&mut self, launch: &Launch) -> Result<Binding, Gap> {
        self.0.current(launch)
    }

    fn snapshot_marker(&self) -> Option<super::super::snapshots::Marker> {
        fn end(line: &[u8]) -> Result<bool, Gap> {
            Ok(line.starts_with(b"{\"end\""))
        }
        Some(end)
    }
}

/// A pull withheld during a short rewrite succeeds once the identical image returns.
/// The worker's commit then changes neither status nor queue length, yet must wake
/// the blocked receive.
#[cfg(feature = "turns")]
#[test]
fn a_commit_without_a_visible_change_retries_a_withheld_pull() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), "rewrite", false, None);
    fs::write(&peer.file, HISTORY).unwrap();
    let (clock, options) = clock();
    let rx = peer
        .term
        .attach_turns(Rewrites(peer.source()), JsonParser::default(), options)
        .unwrap();
    observed(&rx, &peer.file);
    turn(&peer.file, "withheld");
    tick(&clock);
    observed(&rx, &peer.file);
    let image = fs::read(&peer.file).unwrap();
    let status = rx.status();
    fs::write(&peer.file, b"").unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::ZERO).unwrap_err(),
        TurnRecvError::Timeout
    );
    std::thread::scope(|s| {
        let reply = waiter(s, || rx.recv_timeout(LONG));
        parked(&rx, 1);
        fs::write(&peer.file, &image).unwrap();
        tick(&clock);
        assert_eq!(woken(&reply).unwrap().text, "withheld");
    });
    assert_eq!(rx.status(), status);
}

#[cfg(all(feature = "turns", target_os = "macos", target_arch = "aarch64"))]
#[test]
fn lease_revocation_wakes_waiters_at_the_next_worker_sample() {
    let (_lock, home) = project();
    let root = home.path().join("records");
    fs::create_dir(&root).unwrap();
    let lease = Arc::new(ExclusiveClaudeRoot::new(root).unwrap());
    let authority = lease.clone();
    let capture = Arc::new(move |spec: &mut SpawnSpec, plan: &LaunchPlan| {
        let mut claim = super::peer::capture(spec, plan)?;
        authority.bind(&mut claim)?;
        Ok(claim)
    });
    let peer = Peer::start_with_capture(home.path(), "revoked", false, None, capture);
    let (rx, clock) = bound(&peer);
    let seen = rx.status();
    std::thread::scope(|s| {
        let reply = waiter(s, || rx.recv_timeout(LONG));
        let status = waiter(s, || rx.wait_status_change(seen, LONG));
        parked(&rx, 2);
        // The lease flag has no wakeup of its own. The worker's next pass records it.
        lease.revoke();
        tick(&clock);
        let revoked = Gap::AuthorityRevoked;
        assert_eq!(woken(&reply).unwrap_err(), TurnRecvError::Gap(revoked));
        assert_eq!(woken(&status), Some(Status::Gap(revoked)));
    });
}

/// `recv` and `status_change` (feature `tokio`) on a current-thread runtime. The
/// fixture lock stays outside the async block.
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

    #[test]
    fn a_pull_waker_can_synchronously_receive_after_dequeue_or_gap() {
        use std::task::{Context, Wake, Waker};

        struct ReceiveOnWake(Arc<TurnReceiver>, Sender<Result<String, TurnRecvError>>);
        impl Wake for ReceiveOnWake {
            fn wake(self: Arc<Self>) {
                let reply = self.0.recv_timeout(Duration::ZERO).map(|reply| reply.text);
                self.1.send(reply).unwrap();
            }
        }

        struct Published(Sender<()>);
        impl Wake for Published {
            fn wake(self: Arc<Self>) {
                self.0.send(()).unwrap();
            }
        }

        let (_lock, home) = project();
        for replaced in [false, true] {
            let peer = Peer::start(
                home.path(),
                if replaced { "replaced" } else { "queued" },
                false,
                None,
            );
            let (rx, clock) = bound(&peer);
            let rx = Arc::new(rx);
            let (tx, published) = bounded(1);
            let after_commit = Waker::from(Arc::new(Published(tx)));
            let mut changed = std::pin::pin!(rx.status_change(rx.status()));
            assert!(
                changed
                    .as_mut()
                    .poll(&mut Context::from_waker(&after_commit))
                    .is_pending()
            );
            turn(&peer.file, "one");
            turn(&peer.file, "two");
            tick(&clock);
            woken(&published); // Only a pull can wake the next listener.
            let (tx, nested) = bounded(1);
            let waker = Waker::from(Arc::new(ReceiveOnWake(rx.clone(), tx)));
            let mut listener = std::pin::pin!(rx.status_change(rx.status()));
            assert!(
                listener
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            if replaced {
                let replacement = peer.file.with_extension("new");
                fs::copy(&peer.file, &replacement).unwrap();
                fs::rename(replacement, &peer.file).unwrap();
            }
            let outer = detached(&rx, |rx| {
                rx.recv_timeout(Duration::ZERO).map(|reply| reply.text)
            });
            if replaced {
                assert_eq!(woken(&outer), Err(TurnRecvError::Gap(Gap::Replaced)));
                assert_eq!(woken(&nested), Err(TurnRecvError::Gap(Gap::Replaced)));
            } else {
                assert_eq!(woken(&outer), Ok("one".into()));
                assert_eq!(woken(&nested), Ok("two".into()));
            }
        }
    }

    #[test]
    fn receivers_are_shareable_and_their_futures_send() {
        fn shareable<T: Send + Sync>() {}
        fn send<T: Send>(_: &T) {}
        shareable::<TurnReceiver>();
        #[cfg(feature = "turns")]
        shareable::<TranscriptReceiver>();
        let (_lock, home) = project();
        let peer = Peer::start(home.path(), "send", false, None);
        let rx = peer.attach();
        send(&rx.recv());
        send(&rx.status_change(Status::Waiting));
    }

    #[test]
    fn tasks_wake_on_binding_publication_and_gap_and_cancellation_loses_nothing() {
        let (_lock, home) = project();
        let peer = Peer::start(home.path(), "tasks", false, None);
        fs::remove_file(&peer.file).unwrap();
        let (clock, options) = clock();
        let rx = Arc::new(
            peer.term
                .attach_turns(peer.source(), JsonParser::default(), options)
                .unwrap(),
        );
        tick(&clock);
        assert_eq!(rx.status(), Status::Waiting);
        runtime().block_on(async {
            let recv = |rx: &Arc<TurnReceiver>| {
                let rx = rx.clone();
                tokio::spawn(async move { rx.recv().await })
            };
            let active = {
                let rx = rx.clone();
                tokio::spawn(async move { rx.status_change(Status::Waiting).await })
            };
            let replies = [recv(&rx), recv(&rx)];
            // Current-thread runtime: the tasks run and register before the clock ticks.
            yield_now().await;
            let late = home.path().join("late");
            fs::write(&late, HISTORY).unwrap();
            fs::rename(late, &peer.file).unwrap();
            tick(&clock);
            assert!(matches!(within(active).await, Status::Active { .. }));

            turn(&peer.file, "one");
            turn(&peer.file, "two");
            tick(&clock);
            let mut texts = Vec::new();
            for reply in replies {
                texts.push(within(reply).await.unwrap().text);
            }
            texts.sort();
            assert_eq!(texts, ["one", "two"]);

            // Neither an aborted task nor an elapsed timeout removes a reply.
            let aborted = recv(&rx);
            yield_now().await;
            aborted.abort();
            assert!(aborted.await.unwrap_err().is_cancelled());
            assert!(timeout(Duration::from_millis(20), rx.recv()).await.is_err());
            turn(&peer.file, "kept");
            tick(&clock);
            let kept = timeout(WAIT, rx.recv()).await.unwrap().unwrap();
            assert_eq!(kept.text, "kept");

            let reply = recv(&rx);
            let gap = {
                let rx = rx.clone();
                let seen = rx.status();
                tokio::spawn(async move { rx.status_change(seen).await })
            };
            yield_now().await;
            append(&peer.file, b"malformed\n");
            tick(&clock);
            assert_eq!(
                within(reply).await.unwrap_err(),
                TurnRecvError::Gap(Gap::Format)
            );
            assert_eq!(within(gap).await, Status::Gap(Gap::Format));
        });
    }

    #[test]
    fn stop_on_another_thread_wakes_every_task() {
        let (_lock, home) = project();
        let peer = Peer::start(home.path(), "task-stop", false, None);
        let (rx, _clock) = bound(&peer);
        let rx = Arc::new(rx);
        runtime().block_on(async {
            let seen = rx.status();
            let replies: Vec<_> = (0..2)
                .map(|_| {
                    let rx = rx.clone();
                    tokio::spawn(async move { rx.recv().await })
                })
                .collect();
            let status = {
                let rx = rx.clone();
                tokio::spawn(async move { rx.status_change(seen).await })
            };
            yield_now().await;
            let stopper = {
                let rx = rx.clone();
                std::thread::spawn(move || rx.stop())
            };
            for reply in replies {
                assert_eq!(within(reply).await.unwrap_err(), TurnRecvError::Closed);
            }
            assert_eq!(within(status).await, Status::Closed);
            stopper.join().unwrap();
        });
    }

    /// The task has checked and found nothing, but has not awaited yet, when the
    /// worker publishes on another thread.
    #[test]
    fn a_publication_between_a_check_and_the_await_is_not_missed() {
        let (_lock, home) = project();
        let peer = Peer::start(home.path(), "task-between", false, None);
        let (rx, clock) = bound(&peer);
        let (barrier, pause) = Barrier::new();
        rx.pause_next_wait(pause);
        std::thread::scope(|s| {
            s.spawn(|| {
                barrier.wait();
                turn(&peer.file, "between");
                tick(&clock);
                observed(&rx, &peer.file);
                barrier.release();
            });
            let reply = runtime().block_on(async { timeout(WAIT, rx.recv()).await });
            let reply = reply.expect("task woken before the deadline");
            assert_eq!(reply.unwrap().text, "between");
        });
    }
}
