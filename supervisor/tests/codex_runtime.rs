use agent_link::{agent::RpcLaunch, rpc::RpcSpawnSpec};
use agent_link_supervisor::{
    codex::{CodexRuntime, Config},
    *,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
fn setup(scene: &str, config: Config) -> (tempfile::TempDir, Supervisor, Arc<CodexRuntime>) {
    let dir = tempfile::tempdir().unwrap();
    let binding = Binding {
        provider: "codex".into(),
        session: "t".into(),
    };
    let mut worker = Supervisor::open(dir.path(), binding.clone(), Limits::default()).unwrap();
    let launch = RpcLaunch::new(
        env!("CARGO_BIN_EXE_supervisor-fixture"),
        &["codex-peer", scene],
    );
    let mut spec = RpcSpawnSpec::new(launch, dir.path());
    spec.env.inherit = false;
    spec.env.set(
        "AGENT_LINK_TEST_START_MARKER",
        dir.path().join("start-received"),
    );
    spec.env.set(
        "AGENT_LINK_TEST_HANG_MARKER",
        dir.path().join("hang-received"),
    );
    spec.env
        .set("PATH", std::env::var_os("PATH").unwrap_or_default());
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        spec.env.set("SystemRoot", root);
    }
    #[cfg(unix)]
    {
        spec.shell = Some("/bin/sh".into());
    }
    let mut handle = None;
    worker
        .resume(Instant::now(), |binding, id| {
            let runtime = CodexRuntime::spawn(spec, binding.clone(), id, config)?;
            handle = Some(runtime.clone());
            runtime.resume()?;
            Ok(runtime)
        })
        .unwrap();
    (dir, worker, handle.unwrap())
}
fn terminal(runtime: &CodexRuntime) -> Receipt {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(r) = runtime.poll_evidence() {
            return r;
        }
        assert!(Instant::now() < deadline, "no exact terminal receipt");
        std::thread::yield_now();
    }
}
fn finish(worker: &mut Supervisor, runtime: &CodexRuntime) {
    worker.stop_handle().unwrap().request();
    assert!(runtime.wait_cleanup_until(Instant::now() + Duration::from_secs(10)));
    worker
        .confirm_stopped(worker.fenced_runtime().unwrap().unwrap())
        .unwrap();
}
#[test]
fn early_terminal_correlates_and_replay_never_resends() {
    let (_dir, mut worker, runtime) = setup("normal", Config::default());
    let accepted = worker.submit("op", b"exact prompt").unwrap();
    assert!(matches!(accepted.outcome, Outcome::Accepted(_)));
    let evidence = terminal(&runtime);
    assert_eq!(evidence.id, accepted.id);
    assert_eq!(evidence.digest, accepted.digest);
    assert_eq!(evidence.runtime, accepted.runtime);
    let binding = worker.binding().clone();
    let committed = worker.reconcile(&binding, evidence).unwrap();
    assert_eq!(worker.submit("op", b"exact prompt").unwrap(), committed);
    finish(&mut worker, &runtime);
}
#[test]
fn eof_never_manufactures_acceptance_or_terminal() {
    let (_dir, mut worker, runtime) = setup("eof", Config::default());
    assert_eq!(
        worker.submit("unknown", b"prompt").unwrap().outcome,
        Outcome::SubmittedUnknown
    );
    assert!(runtime.poll_evidence().is_none());
    assert!(matches!(worker.submit("new", b"prompt"), Err(Error::Busy)));
    finish(&mut worker, &runtime);
}
#[test]
fn exact_interrupt_terminal_settles_both_receipts() {
    let (_dir, mut worker, runtime) = setup("interrupt", Config::default());
    worker.submit("op", b"prompt").unwrap();
    assert!(matches!(
        worker.interrupt("cancel", "op").unwrap().outcome,
        Outcome::Accepted(_)
    ));
    let binding = worker.binding().clone();
    worker.reconcile(&binding, terminal(&runtime)).unwrap();
    worker.reconcile(&binding, terminal(&runtime)).unwrap();
    finish(&mut worker, &runtime);
}
#[test]
fn stop_bypasses_waiting_native_response() {
    let (dir, mut worker, runtime) = setup("hang", Config::default());
    let intent = Receipt {
        id: "direct".into(),
        digest: {
            use sha2::{Digest as _, Sha256};
            let mut hash = Sha256::new();
            for part in [b"agent-link-supervisor/submit/v1".as_slice(), b"prompt"] {
                hash.update((part.len() as u64).to_le_bytes());
                hash.update(part);
            }
            Digest(hash.finalize().into())
        },
        runtime: worker.fenced_runtime().unwrap().unwrap(),
        operation: Operation::Submit,
        outcome: Outcome::SubmittedUnknown,
    };
    let caller = runtime.clone();
    let thread = std::thread::spawn(move || caller.submit(&intent, b"prompt"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dir.path().join("hang-received").exists() {
        assert!(Instant::now() < deadline, "native request was not received");
        std::thread::yield_now();
    }
    worker.stop_handle().unwrap().request();
    assert_eq!(thread.join().unwrap(), Observation::Unknown);
    assert!(runtime.wait_cleanup_until(Instant::now() + Duration::from_secs(10)));
    worker
        .confirm_stopped(worker.fenced_runtime().unwrap().unwrap())
        .unwrap();
}

#[test]
fn unrelated_terminal_never_settles_current_operation() {
    let (_dir, mut worker, runtime) = setup("stale-terminal", Config::default());
    worker.submit("op", b"prompt").unwrap();
    let evidence = terminal(&runtime);
    assert!(matches!(evidence.outcome, Outcome::Terminal { ref turn, .. } if turn == "a"));
    assert!(runtime.poll_evidence().is_none());
    finish(&mut worker, &runtime);
}
#[test]
fn lost_start_receipt_leaves_terminal_uncorrelated() {
    let config = Config {
        request_timeout: Duration::from_millis(500),
        ..Config::default()
    };
    let (_dir, mut worker, runtime) = setup("lost-receipt", config);
    assert_eq!(
        worker.submit("op", b"prompt").unwrap().outcome,
        Outcome::SubmittedUnknown
    );
    assert!(runtime.poll_evidence().is_none());
    finish(&mut worker, &runtime);
}
#[test]
fn oversized_terminal_closes_without_truncating_evidence() {
    let config = Config {
        result_bytes: 1,
        ..Config::default()
    };
    let (_dir, mut worker, runtime) = setup("normal", config);
    let _ = worker.submit("op", b"prompt").unwrap();
    assert!(runtime.poll_evidence().is_none());
    finish(&mut worker, &runtime);
}

#[test]
fn terminal_response_without_notification_is_exact_evidence() {
    let (_dir, mut worker, runtime) = setup("terminal-response", Config::default());
    worker.submit("op", b"prompt").unwrap();
    let evidence = terminal(&runtime);
    assert!(matches!(evidence.outcome, Outcome::Terminal { ref turn, .. } if turn == "a"));
    let binding = worker.binding().clone();
    worker.reconcile(&binding, evidence).unwrap();
    finish(&mut worker, &runtime);
}
#[test]
fn terminal_count_saturation_stops_without_guessing() {
    let config = Config {
        evidence_capacity: 1,
        ..Config::default()
    };
    let (_dir, mut worker, runtime) = setup("stale-terminal", config);
    let _ = worker.submit("op", b"prompt").unwrap();
    assert!(runtime.poll_evidence().is_none());
    finish(&mut worker, &runtime);
}

#[test]
fn forged_receipts_are_rejected_without_native_dispatch() {
    let (dir, mut worker, runtime) = setup("normal", Config::default());
    let intent = Receipt {
        id: "x".repeat(129),
        digest: Digest([0; 32]),
        operation: Operation::Submit,
        runtime: worker.fenced_runtime().unwrap().unwrap(),
        outcome: Outcome::SubmittedUnknown,
    };
    assert_eq!(runtime.submit(&intent, b"prompt"), Observation::Unknown);
    let mut intent = Receipt {
        id: "op".into(),
        ..intent
    };
    assert_eq!(runtime.submit(&intent, b"prompt"), Observation::Unknown);
    intent.operation = Operation::Interrupt("x".repeat(129));
    assert_eq!(runtime.interrupt(&intent, "a"), Observation::Unknown);
    intent.operation = Operation::Interrupt("target".into());
    assert_eq!(
        runtime.interrupt(&intent, &"x".repeat(1025)),
        Observation::Unknown
    );
    assert!(runtime.poll_evidence().is_none());
    finish(&mut worker, &runtime);
    assert!(!dir.path().join("start-received").exists());
}
