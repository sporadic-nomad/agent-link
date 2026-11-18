//! The child's command, built in one place for [`preflight`](super::preflight) and
//! [`RpcChild::spawn`](super::RpcChild::spawn), so both apply the same spec validation
//! before a process exists. Starting the command or its launcher can still fail later.

use std::process::{Command, Stdio};

use super::RpcSpawnSpec;
#[cfg(any(unix, test))]
use crate::agent::RpcLaunch;
#[cfg(any(unix, test))]
use crate::agent::launch::login_shell_command;
use crate::env::EnvPolicy;
use crate::error::{Error, Result};

/// The command `spawn` runs: program, args, env, cwd, piped stdio and process isolation.
pub(super) struct Planned {
    pub(super) command: Command,
    /// Windows: the launcher as resolved on the child's `PATH` (logged at spawn).
    #[cfg(not(unix))]
    pub(super) resolved: std::path::PathBuf,
}

/// Builds `spec`'s command, or refuses it: [`Error::InvalidInput`] for an empty launcher, a
/// cwd that is not a directory, an empty shell or a word the shell quoting refuses (unix);
/// [`Error::NotFound`] for a launcher not on the child's `PATH` (Windows).
pub(super) fn plan(spec: &RpcSpawnSpec) -> Result<Planned> {
    spec.limits.validate()?;
    if spec.launch.launcher.is_empty() {
        return Err(Error::InvalidInput("rpc launcher is empty".into()));
    }
    if !spec.cwd.is_dir() {
        return Err(Error::InvalidInput(format!(
            "cwd {} is not a directory",
            spec.cwd.display()
        )));
    }
    let env = child_env(spec);
    let mut planned = program(spec, &env)?;
    let cmd = &mut planned.command;
    env.apply(cmd);
    cmd.current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolate(cmd);
    Ok(planned)
}

/// The host's policy with launch defaults and explicit overrides. Removals stay removed;
/// Unix login-shell rc files run afterward and can change the environment again.
pub(super) fn child_env(spec: &RpcSpawnSpec) -> EnvPolicy {
    let mut env = spec.env.clone();
    spec.launch.merge_env_into(&mut env);
    env
}

/// unix: the launcher runs through the user's interactive login shell
/// ([`login_shell_argv`](crate::shell::login_shell_argv)) — where most users put the `PATH`
/// entries for npm globals and version managers — as one `exec` command.
#[cfg(unix)]
fn program(spec: &RpcSpawnSpec, _env: &EnvPolicy) -> Result<Planned> {
    let shell = match &spec.shell {
        Some(shell) if shell.is_empty() => {
            return Err(Error::InvalidInput("rpc shell is empty".into()));
        }
        Some(shell) => shell.clone(),
        None => crate::shell::login_shell(),
    };
    let command = shell_command(&spec.launch, &shell)?;
    let (program, args) = crate::shell::login_shell_argv(&shell, &command);
    let mut command = Command::new(program);
    command.args(args);
    Ok(Planned { command })
}

/// Windows: the launcher is resolved to an absolute path WITH its extension first, against
/// the `PATH` the child will get ([`EnvPolicy::effective_path`]) — the resolution an
/// install probe does, so the two cannot disagree. It must happen here rather than being
/// left to `Command`: `Command::new("claude")` appends only `.exe` and ignores `PATHEXT`,
/// so an npm `.cmd` shim would never resolve; and with the extension explicit, std escapes
/// a `.cmd`'s arguments under cmd.exe rules (since Rust 1.77.2; the crate's MSRV is above
/// it). An unresolvable launcher is an error before any spawn attempt, naming it.
#[cfg(not(unix))]
fn program(spec: &RpcSpawnSpec, env: &EnvPolicy) -> Result<Planned> {
    let bin = spec.launch.launcher.as_str();
    let resolved =
        crate::resolve::find_in_path_os(&env.effective_path(), &[bin]).ok_or_else(|| {
            Error::NotFound(format!(
                "`{bin}` was not found on PATH — install the {bin} CLI, or pick a different agent for this project"
            ))
        })?;
    let mut command = Command::new(&resolved);
    command.args(&spec.launch.args);
    Ok(Planned { command, resolved })
}

/// `exec <launcher> <args…>` for `shell`, each word quoted by [`login_shell_command`].
///
/// `exec` is load-bearing: the agent replaces the shell, so the pid the handle holds, the
/// exit code it reaps and the process group it kills are the agent's, not those of a
/// wrapping shell.
#[cfg(any(unix, test))]
pub(super) fn shell_command(launch: &RpcLaunch, shell: &str) -> Result<String> {
    let words =
        std::iter::once(launch.launcher.as_str()).chain(launch.args.iter().map(String::as_str));
    Ok(format!("exec {}", login_shell_command(shell, words)?))
}

/// unix: a new session (`setsid`), so the child leads its own process group (teardown kills
/// that group) and has no controlling terminal — an interactive login shell in a
/// background process group of the host's terminal session is stopped by SIGTTIN/SIGTTOU
/// before it reaches `exec` (only when the host runs from a terminal; a GUI host has none).
/// Windows: no console window (a GUI host has no console to give the child, so Windows
/// would open one per child).
fn isolate(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe, the only call between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    crate::proc::no_console_window(cmd);
    #[cfg(not(any(unix, windows)))]
    let _ = cmd;
}
