//! Finite, nonblocking admission against a child that never reads its terminal.
#![cfg(unix)]

use agent_link::{Error, Event, InputLimits, PtySize, SpawnSpec, Terminal};
use std::time::{Duration, Instant};

#[test]
fn nonreading_child_refuses_input_without_blocking_the_host() {
    let mut spec = SpawnSpec::direct(
        "/bin/sh",
        ["-c", "stty raw -echo; printf ready; exec sleep 60"],
        std::env::temp_dir(),
        PtySize { rows: 24, cols: 80 },
    );
    spec.input_limits = InputLimits {
        max_write_bytes: 64 * 1024,
        max_pending_bytes: 128 * 1024,
        max_pending_chunks: 2,
    };
    let (term, events) = Terminal::spawn(spec).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut output = Vec::new();
    while !output.windows(5).any(|s| s == b"ready") {
        let left = deadline.saturating_duration_since(Instant::now());
        match events
            .recv_timeout(left)
            .expect("child did not become ready")
        {
            Event::Output { bytes, .. } => output.extend_from_slice(&bytes),
            Event::Exit { .. } => panic!("child exited before becoming ready"),
            _ => {}
        }
    }
    let large = vec![b'x'; 64 * 1024 + 1];
    assert!(matches!(
        term.write(&large),
        Err(Error::InputTooLarge { .. })
    ));
    let mut refused = false;
    for _ in 0..1000 {
        match term.write(&large[..64 * 1024]) {
            Ok(()) => {}
            Err(Error::QueueFull) => {
                refused = true;
                break;
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    assert!(
        refused,
        "nonreading child exhausted the bounded input queue"
    );
    drop(term);
}

#[test]
fn invalid_input_limits_are_rejected_before_spawning() {
    for limits in [
        InputLimits {
            max_pending_chunks: usize::MAX,
            ..InputLimits::default()
        },
        InputLimits {
            max_write_bytes: usize::MAX,
            max_pending_bytes: usize::MAX,
            ..InputLimits::default()
        },
        InputLimits {
            max_pending_chunks: 0,
            ..InputLimits::default()
        },
        InputLimits {
            max_pending_bytes: 1,
            ..InputLimits::default()
        },
    ] {
        // A spawn attempt would instead fail with Error::Spawn for this nonexistent path.
        let mut spec = SpawnSpec::direct(
            "/nonexistent-agent-link-input-test",
            [] as [&str; 0],
            std::env::temp_dir(),
            PtySize { rows: 24, cols: 80 },
        );
        spec.input_limits = limits;
        assert!(matches!(Terminal::spawn(spec), Err(Error::InvalidInput(_))));
    }
}
