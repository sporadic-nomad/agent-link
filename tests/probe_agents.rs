//! `probe_agents` through its public API: on unix the user's real login shell running stub
//! CLIs by absolute path (an rc file cannot put a real CLI in front of them), the answers and
//! the exact `[probe-agents]` log lines, a timeout; on Windows `.cmd` shims resolved on the
//! policy's `PATH`. Stubs live in temp dirs; no real CLI is run.
#![cfg(feature = "probe")]

mod common;

use std::time::Duration;

use agent_link::env::EnvPolicy;
use agent_link::probe::{ProbeResult, probe_agents};

/// Per probe command: a login shell reads the user's rc files, and where endpoint-security
/// software vets every exec, starts under parallel load have taken over 20 s. A passing probe
/// is not slower.
const WAIT: Duration = Duration::from_secs(60);

fn result(installed: bool, signed_in: Option<bool>) -> ProbeResult {
    ProbeResult {
        installed,
        signed_in,
    }
}

#[test]
fn no_profiles_no_answers() {
    assert!(probe_agents(&[], &EnvPolicy::default(), WAIT).is_empty());
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Instant;

    use agent_link::agent::{AgentProfile, Dialog, LoginCheck, ShellKind};

    use super::common::logger;
    use super::*;

    /// A test profile: launchers (absolute stub paths) and a login check.
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

    const STATUS: LoginCheck = LoginCheck {
        args: &["login", "status"],
        signed_out_marker: "Not logged in",
    };

    /// An executable `#!/bin/sh` stub `dir/name`, returned as a `'static` path string
    /// (a profile's launchers are `'static`; test-only leak).
    fn stub(dir: &Path, name: &str, body: &str) -> &'static str {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        leak(path.to_str().expect("a UTF-8 temp path").to_string())
    }

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    fn launchers(list: Vec<&'static str>) -> &'static [&'static str] {
        Box::leak(list.into_boxed_slice())
    }

    /// `path` as the probe writes it in a command (a temp path is plain: unquoted).
    fn word(path: &str) -> String {
        ShellKind::Posix.quote(path).expect("quotable")
    }

    /// The `[probe-agents] <id>: …` lines logged so far, in order.
    fn lines_for(id: &str) -> Vec<String> {
        let tag = format!("[probe-agents] {id}: ");
        logger::lines()
            .into_iter()
            .filter(|l| l.contains(&tag))
            .collect()
    }

    /// Every answer and every log line, through the user's login shell: signed in, signed
    /// out, inconclusive, a fallback launcher, a missing one, none at all.
    #[test]
    fn answers_and_log_lines_through_the_login_shell() {
        logger::install();
        let dir = tempfile::tempdir().expect("temp dir");
        let d = dir.path();
        let signed_in = stub(d, "probe-it-in", "exit 0");
        let signed_out = stub(
            d,
            "probe-it-out",
            "[ \"$1\" = --version ] && exit 0\necho 'Not logged in' >&2\nexit 1",
        );
        let odd = stub(
            d,
            "probe-it-odd",
            "[ \"$1\" = --version ] && exit 0\necho 'error: unexpected argument' >&2\nexit 2",
        );
        let broken = stub(d, "probe-it-broken", "exit 3");
        let works = stub(d, "probe-it-works", "exit 0");
        let missing = leak(d.join("probe-it-missing").to_str().unwrap().to_string());

        let profiles: Vec<Arc<dyn AgentProfile>> = vec![
            Arc::new(Stubbed {
                id: "probe-it-in",
                launchers: launchers(vec![signed_in]),
                check: Some(LoginCheck {
                    args: &["auth", "status"],
                    signed_out_marker: "\"loggedIn\": false",
                }),
            }),
            Arc::new(Stubbed {
                id: "probe-it-out",
                launchers: launchers(vec![signed_out]),
                check: Some(STATUS),
            }),
            Arc::new(Stubbed {
                id: "probe-it-odd",
                launchers: launchers(vec![odd]),
                check: Some(STATUS),
            }),
            Arc::new(Stubbed {
                id: "probe-it-two",
                launchers: launchers(vec![broken, works]),
                check: None,
            }),
            Arc::new(Stubbed {
                id: "probe-it-none",
                launchers: launchers(vec![missing]),
                check: Some(STATUS),
            }),
            Arc::new(Stubbed {
                id: "probe-it-empty",
                launchers: &[],
                check: Some(STATUS),
            }),
        ];
        let results = probe_agents(&profiles, &EnvPolicy::default(), WAIT);

        assert_eq!(results.len(), 6);
        assert_eq!(results["probe-it-in"], result(true, Some(true)));
        assert_eq!(results["probe-it-out"], result(true, Some(false)));
        assert_eq!(results["probe-it-odd"], result(true, None));
        assert_eq!(results["probe-it-two"], result(true, None));
        assert_eq!(results["probe-it-none"], result(false, None));
        assert_eq!(results["probe-it-empty"], result(false, None));

        let (i, o, x) = (word(signed_in), word(signed_out), word(odd));
        let (b, w, m) = (word(broken), word(works), word(missing));
        assert_eq!(
            lines_for("probe-it-in"),
            [
                format!("INFO [probe-agents] probe-it-in: `{i} --version` exited 0"),
                format!("INFO [probe-agents] probe-it-in: `{i} auth status` ⇒ signed in"),
            ]
        );
        assert_eq!(
            lines_for("probe-it-out"),
            [
                format!("INFO [probe-agents] probe-it-out: `{o} --version` exited 0"),
                format!("INFO [probe-agents] probe-it-out: `{o} login status` ⇒ NOT signed in"),
            ]
        );
        assert_eq!(
            lines_for("probe-it-odd"),
            [
                format!("INFO [probe-agents] probe-it-odd: `{x} --version` exited 0"),
                format!(
                    "INFO [probe-agents] probe-it-odd: `{x} login status` ⇒ inconclusive ⇒ \
                     reported as unknown (assume fine)"
                ),
            ]
        );
        assert_eq!(
            lines_for("probe-it-two"),
            [
                format!(
                    "INFO [probe-agents] probe-it-two: `{b} --version` exited non-zero; \
                     trying next launcher"
                ),
                format!("INFO [probe-agents] probe-it-two: `{w} --version` exited 0"),
            ]
        );
        assert_eq!(
            lines_for("probe-it-none"),
            [
                format!(
                    "INFO [probe-agents] probe-it-none: `{m} --version` exited non-zero; \
                     trying next launcher"
                ),
                format!(
                    "INFO [probe-agents] probe-it-none: no launcher among [{missing:?}] \
                     succeeded ⇒ unavailable"
                ),
            ]
        );
        assert_eq!(
            lines_for("probe-it-empty"),
            ["WARN [probe-agents] probe-it-empty: no launcher names configured ⇒ unavailable"]
        );
    }

    /// `pid` no longer names a process.
    fn gone(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        let gone = unsafe { libc::kill(pid, 0) } != 0;
        gone && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    /// A command still running at the timeout is killed with its process group and logged;
    /// the call returns long before the stub would have ended.
    #[test]
    fn a_timeout_is_killed_and_logged() {
        logger::install();
        let dir = tempfile::tempdir().expect("temp dir");
        let pid_file = dir.path().join("pid");
        // A CLI that hangs, with a child of its own in its process group.
        let hangs = stub(
            dir.path(),
            "probe-it-hangs",
            &format!("/bin/sleep 120 &\necho $! > '{}'\nwait", pid_file.display()),
        );
        let profile = Stubbed {
            id: "probe-it-hangs",
            launchers: launchers(vec![hangs]),
            check: None,
        };
        let started = Instant::now();
        // A login shell usually reaches the stub in well under a second.
        let timeout = Duration::from_secs(10);
        let results = probe_agents(&[Arc::new(profile)], &EnvPolicy::default(), timeout);
        assert!(started.elapsed() < WAIT, "{:?}", started.elapsed());
        assert_eq!(results["probe-it-hangs"], result(false, None));

        // The stub's sleep was in the shell's process group: killed with it (and reaped by
        // init a moment later, an orphan). No pid file, or an empty one: the shell was killed
        // before the stub wrote it (a host loaded past the timeout) — nothing of the stub's
        // left to check.
        let pid = std::fs::read_to_string(&pid_file).unwrap_or_default();
        if !pid.trim().is_empty() {
            let pid: libc::pid_t = pid.trim().parse().expect("a pid");
            let deadline = Instant::now() + WAIT;
            while !gone(pid) {
                assert!(
                    Instant::now() < deadline,
                    "the stub's sleep {pid} survived the timeout"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        let command = format!("{} --version", word(hangs));
        let timed_out = format!("WARN [probe-agents] `{command}` timed out after 10s, killed");
        assert!(
            logger::lines().contains(&timed_out),
            "no {timed_out:?} in {:#?}",
            logger::lines()
        );
        assert_eq!(
            lines_for("probe-it-hangs"),
            [format!(
                "INFO [probe-agents] probe-it-hangs: no launcher among [{hangs:?}] succeeded ⇒ \
                 unavailable"
            )]
        );
    }
    fn detailed(profile: Stubbed, dir: &Path, env: &EnvPolicy) -> agent_link::probe::ProbeReport {
        use agent_link::probe::*;
        let mut options = ProbeOptions::default();
        options.mode = ProbeMode::Direct;
        options.cwd = Some(dir.to_path_buf());
        options.timeout = WAIT;
        probe_agents_detailed(&[Arc::new(profile)], env, &options)
            .remove("detail")
            .unwrap()
    }

    #[test]
    fn detailed_probe_matches_environment_cwd_and_retains_future_version() {
        use agent_link::probe::*;
        logger::install();
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir(&config).unwrap();
        let bin = stub(
            dir.path(),
            "detail",
            r#"
[ "$CONFIG_HOME" -ef "$PWD/config" ] || exit 10
[ "$HOME" -ef "$PWD" ] || exit 11
[ -z "${USER+x}" ] || exit 12
[ -z "${REMOVED+x}" ] || exit 13
if [ "$1" = --version ]; then echo 'future-cli 999.0.0-private-marker' >&2; exit 0; fi
printf login > "$CONFIG_HOME/login-ran"
echo 'Not logged in' >&2
exit 1
"#,
        );
        let mut env = EnvPolicy {
            inherit: false,
            ..EnvPolicy::default()
        };
        env.set("HOME", dir.path())
            .set("CONFIG_HOME", &config)
            .set("REMOVED", "secret")
            .remove("REMOVED");
        let report = detailed(
            Stubbed {
                id: "detail",
                launchers: launchers(vec![bin]),
                check: Some(STATUS),
            },
            dir.path(),
            &env,
        );
        assert_eq!(report.result, result(true, Some(false)));
        assert_eq!(report.readiness, RuntimeReadiness::Available);
        assert_eq!(
            report.version_output.as_deref(),
            Some("future-cli 999.0.0-private-marker")
        );
        assert_eq!(report.mode, ProbeMode::Direct);
        assert!(report.shell.is_none());
        assert_eq!(report.cwd.as_deref(), Some(dir.path()));
        assert!(report.diagnostics.is_empty());
        assert_eq!(report.observations.len(), 2);
        assert_eq!(
            report.observations[0].executable.as_deref(),
            Some(Path::new(bin))
        );
        assert_eq!(
            report.observations[1].operation,
            ProbeOperation::LoginStatus
        );
        assert!(config.join("login-ran").exists());
        assert!(!format!("{report:?}").contains("private-marker"));
        assert!(
            !logger::lines()
                .iter()
                .any(|line| line.contains("private-marker"))
        );
        assert_eq!(
            report.compatibility,
            Default::default(),
            "custom profiles have no inferred claims"
        );
    }

    #[test]
    fn direct_resolution_uses_relative_path_from_probe_cwd_and_keeps_attempts() {
        use agent_link::probe::*;
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        stub(&bin_dir, "detail", "echo detail-1; exit 0");
        let mut env = EnvPolicy {
            inherit: false,
            ..EnvPolicy::default()
        };
        env.set("PATH", "bin");
        let report = detailed(
            Stubbed {
                id: "detail",
                launchers: &["missing-detail", "detail"],
                check: None,
            },
            dir.path(),
            &env,
        );
        assert_eq!(report.result, result(true, None));
        assert_eq!(report.observations[0].outcome, ProbeOutcome::NotFound);
        assert_eq!(
            report.observations[1].executable,
            Some(bin_dir.join("detail"))
        );
    }

    #[test]
    fn missing_unsupported_broken_and_spawn_failure_remain_distinct() {
        use agent_link::probe::*;
        let dir = tempfile::tempdir().unwrap();
        let mut env = EnvPolicy {
            inherit: false,
            ..EnvPolicy::default()
        };
        env.set("PATH", dir.path());
        let profile = |bins| Stubbed {
            id: "detail",
            launchers: bins,
            check: Some(STATUS),
        };
        let missing = detailed(profile(&["missing-detail"]), dir.path(), &env);
        assert_eq!(missing.readiness, RuntimeReadiness::Missing);
        assert_eq!(missing.result, result(false, None));
        assert_eq!(missing.observations.len(), 1);
        let no_check = detailed(profile(&[]), dir.path(), &env);
        assert_eq!(no_check.readiness, RuntimeReadiness::Unsupported);
        assert!(no_check.observations.is_empty());
        let broken = stub(dir.path(), "broken", "exit 7");
        let report = detailed(profile(launchers(vec![broken])), dir.path(), &env);
        assert_eq!(report.readiness, RuntimeReadiness::Unknown);
        assert_eq!(
            report.observations[0].outcome,
            ProbeOutcome::Exited {
                success: false,
                code: Some(7)
            }
        );
        assert_eq!(
            report.observations.len(),
            1,
            "login must not run after failed version"
        );
        let report = detailed(
            profile(launchers(vec![broken])),
            &dir.path().join("absent-cwd"),
            &env,
        );
        assert!(matches!(
            report.observations[0].outcome,
            ProbeOutcome::IoError(_)
        ));
        assert_eq!(report.readiness, RuntimeReadiness::Unknown);
    }

    #[test]
    fn bounded_version_output_and_corrupt_login_never_claim_signed_out() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(
            dir.path(),
            "detail",
            &format!(
                "if [ \"$1\" = --version ]; then printf '%s' '{}'; exit 0; fi\nprintf '\\377Not logged in'; exit 1",
                "x".repeat(32 * 1024)
            ),
        );
        let env = EnvPolicy {
            inherit: false,
            ..EnvPolicy::default()
        };
        let report = detailed(
            Stubbed {
                id: "detail",
                launchers: launchers(vec![bin]),
                check: Some(STATUS),
            },
            dir.path(),
            &env,
        );
        assert_eq!(report.result, result(true, None));
        assert_eq!(report.version_output.as_ref().unwrap().len(), 8192);
        assert!(report.observations[0].output_truncated);
        assert!(report.observations[1].output_lossy);

        let bin = stub(
            dir.path(),
            "detail",
            &format!(
                "[ \"$1\" = --version ] && exit 0\nprintf '%s' 'Not logged in{}'; exit 1",
                "x".repeat(32 * 1024)
            ),
        );
        let report = detailed(
            Stubbed {
                id: "detail",
                launchers: launchers(vec![bin]),
                check: Some(STATUS),
            },
            dir.path(),
            &env,
        );
        assert_eq!(report.result, result(true, None));
        assert!(report.version_output.is_none());
        assert!(report.observations[1].output_truncated);
    }

    #[test]
    fn shell_override_named_sh_runs_the_selected_executable_for_both_checks() {
        use agent_link::probe::*;
        let dir = tempfile::tempdir().unwrap();
        let shell_dir = dir.path().join("custom shell");
        std::fs::create_dir(&shell_dir).unwrap();
        let shell = stub(
            &shell_dir,
            "sh",
            r#"
[ "$#" = 2 ] && [ "$1" = -c ] || exit 41
printf '%s\n' "$0" >> shell-invocations
export CUSTOM_PROBE_SHELL=selected
exec /bin/sh "$@"
"#,
        );
        let bin = stub(
            dir.path(),
            "detail",
            r#"
[ "$CUSTOM_PROBE_SHELL" = selected ] || exit 42
if [ "$1" = --version ]; then echo custom-shell-version; exit 0; fi
echo 'Not logged in'
exit 1
"#,
        );
        let profile = Stubbed {
            id: "detail",
            launchers: launchers(vec![bin]),
            check: Some(STATUS),
        };
        let env = EnvPolicy {
            inherit: false,
            ..EnvPolicy::default()
        };
        let mut options = ProbeOptions::default();
        options.shell = Some(shell.into());
        options.cwd = Some(dir.path().to_path_buf());
        options.timeout = WAIT;
        let report = probe_agents_detailed(&[Arc::new(profile)], &env, &options)
            .remove("detail")
            .unwrap();
        assert_eq!(report.result, result(true, Some(false)));
        assert_eq!(
            report.version_output.as_deref(),
            Some("custom-shell-version")
        );
        let invocations = std::fs::read_to_string(dir.path().join("shell-invocations")).unwrap();
        assert_eq!(invocations.lines().collect::<Vec<_>>(), [shell, shell]);
        assert_eq!(report.shell.as_deref(), Some(shell));
        assert_eq!(report.observations.len(), 2);
    }

    #[test]
    fn missing_shell_override_named_sh_fails_without_running_the_cli() {
        use agent_link::probe::*;
        let dir = tempfile::tempdir().unwrap();
        let shell = dir.path().join("missing").join("sh");
        let bin = stub(dir.path(), "detail", "echo ran > cli-ran\necho version-1");
        let profile = Stubbed {
            id: "detail",
            launchers: launchers(vec![bin]),
            check: Some(STATUS),
        };
        let env = EnvPolicy {
            inherit: false,
            ..EnvPolicy::default()
        };
        let mut options = ProbeOptions::default();
        options.shell = Some(shell.to_str().unwrap().into());
        options.cwd = Some(dir.path().to_path_buf());
        options.timeout = WAIT;
        let report = probe_agents_detailed(&[Arc::new(profile)], &env, &options)
            .remove("detail")
            .unwrap();
        assert_eq!(report.result, result(false, None));
        assert_eq!(report.readiness, RuntimeReadiness::Unknown);
        assert_eq!(report.shell, options.shell);
        assert_eq!(report.observations.len(), 1);
        assert_eq!(
            report.observations[0].outcome,
            ProbeOutcome::IoError(std::io::ErrorKind::NotFound)
        );
        assert!(!dir.path().join("cli-ran").exists());
    }

    #[test]
    fn login_shell_reports_context_uncertainty_and_unknown_executable() {
        use agent_link::probe::*;
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(dir.path(), "detail", "echo version-1");
        let profile = Stubbed {
            id: "detail",
            launchers: launchers(vec![bin]),
            check: None,
        };
        let env = EnvPolicy {
            inherit: false,
            ..EnvPolicy::default()
        };
        let mut options = ProbeOptions::default();
        options.shell = Some("/bin/sh".into());
        options.cwd = Some(dir.path().to_path_buf());
        let report = probe_agents_detailed(&[Arc::new(profile)], &env, &options)
            .remove("detail")
            .unwrap();
        assert_eq!(report.result, result(true, None));
        assert_eq!(report.mode, ProbeMode::LoginShell);
        assert!(
            report
                .diagnostics
                .contains(&ContextDiagnostic::ShellMayOverrideContext)
        );
        assert!(report.observations[0].executable.is_none());
    }
    #[test]
    fn detailed_timeout_is_unknown_and_does_not_attempt_login() {
        use agent_link::probe::*;
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(dir.path(), "hangs", "exec /bin/sleep 120");
        let profile = Stubbed {
            id: "detail",
            launchers: launchers(vec![bin]),
            check: Some(STATUS),
        };
        let env = EnvPolicy {
            inherit: false,
            ..EnvPolicy::default()
        };
        let mut options = ProbeOptions::default();
        options.mode = ProbeMode::Direct;
        options.timeout = Duration::from_secs(2);
        options.cwd = Some(dir.path().to_path_buf());
        let started = Instant::now();
        let report = probe_agents_detailed(&[Arc::new(profile)], &env, &options)
            .remove("detail")
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(30));
        assert_eq!(report.readiness, RuntimeReadiness::Unknown);
        assert_eq!(report.result, result(false, None));
        assert_eq!(report.observations.len(), 1);
        assert_eq!(report.observations[0].outcome, ProbeOutcome::TimedOut);
    }
}

#[cfg(windows)]
mod windows {
    use agent_link::agent::{AgentProfile, Dialog, ProfileRegistry};
    use agent_link::probe::{ProbeOptions, RuntimeReadiness, probe_agents_detailed};
    use std::sync::Arc;

    use super::*;

    struct PathProfile(&'static [&'static str]);

    impl AgentProfile for PathProfile {
        fn id(&self) -> &str {
            "path-test"
        }
        fn dialog(&self, _: &str) -> Option<Dialog> {
            None
        }
        fn input_ready(&self, _: &str) -> bool {
            false
        }
        fn launchers(&self) -> &'static [&'static str] {
            self.0
        }
    }

    fn isolated_env() -> EnvPolicy {
        let mut env = EnvPolicy {
            inherit: false,
            ..EnvPolicy::default()
        };
        if let Some(root) = std::env::var_os("SystemRoot") {
            env.set("SystemRoot", root);
        }
        env
    }

    #[test]
    fn windows_empty_path_segments_never_launch_a_workspace_shim() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("workspace");
        let trusted = root.path().join("trusted");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(&trusted).unwrap();
        std::fs::write(
            cwd.join("claude.cmd"),
            "@echo workspace>workspace-ran\r\n@echo workspace-version\r\n@exit /b 0\r\n",
        )
        .unwrap();
        std::fs::write(
            trusted.join("claude.cmd"),
            "@echo trusted-version\r\n@exit /b 0\r\n",
        )
        .unwrap();
        let profile = ProfileRegistry::builtin().get("claude").unwrap();
        let mut options = ProbeOptions::default();
        options.cwd = Some(cwd.clone());
        options.timeout = WAIT;
        // LoginShell is the convenience API's default and also resolves directly
        // on Windows. Include leading, adjacent, trailing and all-empty entries.
        for entries in [
            vec!["", "trusted"],
            vec!["", "", "trusted", ""],
            vec!["trusted", ""],
            vec![""],
            vec!["", ""],
        ] {
            let path = std::env::join_paths(entries.iter().map(|s| {
                if s.is_empty() {
                    std::path::Path::new("")
                } else {
                    trusted.as_path()
                }
            }))
            .unwrap();
            let mut env = isolated_env();
            env.set("PATH", &path);
            let report = probe_agents_detailed(std::slice::from_ref(&profile), &env, &options)
                .remove("claude")
                .unwrap();
            // The probe must select exactly the path selected by the RPC resolver.
            let resolved = agent_link::resolve::find_in_path_os(&path, &["claude"]);
            assert_eq!(report.observations[0].executable, resolved);
            if entries.contains(&"trusted") {
                assert_eq!(report.version_output.as_deref(), Some("trusted-version"));
                assert_eq!(report.result, result(true, Some(true)));
            } else {
                assert_eq!(report.readiness, RuntimeReadiness::Missing);
                assert_eq!(report.result, result(false, None));
            }
            assert!(
                !cwd.join("workspace-ran").exists(),
                "workspace shim ran for {path:?}"
            );
        }
    }

    #[test]
    fn windows_explicit_extensionless_paths_probe_through_dotted_parents() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("workspace");
        let dotted = root.path().join("tools").join("v1.2");
        let bin = root.path().join("bin");
        for dir in [&cwd, &dotted, &bin] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for dir in [&dotted, &bin] {
            std::fs::write(
                dir.join("codex.cmd"),
                "@echo codex-version\r\n@exit /b 0\r\n",
            )
            .unwrap();
        }
        let mut options = ProbeOptions::default();
        options.cwd = Some(cwd.clone());
        options.timeout = WAIT;
        for launcher in [
            dotted.join("codex"),
            std::path::PathBuf::from(r"..\bin\codex"),
        ] {
            let expected = cwd.join(&launcher).with_extension("cmd");
            let launcher: &'static str = Box::leak(
                launcher
                    .into_os_string()
                    .into_string()
                    .unwrap()
                    .into_boxed_str(),
            );
            let profile: Arc<dyn AgentProfile> =
                Arc::new(PathProfile(Box::leak(Box::new([launcher]))));
            let report = probe_agents_detailed(&[profile], &isolated_env(), &options)
                .remove("path-test")
                .unwrap();
            assert_eq!(report.result, result(true, None));
            assert_eq!(report.version_output.as_deref(), Some("codex-version"));
            assert_eq!(report.observations[0].executable.as_ref(), Some(&expected));
        }
    }

    /// An agent is installed only when its launcher RESOLVES on the policy's `PATH` **and**
    /// `--version` exits 0: a resolvable launcher that exits non-zero is not installed (the
    /// old "anything but 9009" rule called it installed), and the `.cmd` case is the one
    /// `Command::new("<bare name>")` could never resolve.
    #[test]
    fn windows_probe_requires_a_resolvable_launcher_that_exits_zero() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("claude.cmd"), "@exit 0\r\n").expect("write shim");
        std::fs::write(dir.path().join("codex.cmd"), "@exit 3\r\n").expect("write broken shim");
        let mut env = EnvPolicy::default();
        env.set("PATH", dir.path());
        let registry = ProfileRegistry::builtin();
        let profiles: Vec<_> = ["claude", "codex", "opencode"]
            .iter()
            .map(|id| registry.get(id).expect(id))
            .collect();
        let results = probe_agents(&profiles, &env, WAIT);
        // The shim exits 0 for `auth status` too: signed in.
        assert_eq!(results["claude"], result(true, Some(true)));
        assert_eq!(results["codex"], result(false, None));
        assert_eq!(results["opencode"], result(false, None));
    }
    #[test]
    fn windows_detailed_probe_captures_version_and_applies_config_environment() {
        use agent_link::probe::*;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("claude.cmd"),
            "@echo off\r\nif not defined CLAUDE_CONFIG_DIR exit /b 4\r\nif defined REMOVED exit /b 5\r\nif \"%1\"==\"--version\" (echo claude-future-999& exit /b 0)\r\necho {\"loggedIn\": false}\r\nexit /b 1\r\n").unwrap();
        let mut env = EnvPolicy {
            inherit: false,
            ..EnvPolicy::default()
        };
        env.set("PATH", dir.path())
            .set("CLAUDE_CONFIG_DIR", dir.path())
            .set("REMOVED", "secret")
            .remove("REMOVED");
        if let Some(root) = std::env::var_os("SystemRoot") {
            env.set("SystemRoot", root);
        }
        let mut options = ProbeOptions::default();
        options.cwd = Some(dir.path().to_path_buf());
        options.timeout = WAIT;
        let profile = ProfileRegistry::builtin().get("claude").unwrap();
        let report = probe_agents_detailed(&[profile], &env, &options)
            .remove("claude")
            .unwrap();
        assert_eq!(report.result, result(true, Some(false)));
        assert_eq!(report.version_output.as_deref(), Some("claude-future-999"));
        assert_eq!(report.mode, ProbeMode::Direct);
        assert_eq!(
            report.observations[0].executable,
            Some(dir.path().join("claude.cmd"))
        );
        assert!(report.diagnostics.contains(&ContextDiagnostic::HostPathExt));
    }
}
