//! [`AgentProfile`]: what a host needs to know about one agent CLI — its startup dialogs
//! and the key that answers them, its ready composer, whether text just sent still sits in
//! that composer; and how to start it ([`LaunchPlan`]), reset it, find it on `PATH`, ask
//! whether it is signed in ([`LoginCheck`]) and run it over stdio ([`RpcLaunch`]).

use std::fmt;
use std::path::Path;

use super::launch::{env_keys, merge_launch_env};
use super::{LaunchPlan, Permissions, ShellKind, TrustPolicy, detect};
use crate::env::EnvPolicy;
use crate::identity::KnownTerminal;
use crate::{Error, Result};

/// A key the startup gate sends to a dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TrustKey {
    /// Confirms the selected option (`\r`).
    Enter,
    /// Moves the selection up (`ESC [ A`).
    Up,
    /// Moves the selection down (`ESC [ B`).
    Down,
}

impl TrustKey {
    /// The bytes to write to the PTY.
    pub fn bytes(self) -> &'static [u8] {
        match self {
            TrustKey::Enter => b"\r",
            TrustKey::Up => b"\x1b[A",
            TrustKey::Down => b"\x1b[B",
        }
    }
}

/// Which startup dialog is on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DialogKind {
    /// "Do you trust this folder?" — the CLI loads project files only once trusted.
    Trust,
    /// codex's "Enable full access?" confirmation.
    FullAccess,
    /// claude's bypass-permissions warning (`--permission-mode bypassPermissions`).
    BypassWarning,
    /// codex's startup "Update available!" prompt. Its default option runs an installer,
    /// so the answer steered to is "Skip".
    UpdatePrompt,
}

/// A recognised startup dialog and the next key toward the answer the profile gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Dialog {
    /// Which dialog.
    pub kind: DialogKind,
    /// The next key: Enter once the wanted option is selected, an arrow toward it
    /// otherwise. `None` when the selection cannot be read or the profile does not answer
    /// this dialog: leave it to the user — never press Enter blind, the default may be
    /// "exit" or "update now".
    pub key: Option<TrustKey>,
}

/// What a screen shows, for the startup gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScreenState {
    /// A startup dialog: the agent is not safe for programmatic input.
    Dialog(Dialog),
    /// The agent's composer is up and ready for input.
    InputReady,
    /// Neither (still starting, or a screen the profile does not know).
    Unknown,
}

/// When an agent emits a terminal signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Unlock {
    /// Never seen.
    Never,
    /// Always, whatever the terminal identity.
    Always,
    /// Only when the terminal identity claims this terminal
    /// ([`TerminalIdentity::Impersonate`](crate::TerminalIdentity::Impersonate)).
    WithIdentity(KnownTerminal),
    /// Only with this setting of the agent's own turned on.
    WithAgentSetting(&'static str),
    /// Only once the agent's terminal-mode query (DECRQM) for it is answered as supported:
    /// by the crate ([`ScreenConfig::answer_queries`](crate::ScreenConfig::answer_queries))
    /// or by a host renderer that knows the mode — one that does not (xterm.js 5.5 has no
    /// DEC 2026) turns it off.
    WhenQueryAnswered,
    /// Not measured.
    #[default]
    Unknown,
}

/// Which activity signals an agent emits, for a host choosing how to tell "working" from
/// "idle". [`Default`]: nothing known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct SignalHints {
    /// The window title animates a Braille spinner (`⠋⠙⠹…`) while the agent works.
    pub title_spinner: bool,
    /// OSC 9;4 progress reports ([`Event::Progress`](crate::Event::Progress)).
    pub osc_progress: Unlock,
    /// Synchronized-output frames (DEC mode 2026, [`Event::Frame`](crate::Event::Frame)).
    pub sync_frames: Unlock,
}

/// How to ask an agent CLI whether it is signed in: run `<launcher> <args>`.
///
/// Every row is verified in **both** states and against an unsupported subcommand, because
/// the failure that must never happen is "signed out" reported for a working account:
///
/// | | signed out | signed in | unsupported subcommand |
/// | --- | --- | --- | --- |
/// | `codex login status` | 1, stderr `Not logged in` | 0 | **2** |
/// | `claude auth status` | 1, stdout `"loggedIn": false` | 0 | **1**, stderr, no stdout |
///
/// So a non-zero exit means "signed out" only when the output also holds
/// [`signed_out_marker`](Self::signed_out_marker): claude's unsupported-subcommand exit is
/// `1` too, and only the marker tells it apart (signed out prints JSON on stdout, an unknown
/// subcommand an error on stderr). Anything else is "unknown" — which a host treats as
/// "assume fine", never as signed out. An agent without a verified command has no check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LoginCheck {
    /// The argv after the launcher.
    pub args: &'static [&'static str],
    /// Text the output must contain for a non-zero exit to mean "signed out". Match it
    /// case-insensitively with all whitespace removed on both sides, so pretty-printed and
    /// compact output both match.
    pub signed_out_marker: &'static str,
}

/// Options for [`AgentProfile::rpc_launch_with_options`].
///
/// Defaults to a fresh, non-probe launch with [`Permissions::Unattended`] (YOLO or
/// the profile's equivalent), matching [`AgentSpec::new`](super::AgentSpec::new).
/// Launch policy does not implement protocol approval handling: the host must service
/// any approval requests produced by its chosen CLI configuration. Profiles that cannot
/// honor these options return [`Error::Unsupported`] before a child is spawned.
///
/// The struct is `#[non_exhaustive]` so options can be added without breaking hosts:
/// build it from [`default`](Default::default) with the `with_*` methods, or assign
/// the public fields of a default value.
///
/// ```
/// use agent_link::agent::{Permissions, RpcLaunchOptions};
///
/// let options = RpcLaunchOptions::default()
///     .with_resume(true)
///     .with_permissions(Permissions::AgentDefault);
/// assert!(options.resume && !options.probe);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RpcLaunchOptions {
    /// Continue the previous conversation where supported. Codex and OpenCode resume
    /// through their protocols instead of a launch flag; the host sends those requests.
    pub resume: bool,
    /// Short-lived child (e.g. a model-catalog query); avoid creating a session where
    /// the CLI supports it. Pi's `resume` flag takes precedence over `probe`.
    pub probe: bool,
    /// Approval posture. Defaults to [`Permissions::Unattended`]. Explicitly select
    /// [`Permissions::AgentDefault`] or [`Permissions::Custom`] to configure restrictions.
    pub permissions: Permissions,
}

impl RpcLaunchOptions {
    /// Sets [`resume`](Self::resume).
    #[must_use]
    pub fn with_resume(mut self, resume: bool) -> Self {
        self.resume = resume;
        self
    }

    /// Sets [`probe`](Self::probe).
    #[must_use]
    pub fn with_probe(mut self, probe: bool) -> Self {
        self.probe = probe;
        self
    }

    /// Sets [`permissions`](Self::permissions).
    #[must_use]
    pub fn with_permissions(mut self, permissions: Permissions) -> Self {
        self.permissions = permissions;
        self
    }
}

/// How to run an agent as a headless child speaking JSON lines over stdio.
///
/// Built-in profiles apply [`RpcLaunchOptions::permissions`]; [`new`](Self::new) is
/// a raw command constructor and adds no permission settings itself. Arguments and
/// environment entries are inspectable before spawning. They describe launch inputs,
/// not a guarantee that native policy, managed rules or resumed sessions allow tools.
#[derive(Clone, PartialEq, Eq)]
pub struct RpcLaunch {
    /// The launcher name, e.g. `claude` (looked up on the child's `PATH`).
    pub launcher: String,
    /// Arguments after the launcher, unquoted.
    pub args: Vec<String>,
    /// Variables always set for the agent, overriding host values for the same key.
    /// A key the host environment policy removes stays removed.
    pub env: Vec<(String, String)>,
    /// Variables set only when the host's env policy does not set the key.
    pub env_defaults: Vec<(String, String)>,
    /// Optional tool configuration and its shared temporary-file lifetime. Constructors
    /// default to `None`. Keep this attached when forwarding a prepared launch.
    #[cfg(feature = "tools")]
    pub tools: Option<crate::tools::ToolAttachment>,
}

impl RpcLaunch {
    /// `launcher` with `args` and no environment or permission overrides.
    pub fn new(launcher: &str, args: &[&str]) -> Self {
        Self {
            launcher: launcher.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: Vec::new(),
            env_defaults: Vec::new(),
            #[cfg(feature = "tools")]
            tools: None,
        }
    }

    /// Append the permission settings after the transport arguments. Unattended uses
    /// the supplied profile args/env defaults; native policy adds nothing; custom
    /// settings replace the unattended ones. This does not install an approval handler.
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

    /// Add env defaults unless the host already sets the key, then explicit
    /// [`env`](Self::env) overrides. Host removals take precedence over both.
    /// Unix login-shell startup files can subsequently change the child's environment.
    pub fn merge_env_into(&self, policy: &mut EnvPolicy) {
        merge_launch_env(policy, &self.env_defaults, &self.env);
        #[cfg(feature = "tools")]
        if let Some(tools) = &self.tools {
            tools.apply_path(policy);
        }
    }
}

impl fmt::Debug for RpcLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcLaunch")
            .field("launcher", &self.launcher)
            .field("args", &self.args)
            .field("env_keys", &env_keys(&self.env))
            .field("env_default_keys", &env_keys(&self.env_defaults))
            .finish()
    }
}

/// One agent CLI: its screens and how to start it.
///
/// Screen checks take text, not a screen: either the visible screen's rows joined by `\n`,
/// or — for [`dialog`](Self::dialog) only — the raw output stream decoded as UTF-8 (a
/// rolling buffer, for a dialog drawn with cursor moves that the screen read missed).
/// The delivery thread calls [`question_dialog`](Self::question_dialog) on the visible
/// rows drawn since launch. Startup classification and steering use [`dialog`](Self::dialog).
///
/// A host profile implements [`id`](Self::id), [`dialog`](Self::dialog) and
/// [`input_ready`](Self::input_ready); the launch and data methods have defaults (launch
/// the first launcher, else the id, with no flags; no reset, login check or RPC launch).
///
/// Callbacks must return promptly. Screen classification runs on the monitor or delivery
/// thread, outside the controller state mutex; the same profile may be called concurrently.
/// Teardown does not wait for a blocked callback. Its detached thread and bounded snapshots
/// remain until it returns. Screen results are discarded when output, geometry, or the
/// agent lifecycle changed during evaluation, and cancellation is rechecked before use.
/// Callbacks may be repeated on a newer observation; a stale absence of a dialog cannot
/// authorize input, and a stale composer result cannot prove delivery.
/// A stalled monitor has a bounded event queue: overflow disables automation for that
/// session (`NotConnected`) while the host's raw terminal stream remains available.
/// Metadata and launch callbacks run synchronously before a child is spawned.
pub trait AgentProfile: Send + Sync {
    /// Per-run MCP configuration supported by this profile and transport. This is a
    /// configuration mapping, not evidence that an installed CLI connected successfully.
    /// Custom profiles opt in explicitly; the default never guesses from a profile ID.
    #[cfg(feature = "tools")]
    fn mcp_backend(
        &self,
        _transport: crate::tools::ToolTransport,
    ) -> Option<crate::tools::McpBackend> {
        None
    }

    /// The agent's id (`claude`, `codex`, …).
    fn id(&self) -> &str;

    /// A recognised startup dialog in `text` (screen text or the raw rolling buffer), with
    /// the next key toward the profile's answer. When `text` holds several dialogs (a
    /// rolling buffer), the latest one counts.
    fn dialog(&self, text: &str) -> Option<Dialog>;

    /// The ready composer is on `screen`. Pass the **current screen** only, never a rolling
    /// buffer: old output in a buffer can fake readiness under a dialog. Return false
    /// while a startup dialog is visible. Delivery checks for questions separately.
    fn input_ready(&self, screen: &str) -> bool;

    /// Dialog first, then the ready composer, else [`ScreenState::Unknown`]: a dialog wins
    /// over ready-looking text on the same screen.
    fn classify(&self, screen: &str) -> ScreenState {
        if let Some(dialog) = self.dialog(screen) {
            ScreenState::Dialog(dialog)
        } else if self.input_ready(screen) {
            ScreenState::InputReady
        } else {
            ScreenState::Unknown
        }
    }

    /// Whether `sent` (text the host just pasted or typed) still sits unsubmitted in the
    /// composer on `screen` (the current screen). `Some(false)` must mean a readable
    /// composer no longer holds `sent`; `None` means the composer cannot be read and
    /// never proves submission or permits an Enter retry. Verification first observes
    /// `Some(true)` after the paste, then requires fresh output and stable `Some(false)`
    /// after Enter. See [`detect::pending_input_default`] for the default rule.
    fn pending_input(&self, screen: &str, sent: &str) -> Option<bool> {
        detect::pending_input_default(screen, sent)
    }

    /// Recognizes a visible diagnostic that the agent refused its explicit reset command.
    /// Delivery passes the visible rows drawn since the reset's Enter. On a match it also
    /// checks the pre-Enter screen to distinguish a newly drawn refusal from an old one.
    /// Match the agent's diagnostic in its UI context, accounting for wrapping, quoted
    /// replies and editable composer text. The default recognizes no refusal.
    ///
    /// A fresh match ends the reset with
    /// [`AbortReason::ResetRejected`](super::AbortReason::ResetRejected).
    /// Ordinary [`send`](super::AgentTerminal::send) calls skip this hook.
    fn reset_rejected(&self, _fresh_screen: &str) -> bool {
        false
    }

    /// Whether a question or choice on the current screen owns keyboard input. Delivery
    /// passes only the visible rows drawn since launch. Recognize the active dialog,
    /// accounting for old prompts or quoted instructions above a redrawn composer.
    ///
    /// A match stops the pending paste or Enter. After a paste, delivery rechecks the
    /// foreground owner before reporting
    /// [`AbortReason::DialogBlocking`](super::AbortReason::DialogBlocking).
    /// This hook defaults to `false`. See [`detect::dialog_at_bottom`]
    /// for matching a recorded layout. Reserve [`dialog`](Self::dialog) for startup
    /// prompts because the startup gate may independently steer prompts it recognizes.
    fn question_dialog(&self, _screen: &str) -> bool {
        false
    }

    /// Operation-scoped declarations and provenance, independent of runtime readiness.
    /// Defaults to unknown; overriding a built-in profile does not inherit its evidence.
    fn compatibility(&self) -> super::compatibility::Compatibility {
        super::compatibility::Compatibility::default()
    }

    /// Launcher names to look for on `PATH`, in priority order (an install probe tries
    /// each). Default: none.
    fn launchers(&self) -> &'static [&'static str] {
        &[]
    }

    /// The command that starts the agent in a shell running in `cwd`, for this approval
    /// posture and trust policy, quoted later for `shell` (some flags are left out for a
    /// shell kind they do not survive). Default: the first [`launchers`](Self::launchers)
    /// entry, else [`id`](Self::id), with only [`Permissions::Custom`]'s args and env.
    fn launch(
        &self,
        perms: &Permissions,
        trust: &TrustPolicy,
        cwd: &Path,
        shell: ShellKind,
    ) -> LaunchPlan {
        let _ = (trust, cwd, shell);
        let program = self.launchers().first().copied().unwrap_or(self.id());
        LaunchPlan::new(program).with_permissions(perms, &[], &[])
    }

    /// The command typed into the composer to start a fresh conversation (`/clear`,
    /// `/new`). Default: none.
    fn reset_command(&self) -> Option<&'static str> {
        None
    }

    /// The activity signals the agent emits. Default: nothing known.
    fn signals(&self) -> SignalHints {
        SignalHints::default()
    }

    /// A verified "is it signed in" command. Default: none (the answer is unknown).
    fn login_check(&self) -> Option<LoginCheck> {
        None
    }

    /// Convenience launch with default unattended permissions (YOLO or equivalent).
    /// `resume` and `probe` have the meanings in [`RpcLaunchOptions`].
    ///
    /// Calls [`rpc_launch_with_options`](Self::rpc_launch_with_options) and maps errors
    /// to `None`. Use that fallible method to configure permissions
    /// and retain the reason for refusal. Existing host profiles overriding this method
    /// retain their own behavior; implementing the options method is required to support
    /// explicit policy selection.
    fn rpc_launch(&self, resume: bool, probe: bool) -> Option<RpcLaunch> {
        self.rpc_launch_with_options(
            &RpcLaunchOptions::default()
                .with_resume(resume)
                .with_probe(probe),
        )
        .ok()
    }

    /// Build an inspectable stdio launch for this policy, before spawning any child.
    /// Default: [`Error::Unsupported`] (no policy-aware stdio launch). This also applies
    /// to host profiles that implement only the older [`rpc_launch`](Self::rpc_launch):
    /// the crate cannot safely remove their hardcoded permission settings.
    ///
    /// Do not implement this method by calling the default
    /// [`rpc_launch`](Self::rpc_launch): that default calls this method, so the two
    /// would recurse until the stack overflows. Build the [`RpcLaunch`] directly.
    ///
    /// Built-ins support unattended, native and custom launch settings. Cursor has no
    /// supported stdio preset and returns `Unsupported` for every policy. No profile
    /// method installs protocol approval callbacks; the RPC transport only carries lines.
    fn rpc_launch_with_options(&self, options: &RpcLaunchOptions) -> Result<RpcLaunch> {
        let _ = options;
        Err(Error::Unsupported(format!(
            "profile {} does not support policy-aware RPC launch",
            self.id()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed {
        dialog: bool,
        ready: bool,
    }

    impl AgentProfile for Fixed {
        fn id(&self) -> &str {
            "fixed"
        }
        fn dialog(&self, _: &str) -> Option<Dialog> {
            self.dialog.then_some(Dialog {
                kind: DialogKind::Trust,
                key: None,
            })
        }
        fn input_ready(&self, _: &str) -> bool {
            self.ready
        }
    }

    #[test]
    fn keys_write_enter_and_arrow_sequences() {
        assert_eq!(TrustKey::Enter.bytes(), b"\r");
        assert_eq!(TrustKey::Up.bytes(), b"\x1b[A");
        assert_eq!(TrustKey::Down.bytes(), b"\x1b[B");
    }

    #[test]
    fn the_launch_and_data_defaults_need_no_override() {
        let p = Fixed {
            dialog: false,
            ready: false,
        };
        let trust = TrustPolicy::default();
        let plan = p.launch(
            &Permissions::Unattended,
            &trust,
            Path::new("/x"),
            ShellKind::Posix,
        );
        assert_eq!(plan, LaunchPlan::new("fixed"));
        let custom = Permissions::Custom {
            args: vec!["--go".into()],
            env: vec![("K".into(), "v".into())],
        };
        let plan = p.launch(&custom, &trust, Path::new("/x"), ShellKind::Cmd);
        assert_eq!(plan.args, ["--go"]);
        assert_eq!(plan.env, [("K".into(), "v".into())]);
        assert!(!p.question_dialog("Choose an option\nEnter to confirm"));
        assert!(p.launchers().is_empty());
        assert_eq!(p.reset_command(), None);
        assert_eq!(p.signals(), SignalHints::default());
        assert_eq!(p.signals().sync_frames, Unlock::Unknown);
        assert_eq!(p.login_check(), None);
        assert_eq!(p.rpc_launch(true, true), None);
        assert!(matches!(
            p.rpc_launch_with_options(&RpcLaunchOptions::default()),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn rpc_launch_debug_never_prints_env_values() {
        let mut launch = RpcLaunch::new("x", &["--rpc"]);
        launch.env_defaults = vec![("K".into(), "default-secret".into())];
        launch.env = vec![("TOKEN".into(), "override-secret".into())];
        let shown = format!("{launch:?}");
        assert!(shown.contains("--rpc") && shown.contains('K'), "{shown}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("TOKEN"), "{shown}");
    }

    #[test]
    fn classify_puts_a_dialog_before_readiness() {
        let both = Fixed {
            dialog: true,
            ready: true,
        };
        assert!(matches!(both.classify(""), ScreenState::Dialog(_)));
        let ready = Fixed {
            dialog: false,
            ready: true,
        };
        assert_eq!(ready.classify(""), ScreenState::InputReady);
        let neither = Fixed {
            dialog: false,
            ready: false,
        };
        assert_eq!(neither.classify(""), ScreenState::Unknown);
    }
}
