use super::*;
use crate::agent::turns::Claim;
use crate::agent::turns::tests::peer::{clock, tick};
use crate::proc::instance::Instance;

#[test]
fn immediate_attachment_waits_for_the_first_writer_record_and_starts_at_verified_eof() {
    let (_lock, home) = project();
    let (clock, options) = clock();
    let mut receiver = None;
    let peer = Peer::launch(home.path(), false, false, true, |term| {
        let rx = ClaudeSource::attach(term, options).unwrap();
        // Rendezvous after the worker has sampled the still-missing record.
        tick(&clock);
        assert_eq!(rx.status(), Status::Waiting);
        assert!(rx.try_recv().is_none());
        receiver = Some(rx);
    });
    let rx = receiver.unwrap();
    tick(&clock);
    observed(&rx, &peer.file);
    assert!(rx.try_recv().is_none());
    peer.write(&peer.initial, "fresh after startup");
    tick(&clock);
    observed(&rx, &peer.file);
    assert_eq!(rx.try_recv().unwrap().text, "fresh after startup");
    fs::remove_file(&peer.witness).unwrap();
    tick(&clock);
    gap(&rx, Gap::Uncorroborated);
}

#[test]
fn a_delayed_peer_cannot_authorize_an_existing_readers_queue() {
    let (_lock, home) = project();
    let a = Peer::start(home.path(), false, false);
    let (clock, options) = clock();
    let rx = ClaudeSource::attach(&a.term, options).unwrap();
    observed(&rx, &a.file);
    a.write(&a.initial, "queued");
    tick(&clock);
    observed(&rx, &a.file);
    let _b = Peer::launch(home.path(), false, false, true, |_| {
        assert!(rx.try_recv().is_none());
        gap(&rx, Gap::Uncorroborated);
    });
}

#[test]
fn a_queued_reply_is_withheld_when_a_peer_switches_before_its_first_attachment() {
    let (_lock, home) = project();
    for attach_peer_after_switch in [false, true] {
        let a = Peer::start(home.path(), false, false);
        let b = Peer::start(home.path(), false, false);
        let (clock, options) = clock();
        let rx = ClaudeSource::attach(&a.term, options).unwrap();
        observed(&rx, &a.file);
        a.write(&a.initial, "queued before overlap");
        tick(&clock);
        observed(&rx, &a.file);

        fs::write(b.home.path().join("next"), &a.initial).unwrap();
        b.command("/clear");
        // The recorded concurrent --resume case supplies identical row identities.
        fs::write(
            b.home.path().join("batch"),
            encode(&a.rows(&a.initial, "foreign reply")),
        )
        .unwrap();
        b.command("append");
        if attach_peer_after_switch {
            gap(&b.attach(), Gap::Uncorroborated);
        }
        assert!(rx.try_recv().is_none());
        gap(&rx, Gap::Uncorroborated);
        gap(&a.attach(), Gap::Uncorroborated);
    }
}

#[test]
fn first_attachment_after_clear_requires_an_observation_of_the_initial_writer() {
    let (_lock, home) = project();
    for (observed_by_peer, overlap) in [(false, false), (true, false), (true, true)] {
        let a = Peer::start(home.path(), false, false);
        let b = Peer::start(home.path(), false, false);
        let _overlapping_peer = overlap.then(|| {
            let c = Peer::start(home.path(), false, false);
            fs::write(c.home.path().join("next"), &b.initial).unwrap();
            c.command("/clear");
            c
        });
        if observed_by_peer {
            let rx = a.attach();
            observed(&rx, &a.file);
        }

        let next = fresh_id();
        fs::write(b.home.path().join("next"), &next).unwrap();
        b.command("/clear");
        b.write(&next, "before first attachment");
        let rx = b.attach();
        if observed_by_peer && !overlap {
            let dest = b.file.with_file_name(format!("{next}.jsonl"));
            observed(&rx, &dest);
            assert!(rx.try_recv().is_none());
            b.write(&next, "after first attachment");
            observed(&rx, &dest);
            assert_eq!(rx.try_recv().unwrap().text, "after first attachment");
        } else {
            gap(&rx, Gap::Uncorroborated);
        }
    }
}

fn reserved_launch(cwd: &Path, config: &Path) -> Claim {
    let mut spec = SpawnSpec::direct(
        "/bin/bash",
        ["peer.sh", "--session-id", &fresh_id()],
        cwd,
        SIZE,
    );
    spec.env.inherit = false;
    spec.env.set("CLAUDE_CONFIG_DIR", config);
    capture_scripted(&mut spec, &LaunchPlan::new("ignored")).unwrap()
}

#[test]
fn a_launch_that_fails_pty_preparation_reserves_its_uuid_and_preserves_peer_replies() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), false, false);
    let (clock, options) = clock();
    let rx = ClaudeSource::attach(&peer.term, options).unwrap();
    observed(&rx, &peer.file);
    peer.write(&peer.initial, "during preparation");
    tick(&clock);
    observed(&rx, &peer.file);

    let id = fresh_id();
    let mut spec = SpawnSpec::direct(
        "/bin/bash",
        ["peer.sh", "--session-id", &id],
        &peer.cwd,
        SIZE,
    );
    spec.size.rows = 0; // PTY validation runs after capture, before child creation.
    spec.env.set("CLAUDE_CONFIG_DIR", peer.cwd.join("config"));
    let mut duplicate = spec.clone();
    let (barrier, pause) = Barrier::new();
    let agent = AgentSpec {
        turn_launch: Some(Arc::new(move |spec, plan| {
            let claim = capture_scripted(spec, plan)?;
            pause.park();
            Ok(claim)
        })),
        ..AgentSpec::new(Arc::new(Profile(id)))
    };
    std::thread::scope(|scope| {
        let launch = scope.spawn(move || AgentTerminal::spawn(spec, agent));
        barrier.wait();
        assert!(capture_scripted(&mut duplicate, &LaunchPlan::new("ignored")).is_err());
        assert_eq!(rx.try_recv().unwrap().text, "during preparation");
        barrier.release();
        assert!(matches!(
            launch.join().unwrap(),
            Err(crate::Error::InvalidInput(_))
        ));
    });
    assert!(capture_scripted(&mut duplicate, &LaunchPlan::new("ignored")).is_ok());
    peer.write(&peer.initial, "after failed launch");
    tick(&clock);
    observed(&rx, &peer.file);
    assert_eq!(rx.try_recv().unwrap().text, "after failed launch");
}

#[test]
fn an_unconfirmed_writer_is_scoped_to_the_same_pinned_transcript_directory() {
    use super::super::super::source::AFTER_WRITER_SAMPLES;

    let (_lock, home) = project();
    let a = Peer::start(home.path(), false, false);
    let (clock, options) = clock();
    let rx = ClaudeSource::attach(&a.term, options).unwrap();
    observed(&rx, &a.file);

    let separate_root = tempfile::tempdir_in(&a.cwd).unwrap();
    let independent = reserved_launch(&a.cwd, separate_root.path());
    let other_project = reserved_launch(separate_root.path(), &a.cwd.join("config"));
    for claim in [&independent, &other_project] {
        claim.writer.as_ref().unwrap().begin_spawn();
    }
    a.write(&a.initial, "confirmed");
    tick(&clock);
    observed(&rx, &a.file);
    assert_eq!(rx.try_recv().unwrap().text, "confirmed");

    a.write(&a.initial, "queued");
    tick(&clock);
    observed(&rx, &a.file);
    let alias = a.cwd.join("alias");
    symlink(&a.cwd, &alias).unwrap();
    // Admission between sampling and reconciliation makes this aliased writer
    // relevant to the in-flight pull, even before its child has a PID.
    let starting = reserved_launch(&a.cwd, &alias.join("config"));
    let (barrier, pause) = Barrier::new();
    std::thread::scope(|scope| {
        let rx = &rx;
        let pull = scope.spawn(move || {
            AFTER_WRITER_SAMPLES.set(Some(pause));
            rx.try_recv()
        });
        barrier.wait();
        starting.writer.as_ref().unwrap().begin_spawn();
        barrier.release();
        assert!(pull.join().unwrap().is_none());
    });
    gap(&rx, Gap::Uncorroborated);
    drop(starting);
    let rx = a.attach();
    observed(&rx, &a.file);
    assert!(rx.try_recv().is_none());
    a.write(&a.initial, "fresh");
    observed(&rx, &a.file);
    assert_eq!(rx.try_recv().unwrap().text, "fresh");
}

#[test]
fn retained_exited_peer_allows_queued_and_fresh_replies_on_the_surviving_terminal() {
    let (_lock, home) = project();
    let a = Peer::start(home.path(), false, false);
    let b = Peer::start(home.path(), false, false);
    let (clock, options) = clock();
    let rx = ClaudeSource::attach(&a.term, options).unwrap();
    observed(&rx, &a.file);
    a.write(&a.initial, "already queued");
    tick(&clock);
    observed(&rx, &a.file);

    b.input("exit-peer");
    let pid = b.term.terminal().pid().unwrap();
    until("peer process exited", || {
        crate::pty::peek_exit(pid).unwrap().is_some()
    });
    assert_eq!(rx.try_recv().unwrap().text, "already queued");
    drop(rx);
    let fresh = a.attach();
    observed(&fresh, &a.file);
    a.write(&a.initial, "after exit");
    observed(&fresh, &a.file);
    assert_eq!(fresh.try_recv().unwrap().text, "after exit");
    assert!(
        !b.term
            .terminal()
            .attach()
            .0
            .into_bytes()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_pull_reconciles_a_peer_that_closes_after_its_writer_was_sampled() {
    use super::super::super::source::AFTER_WRITER_SAMPLES;

    let (_lock, home) = project();
    for overlap in [false, true] {
        let a = Peer::start(home.path(), false, false);
        let b = Peer::start(home.path(), false, false);
        let c = Peer::start(home.path(), false, false);
        let (a_clock, options) = clock();
        let rx = ClaudeSource::attach(&a.term, options).unwrap();
        let (c_clock, c_options) = clock();
        let cr = ClaudeSource::attach(&c.term, c_options).unwrap();
        observed(&cr, &c.file);
        observed(&rx, &a.file);
        a.write(&a.initial, "queued before close");
        tick(&a_clock);
        observed(&rx, &a.file);
        c.write(&c.initial, "independent");
        tick(&c_clock);
        observed(&cr, &c.file);
        if overlap {
            fs::write(b.home.path().join("next"), &a.initial).unwrap();
            b.command("/clear");
        }
        let (barrier, pause) = Barrier::new();
        let reply = std::thread::scope(|scope| {
            let rx = &rx;
            let pull = scope.spawn(move || {
                AFTER_WRITER_SAMPLES.set(Some(pause));
                rx.try_recv()
            });
            barrier.wait();
            drop(b);
            barrier.release();
            pull.join().unwrap()
        });
        assert_eq!(cr.try_recv().unwrap().text, "independent");
        if overlap {
            assert!(reply.is_none());
            gap(&rx, Gap::Uncorroborated);
        } else {
            assert_eq!(reply.unwrap().text, "queued before close");
            a.write(&a.initial, "fresh after close");
            tick(&a_clock);
            observed(&rx, &a.file);
            assert_eq!(rx.try_recv().unwrap().text, "fresh after close");
        }
    }
}

#[test]
fn a_queued_pull_closes_when_its_own_child_exits_before_pty_eof() {
    let (_lock, home) = project();
    let peer = Peer::start(home.path(), false, false);
    let (clock, options) = clock();
    let rx = ClaudeSource::attach(&peer.term, options).unwrap();
    observed(&rx, &peer.file);
    peer.write(&peer.initial, "queued before exit");
    tick(&clock);
    observed(&rx, &peer.file);
    crate::agent::turns::tests::peer::with_undelivered_exit(
        peer.term.terminal(),
        peer.events,
        || {
            assert!(matches!(rx.status(), Status::Active { .. }));
            assert!(rx.try_recv().is_none());
            assert_eq!(rx.status(), Status::Closed);
        },
    );
}

#[test]
fn case_variant_project_slugs_fence_queued_replies_until_directories_are_proven_distinct() {
    use std::os::unix::fs::MetadataExt;

    let (_lock, home) = project();
    let project_a = home.path().join("A.b");
    let project_b = home.path().join("a-b");
    fs::create_dir(&project_a).unwrap();
    fs::create_dir(&project_b).unwrap();
    let a = Peer::start(&project_a, false, false);
    let (clock, options) = clock();
    let rx = ClaudeSource::attach(&a.term, options).unwrap();
    observed(&rx, &a.file);
    a.write(&a.initial, "queued");
    tick(&clock);
    observed(&rx, &a.file);
    let starting = reserved_launch(&project_b, &a.cwd.join("config"));
    starting.writer.as_ref().unwrap().begin_spawn();
    assert!(rx.try_recv().is_none());
    gap(&rx, Gap::Uncorroborated);

    let dir_a = a.file.parent().unwrap();
    let slug_a = dir_a.file_name().unwrap().to_str().unwrap();
    let dir_b = dir_a.with_file_name(format!("{}a-b", slug_a.strip_suffix("A-b").unwrap()));
    fs::create_dir_all(&dir_b).unwrap();
    let a_meta = dir_a.metadata().unwrap();
    let b_meta = dir_b.metadata().unwrap();
    let fresh = a.attach();
    if (a_meta.dev(), a_meta.ino()) == (b_meta.dev(), b_meta.ino()) {
        gap(&fresh, Gap::Uncorroborated);
    } else {
        observed(&fresh, &a.file);
        a.write(&a.initial, "separate directories");
        observed(&fresh, &a.file);
        assert_eq!(fresh.try_recv().unwrap().text, "separate directories");
    }
}

#[test]
fn process_instance_rejects_live_pids_outside_the_direct_pty_launch() {
    use std::process::{Command, Stdio};
    assert!(Instance::at_spawn(std::process::id()).is_none());
    // A direct stdio child has the right parent PID, but it doesn't lead a PTY session.
    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let instance = Instance::at_spawn(child.id());
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    assert!(instance.is_none());
}

#[test]
fn the_child_builder_uses_the_captured_directory_and_environment() {
    use portable_pty::CommandBuilder;

    let (_lock, home) = project();
    let link = home.path().join("launch-cwd");
    symlink(home.path(), &link).unwrap();
    let mut spec = SpawnSpec::direct(
        "/bin/bash",
        ["peer.sh", "--session-id", &fresh_id()],
        link,
        SIZE,
    );
    let config = home.path().join("config");
    fs::create_dir(&config).unwrap();
    spec.identity = crate::TerminalIdentity::Custom {
        vars: vec![("CLAUDE_CONFIG_DIR".into(), config.to_str().unwrap().into())],
    };
    let inherited_home = std::env::var_os("HOME");
    let missing = format!("AGENT_LINK_{}", fresh_id());
    assert!(spec.freeze_launch_env([&missing])[0].is_none());
    let claim = capture_scripted(&mut spec, &LaunchPlan::new("ignored")).unwrap();
    assert_eq!(spec.cwd, home.path().canonicalize().unwrap());
    assert_eq!(spec.cwd, claim.cwd);

    // Model an ambient environment change between initial capture and the later
    // command construction without modifying the test process's environment.
    let mut command = CommandBuilder::new("/bin/bash");
    command.env("HOME", "late-home");
    command.env("CLAUDE_CONFIG_DIR", "late-root");
    command.env(&missing, "late-value");
    spec.env.apply(&mut command);
    assert!(command.get_env("HOME") == inherited_home.as_deref());
    assert_eq!(
        command.get_env("CLAUDE_CONFIG_DIR"),
        Some(config.as_os_str())
    );
    assert!(command.get_env(&missing).is_none());
}
