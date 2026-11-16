//! Unit tests: line framing, the composed launch of every built-in, the shell argv, the env
//! defaults, trace cutting and a child reaped outside its handle.

use std::ffi::{OsStr, OsString};

use super::command::{child_env, shell_command};
use super::pump::trace_snip;
use super::*;
use crate::agent::ProfileRegistry;
use crate::agent::profiles::builtin;

#[test]
fn handles_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RpcChild>();
}

#[test]
fn limits_are_validated_before_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let invalid = [
        RpcLimits {
            max_stdout_line_bytes: 0,
            ..RpcLimits::default()
        },
        RpcLimits {
            max_stderr_line_bytes: 0,
            ..RpcLimits::default()
        },
        RpcLimits {
            max_stdin_line_bytes: 0,
            ..RpcLimits::default()
        },
        RpcLimits {
            max_pending_stdin_bytes: 0,
            ..RpcLimits::default()
        },
        RpcLimits {
            max_pending_stdin_lines: 0,
            ..RpcLimits::default()
        },
        RpcLimits {
            event_capacity: 0,
            ..RpcLimits::default()
        },
        RpcLimits {
            max_pending_stdin_bytes: 1,
            ..RpcLimits::default()
        },
        RpcLimits {
            event_capacity: usize::MAX,
            ..RpcLimits::default()
        },
    ];
    for limits in invalid {
        let mut spec = RpcSpawnSpec::new(RpcLaunch::new("unused", &[]), dir.path());
        spec.limits = limits;
        assert!(matches!(preflight(&spec), Err(Error::InvalidInput(_))));
        assert!(matches!(RpcChild::spawn(spec), Err(Error::InvalidInput(_))));
    }
}

// ---- frame_line: strip `\n` then `\r`, lossy UTF-8 ----------------------------------------

/// A bare `\n`-terminated line loses only the `\n`; an unterminated one is unchanged.
#[test]
fn frame_line_strips_trailing_newline() {
    assert_eq!(frame_line(b"hello\n"), "hello");
    assert_eq!(frame_line(b"hello"), "hello");
}

/// `\r\n` loses both; an interior `\r` stays; a lone trailing `\r` (an unterminated read at
/// EOF) goes too — the two pops are independent.
#[test]
fn frame_line_strips_crlf() {
    assert_eq!(frame_line(b"hello\r\n"), "hello");
    assert_eq!(frame_line(b"a\rb\r\n"), "a\rb");
    assert_eq!(frame_line(b"hello\r"), "hello");
}

/// Invalid UTF-8 becomes U+FFFD instead of dropping the line or panicking.
#[test]
fn frame_line_survives_invalid_utf8() {
    let out = frame_line(&[0xFF, 0xFE, b'x', b'\n']);
    assert!(out.contains('\u{FFFD}'), "{out:?}");
    assert!(out.ends_with('x'), "{out:?}");
}

// ---- the composed launch ------------------------------------------------------------------

const CLAUDE: &str = "exec claude --print --input-format stream-json --output-format stream-json \
     --verbose --include-partial-messages --replay-user-messages --permission-mode bypassPermissions";

/// Every built-in stdio launch is one `exec` command with unattended settings;
/// cursor has no supported stdio preset.
#[test]
fn every_builtin_launch_execs_with_unattended_settings() {
    let registry = ProfileRegistry::builtin();
    let line = |id: &str, resume: bool, probe: bool| -> Option<String> {
        let launch = registry.get(id)?.rpc_launch(resume, probe)?;
        Some(shell_command(&launch, "/bin/zsh").expect("built-in args need no refusal"))
    };
    let claude_resumed = format!("{CLAUDE} --continue");
    let cases: [(&str, bool, bool, &str); 11] = [
        ("pi", false, false, "exec pi --mode rpc"),
        ("pi", true, false, "exec pi --mode rpc --continue"),
        ("pi", false, true, "exec pi --mode rpc --no-session"),
        // resume wins over the probe-only `--no-session`
        ("pi", true, true, "exec pi --mode rpc --continue"),
        ("claude", false, false, CLAUDE),
        ("claude", false, true, CLAUDE),
        ("claude", true, false, claude_resumed.as_str()),
        (
            "codex",
            false,
            false,
            r#"exec codex app-server -c 'approval_policy="never"' -c 'sandbox_mode="danger-full-access"'"#,
        ),
        (
            "codex",
            true,
            true,
            r#"exec codex app-server -c 'approval_policy="never"' -c 'sandbox_mode="danger-full-access"'"#,
        ),
        ("opencode", false, false, "exec opencode acp"),
        ("opencode", true, true, "exec opencode acp"),
    ];
    for (id, resume, probe, want) in cases {
        assert_eq!(
            line(id, resume, probe).as_deref(),
            Some(want),
            "{id} resume={resume} probe={probe}"
        );
    }
    assert_eq!(line("cursor", false, false), None);

    // `exec` is load-bearing (the reaped code and the group are the agent's, not a
    // wrapping shell's): every built-in launch starts with it.
    for profile in builtin() {
        for (resume, probe) in [(false, false), (true, false), (false, true), (true, true)] {
            if let Some(launch) = profile.rpc_launch(resume, probe) {
                let command = shell_command(&launch, "/bin/zsh").expect("quotable");
                assert!(command.starts_with("exec "), "{}: {command}", profile.id());
            }
        }
    }
}

/// Words that need it are single-quoted (launcher too); a control character is refused.
#[test]
fn launch_words_are_posix_quoted_and_controls_refused() {
    let launch = RpcLaunch::new("my agent", &["two words", "it's", "--plain=1", ""]);
    assert_eq!(
        shell_command(&launch, "/bin/zsh").unwrap(),
        r"exec 'my agent' 'two words' 'it'\''s' --plain=1 ''"
    );
    for shell in ["/bin/zsh", "/opt/homebrew/bin/fish"] {
        for bad in ["a\nb", "a\u{1b}[Ab", "\u{0}"] {
            let err = shell_command(&RpcLaunch::new("agent", &[bad]), shell).unwrap_err();
            assert!(
                matches!(err, Error::InvalidInput(_)),
                "{shell} {bad:?}: {err}"
            );
        }
    }
}

/// fish reads `\\` and `\'` as escapes inside single quotes: a word holding a backslash is
/// quoted with both escaped there; any other word is quoted as for POSIX shells.
#[test]
fn fish_gets_backslashes_escaped_inside_single_quotes() {
    let launch = RpcLaunch::new(
        "agent",
        &[r"a\b", r"a\\b", "end\\", r"it\'s", "it's", "plain"],
    );
    assert_eq!(
        shell_command(&launch, "/opt/homebrew/bin/fish").unwrap(),
        r"exec agent 'a\\b' 'a\\\\b' 'end\\' 'it\\\'s' 'it'\''s' plain"
    );
    assert_eq!(
        shell_command(&launch, "/bin/bash").unwrap(),
        r"exec agent 'a\b' 'a\\b' 'end\' 'it\'\''s' 'it'\''s' plain"
    );
}

/// The launch runs through the given shell: `-l -i -c` for zsh and bash (and any other
/// shell), plain `<shell> -c` for a shell named `sh`, preserving its path.
#[cfg(unix)]
#[test]
fn the_launch_runs_through_the_shell_as_one_exec_command() {
    let dir = tempfile::tempdir().unwrap();
    let launch = ProfileRegistry::builtin()
        .get("claude")
        .and_then(|p| p.rpc_launch(false, false))
        .unwrap();
    for (shell, program, flags) in [
        ("/bin/zsh", "/bin/zsh", &["-l", "-i", "-c"][..]),
        ("/usr/bin/bash", "/usr/bin/bash", &["-l", "-i", "-c"][..]),
        ("/usr/local/bin/sh", "/usr/local/bin/sh", &["-c"][..]),
    ] {
        let mut spec = RpcSpawnSpec::new(launch.clone(), dir.path());
        spec.shell = Some(shell.into());
        let planned = command::plan(&spec).expect("plan");
        let cmd = &planned.command;
        assert_eq!(cmd.get_program(), OsStr::new(program), "{shell}");
        let args: Vec<&OsStr> = cmd.get_args().collect();
        let mut want: Vec<&OsStr> = flags.iter().map(OsStr::new).collect();
        want.push(OsStr::new(CLAUDE));
        assert_eq!(args, want, "{shell}");
        assert_eq!(cmd.get_current_dir(), Some(dir.path()));
    }
}

// ---- env defaults -------------------------------------------------------------------------

/// What `key` ends up as in `env`: the last `set` value unless removed.
fn value(env: &EnvPolicy, key: &str) -> Option<OsString> {
    if !env.sets(OsStr::new(key)) {
        return None;
    }
    env.set
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

/// A default applies only where the policy does not set the key: a user's value wins and
/// a key the policy removes stays removed.
#[test]
fn env_defaults_yield_to_the_host_policy() {
    let dir = tempfile::tempdir().unwrap();
    let mut launch = RpcLaunch::new("agent", &[]);
    launch.env_defaults = vec![("DEF".into(), "default".into())];
    let mut spec = RpcSpawnSpec::new(launch, dir.path());
    assert_eq!(value(&child_env(&spec), "DEF"), Some("default".into()));

    spec.env.set("DEF", "user");
    assert_eq!(value(&child_env(&spec), "DEF"), Some("user".into()));

    spec.env.remove("DEF");
    assert_eq!(value(&child_env(&spec), "DEF"), None);
    assert!(spec.env.remove.iter().any(|k| k == "DEF"));
}

#[test]
fn explicit_launch_env_overrides_host_values_but_host_removals_win() {
    let dir = tempfile::tempdir().unwrap();
    let mut launch = RpcLaunch::new("agent", &[]);
    launch.env_defaults = vec![("POLICY".into(), "default".into())];
    launch.env = vec![("POLICY".into(), "custom".into())];
    let mut spec = RpcSpawnSpec::new(launch, dir.path());
    spec.env.set("POLICY", "host");
    assert_eq!(value(&child_env(&spec), "POLICY"), Some("custom".into()));
    spec.env.remove("POLICY");
    assert_eq!(value(&child_env(&spec), "POLICY"), None);
}

/// The built-ins through the real command: only opencode's child carries the
/// `OPENCODE_PERMISSION` default (the others run unattended through their own flags), a
/// user value is not clobbered, and a removal stays a removal.
#[cfg(unix)]
#[test]
fn only_opencode_gets_the_permission_default_and_a_user_value_wins() {
    use std::collections::HashMap;
    use std::process::Command;

    use crate::agent::profiles::opencode::{OPENCODE_PERMISSION_DEFAULT, OPENCODE_PERMISSION_ENV};

    fn envs(cmd: &Command) -> HashMap<OsString, Option<OsString>> {
        cmd.get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(OsStr::to_owned)))
            .collect()
    }
    let key = OsStr::new(OPENCODE_PERMISSION_ENV);
    let dir = tempfile::tempdir().unwrap();
    for profile in builtin() {
        let Some(launch) = profile.rpc_launch(false, false) else {
            continue;
        };
        let spec = RpcSpawnSpec::new(launch, dir.path());
        let envs = envs(&command::plan(&spec).expect("plan").command);
        let want: Option<Option<OsString>> =
            (profile.id() == "opencode").then(|| Some(OPENCODE_PERMISSION_DEFAULT.into()));
        assert_eq!(envs.get(key), want.as_ref(), "{}", profile.id());
    }

    let opencode = ProfileRegistry::builtin()
        .get("opencode")
        .and_then(|p| p.rpc_launch(false, false))
        .unwrap();
    let user = r#"{"external_directory":"ask","bash":"ask"}"#;
    let mut spec = RpcSpawnSpec::new(opencode, dir.path());
    spec.env.set(OPENCODE_PERMISSION_ENV, user);
    let got = envs(&command::plan(&spec).unwrap().command);
    assert_eq!(got.get(key), Some(&Some(OsString::from(user))));

    spec.env.remove(OPENCODE_PERMISSION_ENV);
    let got = envs(&command::plan(&spec).unwrap().command);
    assert_eq!(got.get(key), Some(&None), "recorded as a removal");
}

// ---- logging ------------------------------------------------------------------------------

#[test]
fn trace_lines_are_cut_by_characters() {
    assert_eq!(trace_snip("short"), "short");
    let long = "é".repeat(450);
    let cut = trace_snip(&long);
    assert!(cut.starts_with(&"é".repeat(400)), "{cut}");
    assert!(cut.ends_with("… (+50 chars)"), "{cut}");
}

// ---- a child reaped outside the handle ----------------------------------------------------

/// A `/bin/sh -c 'exit 4'` child that a plain `waitpid` already reaped, as a host's own
/// `SIGCHLD` handling would: its pid may name another process from now on.
#[cfg(unix)]
fn child_reaped_elsewhere() -> RpcChild {
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let mut spec = RpcSpawnSpec::new(RpcLaunch::new("sh", &["-c", "exit 4"]), dir.path());
    spec.shell = Some("/bin/sh".into());
    let (child, events) = RpcChild::spawn(spec).expect("spawn");
    // Generous: a child start is two execs, and exec vetting under load has taken over 20 s.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match events.recv_deadline(deadline) {
            Ok(RpcEvent::Exit { .. }) => break,
            Ok(_) => {}
            Err(e) => panic!("no Exit: {e}"),
        }
    }
    let pid = child.pid() as libc::pid_t;
    let mut status = 0;
    // SAFETY: a blocking waitpid on our own child, as a host's reaper would do.
    let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
    assert_eq!(reaped, pid, "{}", std::io::Error::last_os_error());
    child
}

/// The exit reads as "exited, code unknown" and teardown signals nothing.
#[cfg(unix)]
#[test]
fn a_child_reaped_elsewhere_reads_exited_and_is_never_signalled() {
    let child = child_reaped_elsewhere();
    assert_eq!(child.try_wait().unwrap(), Some(None));
    assert_eq!(child.try_wait().unwrap(), Some(None), "sticky");
    assert!(lock(&child.child).is_reaped(), "never signalled again");
    let observation = child.reap_observation();
    assert_eq!(child.kill(), None);
    assert_eq!(observation.status(), RpcReapStatus::Unknown);
}

/// With no `try_wait` after the reap elsewhere, teardown still finds it right before the
/// kill: it returns early — no kill, and its own reap never takes the child.
#[cfg(unix)]
#[test]
fn teardown_checks_for_a_reap_elsewhere_right_before_the_kill() {
    let child = child_reaped_elsewhere();
    {
        let mut state = lock(&child.child);
        assert_eq!(state.kill_and_reap(), None);
        assert!(state.is_reaped());
        assert!(state.holds_child(), "returned before the kill and the reap");
    }
    assert_eq!(child.try_wait().unwrap(), Some(None));
}

#[cfg(unix)]
#[test]
fn owned_reap_observation_survives_teardown() {
    let temp = tempfile::tempdir().unwrap();
    let mut spec = RpcSpawnSpec::new(RpcLaunch::new("/bin/cat", &[]), temp.path());
    spec.shell = Some("/bin/sh".into());
    spec.env.inherit = false;
    let (child, _events) = RpcChild::spawn(spec).unwrap();
    let observation = child.reap_observation();
    assert_eq!(observation.status(), RpcReapStatus::Pending);
    child.kill();
    assert_eq!(observation.status(), RpcReapStatus::Reaped);
}
