//! [`LaunchPlan`]: the argv and env that start an agent, and [`ShellKind`]: how that argv
//! is typed into the PTY's shell as one command line (quoting, line terminator).

use std::ffi::OsStr;
use std::fmt;
use std::path::Path;

use super::Permissions;
use crate::env::EnvPolicy;
use crate::{Error, Result};

/// The shell a launch line is typed into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ShellKind {
    /// sh, bash and zsh: POSIX single-quote quoting.
    Posix,
    /// fish: single quotes with escapes for backslashes and apostrophes.
    Fish,
    /// Windows `cmd.exe` (under ConPTY): double-quote quoting by the MSVC argv rules.
    Cmd,
}

// Canonical input must also accommodate the terminating newline. Use the POSIX
// minimum (255 including the delimiter) on targets without a known larger PTY limit.
const POSIX_COMMAND_BYTES: usize = if cfg!(any(target_os = "linux", target_os = "android")) {
    4095
} else if cfg!(target_vendor = "apple") {
    1023
} else {
    254
};
const CMD_COMMAND_UNITS: usize = 8191;

impl ShellKind {
    /// The platform's default quoting convention, without looking up the user's shell:
    /// [`Cmd`](Self::Cmd) on Windows and [`Posix`](Self::Posix) elsewhere.
    pub fn native() -> Self {
        if cfg!(windows) {
            ShellKind::Cmd
        } else {
            ShellKind::Posix
        }
    }

    pub(super) fn for_unix_shell(program: &Path) -> Self {
        if program.file_name() == Some(OsStr::new("fish")) {
            Self::Fish
        } else {
            Self::Posix
        }
    }

    /// What ends a typed line: `"\n"` for Unix shells, `"\r"` for cmd.exe under ConPTY —
    /// the byte a real Enter produces there; a bare LF does not complete a cmd line.
    pub fn line_terminator(self) -> &'static str {
        match self {
            ShellKind::Posix | ShellKind::Fish => "\n",
            ShellKind::Cmd => "\r",
        }
    }

    /// Keys that empty this shell's input line — one line, cursor at its end — without
    /// running anything; the controller types them before every launch line, so text left
    /// there (an unsubmitted paste, a draft) never runs as part of it. `None` when the crate
    /// knows no such keys.
    ///
    /// - `Posix` and `Fish`: a space, then Ctrl-U (`\x15`) once. One press empties a single line in
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
            ShellKind::Posix | ShellKind::Fish => Some(" \u{15}"),
            ShellKind::Cmd => None,
        }
    }

    /// `arg` as one word of a command line typed into this shell.
    ///
    /// - An arg made only of `A-Z a-z 0-9 _ . / : = @ % + , -`, not starting
    ///   with `=` (zsh expands `=cmd`), stays unquoted unless fish can expand it as a job reference.
    ///   An arg with a control character (newline, CR, tab, ESC, NUL, …) is refused
    ///   ([`Error::InvalidInput`]): a line editor would act on it (submit, complete, erase).
    /// - `Posix`: anything else is single-quoted, `'` written as `'\''`.
    /// - `Fish`: anything else is single-quoted, with backslashes and apostrophes escaped.
    /// - `Cmd`: `%`, `^`, `&`, `|`, `<`, `>` and `!` are refused — cmd.exe expands or
    ///   interprets them even inside quotes on an interactive line. Anything else is
    ///   double-quoted: `"` becomes `\"`, backslashes before a `"` or the closing quote are
    ///   doubled.
    pub fn quote(self, arg: &str) -> Result<String> {
        self.validate_word(arg)?;
        if is_plain(arg) && !(self == Self::Fish && arg.starts_with('%')) {
            return Ok(arg.to_owned());
        }
        Ok(match self {
            ShellKind::Posix => format!("'{}'", arg.replace('\'', r"'\''")),
            ShellKind::Fish => format!("'{}'", arg.replace('\\', r"\\").replace('\'', r"\'")),
            ShellKind::Cmd => msvc_quote(arg),
        })
    }

    fn validate_word(self, arg: &str) -> Result<()> {
        if arg.chars().any(char::is_control) {
            return Err(Error::InvalidInput(
                "a typed launch argument cannot carry a control character".into(),
            ));
        }
        if self == ShellKind::Cmd {
            if let Some(c) = arg.chars().find(|c| CMD_SPECIAL.contains(c)) {
                return Err(Error::InvalidInput(format!(
                    "cmd.exe interprets {c:?} in a typed launch argument"
                )));
            }
        }
        Ok(())
    }

    /// The quote-expanded command as the shell receives it after line clearing and
    /// before Enter. The PTY queue separately accounts for every byte of the write.
    fn validate_command_length(self, line: &str) -> Result<()> {
        match self {
            Self::Posix | Self::Fish if line.len() > POSIX_COMMAND_BYTES => {
                Err(Error::InvalidInput(format!(
                    "quoted launch command exceeds the canonical PTY limit of {POSIX_COMMAND_BYTES} bytes before Enter"
                )))
            }
            Self::Cmd if line.encode_utf16().count() > CMD_COMMAND_UNITS => {
                Err(Error::InvalidInput(format!(
                    "quoted launch command exceeds the cmd.exe limit of {CMD_COMMAND_UNITS} UTF-16 code units"
                )))
            }
            _ => Ok(()),
        }
    }
}

/// `words` as one command line for a one-shot `<shell> -c` run (an RPC launch, an install
/// probe), `shell` being a unix shell path such as `$SHELL`. Each word is quoted for
/// that shell (see [`ShellKind::quote`]).
// Used by the unix `rpc` and `probe` paths only.
#[cfg_attr(
    not(all(unix, any(feature = "rpc", feature = "probe"))),
    allow(dead_code)
)]
pub(crate) fn login_shell_command<'a>(
    shell: &str,
    words: impl IntoIterator<Item = &'a str>,
) -> Result<String> {
    let shell = ShellKind::for_unix_shell(Path::new(shell));
    let quoted = words
        .into_iter()
        .map(|word| shell.quote(word))
        .collect::<Result<Vec<_>>>()?;
    Ok(quoted.join(" "))
}

/// Characters cmd.exe acts on in an interactive line, quoted or not (`!` only with delayed
/// expansion on, which a user can enable in the registry).
const CMD_SPECIAL: &[char] = &['%', '^', '&', '|', '<', '>', '!'];

/// Uses only characters that can be left bare, subject to shell-specific expansion.
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

/// The model and optional reasoning effort requested for a profile-built PTY shell launch.
/// Model IDs may contain spaces, brackets and provider-specific punctuation. The profile
/// defines its effort labels. The CLI determines availability for the account at runtime.
/// [`AgentTerminal::spawn`](super::AgentTerminal::spawn) validates both identifiers
/// before calling the profile. Each must be nonblank, must not start with `-`, and
/// must be representable by [`ShellKind::quote`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ModelChoice {
    /// Model name or alias, passed as one argument by the supported built-in profiles.
    pub model: String,
    /// Profile-specific effort label. `None` omits a separate effort argument.
    pub effort: Option<String>,
}

impl ModelChoice {
    /// Requests a model without a separate effort argument. The CLI interprets the
    /// model string. For example, Pi can select a thinking level from a `:high` suffix.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            effort: None,
        }
    }

    /// Also requests this effort. Spawn fails if the profile cannot map the whole choice.
    #[must_use]
    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    pub(super) fn validate(&self, shell: ShellKind) -> Result<()> {
        for id in std::iter::once(&self.model).chain(&self.effort) {
            if id.trim().is_empty() || id.starts_with('-') {
                return Err(Error::InvalidInput(
                    "model and effort identifiers must be nonblank and cannot start with '-'"
                        .into(),
                ));
            }
            shell.validate_word(id)?;
        }
        Ok(())
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
    /// Prepared tool configuration. Clones share ownership of temporary files.
    #[cfg(feature = "tools")]
    pub tools: Option<crate::tools::ToolAttachment>,
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
            #[cfg(feature = "tools")]
            tools: None,
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

    pub(super) fn append_model_args(&mut self, args: Vec<String>) -> Result<()> {
        if self.args.iter().any(|arg| arg == "--") {
            return Err(Error::InvalidInput(
                "cannot append model arguments after an end-of-options marker".into(),
            ));
        }
        if args.is_empty() {
            return Err(Error::InvalidInput(
                "profile returned no arguments for a model choice".into(),
            ));
        }
        self.args.extend(args);
        Ok(())
    }

    /// The controller stores the terminated line and prepends the shell's clear keys
    /// when writing it. A selected command must fit the shell input line and, with
    /// those keys and the terminator, one PTY write.
    pub(super) fn launch_line(
        &self,
        shell: ShellKind,
        max_write_bytes: Option<usize>,
    ) -> Result<String> {
        let mut line = self.command_line(shell)?;
        let terminator = shell.line_terminator();
        if let Some(limit) = max_write_bytes {
            let size = line
                .len()
                .checked_add(terminator.len())
                .and_then(|n| n.checked_add(shell.clear_line().unwrap_or_default().len()))
                .unwrap_or(usize::MAX);
            if size > limit {
                return Err(Error::InputTooLarge { size, limit });
            }
            shell.validate_command_length(&line)?;
        }
        line.push_str(terminator);
        Ok(line)
    }

    /// Adds the plan's env to `policy`: each [`env_defaults`](Self::env_defaults) entry
    /// unless `policy` already sets the key ([`EnvPolicy::sets`]), then every
    /// [`env`](Self::env) entry (overriding). A key the policy removes stays removed.
    pub fn merge_env_into(&self, policy: &mut EnvPolicy) {
        merge_launch_env(policy, &self.env_defaults, &self.env);
        #[cfg(feature = "tools")]
        if let Some(tools) = &self.tools {
            tools.apply_path(policy);
        }
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
    fn model_and_effort_must_be_identifiers_the_shell_can_carry() {
        for shell in [ShellKind::Posix, ShellKind::Fish, ShellKind::Cmd] {
            for id in [
                "",
                " \u{2003}",
                "-m",
                "--high",
                "foo\0bar",
                "a\rb",
                "a\u{9b}b",
            ] {
                for choice in [
                    ModelChoice::new(id),
                    ModelChoice::new("model").with_effort(id),
                ] {
                    assert!(
                        matches!(choice.validate(shell), Err(Error::InvalidInput(_))),
                        "{id:?}"
                    );
                }
            }
            ModelChoice::new("provider/model:tag [1m]")
                .validate(shell)
                .unwrap();
        }
        for id in ["%PATH%", "a!b", "a&b"] {
            let choice = ModelChoice::new(id);
            choice.validate(ShellKind::Posix).unwrap();
            assert!(matches!(
                choice.validate(ShellKind::Cmd),
                Err(Error::InvalidInput(_))
            ));
        }
    }

    #[test]
    fn a_model_mapping_needs_words_and_an_open_option_list() {
        let mut base = plan("agent", &["--policy", "custom"]);
        assert!(matches!(
            base.append_model_args(Vec::new()),
            Err(Error::InvalidInput(_))
        ));
        base.args.push("--".into());
        assert!(matches!(
            base.append_model_args(vec!["--model".into(), "opus[1m]".into()]),
            Err(Error::InvalidInput(_))
        ));
        assert_eq!(base.args, ["--policy", "custom", "--"]);
    }

    #[test]
    fn selected_launch_budget_covers_encoded_bytes_clear_keys_and_enter() {
        let p = plan("agent", &["--model", "it's π\\"]);
        for (shell, line, keys) in [
            (
                ShellKind::Posix,
                "agent --model 'it'\\''s π\\'\n",
                " \u{15}",
            ),
            (ShellKind::Fish, "agent --model 'it\\'s π\\\\'\n", " \u{15}"),
            (ShellKind::Cmd, "agent --model \"it's π\\\\\"\r", ""),
        ] {
            let bytes = keys.len() + line.len();
            assert_eq!(p.launch_line(shell, Some(bytes)).unwrap(), line);
            assert!(matches!(
                p.launch_line(shell, Some(bytes - 1)),
                Err(Error::InputTooLarge { size, limit }) if size == bytes && limit == bytes - 1
            ));
        }
    }

    #[test]
    fn selected_launch_checks_the_quoted_shell_line_even_when_the_write_fits() {
        for (shell, limit) in [
            (ShellKind::Posix, POSIX_COMMAND_BYTES),
            (ShellKind::Fish, POSIX_COMMAND_BYTES),
            (ShellKind::Cmd, 8191),
        ] {
            // The base argument needs quoting. The supplementary character costs
            // four UTF-8 bytes on a POSIX tty and two UTF-16 units in cmd.exe.
            let mut p = plan("agent", &[" a🦀", "--model", "chosen", "--effort", "high"]);
            let base = p.command_line(shell).unwrap();
            let size = match shell {
                ShellKind::Posix | ShellKind::Fish => base.len(),
                ShellKind::Cmd => base.encode_utf16().count(),
            };
            p.args[0].push_str(&"x".repeat(limit - size));
            assert!(p.launch_line(shell, Some(1024 * 1024)).is_ok());
            p.args[0].push('x');
            assert!(matches!(
                p.launch_line(shell, Some(1024 * 1024)),
                Err(Error::InvalidInput(_))
            ));
        }
    }

    #[test]
    fn lines_end_with_lf_on_unix_and_cr_in_cmd() {
        assert_eq!(ShellKind::Posix.line_terminator(), "\n");
        assert_eq!(ShellKind::Fish.line_terminator(), "\n");
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
        assert_eq!(ShellKind::Fish.clear_line(), Some(" \u{15}"));
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
    fn all_shell_kinds_refuse_control_characters() {
        for arg in [
            "a\nb",
            "a\rb",
            "a\u{0}b",
            "a\tb",
            "a\u{1b}[Ab",
            "\u{7f}",
            "\u{9b}",
        ] {
            for shell in [ShellKind::Posix, ShellKind::Fish, ShellKind::Cmd] {
                let err = shell.quote(arg).unwrap_err();
                assert!(matches!(err, Error::InvalidInput(_)), "{shell:?} {arg:?}");
            }
        }
    }

    #[test]
    fn a_login_shell_command_quotes_each_word_for_the_named_shell() {
        let words = ["my agent", "--flag", r"a\b", "it's", "", "%self"];
        assert_eq!(
            login_shell_command("/bin/zsh", words).unwrap(),
            r"'my agent' --flag 'a\b' 'it'\''s' '' %self"
        );
        assert_eq!(
            login_shell_command("/opt/homebrew/bin/fish", words).unwrap(),
            r"'my agent' --flag 'a\\b' 'it\'s' '' '%self'"
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
