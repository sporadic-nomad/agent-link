use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use agent_link_supervisor::*;

fn binding() -> Binding {
    Binding {
        provider: "fake".into(),
        session: "exact-session".into(),
    }
}

#[derive(Default)]
struct Fake {
    sends: AtomicUsize,
    interrupts: AtomicUsize,
    stops: AtomicUsize,
    unknown: bool,
    wrong_binding: bool,
}

impl Runtime for Fake {
    fn binding(&self) -> Binding {
        let mut result = binding();
        if self.wrong_binding {
            result.session = "fresh-fallback".into();
        }
        result
    }
    fn submit(&self, _: &Receipt, _: &[u8]) -> Observation {
        let n = self.sends.fetch_add(1, Ordering::SeqCst);
        if self.unknown {
            Observation::Unknown
        } else {
            Observation::Accepted(format!("turn-{n}"))
        }
    }
    fn interrupt(&self, _: &Receipt, turn: &str) -> Observation {
        self.interrupts.fetch_add(1, Ordering::SeqCst);
        Observation::Accepted(turn.into())
    }
    fn stop(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
}

fn open(path: &std::path::Path) -> Supervisor {
    Supervisor::open(path, binding(), Limits::default()).unwrap()
}

fn resumed(path: &std::path::Path) -> (Supervisor, Arc<Fake>, RuntimeId) {
    let mut worker = open(path);
    let fake = Arc::new(Fake::default());
    let id = worker
        .resume(Instant::now(), |_, _| Ok(fake.clone()))
        .unwrap();
    (worker, fake, id)
}

fn terminal(worker: &mut Supervisor, mut receipt: Receipt) -> Receipt {
    let Outcome::Accepted(turn) = &receipt.outcome else {
        panic!("expected native acceptance")
    };
    receipt.outcome = Outcome::Terminal {
        turn: turn.clone(),
        result: NativeResult(b"completed".to_vec()),
    };
    worker.reconcile(&binding(), receipt).unwrap()
}

#[test]
fn full_identity_lock_binding_and_cleanup_fence() {
    let dir = tempfile::tempdir().unwrap();
    let (mut worker, fake, id) = resumed(dir.path());
    assert!(matches!(
        Supervisor::open(dir.path(), binding(), Limits::default()),
        Err(Error::Owned)
    ));
    assert!(matches!(worker.confirm_stopped(id), Err(Error::Busy)));
    let identity = worker.id();
    drop(worker);
    assert_eq!(fake.stops.load(Ordering::SeqCst), 1);
    let mut other = binding();
    other.session = "different".into();
    assert!(matches!(
        Supervisor::open(dir.path(), other, Limits::default()),
        Err(Error::BindingMismatch)
    ));
    let mut worker = open(dir.path());
    assert_eq!(worker.id(), identity);
    assert_eq!(worker.fenced_runtime().unwrap(), Some(id));
    assert!(matches!(
        worker.resume(Instant::now(), |_, _| panic!("must not spawn duplicate")),
        Err(Error::RecoveryRequired)
    ));
    let wrong = RuntimeId {
        generation: id.generation + 1,
        ..id
    };
    assert!(matches!(worker.confirm_stopped(wrong), Err(Error::Stale)));
    worker.confirm_stopped(id).unwrap();
    let next = worker
        .resume(Instant::now(), |expected, _| {
            assert_eq!(expected, &binding());
            Ok(fake.clone())
        })
        .unwrap();
    assert!(next.generation > id.generation);
}

#[test]
fn repeat_ids_replay_terminal_data_and_conflicting_payloads_never_send() {
    let dir = tempfile::tempdir().unwrap();
    let (mut worker, fake, _) = resumed(dir.path());
    let first = worker.submit("op-1", b"payload").unwrap();
    assert_eq!(worker.submit("op-1", b"payload").unwrap(), first);
    assert!(matches!(
        worker.submit("op-1", b"different"),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        worker.submit("op-2", b"payload"),
        Err(Error::Busy)
    ));
    let completed = terminal(&mut worker, first);
    assert_eq!(worker.submit("op-1", b"payload").unwrap(), completed);
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    drop(worker);
    let mut worker = open(dir.path());
    assert_eq!(worker.submit("op-1", b"payload").unwrap(), completed);
}

#[test]
fn unknown_survives_reopen_and_cleanup_and_requires_exact_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let mut worker = open(dir.path());
    let fake = Arc::new(Fake {
        unknown: true,
        ..Fake::default()
    });
    let id = worker
        .resume(Instant::now(), |_, _| Ok(fake.clone()))
        .unwrap();
    let receipt = worker.submit("op", b"payload").unwrap();
    assert_eq!(receipt.outcome, Outcome::SubmittedUnknown);
    drop(worker);
    let mut worker = open(dir.path());
    assert_eq!(worker.submit("op", b"payload").unwrap(), receipt);
    worker.confirm_stopped(id).unwrap();
    worker
        .resume(Instant::now(), |_, _| Ok(Arc::new(Fake::default())))
        .unwrap();
    assert!(matches!(worker.submit("new", b"payload"), Err(Error::Busy)));
    let mut evidence = receipt.clone();
    evidence.outcome = Outcome::Terminal {
        turn: "exact-recovered-turn".into(),
        result: NativeResult(b"done".to_vec()),
    };
    let mut wrong = evidence.clone();
    wrong.runtime.generation += 1;
    assert!(matches!(
        worker.reconcile(&binding(), wrong),
        Err(Error::Stale)
    ));
    let mut wrong = evidence.clone();
    wrong.digest.0[0] ^= 1;
    assert!(matches!(
        worker.reconcile(&binding(), wrong),
        Err(Error::Stale)
    ));
    worker.reconcile(&binding(), evidence.clone()).unwrap();
    assert_eq!(
        worker.reconcile(&binding(), evidence.clone()).unwrap(),
        evidence
    );
    assert!(matches!(
        worker.reconcile(&binding(), receipt),
        Err(Error::Conflict)
    ));
    worker.submit("new", b"payload").unwrap();
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
}

#[test]
fn independent_control_budget_and_exact_turn_prevent_successor_interrupt() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        operations: 2,
        controls: 1,
        ..Limits::default()
    };
    let mut worker = Supervisor::open(dir.path(), binding(), limits).unwrap();
    let fake = Arc::new(Fake::default());
    worker
        .resume(Instant::now(), |_, _| Ok(fake.clone()))
        .unwrap();
    let first = worker.submit("first", b"one").unwrap();
    terminal(&mut worker, first);
    let second = worker.submit("second", b"two").unwrap();
    let cancel = worker.interrupt("cancel", "second").unwrap();
    assert_eq!(worker.interrupt("cancel", "second").unwrap(), cancel);
    assert!(matches!(
        worker.interrupt("cancel-first", "first"),
        Err(Error::Stale)
    ));
    assert!(matches!(
        worker.interrupt("other-cancel", "second"),
        Err(Error::Capacity)
    ));
    assert!(matches!(
        worker.interrupt("second", "second"),
        Err(Error::Conflict)
    ));
    terminal(&mut worker, second);
    assert!(matches!(
        worker.interrupt("cancel", "second").unwrap().outcome,
        Outcome::Terminal { .. }
    ));
    assert!(matches!(
        worker.submit("third", b"three"),
        Err(Error::Capacity)
    ));
    worker.stop_handle().unwrap().request();
    assert_eq!(fake.interrupts.load(Ordering::SeqCst), 1);
    assert!(fake.stops.load(Ordering::SeqCst) > 0);
}

#[test]
fn cancellation_of_predecessor_and_wrong_turn_evidence_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (mut worker, _, id) = resumed(dir.path());
    let first = worker.submit("first", b"one").unwrap();
    let mut wrong = first.clone();
    wrong.outcome = Outcome::Accepted("other-turn".into());
    assert!(matches!(
        worker.reconcile(&binding(), wrong),
        Err(Error::Conflict)
    ));
    worker.stop_handle().unwrap().request();
    worker.confirm_stopped(id).unwrap();
    let replacement = Arc::new(Fake::default());
    worker
        .resume(Instant::now(), |_, _| Ok(replacement.clone()))
        .unwrap();
    assert!(matches!(
        worker.interrupt("cancel", "first"),
        Err(Error::Stale)
    ));
    assert_eq!(replacement.interrupts.load(Ordering::SeqCst), 0);
}

#[test]
fn leases_bound_capacity_expiry_and_stale_handles() {
    let dir = tempfile::tempdir().unwrap();
    let now = Instant::now();
    let limits = Limits {
        leases: 1,
        lease_ttl: Duration::from_secs(5),
        detached_ttl: Duration::from_secs(10),
        ..Limits::default()
    };
    let mut worker = Supervisor::open(dir.path(), binding(), limits).unwrap();
    let fake = Arc::new(Fake::default());
    let id = worker.resume(now, |_, _| Ok(fake.clone())).unwrap();
    let lease = worker.attach(now).unwrap();
    assert!(matches!(worker.attach(now), Err(Error::Capacity)));
    assert!(matches!(
        worker.renew(lease, now + Duration::from_secs(5)),
        Err(Error::Stale)
    ));
    assert!(!worker.tick(now + Duration::from_secs(14)).unwrap());
    assert!(worker.tick(now + Duration::from_secs(15)).unwrap());
    assert!(matches!(
        worker.attach(now + Duration::from_secs(16)),
        Err(Error::Stopped)
    ));
    let old_stop = worker.stop_handle().unwrap();
    worker.confirm_stopped(id).unwrap();
    let next = Arc::new(Fake::default());
    worker
        .resume(now + Duration::from_secs(20), |_, _| Ok(next.clone()))
        .unwrap();
    assert!(matches!(
        worker.renew(lease, now + Duration::from_secs(20)),
        Err(Error::Stale)
    ));
    old_stop.request();
    assert_eq!(next.stops.load(Ordering::SeqCst), 0);
    assert!(matches!(worker.tick(now), Err(Error::InvalidInput)));
}

#[test]
fn delayed_tick_uses_actual_last_lease_expiry_and_detach_never_stops_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let (mut worker, fake, _) = resumed(dir.path());
    let now = Instant::now();
    let lease = worker.attach(now).unwrap();
    worker.detach(lease, now).unwrap();
    assert_eq!(fake.stops.load(Ordering::SeqCst), 0);
    assert!(matches!(worker.detach(lease, now), Err(Error::Stale)));
    worker.attach(now).unwrap();
    assert!(worker.tick(now + Duration::from_secs(331)).unwrap());
}

#[test]
fn fresh_session_fallback_and_factory_failure_leave_recovery_fences() {
    let dir = tempfile::tempdir().unwrap();
    let mut worker = open(dir.path());
    let wrong = Arc::new(Fake {
        wrong_binding: true,
        ..Fake::default()
    });
    assert!(matches!(
        worker.resume(Instant::now(), |_, _| Ok(wrong.clone())),
        Err(Error::BindingMismatch)
    ));
    assert_eq!(wrong.stops.load(Ordering::SeqCst), 1);
    assert!(worker.fenced_runtime().unwrap().is_some());
    let id = worker.fenced_runtime().unwrap().unwrap();
    worker.confirm_stopped(id).unwrap();
    assert!(matches!(
        worker.resume(Instant::now(), |_, _| Err(Error::Stopped)),
        Err(Error::Stopped)
    ));
    assert!(worker.fenced_runtime().unwrap().unwrap().generation > id.generation);
}

#[test]
fn invalid_limits_and_inputs_do_not_create_or_call_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("must-not-exist");
    let limits = Limits {
        database_bytes: 1,
        ..Limits::default()
    };
    assert!(matches!(
        Supervisor::open(&path, binding(), limits),
        Err(Error::InvalidInput)
    ));
    assert!(!path.exists());
    let (mut worker, fake, _) = resumed(dir.path());
    for (id, payload) in [("", b"hello".as_slice()), ("op", b"".as_slice())] {
        assert!(matches!(
            worker.submit(id, payload),
            Err(Error::InvalidInput)
        ));
    }
    assert!(matches!(
        worker.submit("big", &vec![0; 1024 * 1024 + 1]),
        Err(Error::InvalidInput)
    ));
    assert_eq!(fake.sends.load(Ordering::SeqCst), 0);
    let receipt = worker.submit("op", b"valid").unwrap();
    let mut evidence = receipt.clone();
    evidence.outcome = Outcome::Terminal {
        turn: "turn-0".into(),
        result: NativeResult(vec![0; 16 * 1024 + 1]),
    };
    assert!(matches!(
        worker.reconcile(&binding(), evidence),
        Err(Error::Capacity)
    ));
    assert_eq!(worker.receipt("op").unwrap().unwrap(), receipt);
}

#[test]
fn stop_bypasses_a_blocked_dispatch_and_full_admission() {
    struct Blocked {
        entered: std::sync::mpsc::SyncSender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        stops: AtomicUsize,
    }
    impl Runtime for Blocked {
        fn binding(&self) -> Binding {
            binding()
        }
        fn submit(&self, _: &Receipt, _: &[u8]) -> Observation {
            self.entered.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            Observation::Unknown
        }
        fn interrupt(&self, _: &Receipt, _: &str) -> Observation {
            panic!("not used")
        }
        fn stop(&self) {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let (entered, waiting) = std::sync::mpsc::sync_channel(1);
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let fake = Arc::new(Blocked {
        entered,
        release: Mutex::new(released),
        stops: AtomicUsize::new(0),
    });
    let mut worker = open(dir.path());
    worker
        .resume(Instant::now(), |_, _| Ok(fake.clone()))
        .unwrap();
    let stop = worker.stop_handle().unwrap();
    let job = std::thread::spawn(move || worker.submit("op", b"payload"));
    let entered = waiting.recv_timeout(Duration::from_secs(10));
    stop.request();
    let stops = fake.stops.load(Ordering::SeqCst);
    let _ = release.send(());
    let receipt = job.join().unwrap().unwrap();
    entered.unwrap();
    assert!(stops > 0);
    assert_eq!(receipt.outcome, Outcome::SubmittedUnknown);
}

#[test]
fn disk_failure_before_dispatch_refuses_io_and_after_dispatch_preserves_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (mut worker, fake, _) = resumed(dir.path());
    let db = rusqlite::Connection::open(dir.path().join("worker.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_intent BEFORE INSERT ON operations BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(matches!(
        worker.submit("op", b"payload"),
        Err(Error::Database(_))
    ));
    assert_eq!(fake.sends.load(Ordering::SeqCst), 0);
    assert!(matches!(
        worker.submit("op", b"payload"),
        Err(Error::Poisoned)
    ));
    worker.stop_handle().unwrap().request();
    drop(worker);
    db.execute_batch("DROP TRIGGER fail_intent; CREATE TRIGGER fail_accept BEFORE UPDATE ON operations BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let mut worker = open(dir.path());
    let old = worker.fenced_runtime().unwrap().unwrap();
    worker.confirm_stopped(old).unwrap();
    worker
        .resume(Instant::now(), |_, _| Ok(fake.clone()))
        .unwrap();
    assert!(matches!(
        worker.submit("op", b"payload"),
        Err(Error::Database(_))
    ));
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    drop(worker);
    db.execute_batch("DROP TRIGGER fail_accept").unwrap();
    let mut worker = open(dir.path());
    assert_eq!(
        worker.submit("op", b"payload").unwrap().outcome,
        Outcome::SubmittedUnknown
    );
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
}

#[test]
fn store_configuration_schema_and_corruption_are_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    drop(open(dir.path()));
    let limits = Limits {
        operations: 10,
        ..Limits::default()
    };
    assert!(matches!(
        Supervisor::open(dir.path(), binding(), limits),
        Err(Error::InvalidInput)
    ));
    let db = rusqlite::Connection::open(dir.path().join("worker.sqlite3")).unwrap();
    db.pragma_update(None, "user_version", 99).unwrap();
    assert!(matches!(
        Supervisor::open(dir.path(), binding(), Limits::default()),
        Err(Error::InvalidStore)
    ));
    drop(db);
    std::fs::write(dir.path().join("worker.sqlite3"), b"not a database").unwrap();
    assert!(Supervisor::open(dir.path(), binding(), Limits::default()).is_err());
}

#[test]
fn adoption_preserves_birth_and_rejects_old_owner_leases() {
    let dir = tempfile::tempdir().unwrap();
    let (mut worker, _, id) = resumed(dir.path());
    let lease = worker.attach(Instant::now()).unwrap();
    drop(worker);
    let mut worker = open(dir.path());
    let now = Instant::now();
    let wrong = RuntimeId {
        generation: id.generation + 1,
        ..id
    };
    assert!(matches!(
        worker.adopt(wrong, now, |_, _| panic!("stale birth must not attach")),
        Err(Error::Stale)
    ));
    let adopted = Arc::new(Fake::default());
    worker
        .adopt(id, now, |expected, actual| {
            assert_eq!(expected, &binding());
            assert_eq!(actual, id);
            Ok(adopted.clone())
        })
        .unwrap();
    assert_eq!(worker.fenced_runtime().unwrap(), Some(id));
    assert!(matches!(worker.renew(lease, now), Err(Error::Stale)));
    assert!(matches!(
        worker.adopt(id, now, |_, _| panic!(
            "must not replace an attached runtime"
        )),
        Err(Error::Busy)
    ));
    assert_eq!(worker.submit("adopted", b"payload").unwrap().runtime, id);
}

#[test]
fn stop_callback_must_settle_before_reuse_and_old_handles_cannot_call_it_again() {
    struct Stopping {
        entered: std::sync::mpsc::SyncSender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        calls: AtomicUsize,
    }
    impl Runtime for Stopping {
        fn binding(&self) -> Binding {
            binding()
        }
        fn submit(&self, _: &Receipt, _: &[u8]) -> Observation {
            Observation::Unknown
        }
        fn interrupt(&self, _: &Receipt, _: &str) -> Observation {
            Observation::Unknown
        }
        fn stop(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let (entered, waiting) = std::sync::mpsc::sync_channel(1);
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let native = Arc::new(Stopping {
        entered,
        release: Mutex::new(released),
        calls: AtomicUsize::new(0),
    });
    let mut worker = open(dir.path());
    let id = worker
        .resume(Instant::now(), |_, _| Ok(native.clone()))
        .unwrap();
    let old = worker.stop_handle().unwrap();
    let stop = old.clone();
    let callback = std::thread::spawn(move || stop.request());
    let entered = waiting.recv_timeout(Duration::from_secs(10));
    let refused = worker.confirm_stopped(id);
    let _ = release.send(());
    callback.join().unwrap();
    entered.unwrap();
    assert!(matches!(refused, Err(Error::Busy)));
    worker.confirm_stopped(id).unwrap();
    old.request();
    assert_eq!(native.calls.load(Ordering::SeqCst), 1);
}
