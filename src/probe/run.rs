//! Shared version/login runner. Observations retain outcomes, never login payloads.
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(all(test, unix))]
use std::time::Duration;

use super::capture::{Captured, STREAM_LIMIT, capture};
use super::login::{classify_login, describe_login};
use super::*;
use crate::agent::{AgentProfile, Timing};
use crate::env::EnvPolicy;

pub(super) struct Probe {
    env: EnvPolicy,
    path: Option<OsString>,
    options: ProbeOptions,
    cwd: Option<PathBuf>,
    shell: Option<String>,
}

impl Probe {
    #[cfg(all(test, unix))]
    pub(super) fn new(env: &EnvPolicy, timeout: Duration, shell: Option<&str>) -> Self {
        let options = ProbeOptions {
            timeout,
            shell: shell.map(str::to_owned),
            ..ProbeOptions::default()
        };
        Self::with_options(env, &options)
    }

    pub(super) fn with_options(env: &EnvPolicy, options: &ProbeOptions) -> Self {
        let mut options = options.clone();
        options.timeout = options.timeout.min(Timing::MAX_WAIT);
        let cwd = match &options.cwd {
            Some(path) if path.is_absolute() => Some(path.clone()),
            other => std::env::current_dir()
                .ok()
                .map(|base| other.as_ref().map_or(base.clone(), |p| base.join(p))),
        };
        #[cfg(unix)]
        let shell = (options.mode == ProbeMode::LoginShell).then(|| {
            options
                .shell
                .clone()
                .unwrap_or_else(crate::shell::login_shell)
        });
        #[cfg(not(unix))]
        let shell = None;
        Self {
            env: env.clone(),
            path: child_path(env),
            options,
            cwd,
            shell,
        }
    }

    pub(super) fn empty_report(&self) -> ProbeReport {
        let mut diagnostics = Vec::new();
        if self.shell.is_some() {
            diagnostics.push(ContextDiagnostic::ShellMayOverrideContext);
        }
        if self.cwd.is_none() {
            diagnostics.push(ContextDiagnostic::WorkingDirectoryUnavailable);
        }
        #[cfg(windows)]
        diagnostics.push(ContextDiagnostic::HostPathExt);
        ProbeReport {
            result: ProbeResult::default(),
            readiness: RuntimeReadiness::Untested,
            compatibility: Default::default(),
            platform: (std::env::consts::OS, std::env::consts::ARCH),
            mode: if self.shell.is_some() {
                ProbeMode::LoginShell
            } else {
                ProbeMode::Direct
            },
            shell: self.shell.clone(),
            cwd: self.cwd.clone(),
            version_output: None,
            observations: Vec::new(),
            diagnostics,
        }
    }

    pub(super) fn profile(&self, profile: &dyn AgentProfile) -> ProbeReport {
        let mut report = self.empty_report();
        report.compatibility = profile.compatibility();
        if self.cwd.is_none() {
            return report;
        }
        let id = profile.id();
        let bins = profile.launchers();
        if bins.is_empty() {
            log::warn!("[probe-agents] {id}: no launcher names configured ⇒ unavailable");
            report.readiness = RuntimeReadiness::Unsupported;
            return report;
        }
        for &bin in bins {
            let executable = if self.shell.is_none() {
                self.resolve(bin)
            } else {
                None
            };
            let (observation, output) = self.observe(
                bin,
                executable.as_deref(),
                &["--version"],
                ProbeOperation::Version,
            );
            let success = matches!(
                observation.outcome,
                ProbeOutcome::Exited { success: true, .. }
            );
            let ran = !matches!(observation.outcome, ProbeOutcome::NotFound);
            let shown = self.shown(bin, executable.as_deref(), &["--version"]);
            if success {
                log::info!("[probe-agents] {id}: `{shown}` exited 0");
            } else if matches!(observation.outcome, ProbeOutcome::Exited { .. }) {
                log::info!("[probe-agents] {id}: `{shown}` exited non-zero; trying next launcher");
            }
            report.observations.push(observation);
            if success {
                report.result.installed = true;
                report.readiness = RuntimeReadiness::Available;
                report.version_output = output
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.trim().to_string());
                if let Some(check) = profile.login_check() {
                    let (observation, output) = self.observe(
                        bin,
                        executable.as_deref(),
                        check.args,
                        ProbeOperation::LoginStatus,
                    );
                    let ran = match observation.outcome {
                        ProbeOutcome::Exited { success, .. }
                            if !observation.output_truncated && !observation.output_lossy =>
                        {
                            output.map(|text| (success, text))
                        }
                        _ => None,
                    };
                    report.result.signed_in = classify_login(&check, ran);
                    let shown = self.shown(bin, executable.as_deref(), check.args);
                    log::info!(
                        "[probe-agents] {id}: `{shown}` ⇒ {}",
                        describe_login(report.result.signed_in)
                    );
                    report.observations.push(observation);
                }
                return report;
            }
            // Preserve Windows' first-resolved-launcher policy. Unix tries all names.
            #[cfg(windows)]
            if ran {
                break;
            }
            #[cfg(not(windows))]
            let _ = ran;
        }
        report.readiness = if report
            .observations
            .iter()
            .all(|o| o.outcome == ProbeOutcome::NotFound)
        {
            RuntimeReadiness::Missing
        } else {
            RuntimeReadiness::Unknown
        };
        log::info!("[probe-agents] {id}: no launcher among {bins:?} succeeded ⇒ unavailable");
        report
    }

    fn resolve(&self, bin: &str) -> Option<PathBuf> {
        let cwd = self.cwd.as_ref()?;
        let bin_path = Path::new(bin);
        if bin_path.is_absolute() || bin_path.components().count() > 1 {
            return crate::resolve::find_bin_in(std::slice::from_ref(cwd), &[bin]);
        }
        let dirs: Vec<_> = std::env::split_paths(self.path.as_ref()?)
            // Match the Windows RPC resolver: empty entries must not implicitly
            // search a possibly untrusted workspace. Unix direct mode retains
            // the shell convention that an empty PATH entry denotes cwd.
            .filter(|dir| !cfg!(windows) || !dir.as_os_str().is_empty())
            .map(|dir| {
                if dir.is_absolute() {
                    dir
                } else {
                    cwd.join(dir)
                }
            })
            .collect();
        crate::resolve::find_bin_in(&dirs, &[bin])
    }

    fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.stdin(Stdio::null());
        self.env.apply(&mut cmd);
        match &self.path {
            Some(path) => {
                cmd.env("PATH", path);
            }
            None => {
                cmd.env_remove("PATH");
            }
        }
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        cmd
    }

    fn shown(&self, bin: &str, executable: Option<&Path>, args: &[&str]) -> String {
        if let Some(shell) = &self.shell {
            return crate::agent::launch::login_shell_command(
                shell,
                std::iter::once(bin).chain(args.iter().copied()),
            )
            .unwrap_or_else(|_| "[invalid command]".into());
        }
        format!(
            "{} {}",
            executable.unwrap_or_else(|| Path::new(bin)).display(),
            args.join(" ")
        )
    }

    fn observe(
        &self,
        bin: &str,
        executable: Option<&Path>,
        args: &[&str],
        operation: ProbeOperation,
    ) -> (ProbeObservation, Option<String>) {
        let mut observation = ProbeObservation {
            operation,
            launcher: bin.into(),
            executable: executable.map(Path::to_path_buf),
            outcome: ProbeOutcome::NotFound,
            output_truncated: false,
            output_lossy: false,
        };
        let cmd = if let Some(shell) = &self.shell {
            let Ok(line) = crate::agent::launch::login_shell_command(
                shell,
                std::iter::once(bin).chain(args.iter().copied()),
            ) else {
                observation.outcome = ProbeOutcome::InvalidCommand;
                return (observation, None);
            };
            let (program, argv) = crate::shell::login_shell_argv(shell, &line);
            let mut cmd = self.command(program);
            cmd.args(argv);
            cmd
        } else if let Some(path) = executable {
            let mut cmd = self.command(path);
            cmd.args(args);
            cmd
        } else {
            return (observation, None);
        };
        let shown = self.shown(bin, executable, args);
        match capture(cmd, self.options.timeout) {
            Ok(Some(output)) => {
                observation.outcome = ProbeOutcome::Exited {
                    success: output.code == Some(0),
                    code: output.code,
                };
                observation.output_truncated = output.stdout.truncated || output.stderr.truncated;
                observation.output_lossy = std::str::from_utf8(&output.stdout.bytes).is_err()
                    || std::str::from_utf8(&output.stderr.bytes).is_err();
                (observation, Some(combined(&output)))
            }
            Ok(None) => {
                log::warn!(
                    "[probe-agents] `{shown}` timed out after {:?}, killed",
                    self.options.timeout
                );
                observation.outcome = ProbeOutcome::TimedOut;
                (observation, None)
            }
            Err(e) => {
                log::warn!("[probe-agents] could not run `{shown}`: {:?}", e.kind());
                observation.outcome = ProbeOutcome::IoError(e.kind());
                (observation, None)
            }
        }
    }

    #[cfg(all(test, unix))]
    pub(super) fn shell_command_for(&self, command: &str) -> Command {
        let (program, args) =
            crate::shell::login_shell_argv(self.shell.as_deref().unwrap(), command);
        let mut cmd = self.command(program);
        cmd.args(args);
        cmd
    }
    #[cfg(all(test, unix))]
    pub(super) fn run(&self, command: &str) -> Option<(bool, String)> {
        let output = capture(self.shell_command_for(command), self.options.timeout).ok()??;
        Some((output.code == Some(0), combined(&output)))
    }
}

fn child_path(env: &EnvPolicy) -> Option<OsString> {
    let key = OsStr::new("PATH");
    (!env.path_prepend.is_empty()
        || env.sets(key)
        || (env.inherit && !env.removes(key) && std::env::var_os(key).is_some()))
    .then(|| env.effective_path())
}
fn combined(output: &Captured) -> String {
    fn text(bytes: &[u8]) -> String {
        let mut s = String::from_utf8_lossy(bytes).into_owned();
        let mut end = s.len().min(STREAM_LIMIT);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s
    }
    format!(
        "{}\n{}",
        text(&output.stdout.bytes),
        text(&output.stderr.bytes)
    )
}
