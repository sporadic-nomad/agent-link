#![cfg(unix)]
use agent_link_daemon::{Client, FRAME_BYTES, read_frame, write_frame};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Seek, Write},
    net::TcpStream,
    path::Path,
    process::{Child, Command},
    time::{Duration, Instant},
};
fn wait(mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < end, "condition deadline");
        std::thread::yield_now();
    }
}
fn private_root() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}
struct Host {
    child: Option<Child>,
}
impl Drop for Host {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn host(root: &Path, scene: &str) -> Host {
    host_with_detached_ttl(root, scene, Duration::from_millis(800))
}
fn host_with_detached_ttl(root: &Path, scene: &str, detached_ttl: Duration) -> Host {
    let child = Command::new(env!("CARGO_BIN_EXE_daemon-fixture"))
        .args(["serve", root.to_str().unwrap(), scene])
        .arg(detached_ttl.as_millis().to_string())
        .spawn()
        .unwrap();
    let host = Host { child: Some(child) };
    wait(|| root.join("endpoint.json").exists());
    host
}
fn call(c: &mut Client, v: Value) -> Value {
    c.request(v).unwrap()
}
fn stop(root: &Path) {
    let mut c = Client::connect(root).unwrap();
    assert_eq!(call(&mut c, json!({"op":"stop"}))["stopping"], true);
    wait(|| !root.join("endpoint.json").exists());
}
#[test]
fn disconnect_replay_conflict_and_exact_terminal_persistence() {
    let root = private_root();
    let mut host = host(root.path(), "normal");
    let mut c = Client::connect(root.path()).unwrap();
    assert_eq!(call(&mut c, json!({"op":"attach"}))["attached"], true);
    let r = call(&mut c, json!({"op":"submit","id":"one","payload":"exact"}));
    assert_eq!(r["outcome"]["state"], "unknown");
    drop(c);
    let mut c = Client::connect(root.path()).unwrap();
    wait(|| {
        call(&mut c, json!({"op":"receipt","id":"one"}))["receipt"]["outcome"]["state"]
            == "terminal"
    });
    assert_eq!(
        call(&mut c, json!({"op":"submit","id":"one","payload":"exact"}))["outcome"]["state"],
        "terminal"
    );
    assert!(
        call(
            &mut c,
            json!({"op":"submit","id":"one","payload":"different"})
        )["error"]
            .as_str()
            .unwrap()
            .contains("conflict")
    );
    assert_eq!(fs::read_to_string(root.path().join("count")).unwrap(), "1");
    stop(root.path());
    assert!(host.child.as_mut().unwrap().wait().unwrap().success());
    // Even controlled stop cannot claim escaped-descendant containment.
    let status = Command::new(env!("CARGO_BIN_EXE_daemon-fixture"))
        .args(["serve", root.path().to_str().unwrap(), "normal"])
        .status()
        .unwrap();
    assert!(!status.success());
}
#[test]
fn stop_and_detached_tick_progress_while_native_submit_blocks() {
    for explicit in [true, false] {
        let root = private_root();
        let mut host = host(root.path(), "hang");
        let mut c = Client::connect(root.path()).unwrap();
        call(&mut c, json!({"op":"submit","id":"one","payload":"prompt"}));
        wait(|| root.path().join("hang").exists());
        let start = Instant::now();
        if explicit {
            stop(root.path());
        } else {
            drop(c);
            wait(|| !root.path().join("endpoint.json").exists());
        }
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(host.child.as_mut().unwrap().wait().unwrap().success());
    }
}
#[test]
fn unauthenticated_oversized_and_full_clients_do_not_block_reserved_stop() {
    let root = private_root();
    // This checks rejection and reserved stop, not detached expiry. Keep the
    // finite fixture lifetime independent of CLI startup and socket timeouts.
    let mut host = host_with_detached_ttl(root.path(), "normal", Duration::from_secs(30));
    let endpoint: Value =
        serde_json::from_slice(&fs::read(root.path().join("endpoint.json")).unwrap()).unwrap();
    let address = endpoint["address"].as_str().unwrap().parse().unwrap();
    // A file supplies the complete oversized input without a blocking writer
    // racing the CLI's rejection/closed stdin. Check the actual rejection below.
    let mut input = tempfile::tempfile().unwrap();
    input.write_all(&vec![b'x'; FRAME_BYTES + 1]).unwrap();
    input.rewind().unwrap();
    let mut cli = Host {
        child: Some(
            Command::new(env!("CARGO_BIN_EXE_agent-link-daemon"))
                .args(["client", root.path().to_str().unwrap()])
                .stdin(input)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        ),
    };
    // Host's guard kills/reaps the CLI if the finite wait or an assertion fails.
    wait(|| cli.child.as_mut().unwrap().try_wait().unwrap().is_some());
    let output = cli.child.take().unwrap().wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "CLI unexpectedly succeeded: {stderr}"
    );
    assert_eq!(
        stderr.trim(),
        "agent-link-daemon: client input frame too large"
    );
    assert!(output.stdout.is_empty());

    let mut bad = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    bad.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    write_frame(
        &mut bad,
        &json!({"op":"submit","token":"wrong","id":"bad","payload":"prompt"}),
    )
    .unwrap();
    assert!(read_frame(&mut bad).is_err());
    let mut big = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    big.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    big.write_all(&((FRAME_BYTES + 1) as u32).to_be_bytes())
        .unwrap();
    assert!(read_frame(&mut big).is_err());
    let _clients: Vec<_> = (0..20)
        .map(|_| TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap())
        .collect();
    stop(root.path());
    wait(|| host.child.as_mut().unwrap().try_wait().unwrap().is_some());
    assert!(host.child.as_mut().unwrap().wait().unwrap().success());
    assert!(!root.path().join("start").exists());
}
#[test]
fn launcher_exit_does_not_own_worker_and_crash_reopen_is_fenced() {
    let root = private_root();
    assert!(
        Command::new(env!("CARGO_BIN_EXE_daemon-fixture"))
            .args(["launcher", root.path().to_str().unwrap(), "normal"])
            .status()
            .unwrap()
            .success()
    );
    wait(|| root.path().join("endpoint.json").exists());
    let mut c = Client::connect(root.path()).unwrap();
    assert!(call(&mut c, json!({"op":"status"}))["generation"].is_number());
    drop(c);
    stop(root.path());
    let root = private_root();
    let mut host = host(root.path(), "lost-receipt");
    let mut c = Client::connect(root.path()).unwrap();
    call(
        &mut c,
        json!({"op":"submit","id":"unknown","payload":"prompt"}),
    );
    wait(|| root.path().join("start").exists());
    let child = host.child.as_mut().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_daemon-fixture"))
        .args(["serve", root.path().to_str().unwrap(), "normal"])
        .status()
        .unwrap();
    assert!(!status.success());
    let worker = agent_link_supervisor::Supervisor::open(
        root.path(),
        agent_link_supervisor::Binding {
            provider: "codex".into(),
            session: "t".into(),
        },
        agent_link_supervisor::Limits {
            lease_ttl: Duration::from_millis(300),
            detached_ttl: Duration::from_millis(800),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        worker.receipt("unknown").unwrap().unwrap().outcome,
        agent_link_supervisor::Outcome::SubmittedUnknown
    );
}

#[test]
fn exact_interrupt_and_connection_scoped_leases() {
    let root = private_root();
    let mut host = host(root.path(), "interrupt");
    let mut c = Client::connect(root.path()).unwrap();
    assert_eq!(call(&mut c, json!({"op":"attach"}))["attached"], true);
    assert_eq!(call(&mut c, json!({"op":"renew"}))["renewed"], true);
    let mut other = Client::connect(root.path()).unwrap();
    assert!(call(&mut other, json!({"op":"renew"}))["error"].is_string());
    call(
        &mut c,
        json!({"op":"submit","id":"original","payload":"prompt"}),
    );
    wait(|| {
        call(&mut c, json!({"op":"receipt","id":"original"}))["receipt"]["outcome"]["state"]
            == "accepted"
    });
    assert_eq!(
        call(
            &mut c,
            json!({"op":"interrupt","id":"cancel","target":"original"})
        )["outcome"]["state"],
        "unknown"
    );
    wait(|| {
        call(&mut c, json!({"op":"receipt","id":"cancel"}))["receipt"]["outcome"]["state"]
            == "terminal"
    });
    assert_eq!(
        call(&mut c, json!({"op":"receipt","id":"original"}))["receipt"]["outcome"]["state"],
        "terminal"
    );
    call(&mut c, json!({"op":"detach"}));
    stop(root.path());
    assert!(host.child.as_mut().unwrap().wait().unwrap().success());
}

#[test]
fn recovery_requires_exact_birth_and_positive_host_proof() {
    use agent_link_supervisor::{Binding, Limits, Supervisor};
    let root = private_root();
    let mut host = host(root.path(), "normal");
    stop(root.path());
    assert!(host.child.as_mut().unwrap().wait().unwrap().success());
    let binding = Binding {
        provider: "codex".into(),
        session: "t".into(),
    };
    let limits = Limits {
        lease_ttl: Duration::from_millis(300),
        detached_ttl: Duration::from_millis(800),
        ..Default::default()
    };
    let worker = Supervisor::open(root.path(), binding.clone(), limits).unwrap();
    let birth = worker.fenced_runtime().unwrap().unwrap();
    drop(worker);
    assert!(
        agent_link_daemon::recover(root.path(), binding.clone(), limits, birth, |_| false).is_err()
    );
    let stale = agent_link_supervisor::RuntimeId {
        generation: birth.generation + 1,
        ..birth
    };
    assert!(
        agent_link_daemon::recover(root.path(), binding.clone(), limits, stale, |_| panic!(
            "stale proof called"
        ))
        .is_err()
    );
    // Trusted synthetic peer has no descendants/remote work; owned service exited
    // after direct-child cleanup. This fixture proof is not a production OS probe.
    agent_link_daemon::recover(root.path(), binding.clone(), limits, birth, |id| {
        id == birth
    })
    .unwrap();
    let worker = Supervisor::open(root.path(), binding, limits).unwrap();
    assert!(worker.fenced_runtime().unwrap().is_none());
}
