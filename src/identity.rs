//! Which terminal the child believes it runs in: the `TERM` family of variables a PTY
//! session sets, and the inherited ones it strips so the child does not mistake the host's
//! own terminal for the PTY.
//!
//! Agent CLIs change behaviour on these variables (synchronized output, colour depth,
//! hyperlinks), so a session states its identity explicitly instead of passing through
//! whatever terminal the host process was started from.
//!
//! Keep the default, [`TerminalIdentity::Honest`]. Claim a known terminal
//! ([`TerminalIdentity::Impersonate`]) only when the host's renderer really behaves like it
//! (claude draws synchronized-output frames only for VS Code); [`TerminalIdentity::Custom`]
//! sets exactly the given variables and strips nothing. The host's
//! [`EnvPolicy`](crate::env::EnvPolicy) is applied after the identity and wins.
//!
//! ```
//! use agent_link::{KnownTerminal, TerminalIdentity};
//!
//! let (set, remove) = TerminalIdentity::Impersonate(KnownTerminal::VsCode).env();
//! assert!(set.contains(&("TERM_PROGRAM".to_string(), "vscode".to_string())));
//! assert!(remove.contains(&"TMUX")); // the host's multiplexer is not the PTY's
//! ```

/// Inherited variables that describe the host's terminal, multiplexer or screen size.
/// [`TerminalIdentity::Honest`] and [`TerminalIdentity::Impersonate`] remove every one they
/// do not set themselves. Exact names only: other vendor-prefixed variables (`KITTY_*`,
/// `WEZTERM_*`, …) pass through unless the host's policy removes them.
const STRIPPED: &[&str] = &[
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "TERM_FEATURES",
    "TERMINAL_EMULATOR",
    "TERMINFO",
    "COLORFGBG",
    "WT_SESSION",
    "WT_PROFILE_ID",
    "KITTY_WINDOW_ID",
    "VTE_VERSION",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "ITERM_SESSION_ID",
    "ITERM_PROFILE",
    // Multiplexers: the child is not inside the host's tmux / screen / zellij.
    "TMUX",
    "TMUX_PANE",
    "STY",
    "WINDOW",
    "ZELLIJ",
    "ZELLIJ_SESSION_NAME",
    "ZELLIJ_PANE_ID",
    // The PTY has its own size; inherited values would override it for some programs.
    "COLUMNS",
    "LINES",
];

/// `TERM_PROGRAM_VERSION` sent as VS Code (the value the recorded agent fixtures used).
pub const VSCODE_VERSION: &str = "1.104.0";
/// `TERM_PROGRAM_VERSION` sent as WezTerm.
pub const WEZTERM_VERSION: &str = "20240203-110809-5046fc22";
/// `TERM_PROGRAM_VERSION` sent as Ghostty.
pub const GHOSTTY_VERSION: &str = "1.2.0";
/// `WT_SESSION` sent as Windows Terminal: a fixed, made-up session GUID.
pub const WT_SESSION_ID: &str = "6f1c2a8e-4b3d-4e7a-9f05-2c8d1b7e3a90";

/// The terminal a PTY child is told it runs in. Applied before the host's
/// [`EnvPolicy`](crate::env::EnvPolicy), so a variable the policy sets or removes wins.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum TerminalIdentity {
    /// A truthful xterm-compatible terminal: `TERM=xterm-256color`,
    /// `COLORTERM=truecolor`, and the inherited terminal, multiplexer and size variables
    /// removed (`TERM_PROGRAM`, `WT_SESSION`, `TMUX`, `COLUMNS`, …; exact names only).
    #[default]
    Honest,
    /// [`Honest`](Self::Honest) plus the variables a known terminal sets, for CLIs that
    /// enable features only for terminals they recognise. Use only when the host's
    /// renderer really supports what that terminal does.
    Impersonate(KnownTerminal),
    /// Exactly these variables are set; nothing is removed, the rest is inherited.
    Custom {
        /// `(name, value)` pairs, set in order.
        vars: Vec<(String, String)>,
    },
}

/// Terminals [`TerminalIdentity::Impersonate`] can claim to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KnownTerminal {
    /// `TERM_PROGRAM=vscode` with [`VSCODE_VERSION`].
    VsCode,
    /// `WT_SESSION=`[`WT_SESSION_ID`].
    WindowsTerminal,
    /// `TERM_PROGRAM=WezTerm` with [`WEZTERM_VERSION`].
    WezTerm,
    /// `TERM_PROGRAM=ghostty` with [`GHOSTTY_VERSION`].
    Ghostty,
}

impl KnownTerminal {
    /// The variables this terminal adds on top of the honest set.
    fn vars(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::VsCode => &[
                ("TERM_PROGRAM", "vscode"),
                ("TERM_PROGRAM_VERSION", VSCODE_VERSION),
            ],
            Self::WindowsTerminal => &[("WT_SESSION", WT_SESSION_ID)],
            Self::WezTerm => &[
                ("TERM_PROGRAM", "WezTerm"),
                ("TERM_PROGRAM_VERSION", WEZTERM_VERSION),
            ],
            Self::Ghostty => &[
                ("TERM_PROGRAM", "ghostty"),
                ("TERM_PROGRAM_VERSION", GHOSTTY_VERSION),
            ],
        }
    }
}

impl TerminalIdentity {
    /// `(set, remove)`: the variables to set, in order, and the inherited ones to remove.
    /// No name is in both lists.
    pub fn env(&self) -> (Vec<(String, String)>, Vec<&'static str>) {
        let honest = [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")];
        let set: Vec<(String, String)> = match self {
            Self::Honest => pairs(&honest),
            Self::Impersonate(known) => {
                let mut set = pairs(&honest);
                set.extend(pairs(known.vars()));
                set
            }
            Self::Custom { vars } => return (vars.clone(), Vec::new()),
        };
        let remove = STRIPPED
            .iter()
            .copied()
            .filter(|name| !set.iter().any(|(k, _)| k == name))
            .collect();
        (set, remove)
    }
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value<'a>(set: &'a [(String, String)], key: &str) -> Option<&'a str> {
        set.iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn honest_sets_term_and_colorterm_and_strips_every_terminal_name() {
        let (set, remove) = TerminalIdentity::Honest.env();
        assert_eq!(
            set,
            [
                ("TERM".to_string(), "xterm-256color".to_string()),
                ("COLORTERM".to_string(), "truecolor".to_string())
            ]
        );
        assert_eq!(remove, STRIPPED.to_vec());
        for name in [
            "TERM_PROGRAM",
            "TERM_PROGRAM_VERSION",
            "WT_SESSION",
            "KITTY_WINDOW_ID",
            "VTE_VERSION",
            "LC_TERMINAL",
            "LC_TERMINAL_VERSION",
            "ITERM_SESSION_ID",
        ] {
            assert!(remove.contains(&name), "{name} must be stripped");
        }
    }

    #[test]
    fn honest_is_the_default() {
        assert_eq!(TerminalIdentity::default(), TerminalIdentity::Honest);
    }

    #[test]
    fn vscode_adds_term_program_and_keeps_it_out_of_the_strip_list() {
        let (set, remove) = TerminalIdentity::Impersonate(KnownTerminal::VsCode).env();
        assert_eq!(value(&set, "TERM"), Some("xterm-256color"));
        assert_eq!(value(&set, "COLORTERM"), Some("truecolor"));
        assert_eq!(value(&set, "TERM_PROGRAM"), Some("vscode"));
        assert_eq!(value(&set, "TERM_PROGRAM_VERSION"), Some(VSCODE_VERSION));
        assert!(!remove.contains(&"TERM_PROGRAM"));
        assert!(!remove.contains(&"TERM_PROGRAM_VERSION"));
        assert!(remove.contains(&"WT_SESSION"));
        assert!(remove.contains(&"ITERM_SESSION_ID"));
    }

    #[test]
    fn every_known_terminal_sets_its_names_and_never_removes_them() {
        for known in [
            KnownTerminal::VsCode,
            KnownTerminal::WindowsTerminal,
            KnownTerminal::WezTerm,
            KnownTerminal::Ghostty,
        ] {
            let (set, remove) = TerminalIdentity::Impersonate(known).env();
            assert_eq!(value(&set, "TERM"), Some("xterm-256color"), "{known:?}");
            for (k, v) in known.vars() {
                assert_eq!(value(&set, k), Some(*v), "{known:?}");
                assert!(!remove.contains(k), "{known:?} removes its own {k}");
            }
        }
        let (set, _) = TerminalIdentity::Impersonate(KnownTerminal::WindowsTerminal).env();
        assert_eq!(value(&set, "WT_SESSION"), Some(WT_SESSION_ID));
        let (set, _) = TerminalIdentity::Impersonate(KnownTerminal::WezTerm).env();
        assert_eq!(value(&set, "TERM_PROGRAM"), Some("WezTerm"));
        let (set, _) = TerminalIdentity::Impersonate(KnownTerminal::Ghostty).env();
        assert_eq!(value(&set, "TERM_PROGRAM"), Some("ghostty"));
    }

    #[test]
    fn custom_sets_exactly_its_vars_and_removes_nothing() {
        let identity = TerminalIdentity::Custom {
            vars: vec![("TERM".into(), "xterm-256color".into())],
        };
        let (set, remove) = identity.env();
        assert_eq!(set, [("TERM".to_string(), "xterm-256color".to_string())]);
        assert!(remove.is_empty());

        let (set, remove) = TerminalIdentity::Custom { vars: Vec::new() }.env();
        assert!(set.is_empty() && remove.is_empty());
    }
}
