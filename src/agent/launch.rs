//! [`LaunchPlan`]: the argv and env that start an agent, and [`ShellKind`]: how that argv
//! is typed into the PTY's shell as one command line (quoting, line terminator).

use std::ffi::OsStr;
use std::fmt;

use super::Permissions;
use crate::env::EnvPolicy;
use crate::{Error, Result};

/// The shell a launch line is typed into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ShellKind {
    /// sh, bash, zsh: single-quote quoting. Not fish: it treats `\\` and `\'` as escapes
    /// inside single quotes, so an argument holding a backslash would change.
    Posix,
    /// Windows `cmd.exe` (under ConPTY): double-quote quoting by the MSVC argv rules.
    Cmd,
}

impl ShellKind {
    /// The kind of the crate's default shell on this platform: [`Cmd`](Self::Cmd) on Windows
    /// (`%COMSPEC%`, else `cmd.exe`), [`Posix`](Self::Posix) elsewhere.
    pub fn native() -> Self {
        if cfg!(windows) {
            ShellKind::Cmd
        } else {
            ShellKind::Posix
        }
    }

    /// What ends a typed line: `"\n"` for POSIX shells, `"\r"` for cmd.exe under ConPTY —
    /// the byte a real Enter produces there; a bare LF does not complete a cmd line.
    pub fn line_terminator(self) -> &'static str {
        match self {
            ShellKind::Posix => "\n",
            ShellKind::Cmd => "\r",
        }
    }

    /// Keys that empty this shell's input line — one line, cursor at its end — without
    /// running anything; the controller types them before every launch line, so text left
    /// there (an unsubmitted paste, a draft) never runs as part of it. `None` when the crate
    /// knows no such keys.
    ///
    /// - `Posix`: a space, then Ctrl-U (`\x15`) once. One press empties a single line in
    ///   readline (bash: `unix-line-discard`), zsh (`kill-whole-line`), fish
    ///   (`backward-kill-line`) and a terminal in canonical mode (sh/dash without a line
    ///   editor: the tty's kill character); the space gives readline something to kill (on
    ///   an empty line it rings the bell). A multi-line buffer needs more presses in zsh and
    ///   fish. Not Ctrl-C: that interrupts whatever owns the foreground and changes `$?` —
    ///   the controller sends one separately, before these keys, only with the shell read
    ///   in front ([`AgentTerminal::launch`](super::AgentTerminal::launch)).
    /// - `Cmd`: `None`. cmd.exe clears its line on Escape, but ConPTY reads ESC followed by
    ///   a printable as Alt+key: an Escape right before the launch line would eat its first
    ///   character and clear nothing.
    pub fn clear_line(self) -> Option<&'static str> {
        match self {
            ShellKind::Posix => Some(" \u{15}"),
            ShellKind::Cmd => None,
        }
    }

    /// `arg` as one word of a command line typed into this shell.
    ///
    /// - Both kinds: an arg made only of `A-Z a-z 0-9 _ . / : = @ % + , -`, not starting
    ///   with `=` (zsh expands `=cmd`), stays unquoted.
    ///   An arg with a control character (newline, CR, tab, ESC, NUL, …) is refused
    ///   ([`Error::InvalidInput`]): a line editor would act on it (submit, complete, erase).
    /// - `Posix`: anything else is single-quoted, `'` written as `'\''`.
    /// - `Cmd`: `%`, `^`, `&`, `|`, `<`, `>` and `!` are refused — cmd.exe expands or
    ///   interprets them even inside quotes on an interactive line. Anything else is
    ///   double-quoted: `"` becomes `\"`, backslashes before a `"` or the closing quote are
    ///   doubled.
    pub fn quote(self, arg: &str) -> Result<String> {
        if arg.chars().any(char::is_control) {
            return Err(Error::InvalidInput(format!(
                "launch argument {arg:?} contains a control character; a typed command line cannot carry it"
            )));
        }
        match self {
            ShellKind::Posix if is_plain(arg) => Ok(arg.to_string()),
            ShellKind::Posix => Ok(format!("'{}'", arg.replace('\'', r"'\''"))),
            ShellKind::Cmd => {
                if let Some(c) = arg.chars().find(|c| CMD_SPECIAL.contains(c)) {
                    return Err(Error::InvalidInput(format!(
                        "launch argument {arg:?} contains {c:?}, which cmd.exe interprets on a command line"
                    )));
                }
                if is_plain(arg) {
                    Ok(arg.to_string())
                } else {
                    Ok(msvc_quote(arg))
                }
            }
        }
    }
}

/// `words` as one command line for a one-shot `<shell> -c` run (an RPC launch, an install
/// probe), `shell` being a unix shell path such as `$SHELL`. Each word is POSIX-quoted
/// ([`ShellKind::quote`]: a plain word stays bare, a control character is refused), except
/// under fish for a word holding a backslash: fish reads `\\` and `\'` as escapes inside
/// single quotes, so there that word is single-quoted with both escaped (the POSIX form would
/// change it, or leave the quote open). Without a backslash both shells read the POSIX form
/// alike.
// Used by the unix `rpc` and `probe` paths only.
#[cfg_attr(
    not(all(unix, any(feature = "rpc", feature = "probe"))),
    allow(dead_code)
)]
pub(crate) fn login_shell_command<'a>(
    shell: &str,
    words: impl IntoIterator<Item = &'a str>,
) -> Result<String> {
    let fish = crate::shell::shell_base(shell) == "fish";
    let quoted = words
        .into_iter()
        .map(|word| {
            let posix = ShellKind::Posix.quote(word)?;
            Ok(if fish && word.contains('\\') {
                format!("'{}'", word.replace('\\', r"\\").replace('\'', r"\'"))
            } else {
                posix
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(quoted.join(" "))
}

/// Characters cmd.exe acts on in an interactive line, quoted or not (`!` only with delayed
/// expansion on, which a user can enable in the registry).
const CMD_SPECIAL: &[char] = &['%', '^', '&', '|', '<', '>', '!'];

/// Needs no quoting in either shell.
fn is_plain(arg: &str) -> bool {
    !arg.is_empty()
        && !arg.starts_with('=')
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:=@%+,-".contains(&b))
}

/// Double-quoted by the MSVC argv rules (what `CommandLineToArgvW` and the C runtime parse).
fn msvc_quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
        } else {
            if c == '"' {
                // The run of backslashes before a quote is doubled, plus one for the quote.
                out.extend(std::iter::repeat_n('\\', backslashes + 1));
            }
            backslashes = 0;
        }
        out.push(c);
    }
    // Backslashes before the closing quote are doubled.
    out.extend(std::iter::repeat_n('\\', backslashes));
    out.push('"');
    out
}

/// A TOML basic string (`"…"`) holding `s`: `\` and `"` escaped, control characters as
/// `\uXXXX` (so the result also passes [`ShellKind::quote`]).
pub(crate) fn toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The keys of an env list (for `Debug` impls: values can hold tokens).
pub(crate) fn env_keys(env: &[(String, String)]) -> Vec<&str> {
    env.iter().map(|(k, _)| k.as_str()).collect()
}

/// Shared precedence for PTY and RPC launch environments. Removals are applied later
/// by `EnvPolicy`, including when a launch explicitly sets the removed key.
pub(crate) fn merge_launch_env(
    policy: &mut EnvPolicy,
    defaults: &[(String, String)],
    overrides: &[(String, String)],
) {
    for (key, value) in defaults {
        if !policy.sets(OsStr::new(key)) {
            policy.set(key, value);
        }
    }
    for (key, value) in overrides {
        policy.set(key, value);
    }
}

/// How to start an agent: program, args and env. Built by
/// [`AgentProfile::launch`](super::AgentProfile::launch); a host types
/// [`command_line`](Self::command_line) plus [`ShellKind::line_terminator`] into the PTY's
/// shell, and puts the env into the shell's spawn env ([`merge_env_into`](Self::merge_env_into))
/// — a running shell cannot be given env, the agent inherits it from the shell.
#[derive(Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    /// The launcher, e.g. `claude` (looked up on the shell's `PATH`).
    pub program: String,
    /// Arguments after the program, unquoted.
    pub args: Vec<String>,
    /// Variables always set for the agent (over the host's own value for the key).
    pub env: Vec<(String, String)>,
    /// Variables set only when the host's [`EnvPolicy`] does not set the key, so a user's
    /// own value wins.
    pub env_defaults: Vec<(String, String)>,
    /// The shell the plan was built for, when its content depends on it
    /// ([`for_shell`](Self::for_shell)).
    shell: Option<ShellKind>,
}

impl LaunchPlan {
    /// `program` with no args and no env.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            env_defaults: Vec::new(),
            shell: None,
        }
    }

    /// Binds the plan to `shell`: [`command_line`](Self::command_line) for another shell
    /// is then refused. A profile whose argv depends on the shell (codex leaves its
    /// pre-trust out under cmd.exe) binds its plan, so the line typed matches it.
    pub fn for_shell(mut self, shell: ShellKind) -> Self {
        self.shell = Some(shell);
        self
    }

    /// Appends the permission part: for [`Permissions::Unattended`] `unattended_args` and
    /// `unattended_env_defaults`, for [`Permissions::AgentDefault`] nothing, for
    /// [`Permissions::Custom`] its args and env.
    pub fn with_permissions(
        mut self,
        perms: &Permissions,
        unattended_args: &[&str],
        unattended_env_defaults: &[(&str, &str)],
    ) -> Self {
        match perms {
            Permissions::Unattended => {
                self.args
                    .extend(unattended_args.iter().map(|a| a.to_string()));
                self.env_defaults.extend(
                    unattended_env_defaults
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string())),
                );
            }
            Permissions::AgentDefault => {}
            Permissions::Custom { args, env } => {
                self.args.extend(args.iter().cloned());
                self.env.extend(env.iter().cloned());
            }
        }
        self
    }

    /// The program and args as one line for `shell`, each word quoted by
    /// [`ShellKind::quote`] (no line terminator; env is not part of the line).
    /// [`Error::InvalidInput`] for an empty program, a word `quote` refuses, or a plan
    /// bound to another shell ([`for_shell`](Self::for_shell)).
    pub fn command_line(&self, shell: ShellKind) -> Result<String> {
        if let Some(built) = self.shell.filter(|built| *built != shell) {
            return Err(Error::InvalidInput(format!(
                "launch plan for {} was built for {built:?}, not {shell:?}",
                self.program
            )));
        }
        if self.program.is_empty() {
            return Err(Error::InvalidInput(
                "launch plan has an empty program".into(),
            ));
        }
        let mut line = shell.quote(&self.program)?;
        for arg in &self.args {
            line.push(' ');
            line.push_str(&shell.quote(arg)?);
        }
        Ok(line)
    }

    /// Adds the plan's env to `policy`: each [`env_defaults`](Self::env_defaults) entry
    /// unless `policy` already sets the key ([`EnvPolicy::sets`]), then every
    /// [`env`](Self::env) entry (overriding). A key the policy removes stays removed.
    pub fn merge_env_into(&self, policy: &mut EnvPolicy) {
        merge_launch_env(policy, &self.env_defaults, &self.env);
    }
}

impl fmt::Debug for LaunchPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchPlan")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env_keys", &env_keys(&self.env))
            .field("env_default_keys", &env_keys(&self.env_defaults))
            .field("shell", &self.shell)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn plan(program: &str, args: &[&str]) -> LaunchPlan {
        let mut p = LaunchPlan::new(program);
        p.args = args.iter().map(|a| a.to_string()).collect();
        p
    }

    #[test]
    fn lines_end_with_lf_on_posix_and_cr_in_cmd() {
        assert_eq!(ShellKind::Posix.line_terminator(), "\n");
        assert_eq!(ShellKind::Cmd.line_terminator(), "\r");
        let native = if cfg!(windows) {
            ShellKind::Cmd
        } else {
            ShellKind::Posix
        };
        assert_eq!(ShellKind::native(), native);
    }

    #[test]
    fn a_posix_line_is_cleared_with_one_ctrl_u_and_cmd_has_no_clear() {
        assert_eq!(ShellKind::Posix.clear_line(), Some(" \u{15}"));
        assert_eq!(ShellKind::Cmd.clear_line(), None);
    }

    #[test]
    fn posix_quotes_only_what_needs_it() {
        let q = |a: &str| ShellKind::Posix.quote(a).unwrap();
        assert_eq!(
            q("check_for_update_on_startup=false"),
            "check_for_update_on_startup=false"
        );
        assert_eq!(q("--mode"), "--mode");
        assert_eq!(q("/a/b.c:d@e%f+g,h-i_j"), "/a/b.c:d@e%f+g,h-i_j");
        assert_eq!(q("a b"), "'a b'");
        assert_eq!(q("it's"), r"'it'\''s'");
        assert_eq!(q(""), "''");
        assert_eq!(q("~/x"), "'~/x'");
        assert_eq!(q("$HOME"), "'$HOME'");
        assert_eq!(q("a\"b\\c"), "'a\"b\\c'");
        assert_eq!(q("ü"), "'ü'");
        assert_eq!(q("=cmd"), "'=cmd'", "zsh expands a leading `=`");
        assert_eq!(q("a=b"), "a=b");
    }

    #[test]
    fn cmd_quotes_by_the_msvc_rules() {
        let q = |a: &str| ShellKind::Cmd.quote(a).unwrap();
        assert_eq!(
            q("check_for_update_on_startup=false"),
            "check_for_update_on_startup=false"
        );
        assert_eq!(q("a b"), "\"a b\"");
        assert_eq!(q(""), "\"\"");
        assert_eq!(q("say \"hi\""), r#""say \"hi\"""#);
        assert_eq!(q(r"C:\Program Files\x\"), r#""C:\Program Files\x\\""#);
        assert_eq!(q(r#"a\"b c"#), r#""a\\\"b c""#);
        assert_eq!(q(r"a\b c"), r#""a\b c""#);
    }

    #[test]
    fn cmd_refuses_what_it_would_interpret() {
        for arg in ["100%", "a^b", "a&b", "a|b", "a<b", "a>b", "wow!", "%PATH%"] {
            let err = ShellKind::Cmd.quote(arg).unwrap_err();
            assert!(matches!(err, Error::InvalidInput(_)), "{arg}: {err}");
        }
        // POSIX single quotes keep them literal.
        assert_eq!(ShellKind::Posix.quote("a&b|c").unwrap(), "'a&b|c'");
    }

    #[test]
    fn both_refuse_control_characters() {
        for arg in [
            "a\nb",
            "a\rb",
            "a\u{0}b",
            "a\tb",
            "a\u{1b}[Ab",
            "\u{7f}",
            "\u{9b}",
        ] {
            for shell in [ShellKind::Posix, ShellKind::Cmd] {
                let err = shell.quote(arg).unwrap_err();
                assert!(matches!(err, Error::InvalidInput(_)), "{shell:?} {arg:?}");
            }
        }
    }

    #[test]
    fn a_login_shell_command_quotes_each_word_and_escapes_backslashes_only_for_fish() {
        let words = ["my agent", "--flag", r"a\b", "it's", ""];
        assert_eq!(
            login_shell_command("/bin/zsh", words).unwrap(),
            r"'my agent' --flag 'a\b' 'it'\''s' ''"
        );
        assert_eq!(
            login_shell_command("/opt/homebrew/bin/fish", words).unwrap(),
            r"'my agent' --flag 'a\\b' 'it'\''s' ''"
        );
        assert_eq!(
            login_shell_command("/bin/sh", ["claude", "--version"]).unwrap(),
            "claude --version",
            "plain words stay bare"
        );
        for shell in ["/bin/zsh", "/usr/bin/fish"] {
            let err = login_shell_command(shell, ["agent", "a\nb"]).unwrap_err();
            assert!(matches!(err, Error::InvalidInput(_)), "{shell}: {err}");
        }
    }

    #[test]
    fn a_command_line_quotes_every_word() {
        let p = plan("my agent", &["--flag", "two words", ""]);
        assert_eq!(
            p.command_line(ShellKind::Posix).unwrap(),
            "'my agent' --flag 'two words' ''"
        );
        assert_eq!(
            p.command_line(ShellKind::Cmd).unwrap(),
            r#""my agent" --flag "two words" """#
        );
        let err = plan("", &[]).command_line(ShellKind::Posix).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
        let err = plan("x", &["a\nb"])
            .command_line(ShellKind::Posix)
            .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn permissions_pick_the_args_and_env() {
        let unattended = LaunchPlan::new("a").with_permissions(
            &Permissions::Unattended,
            &["--yes"],
            &[("K", "v")],
        );
        assert_eq!(unattended.args, ["--yes"]);
        assert_eq!(unattended.env_defaults, [("K".into(), "v".into())]);
        assert!(unattended.env.is_empty());

        let default = LaunchPlan::new("a").with_permissions(
            &Permissions::AgentDefault,
            &["--yes"],
            &[("K", "v")],
        );
        assert!(default.args.is_empty() && default.env.is_empty());
        assert!(default.env_defaults.is_empty());

        let custom = Permissions::Custom {
            args: vec!["--mine".into()],
            env: vec![("M".into(), "1".into())],
        };
        let custom = LaunchPlan::new("a").with_permissions(&custom, &["--yes"], &[("K", "v")]);
        assert_eq!(custom.args, ["--mine"]);
        assert_eq!(custom.env, [("M".into(), "1".into())]);
        assert!(custom.env_defaults.is_empty());
    }

    #[test]
    fn env_defaults_yield_to_the_host_and_env_overrides_it() {
        let mut p = LaunchPlan::new("a");
        p.env_defaults = vec![("DEF".into(), "plan".into()), ("NEW".into(), "plan".into())];
        p.env = vec![("ALWAYS".into(), "plan".into())];
        let mut policy = EnvPolicy::default();
        policy.set("DEF", "user").set("ALWAYS", "user");
        p.merge_env_into(&mut policy);
        let value = |k: &str| {
            policy
                .set
                .iter()
                .rev()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(value("DEF"), Some(OsString::from("user")));
        assert_eq!(value("NEW"), Some(OsString::from("plan")));
        assert_eq!(value("ALWAYS"), Some(OsString::from("plan")));
    }

    #[test]
    fn toml_strings_escape_quotes_backslashes_and_controls() {
        assert_eq!(toml_basic_string("/tmp/a b"), r#""/tmp/a b""#);
        assert_eq!(toml_basic_string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(toml_basic_string("a\nb\tc"), r#""a\u000Ab\u0009c""#);
        assert!(ShellKind::Posix.quote(&toml_basic_string("x\ny")).is_ok());
    }

    #[test]
    fn debug_never_prints_env_values() {
        let mut p = LaunchPlan::new("a");
        p.env = vec![("TOKEN".into(), "secret-1".into())];
        p.env_defaults = vec![("OTHER".into(), "secret-2".into())];
        let shown = format!("{p:?}");
        assert!(
            shown.contains("TOKEN") && shown.contains("OTHER"),
            "{shown}"
        );
        assert!(!shown.contains("secret"), "{shown}");
    }
}
