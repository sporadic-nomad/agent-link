//! `--identity`: the environment that tells a CLI which terminal it runs in. Every identity
//! first removes what the capture itself inherited from its own terminal, multiplexer and
//! agent session, so a capture run from tmux, Ghostty or a Claude Code session records the
//! CLI as a fresh terminal would show it.

use portable_pty::CommandBuilder;

/// Inherited variables naming the terminal emulator or multiplexer (exact names).
const TERMINAL_VARS: &[&str] = &[
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "TERM_FEATURES",
    "TERMINAL_EMULATOR",
    "TERMINFO",
    "COLORFGBG",
    "VTE_VERSION",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "TMUX",
    "ZELLIJ",
    "STY",
    "WINDOW",
    "TILIX_ID",
    // The caller's window size: ncurses and Python CLIs prefer these over the PTY size.
    "COLUMNS",
    "LINES",
];

/// The same, by prefix: every inherited variable starting with one of these is removed.
const TERMINAL_PREFIXES: &[&str] = &[
    "TMUX_",
    "ZELLIJ_",
    "ITERM",
    "WT_",
    "KITTY_",
    "GHOSTTY_",
    "WEZTERM_",
    "KONSOLE_",
    "ALACRITTY_",
    "GNOME_TERMINAL_",
    "XTERM_",
    "TERMINATOR_",
    "VSCODE_",
    "WARP_",
    "CMUX_",
    "HERDR_",
];

/// Markers an agent CLI sets for the commands it runs: a CLI that sees them may act as a
/// nested session. Configuration the caller passes on purpose (`CLAUDE_CONFIG_DIR`,
/// `CODEX_HOME`, `XDG_*`, API keys, other `CLAUDE_CODE_*` settings) is kept.
const AGENT_SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "CODEX_SESSION_ID",
    "CODEX_THREAD_ID",
    "CODEX_ESCALATE_SOCKET",
    "OPENCODE",
    "OPENCODE_PID",
];

/// Whether an inherited variable is removed before any identity applies.
pub fn stripped(key: &str) -> bool {
    // Windows variable names are case-insensitive.
    let key = if cfg!(windows) {
        key.to_ascii_uppercase()
    } else {
        key.to_string()
    };
    TERMINAL_VARS.contains(&key.as_str())
        || AGENT_SESSION_VARS.contains(&key.as_str())
        || TERMINAL_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// Applies `--identity` to the command's environment (which starts as the inherited one).
/// `custom:` values cannot contain `,` (the pair separator).
pub fn apply(cmd: &mut CommandBuilder, identity: &str, term_version: &str) -> Result<(), String> {
    // `vars_os`, not the builder's `iter_full_env_as_str`: that one skips variables whose
    // name or value is not UTF-8, which would let such a marker through.
    let inherited: Vec<std::ffi::OsString> = std::env::vars_os()
        .map(|(k, _)| k)
        .filter(|k| stripped(&k.to_string_lossy()))
        .collect();
    for key in inherited {
        cmd.env_remove(key);
    }
    if let Some(vars) = identity.strip_prefix("custom:") {
        cmd.env_remove("TERM");
        cmd.env_remove("COLORTERM");
        for pair in vars.split(',').filter(|p| !p.is_empty()) {
            let (k, v) = pair
                .split_once('=')
                .filter(|(k, _)| !k.is_empty())
                .ok_or_else(|| format!("--identity custom: {pair:?} is not K=V"))?;
            cmd.env(k, v);
        }
        return Ok(());
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    if identity == "vscode" {
        cmd.env("TERM_PROGRAM", "vscode");
        cmd.env("TERM_PROGRAM_VERSION", term_version);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_terminal_multiplexer_and_session_markers_only() {
        for key in [
            "TERM_PROGRAM",
            "TMUX",
            "TMUX_PANE",
            "STY",
            "ZELLIJ_SESSION_NAME",
            "GHOSTTY_RESOURCES_DIR",
            "WEZTERM_PANE",
            "ITERM_SESSION_ID",
            "VSCODE_INJECTION",
            "TERMINFO",
            "COLORFGBG",
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_SESSION_ID",
            "CODEX_SANDBOX",
            "CMUX_SURFACE_ID",
        ] {
            assert!(stripped(key), "{key}");
        }
        for key in [
            "TERM",
            "PATH",
            "HOME",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_SUBAGENT_MODEL",
            "CODEX_HOME",
            "XDG_CONFIG_HOME",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "TERMINFO_DIRS",
            "TMUXINATOR_CONFIG",
        ] {
            assert!(!stripped(key), "{key}");
        }
    }
}
