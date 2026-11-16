//! Which shell to launch and with what argv: the user's login shell, the one-shot
//! `-l -i -c` form that runs a command the way the user's own terminal would, and the
//! interactive shell a PTY session starts ([`ShellSpec`]).
//!
//! Agent CLIs are usually installed where only the user's shell set-up finds them (npm
//! globals, version managers exported from `.zshrc`), so the crate starts them through the
//! login shell: a PTY session types the launch line into it (`SpawnSpec::shell`), the RPC
//! transport and the probes run `<shell> -l -i -c '<command>'`. Spawn that one-shot form in
//! a new session (`setsid`): an interactive shell in a background process group is stopped
//! by `SIGTTIN` before it runs anything.
//!
//! ```
//! use agent_link::shell::login_shell_argv;
//!
//! let (program, args) = login_shell_argv("/bin/zsh", "claude --version");
//! assert_eq!(program, "/bin/zsh");
//! assert_eq!(args, ["-l", "-i", "-c", "claude --version"]);
//! // POSIX `sh` has no portable `-l`.
//! assert_eq!(login_shell_argv("/bin/sh", "true").1, ["-c", "true"]);
//! ```

use std::path::PathBuf;

/// The user's login shell: `$SHELL` when set and non-empty, else the first of
/// `/bin/zsh`, `/bin/bash`, `/bin/sh` that exists, else `/bin/sh`.
///
/// A `$SHELL` that is not valid UTF-8 counts as unset.
#[cfg(unix)]
pub fn login_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            ["/bin/zsh", "/bin/bash", "/bin/sh"]
                .into_iter()
                .find(|p| std::path::Path::new(p).exists())
                .unwrap_or("/bin/sh")
                .to_string()
        })
}

/// The file name of a shell path (`/bin/zsh` → `zsh`), or `""` when it has none or it
/// is not UTF-8. Argv shapes are chosen by this name.
pub fn shell_base(shell: &str) -> &str {
    std::path::Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
}

/// `(program, args)` that run `command` through `shell` as an interactive login shell:
/// `(shell, ["-l", "-i", "-c", command])`.
///
/// `-i` is what makes the user's CLIs resolvable: zsh and bash read their interactive rc
/// file (`.zshrc`, `.bashrc`) only in interactive mode, and that is where most users
/// export the `PATH` entries for npm globals and version managers; `-l` adds the login
/// profile. A shell whose file name is `sh` gets plain `(shell, ["-c", command])` —
/// POSIX `sh` has no portable `-l`. The supplied program is always preserved; this
/// helper does not check existence or fall back to a different shell.
///
/// Spawn the result in a new session (`setsid`, no controlling terminal): an
/// interactive shell in a background process group of a terminal session is stopped by
/// `SIGTTIN`/`SIGTTOU` before it runs `command`.
pub fn login_shell_argv(shell: &str, command: &str) -> (String, Vec<String>) {
    match shell_base(shell) {
        "sh" => (
            shell.to_string(),
            vec!["-c".to_string(), command.to_string()],
        ),
        _ => (
            shell.to_string(),
            vec![
                "-l".to_string(),
                "-i".to_string(),
                "-c".to_string(),
                command.to_string(),
            ],
        ),
    }
}

/// The interactive shell a PTY session runs.
///
/// `ShellSpec { program: None, login: true }` is "the user's own shell, as a login
/// shell" on unix and `%COMSPEC%` on Windows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellSpec {
    /// The shell to run; `None` picks the platform default (see [`resolve`](Self::resolve)).
    pub program: Option<PathBuf>,
    /// Start it as a login shell (`-l`) where the shell takes that flag (unix only).
    pub login: bool,
}

impl ShellSpec {
    /// `(program, args)` for the session.
    ///
    /// - unix: `program`, else `login_shell()`; `["-l"]` only when `login` and the shell
    ///   is `zsh` or `bash`, else no args.
    /// - Windows: `program`, else `%COMSPEC%` (when set and non-empty), else `cmd.exe`;
    ///   never any args.
    pub fn resolve(&self) -> (PathBuf, Vec<String>) {
        #[cfg(unix)]
        {
            let program = self
                .program
                .clone()
                .unwrap_or_else(|| PathBuf::from(login_shell()));
            let base = program.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let args = if self.login && matches!(base, "zsh" | "bash") {
                vec!["-l".to_string()]
            } else {
                Vec::new()
            };
            (program, args)
        }
        #[cfg(not(unix))]
        {
            let program = self.program.clone().unwrap_or_else(|| {
                std::env::var_os("COMSPEC")
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("cmd.exe"))
            });
            (program, Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_base_is_the_file_name() {
        assert_eq!(shell_base("/bin/zsh"), "zsh");
        assert_eq!(shell_base("/usr/local/bin/bash"), "bash");
        assert_eq!(shell_base("fish"), "fish");
        assert_eq!(shell_base(""), "");
        assert_eq!(shell_base("/"), "");
    }

    #[test]
    fn login_shell_argv_is_login_interactive_for_zsh_and_bash() {
        for shell in ["/bin/zsh", "/usr/bin/bash"] {
            let (program, args) = login_shell_argv(shell, "exec agent --flag");
            assert_eq!(program, shell, "the program is the shell itself");
            assert_eq!(args, ["-l", "-i", "-c", "exec agent --flag"]);
        }
    }

    #[test]
    fn login_shell_argv_runs_sh_plain_without_replacing_the_program() {
        for shell in ["/bin/sh", "/usr/local/bin/sh", "./custom/sh", "sh"] {
            let (program, args) = login_shell_argv(shell, "exec agent");
            assert_eq!(program, shell);
            assert_eq!(args, ["-c", "exec agent"]);
        }
    }

    #[test]
    fn login_shell_argv_treats_other_shells_like_zsh() {
        let (program, args) = login_shell_argv("/opt/homebrew/bin/fish", "x");
        assert_eq!(program, "/opt/homebrew/bin/fish");
        assert_eq!(args, ["-l", "-i", "-c", "x"]);
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_is_never_empty() {
        let shell = login_shell();
        assert!(!shell.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn shell_spec_adds_login_only_for_zsh_and_bash() {
        for (program, login, args) in [
            ("/bin/zsh", true, vec!["-l"]),
            ("/usr/bin/bash", true, vec!["-l"]),
            ("/bin/zsh", false, vec![]),
            ("/bin/bash", false, vec![]),
            ("/bin/sh", true, vec![]),
            ("/usr/bin/fish", true, vec![]),
        ] {
            let spec = ShellSpec {
                program: Some(program.into()),
                login,
            };
            assert_eq!(
                spec.resolve(),
                (
                    PathBuf::from(program),
                    args.into_iter().map(String::from).collect()
                ),
                "{program} login={login}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn shell_spec_defaults_to_the_login_shell() {
        let (program, args) = ShellSpec {
            program: None,
            login: true,
        }
        .resolve();
        assert_eq!(program, PathBuf::from(login_shell()));
        let base = shell_base(program.to_str().expect("utf-8 shell"));
        let want: Vec<String> = if matches!(base, "zsh" | "bash") {
            vec!["-l".into()]
        } else {
            Vec::new()
        };
        assert_eq!(args, want);

        let (_, args) = ShellSpec::default().resolve();
        assert!(args.is_empty(), "no -l without login");
    }

    #[cfg(windows)]
    #[test]
    fn shell_spec_on_windows_takes_no_args() {
        let spec = ShellSpec {
            program: Some(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe".into()),
            login: true,
        };
        let (program, args) = spec.resolve();
        assert_eq!(program, spec.program.clone().unwrap());
        assert!(args.is_empty());

        let (program, args) = ShellSpec {
            program: None,
            login: true,
        }
        .resolve();
        let want = std::env::var_os("COMSPEC")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("cmd.exe"));
        assert_eq!(program, want);
        assert!(args.is_empty());
    }
}
