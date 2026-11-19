//! Public admission checks for controlled interactive transcripts. No vendor CLI is required.
#![cfg(feature = "turns")]

use agent_link::Error;
use agent_link::agent::turns::ExclusiveClaudeRoot;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
#[test]
fn platforms_without_native_writer_evidence_report_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        ExclusiveClaudeRoot::new(dir.path()),
        Err(Error::Unsupported(_))
    ));
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn invalid_controlled_launches_are_rejected_before_child_creation() {
    use agent_link::agent::profiles::ClaudeProfile;
    use agent_link::agent::{AgentSpec, AgentTerminal};
    use agent_link::{PtySize, SpawnSpec};
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        sync::Arc,
    };

    let scratch = tempfile::tempdir().unwrap();
    let project = scratch.path().canonicalize().unwrap();
    let config = project.join("config");
    fs::create_dir(&config).unwrap();
    let lease = ExclusiveClaudeRoot::new(&config).unwrap();
    assert!(!format!("{lease:?}").contains(project.to_str().unwrap()));
    let alias = project.join("link");
    symlink(&config, &alias).unwrap();
    for link in [&alias, &alias.join(""), &alias.join(".")] {
        assert!(matches!(
            ExclusiveClaudeRoot::new(link),
            Err(Error::Io { .. })
        ));
    }

    let cli = project.join("claude");
    fs::write(&cli, "#!/bin/sh\ntouch started\n").unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    let size = PtySize { rows: 24, cols: 80 };
    let id = "00000000-0000-4000-8000-000000000001";
    let agent = || AgentSpec::new(Arc::new(ClaudeProfile::default()));
    let spec = |root: &std::path::Path, args: &[&str]| {
        let mut spec = SpawnSpec::direct(&cli, args.iter().copied(), &project, size);
        spec.env.set("CLAUDE_CONFIG_DIR", root);
        spec
    };
    assert!(matches!(
        AgentTerminal::spawn_with_claude_turns(SpawnSpec::shell(&project, size), agent(), &lease),
        Err(Error::Unsupported(_))
    ));
    let fresh = ["--session-id", id];
    let fallback = project.join(".claude");
    fs::create_dir(&fallback).unwrap();
    let fallback_lease = ExclusiveClaudeRoot::new(&fallback).unwrap();
    let mut custom = SpawnSpec::direct(&cli, fresh, &project, size);
    custom.env.inherit = false;
    custom.env.set("HOME", &project);
    custom.identity = agent_link::TerminalIdentity::Custom {
        vars: vec![("CLAUDE_CONFIG_DIR".into(), config.to_str().unwrap().into())],
    };
    assert!(matches!(
        AgentTerminal::spawn_with_claude_turns(custom, agent(), &fallback_lease),
        Err(Error::InvalidInput(_))
    ));
    for (root, args) in [
        (project.as_path(), &fresh[..]),
        (config.as_path(), &["--resume", id][..]),
        (
            config.as_path(),
            &["--session-id", id, "--session-id", id][..],
        ),
    ] {
        assert!(matches!(
            AgentTerminal::spawn_with_claude_turns(spec(root, args), agent(), &lease),
            Err(Error::InvalidInput(_))
        ));
    }
    assert!(matches!(
        AgentTerminal::spawn_with_claude_turns(spec(&config, &fresh), AgentSpec::default(), &lease),
        Err(Error::InvalidInput(_))
    ));
    lease.revoke();
    assert!(matches!(
        AgentTerminal::spawn_with_claude_turns(spec(&config, &fresh), agent(), &lease),
        Err(Error::InvalidInput(_))
    ));
    assert!(!project.join("started").exists());
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn revoking_while_a_profile_callback_is_parked_prevents_spawn() {
    use agent_link::agent::{
        AgentProfile, AgentSpec, AgentTerminal, Dialog, LaunchPlan, Permissions, ShellKind,
        TrustPolicy,
    };
    use agent_link::{PtySize, SpawnSpec};
    use crossbeam_channel::{Receiver, Sender, bounded};
    use std::{fs, os::unix::fs::PermissionsExt, path::Path, sync::Arc, time::Duration};

    struct ParkedProfile {
        entered: Sender<()>,
        resume: Receiver<()>,
    }
    impl AgentProfile for ParkedProfile {
        fn id(&self) -> &str {
            "claude"
        }
        fn dialog(&self, _: &str) -> Option<Dialog> {
            None
        }
        fn input_ready(&self, _: &str) -> bool {
            false
        }
        fn launch(&self, _: &Permissions, _: &TrustPolicy, _: &Path, _: ShellKind) -> LaunchPlan {
            LaunchPlan::new("claude")
        }
        fn reset_command(&self) -> Option<&'static str> {
            self.entered.send(()).unwrap();
            self.resume.recv_timeout(Duration::from_secs(5)).unwrap();
            None
        }
    }
    let root = tempfile::tempdir().unwrap();
    let lease = ExclusiveClaudeRoot::new(root.path()).unwrap();
    let exe = root.path().join("claude");
    fs::write(&exe, "#!/bin/sh\ntouch started\n").unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
    let mut spec = SpawnSpec::direct(
        exe,
        ["--session-id", "00000000-0000-4000-8000-000000000009"],
        root.path(),
        PtySize { rows: 24, cols: 80 },
    );
    spec.env.set("CLAUDE_CONFIG_DIR", root.path());
    let (enter, entered) = bounded(1);
    let (resume, resumed) = bounded(1);
    let agent = AgentSpec::new(Arc::new(ParkedProfile {
        entered: enter,
        resume: resumed,
    }));
    std::thread::scope(|scope| {
        let grant = &lease;
        let spawn = scope.spawn(move || AgentTerminal::spawn_with_claude_turns(spec, agent, grant));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        lease.revoke();
        resume.send(()).unwrap();
        assert!(matches!(spawn.join().unwrap(), Err(Error::InvalidInput(_))));
    });
    assert!(!root.path().join("started").exists());
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod logger {
    use super::*;
    use agent_link::agent::profiles::ClaudeProfile;
    use agent_link::agent::turns::{AttachError, TurnOptions};
    use agent_link::agent::{AgentSpec, AgentTerminal};
    use agent_link::{PtySize, SpawnSpec};
    use std::{
        cell::Cell,
        fs,
        os::unix::fs::PermissionsExt,
        sync::{Arc, Once},
        time::Duration,
    };

    #[derive(Copy, Clone)]
    enum Site {
        ShellWarning,
        Spawned,
    }
    thread_local! {
        static LEASE: Cell<Option<(Site, Arc<ExclusiveClaudeRoot>)>> = const { Cell::new(None) };
    }
    struct RevokeOnSpawn;
    impl log::Log for RevokeOnSpawn {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            let Some((site, lease)) = LEASE.take() else {
                return;
            };
            let matches = match site {
                Site::ShellWarning => {
                    record.target() == "portable_pty::cmdbuilder"
                        && record.args().to_string().contains("$SHELL ->")
                }
                Site::Spawned => {
                    record.target() == "agent_link::pty::session"
                        && record.args().to_string().contains("] spawned ")
                }
            };
            if matches {
                lease.revoke();
            } else {
                LEASE.set(Some((site, lease)));
            }
        }
        fn flush(&self) {}
    }

    fn check_revoke(site: Site) {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            log::set_logger(&RevokeOnSpawn).unwrap();
            log::set_max_level(log::LevelFilter::Debug);
        });
        let (done, result) = crossbeam_channel::bounded(1);
        let spawn = std::thread::spawn(move || {
            let root = tempfile::tempdir().unwrap();
            let exe = root.path().join("claude");
            fs::write(&exe, "#!/bin/sh\n: > started\n").unwrap();
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
            let lease = Arc::new(ExclusiveClaudeRoot::new(root.path()).unwrap());
            let shell = root.path().join("non-executable");
            fs::write(&shell, []).unwrap();
            fs::set_permissions(&shell, fs::Permissions::from_mode(0o600)).unwrap();
            let id = match site {
                Site::Spawned => "00000000-0000-4000-8000-000000000017",
                Site::ShellWarning => "00000000-0000-4000-8000-000000000019",
            };
            let mut spec = SpawnSpec::direct(
                exe,
                ["--session-id", id],
                root.path(),
                PtySize { rows: 24, cols: 80 },
            );
            spec.env
                .set("CLAUDE_CONFIG_DIR", root.path())
                .set("SHELL", shell);
            LEASE.set(Some((site, lease.clone())));
            let agent = AgentSpec::new(Arc::new(ClaudeProfile::default()));
            let spawned = AgentTerminal::spawn_with_claude_turns(spec, agent, &lease);
            assert!(
                LEASE.take().is_none(),
                "did not reach the selected log site"
            );
            match site {
                Site::ShellWarning => {
                    assert!(matches!(spawned, Err(Error::InvalidInput(_))));
                    assert!(!root.path().join("started").exists());
                }
                Site::Spawned => {
                    let (term, _events) = spawned.unwrap();
                    assert!(matches!(
                        term.attach_claude_turns(TurnOptions::default()),
                        Err(AttachError::AuthorityRevoked)
                    ));
                }
            }
            done.send(()).unwrap();
        });
        result
            .recv_timeout(Duration::from_secs(10))
            .expect("spawn returned after its logger revoked the lease");
        spawn.join().unwrap();
    }

    #[test]
    fn a_shell_warning_logger_can_revoke_before_child_creation() {
        check_revoke(Site::ShellWarning);
    }

    #[test]
    fn a_post_spawn_logger_can_revoke_its_lease() {
        check_revoke(Site::Spawned);
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn the_child_writes_to_the_leased_directory_after_its_name_is_replaced() {
    use agent_link::agent::profiles::ClaudeProfile;
    use agent_link::agent::{AgentSpec, AgentTerminal};
    use agent_link::{Event, PtySize, SpawnSpec};
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::Arc,
        time::{Duration, Instant},
    };

    let project = tempfile::tempdir().unwrap();
    let config = project.path().join("config");
    fs::create_dir(&config).unwrap();
    let lease = ExclusiveClaudeRoot::new(&config).unwrap();
    let cli = project.path().join("claude");
    fs::write(
        &cli,
        "#!/bin/sh\nread -r line && : > \"$CLAUDE_CONFIG_DIR/marker\"\n",
    )
    .unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    let mut spec = SpawnSpec::direct(
        cli,
        ["--session-id", "00000000-0000-4000-8000-000000000021"],
        project.path(),
        PtySize { rows: 24, cols: 80 },
    );
    spec.env.set("CLAUDE_CONFIG_DIR", "config");
    let agent = AgentSpec::new(Arc::new(ClaudeProfile::default()));
    let (term, events) = AgentTerminal::spawn_with_claude_turns(spec, agent, &lease).unwrap();
    let moved = project.path().join("moved");
    fs::rename(&config, &moved).unwrap();
    fs::create_dir(&config).unwrap();
    term.terminal().write(b"go\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(events.recv_deadline(deadline).unwrap(), Event::Exit { .. }) {}
    assert!(moved.join("marker").is_file());
    assert!(!config.join("marker").exists());
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn a_leased_spawn_reports_exec_failure_then_runs_the_child_on_its_own_pty() {
    use agent_link::agent::profiles::ClaudeProfile;
    use agent_link::agent::{AgentSpec, AgentTerminal};
    use agent_link::{Event, PtySize, SpawnSpec};
    use std::{
        fs,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::{ffi::OsStrExt, fs::PermissionsExt},
        },
        sync::Arc,
        time::{Duration, Instant},
    };

    let root = tempfile::tempdir().unwrap();
    let lease = ExclusiveClaudeRoot::new(root.path()).unwrap();
    let cli = root.path().join("claude");
    fs::write(
        &cli,
        r#"#!/bin/sh
set -eu
test -t 0 && test -t 1 && test -t 2 || exit 1
test ! -e "/dev/fd/$HOST_FD"
test "$SHELL" = "$EXPECTED_SHELL"
printf '%s' "$RAW_ENV" > env-bytes
printf 'pid=%s size=' "$$"
/bin/stty size </dev/tty
"#,
    )
    .unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o600)).unwrap();

    // Deliberately open an inheritable high fd that the executed program can detect.
    let file = fs::File::open(&cli).unwrap();
    // SAFETY: F_DUPFD duplicates the live descriptor; the successful result is uniquely owned.
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 128) };
    assert!(fd >= 128);
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut spec = SpawnSpec::direct(
        &cli,
        ["--session-id", "00000000-0000-4000-8000-000000000018"],
        root.path(),
        PtySize { rows: 24, cols: 80 },
    );
    spec.env.inherit = false;
    spec.env
        .set("CLAUDE_CONFIG_DIR", root.path())
        .set("SHELL", root.path().join("missing"))
        .set("EXPECTED_SHELL", root.path().join("missing"))
        .set("RAW_ENV", std::ffi::OsStr::from_bytes(b"raw-\xff"))
        .set("HOST_FD", fd.as_raw_fd().to_string());
    let spawn = || {
        AgentTerminal::spawn_with_claude_turns(
            spec.clone(),
            AgentSpec::new(Arc::new(ClaudeProfile::default())),
            &lease,
        )
    };
    assert!(matches!(spawn(), Err(Error::Spawn(_))));
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    let (term, events) = spawn().unwrap();
    let pid = term.terminal().pid().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = Vec::new();
    loop {
        match events.recv_deadline(deadline).unwrap() {
            Event::Output { bytes, .. } => output.extend(bytes),
            Event::Exit { .. } => break,
            _ => {}
        }
    }
    assert_eq!(
        String::from_utf8_lossy(&output).trim(),
        format!("pid={pid} size=24 80")
    );
    assert_eq!(
        fs::read(root.path().join("env-bytes")).unwrap(),
        b"raw-\xff"
    );
}
