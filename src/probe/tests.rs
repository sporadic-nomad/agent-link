//! Unit tests: which built-ins have a login check; on unix the probe env, the shell argv,
//! `probe_agents` against stub CLIs through `/bin/sh` (no rc files, so a `PATH` holding only
//! the stubs' temp dir stays the only one — no real CLI can answer), and the login shell
//! running in a session of its own.

use crate::agent::ProfileRegistry;

#[test]
fn login_check_answers_only_for_verified_agents() {
    // Every check is verified signed in, signed out AND against an unsupported subcommand
    // (`LoginCheck`'s table); opencode has none because `auth list` is a listing rather than
    // a check, pi and cursor need none. Adding one is a decision backed by that three-way
    // check, not a drive-by.
    let registry = ProfileRegistry::builtin();
    let check = |id: &str| registry.get(id).expect(id).login_check();
    assert_eq!(
        check("codex").map(|c| c.args),
        Some(&["login", "status"][..])
    );
    assert_eq!(
        check("claude").map(|c| c.args),
        Some(&["auth", "status"][..])
    );
    for id in ["opencode", "pi", "cursor"] {
        assert!(check(id).is_none(), "{id}");
    }
    for id in registry.ids() {
        let profile = registry.get(&id).expect("listed");
        if let Some(check) = profile.login_check() {
            assert!(
                !check.signed_out_marker.trim().is_empty(),
                "{id} has no signed-out marker, so a non-zero exit could never be a verdict"
            );
            assert!(
                !profile.launchers().is_empty(),
                "{id} has a login check but no launcher to run it with"
            );
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::collections::HashMap;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::super::run::Probe;
    use super::super::{ProbeResult, probe_agents_in};
    use crate::agent::{AgentProfile, Dialog, LoginCheck, ProfileRegistry};
    use crate::env::EnvPolicy;

    /// Per probe command. It covers two execs (the shell, then the stub), and where
    /// endpoint-security software vets every exec, starts under parallel load have taken over
    /// 20 s. A passing probe is not slower.
    const WAIT: Duration = Duration::from_secs(60);

    /// An executable `#!/bin/sh` script `name` in `dir`.
    fn stub(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// A policy whose `PATH` is `dir` alone.
    fn path_only(dir: &Path) -> EnvPolicy {
        let mut env = EnvPolicy::default();
        env.set("PATH", dir);
        env
    }

    /// `probe_agents` through `/bin/sh`.
    fn probe_sh(
        profiles: &[Arc<dyn AgentProfile>],
        env: &EnvPolicy,
    ) -> HashMap<String, ProbeResult> {
        probe_agents_in(profiles, env, WAIT, Some("/bin/sh"))
    }

    fn builtin(id: &str) -> Arc<dyn AgentProfile> {
        ProfileRegistry::builtin().get(id).expect(id)
    }

    fn result(installed: bool, signed_in: Option<bool>) -> ProbeResult {
        ProbeResult {
            installed,
            signed_in,
        }
    }

    /// A profile that is only launchers and a login check.
    struct Stubbed {
        id: &'static str,
        launchers: &'static [&'static str],
        check: Option<LoginCheck>,
    }

    impl AgentProfile for Stubbed {
        fn id(&self) -> &str {
            self.id
        }
        fn dialog(&self, _: &str) -> Option<Dialog> {
            None
        }
        fn input_ready(&self, _: &str) -> bool {
            false
        }
        fn launchers(&self) -> &'static [&'static str] {
            self.launchers
        }
        fn login_check(&self) -> Option<LoginCheck> {
            self.check
        }
    }

    /// `name` as a `'static` launcher list (test-only leak).
    fn leaked(name: String) -> &'static [&'static str] {
        Box::leak(Box::new([&*Box::leak(name.into_boxed_str())]))
    }

    const STATUS_CHECK: LoginCheck = LoginCheck {
        args: &["login", "status"],
        signed_out_marker: "Not logged in",
    };

    #[test]
    fn a_runnable_launcher_is_installed_and_a_missing_one_is_not() {
        let dir = tempfile::tempdir().expect("temp dir");
        // A stub `claude` that answers any args with exit 0: `--version` works, and so does
        // `auth status` (signed in).
        stub(dir.path(), "claude", "exit 0");
        let results = probe_sh(
            &[builtin("claude"), builtin("codex")],
            &path_only(dir.path()),
        );
        assert_eq!(
            results,
            HashMap::from([
                ("claude".to_string(), result(true, Some(true))),
                // command not found: not installed, and nothing asked about sign-in
                ("codex".to_string(), result(false, None)),
            ])
        );
    }

    #[test]
    fn a_signed_out_marker_on_stderr_with_exit_1_reads_signed_out() {
        let dir = tempfile::tempdir().expect("temp dir");
        stub(
            dir.path(),
            "codex",
            "[ \"$1\" = --version ] && { echo 'codex-cli 0.0.0'; exit 0; }\n\
             echo 'Not logged in' >&2\nexit 1",
        );
        let results = probe_sh(&[builtin("codex")], &path_only(dir.path()));
        assert_eq!(results["codex"], result(true, Some(false)));
    }

    #[test]
    fn a_failing_check_without_the_marker_is_unknown() {
        let dir = tempfile::tempdir().expect("temp dir");
        // codex's unsupported-subcommand shape: exit 2, unrelated text.
        stub(
            dir.path(),
            "codex",
            "[ \"$1\" = --version ] && exit 0\n\
             echo \"error: unrecognized subcommand 'status'\" >&2\nexit 2",
        );
        // claude's: exit 1 too, an error on stderr and no JSON.
        stub(
            dir.path(),
            "claude",
            "[ \"$1\" = --version ] && exit 0\n\
             echo \"error: unknown command 'auth'\" >&2\nexit 1",
        );
        let results = probe_sh(
            &[builtin("codex"), builtin("claude")],
            &path_only(dir.path()),
        );
        assert_eq!(results["codex"], result(true, None));
        assert_eq!(results["claude"], result(true, None));
    }

    #[test]
    fn an_uninstalled_agent_is_not_installed_and_its_sign_in_unknown() {
        let dir = tempfile::tempdir().expect("temp dir");
        let results = probe_sh(
            &[builtin("codex"), builtin("cursor")],
            &path_only(dir.path()),
        );
        assert_eq!(results["codex"], result(false, None));
        assert_eq!(results["cursor"], result(false, None));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn launchers_are_tried_in_order_and_the_check_asks_the_one_that_answered() {
        let dir = tempfile::tempdir().expect("temp dir");
        // The first launcher fails `--version`; were it asked, it would say signed out.
        stub(
            dir.path(),
            "probe-first",
            "echo 'Not logged in' >&2\nexit 1",
        );
        stub(dir.path(), "probe-second", "exit 0");
        let profile = Stubbed {
            id: "ordered",
            launchers: &["probe-missing", "probe-first", "probe-second"],
            check: Some(STATUS_CHECK),
        };
        let results = probe_sh(&[Arc::new(profile)], &path_only(dir.path()));
        assert_eq!(results["ordered"], result(true, Some(true)));
    }

    #[test]
    fn launcher_words_are_quoted_so_nothing_runs_as_shell_syntax() {
        let dir = tempfile::tempdir().expect("temp dir");
        let marker = dir.path().join("injected");
        // A name with a space resolves as one word.
        stub(dir.path(), "probe agent", "exit 0");
        let spaced = Stubbed {
            id: "spaced",
            launchers: &["probe agent"],
            check: None,
        };
        // Shell syntax in a launcher stays literal: no command runs, the file never appears.
        let injected = Stubbed {
            id: "injected",
            launchers: leaked(format!("true; touch {}", marker.display())),
            check: None,
        };
        // A control character cannot be quoted for a shell: refused, nothing runs.
        let control = Stubbed {
            id: "control",
            launchers: &["probe\nagent"],
            check: None,
        };
        let results = probe_sh(
            &[Arc::new(spaced), Arc::new(injected), Arc::new(control)],
            &path_only(dir.path()),
        );
        assert_eq!(results["spaced"], result(true, None));
        assert_eq!(results["injected"], result(false, None));
        assert_eq!(results["control"], result(false, None));
        assert!(!marker.exists(), "a launcher string ran as shell syntax");
    }

    #[test]
    fn no_launchers_or_a_panicking_profile_read_as_not_installed() {
        struct Panics;
        impl AgentProfile for Panics {
            fn id(&self) -> &str {
                "panics"
            }
            fn dialog(&self, _: &str) -> Option<Dialog> {
                None
            }
            fn input_ready(&self, _: &str) -> bool {
                false
            }
            fn launchers(&self) -> &'static [&'static str] {
                panic!("launchers() broke");
            }
        }
        let dir = tempfile::tempdir().expect("temp dir");
        stub(dir.path(), "claude", "exit 0");
        let none = Stubbed {
            id: "none",
            launchers: &[],
            check: Some(STATUS_CHECK),
        };
        let results = probe_sh(
            &[Arc::new(Panics), Arc::new(none), builtin("claude")],
            &path_only(dir.path()),
        );
        assert_eq!(results["panics"], ProbeResult::default());
        let reports = super::super::probe_agents_detailed(
            &[Arc::new(Panics)],
            &path_only(dir.path()),
            &super::super::ProbeOptions::default(),
        );
        assert_eq!(
            reports["panics"].readiness,
            super::super::RuntimeReadiness::Unknown
        );
        assert!(
            reports["panics"]
                .diagnostics
                .contains(&super::super::ContextDiagnostic::ProfilePanicked)
        );

        assert_eq!(results["none"], ProbeResult::default());
        assert_eq!(
            results["claude"],
            result(true, Some(true)),
            "the other profiles are unaffected"
        );
    }

    #[test]
    fn a_command_still_running_at_the_timeout_is_killed() {
        let dir = tempfile::tempdir().expect("temp dir");
        stub(dir.path(), "probe-hangs", "exec /bin/sleep 120");
        let profile = Stubbed {
            id: "hangs",
            launchers: &["probe-hangs"],
            check: None,
        };
        let started = Instant::now();
        let results = probe_agents_in(
            &[Arc::new(profile)],
            &path_only(dir.path()),
            Duration::from_secs(2),
            Some("/bin/sh"),
        );
        assert_eq!(results["hangs"], result(false, None));
        // 2 s, plus at most 2 s to reap the killed group; far from the 120 s sleep.
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_command_gets_the_complete_environment_policy() {
        let dir = tempfile::tempdir().expect("temp dir");
        // `--version` exits 0 only when HOME was removed and the policy's other `set` key
        // arrived.
        stub(
            dir.path(),
            "probe-env",
            "[ -z \"${HOME+x}\" ] && [ \"$PROBE_SET_KEY\" = 1 ] && exit 0\nexit 1",
        );
        let profile = Stubbed {
            id: "env",
            launchers: &["probe-env"],
            check: None,
        };
        let mut env = EnvPolicy::default();
        env.path_prepend.push(dir.path().to_path_buf());
        env.set("PATH", "/nonexistent-probe-dir")
            .set("PROBE_SET_KEY", "1")
            .remove("HOME");
        env.inherit = false;
        let results = probe_sh(&[Arc::new(profile)], &env);
        assert_eq!(results["env"], result(true, None));
    }

    /// The command `run` starts: `/bin/sh -c <command>` (zsh and bash `-l -i -c`), `PATH` the
    /// policy's effective one, with configured values and removals applied.
    #[test]
    fn the_probe_env_includes_config_and_the_spawns_path() {
        let mut env = EnvPolicy::default();
        env.path_prepend.push("/opt/tools".into());
        env.set("PATH", "/usr/bin").set("CODEX_HOME", "/elsewhere");
        env.remove("SECRET_TOKEN");
        let probe = Probe::new(&env, WAIT, Some("/bin/sh"));
        let cmd = probe.shell_command_for("claude --version");
        assert_eq!(cmd.get_program(), OsStr::new("/bin/sh"));
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            ["-c", "claude --version"]
        );
        let envs: HashMap<OsString, Option<OsString>> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(OsStr::to_owned)))
            .collect();
        assert_eq!(
            envs,
            HashMap::from([
                ("PATH".into(), Some("/opt/tools:/usr/bin".into())),
                ("SECRET_TOKEN".into(), None),
                ("CODEX_HOME".into(), Some("/elsewhere".into())),
            ])
        );

        let zsh = Probe::new(&env, WAIT, Some("/bin/zsh")).shell_command_for("x");
        assert_eq!(zsh.get_args().collect::<Vec<_>>(), ["-l", "-i", "-c", "x"]);

        // `PATH` as a spawn under the policy gets it (`EnvPolicy::apply`): a removed one
        // with a prepend is the prepend alone; without a prepend there is none.
        let path_of = |env: &EnvPolicy| {
            let cmd = Probe::new(env, WAIT, Some("/bin/sh")).shell_command_for("x");
            cmd.get_envs()
                .find(|(k, _)| *k == "PATH")
                .map(|(_, v)| v.map(OsStr::to_owned))
        };
        env.remove("PATH");
        assert_eq!(path_of(&env), Some(Some("/opt/tools".into())));
        env.path_prepend.clear();
        assert_eq!(path_of(&env), Some(None), "removed");
    }

    /// REGRESSION ("checking installed agents takes 8 seconds and reports nothing"): every
    /// probe runs `$SHELL -l -i -c …`, and an INTERACTIVE shell in a BACKGROUND process group
    /// of a session with a controlling terminal is STOPPED by SIGTTIN/SIGTTOU before it can
    /// `exec`. `process_group(0)` did exactly that whenever the host ran from a terminal, so
    /// every agent hit the timeout on every cycle. The RPC spawn had the same bug and fix.
    ///
    /// Asserts the fix's two mechanisms: the login-INTERACTIVE shell actually RUNS its `-c`
    /// command, and it leads its own process group (pgid == pid), which the timeout's group
    /// kill relies on.
    ///
    /// `cargo test` usually has no controlling tty, where neither variant can be stopped —
    /// both assertions hold regardless, and the pgid one is what would catch a "fix" that
    /// dropped the group kill's precondition.
    #[test]
    fn probe_runs_interactive_login_shells_in_their_own_session() {
        // Distinctive: an interactive login shell sources the user's rc, whose banner lands
        // on this same stdout (macOS Terminal's `/etc/zshrc_Apple_Terminal` prints "Restored
        // session: …"), so the marker must be unmistakable in that noise.
        const MARKER: &str = "agent-link-probe-setsid-ok";

        // The real login shell and the inherited PATH (`ps`); the runner the probe uses,
        // with `<launcher> --version` swapped for a line reporting the shell's own pid AND
        // pgid — one line, marker-prefixed, so it survives whatever the rc printed around it.
        let probe = Probe::new(&EnvPolicy::default(), WAIT, None);
        let ran = probe.run(&format!(
            "printf '{MARKER} %s %s\\n' \"$$\" \"$(ps -o pgid= -p $$)\""
        ));
        let (ok, out) = ran.expect("the probe shell must run, not stop and time out");

        let line = out.lines().find(|l| l.contains(MARKER));
        assert!(
            line.is_some(),
            "the login-INTERACTIVE probe shell must actually RUN its -c command — no marker \
             is the stopped-by-SIGTTIN/SIGTTOU failure (exit_zero={ok}, got {out:?})"
        );
        // `$$` is the shell's own pid; setsid also makes it a process-group leader, so the
        // pgid it reports must equal it.
        let ids: Vec<i32> = line
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|t| t.parse().ok())
            .collect();
        assert_eq!(
            ids.len(),
            2,
            "expected `{MARKER} <pid> <pgid>` (got {out:?})"
        );
        assert_eq!(
            ids[0], ids[1],
            "setsid must leave the probe shell leading its own process group so the timeout's \
             group kill still reaches the CLI it started (got {out:?})"
        );
    }
}
