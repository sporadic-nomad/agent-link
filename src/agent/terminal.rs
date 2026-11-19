//! [`AgentTerminal`]: a [`Terminal`] with an agent controller on top — launch, the startup
//! gate (dialog steering, readiness, the overlay flag), activity, state events, and
//! message delivery.

use std::fmt;
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, bounded};

use super::controller::{Job, MessageJob};
use super::delivery::Resolver;
use super::state::{AgentProgram, Config, Controller};
use super::{
    AbortReason, Activity, AgentLimits, AgentProfile, DeliveryOutcome, ForegroundPolicy,
    LaunchPlan, ModelChoice, Permissions, SendOptions, ShellFallback, ShellKind, Ticket, TicketId,
    Timing, TrustPolicy, controller, delivery, monitor,
};
use crate::error::Result;
use crate::{Event, SpawnMode, SpawnSpec, Terminal};

/// What an [`AgentTerminal`] runs and how its controller behaves.
#[derive(Clone)]
pub struct AgentSpec {
    /// In-crate tests can inject a synthetic transcript writer at launch.
    #[cfg(all(test, unix))]
    pub(super) turn_launch: Option<super::turns::CaptureLaunch>,
    /// The agent; `None` for a bare shell: no launch, no gate, no activity and no agent
    /// events — [`state`](AgentTerminal::state) stays [`AgentState::ShellOnly`] until the
    /// session ends ([`AgentState::Exited`]), [`activity`](AgentTerminal::activity)
    /// [`Activity::Unknown`].
    pub profile: Option<Arc<dyn AgentProfile>>,
    /// The approval posture of the launch line. Defaults to
    /// [`Permissions::Unattended`] (YOLO or the profile's equivalent).
    /// Explicit [`SpawnMode::Direct`] argv is supplied by the host and is not rewritten.
    pub permissions: Permissions,
    /// Pre-trust flags and dialog steering.
    pub trust: TrustPolicy,
    /// Optional model and reasoning effort for a profile-built shell launch. `None`
    /// (the default) uses the profile's ordinary launch. An explicit choice requires
    /// a profile and [`SpawnMode::Shell`]. See [`AgentProfile::model_args`].
    pub model: Option<ModelChoice>,
    /// Waits, windows and budgets.
    pub timing: Timing,
    /// Byte and job admission limits, including pending delivery events.
    pub limits: AgentLimits,
    /// Which foreground answers a delivery trusts (heuristic ones: Windows), and so what
    /// proves a strict delivery's agent ([`SendOptions::strict`]).
    pub foreground_policy: ForegroundPolicy,
    /// Controller trace lines (`[agent-link:<id>] …` at `info`): `None` turns them on when
    /// the `AGENT_LINK_TRACE` environment variable is `1` or in a debug build (read once at
    /// spawn).
    pub trace: Option<bool>,
}

#[cfg(feature = "turns")]
#[derive(Clone, Copy)]
enum Observation<'a> {
    Claude(&'a super::turns::ExclusiveClaudeRoot),
    Other(
        super::turns::TranscriptSource,
        &'a super::turns::IsolatedTranscriptRoot,
    ),
}

#[cfg(feature = "turns")]
impl<'a> Observation<'a> {
    fn root(self) -> &'a super::turns::ExclusiveClaudeRoot {
        match self {
            Self::Claude(root) => root,
            Self::Other(_, isolated) => &isolated.0,
        }
    }
}

impl AgentSpec {
    /// `profile` with the defaults: [`Permissions::Unattended`], [`TrustPolicy::default`],
    /// [`Timing::default`], [`ForegroundPolicy::ExactOnly`], trace by environment.
    pub fn new(profile: Arc<dyn AgentProfile>) -> Self {
        Self {
            profile: Some(profile),
            ..Self::default()
        }
    }

    /// Prepare an explicit selection before either tool rendering or child spawn.
    fn selected_launch(&self, spec: &SpawnSpec, shell: ShellKind) -> Result<Option<LaunchPlan>> {
        let Some(choice) = &self.model else {
            return Ok(None);
        };
        let profile = self.profile.as_deref().ok_or_else(|| {
            crate::Error::InvalidInput("model selection requires an agent profile".into())
        })?;
        if !matches!(spec.mode, SpawnMode::Shell(_)) {
            return Err(crate::Error::InvalidInput(
                "model selection requires a profile-built shell launch".into(),
            ));
        }
        choice.validate(shell)?;
        spec.input_limits.validate()?;
        let mut plan = profile.launch(&self.permissions, &self.trust, &spec.cwd, shell);
        let args = profile.model_args(choice, &plan.args)?;
        plan.append_model_args(args)?;
        Ok(Some(plan))
    }
}

impl Default for AgentSpec {
    /// A bare shell.
    fn default() -> Self {
        Self {
            #[cfg(all(test, unix))]
            turn_launch: None,
            profile: None,
            permissions: Permissions::default(),
            trust: TrustPolicy::default(),
            model: None,
            timing: Timing::default(),
            limits: AgentLimits::default(),
            foreground_policy: ForegroundPolicy::default(),
            trace: None,
        }
    }
}

impl fmt::Debug for AgentSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentSpec")
            .field("profile", &self.profile.as_ref().map(|p| p.id()))
            .field("permissions", &self.permissions)
            .field("trust", &self.trust)
            .field("has_model_choice", &self.model.is_some())
            .field("timing", &self.timing)
            .field("limits", &self.limits)
            .field("foreground_policy", &self.foreground_policy)
            .field("trace", &self.trace)
            .finish()
    }
}

/// Where the agent is, as [`Event::AgentState`] reports it with
/// [`blocking`](AgentTerminal::blocking).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AgentState {
    /// No agent runs: nothing launched yet (or no profile), or it left the foreground.
    ShellOnly,
    /// Launched; waiting for the startup to settle.
    Starting,
    /// A startup dialog is on screen: being answered, or left to the user (no key for it,
    /// steering off, its key budget used up, or the trust window over). Never `Ready` while
    /// it is there; one left to the user keeps this state — with `blocking` lifted once the
    /// startup settled, so the user can answer it — until it leaves the screen, or a waiting
    /// delivery finds the shell back in front (the agent exited from it): `ShellOnly`.
    TrustPrompt,
    /// Safe for programmatic input.
    Ready,
    /// The session ended.
    Exited,
}

/// A PTY session running an agent CLI, with a controller that watches its screen.
///
/// - [`spawn`](Self::spawn) starts the session (shell or direct child) with its screen
///   model on. A direct child with a profile *is* the agent: the startup gate is armed at
///   once. In a shell, [`launch`](Self::launch) types the profile's launch line.
/// - The startup gate answers recognised startup dialogs (bounded keys, a trust window),
///   decides when the agent is ready for programmatic input, and sets
///   [`blocking`](Self::blocking) while the host should mask the terminal.
/// - [`activity`](Self::activity) follows the agent's title spinner and progress reports
///   (informational only).
/// - Changes arrive as [`Event::AgentState`] and [`Event::Activity`] on every receiver
///   (also those from [`Terminal::subscribe`] / [`Terminal::attach`]), after every event the
///   controller had received when it changed — its own receiver is served last. The
///   screen may be ahead of the monitor; reactions wait for their source output before
///   publication. The starting values
///   are not sent — read them with [`state`](Self::state) and [`activity`](Self::activity).
///   Without a profile none are sent.
/// - [`send`](Self::send) delivers messages one at a time (see there), and so do
///   [`reset`](Self::reset) and [`reset_and_send`](Self::reset_and_send); each ends on its
///   [`Ticket`]. Admitted work normally also publishes [`Event::Delivery`]; immediate
///   admission refusals do not. A delivery with a handshake asks the host
///   first ([`Event::CommitPoint`], answered with [`confirm`](Self::confirm)).
/// - Two crate threads per terminal: `agent-link-monitor-<id>` and `agent-link-jobs-<id>`
///   (deliveries). Calls never wait on them or on the agent; publishing happens on the
///   monitor, so a full receiver stalls the controller (and the session), never the
///   caller.
///   The internal monitor queue retains at most 1,024 events, each with at most 8 KiB
///   of payload. Overflow logs an error and permanently disables automation for this
///   session; pending deliveries abort `NotConnected`, and new deliveries are refused
///   immediately without an event. Raw terminal events continue. A blocked host callback
///   can delay its thread and delivery events until it returns; tickets remain authoritative.
/// - Dropping it resolves every outstanding ticket as `Aborted(Disposed)`, stops event
///   publication and tears down the underlying [`Terminal`]. Final [`Event::Delivery`]
///   events are best effort: omitted if an earlier publication is in flight or a receiver
///   is full. Tickets remain authoritative, including when a running job is inside a host
///   callback. Teardown never waits for receiver capacity. Process cleanup still blocks
///   (see [`Terminal`]); use `kill_async` from async code. No crate thread is joined.
pub struct AgentTerminal {
    term: Terminal,
    pub(super) ctl: Arc<Controller>,
    #[cfg(all(unix, any(test, feature = "turns")))]
    turns: Option<super::turns::Slot>,
    /// Serializes ticket assignment through enqueue, without holding the state lock.
    pub(super) queueing: Mutex<()>,
    // Dropped after the terminal; never run filesystem cleanup under controller locks.
    #[cfg(feature = "tools")]
    _tools: Option<crate::tools::ToolAttachment>,
}

impl AgentTerminal {
    /// Preserve the resolved program through profile callbacks and PTY spawn so the
    /// same shell is used to prepare and execute the launch line.
    fn resolve_shell(spec: &mut SpawnSpec) -> ShellKind {
        let SpawnMode::Shell(shell) = &mut spec.mode else {
            return ShellKind::native();
        };
        let (program, _) = shell.resolve();
        let kind = if cfg!(unix) {
            ShellKind::for_unix_shell(&program)
        } else {
            ShellKind::native()
        };
        shell.program = Some(program);
        kind
    }

    /// Starts `spec` with an agent controller and returns the terminal with its first event
    /// receiver (PTY and agent events).
    ///
    /// The screen model is forced on. For a profile-built shell launch, the launch plan
    /// uses [`ShellKind::Fish`] for a Unix shell named `fish`, POSIX quoting for other
    /// Unix shells, and `cmd.exe` quoting on Windows. The plan's environment is merged
    /// into `spec.env` (a default only where the host policy does not set the key).
    /// The agent started inside a shell inherits that environment from the shell.
    ///
    /// # Errors
    ///
    /// Preparation can return these errors in addition to those from [`Terminal::spawn`].
    ///
    /// - [`Error::InvalidInput`](crate::Error::InvalidInput) for invalid admission limits
    ///   or an untypable line (an empty program, a control character, or a character
    ///   `cmd.exe` would expand). A [`ModelChoice`] requires a profile and a shell,
    ///   nonblank identifiers that do not start with `-`, and a nonempty mapping
    ///   compatible with the base arguments. Its full quoted command must fit the
    ///   platform's canonical input line or the cmd.exe command-line limit.
    /// - [`Error::Unsupported`](crate::Error::Unsupported) if the profile cannot map
    ///   the whole model and effort request.
    /// - [`Error::InputTooLarge`](crate::Error::InputTooLarge) if an explicit choice
    ///   makes the quoted launch write, including the terminator and clear-line keys,
    ///   exceed [`max_write_bytes`](crate::InputLimits::max_write_bytes).
    ///
    /// If a controller thread fails to start, the session is torn down and spawn
    /// returns [`Error::Spawn`](crate::Error::Spawn).
    pub fn spawn(
        mut spec: SpawnSpec,
        agent: AgentSpec,
    ) -> Result<(AgentTerminal, Receiver<Event>)> {
        agent.limits.validate()?;
        let shell = Self::resolve_shell(&mut spec);
        let plan = agent.selected_launch(&spec, shell)?;
        Self::spawn_prepared(
            spec,
            agent,
            shell,
            plan,
            #[cfg(feature = "turns")]
            None,
        )
    }

    /// Starts a directly executed Claude Code 2.1.296 PTY whose session writers the
    /// host controls through [super::turns::ExclusiveClaudeRoot]. The host supplies an
    /// absolute native executable named `claude`, the Claude profile, and exactly one
    /// fresh, canonical lowercase v4 UUID in its explicit `--session-id` argv.
    /// The configuration root in the merged PTY environment, including terminal
    /// identity settings and host overrides, must match the pinned lease. The child
    /// receives a `CLAUDE_CONFIG_DIR` path based on the leased directory's macOS
    /// volume and file ID. Immediately before spawn both this path and the lease
    /// pathname must still resolve to that directory. Preparation reserves the UUID.
    /// Peer readers treat the launch as a possible writer once process creation can begin.
    ///
    /// The admitted interactive options are `--model MODEL`, `--tools ""`,
    /// `--strict-mcp-config` and `--settings` containing just a boolean `showTurnDuration`.
    /// Startup performs local filesystem/process checks. Conflicting arguments, a
    /// preexisting transcript, a revoked or mismatched lease, and exhausted launch
    /// reservations return [crate::Error::InvalidInput] before spawn. Requires the
    /// `turns` feature and native macOS arm64. Other platforms and shell launches return
    /// [crate::Error::Unsupported]. Remaining errors follow [Self::spawn].
    /// Use [Self::attach_claude_turns] to start observing.
    #[cfg(feature = "turns")]
    pub fn spawn_with_claude_turns(
        mut spec: SpawnSpec,
        agent: AgentSpec,
        exclusive: &super::turns::ExclusiveClaudeRoot,
    ) -> Result<(AgentTerminal, Receiver<Event>)> {
        if !cfg!(all(target_os = "macos", target_arch = "aarch64"))
            || !matches!(spec.mode, SpawnMode::Direct { .. })
        {
            return Err(crate::Error::Unsupported(
                "Claude PTY turns require a native direct launch on macOS arm64".into(),
            ));
        }
        agent.limits.validate()?;
        let shell = Self::resolve_shell(&mut spec);
        let plan = agent.selected_launch(&spec, shell)?;
        Self::spawn_prepared(
            spec,
            agent,
            shell,
            plan,
            Some(Observation::Claude(exclusive)),
        )
    }

    /// Attaches at the verified EOF of the current session of a controlled Claude PTY.
    /// This starts a dedicated worker and performs no filesystem I/O on the caller.
    /// Poll [super::turns::TurnReceiver::status] for the baseline or a typed gap.
    /// A new prompt and an explicit end marker are required for a reply.
    ///
    /// Ordinary spawns, ended launches and unsupported platforms return
    /// [super::turns::AttachError::NoLaunch]. Invalid options or a revoked lease
    /// return a typed error. One worker may run per terminal, with four active or
    /// retiring workers across the process. After a gap, reattach when the worker
    /// has retired to start at a new EOF.
    #[cfg(feature = "turns")]
    pub fn attach_claude_turns(
        &self,
        options: super::turns::TurnOptions,
    ) -> std::result::Result<super::turns::TurnReceiver, super::turns::AttachError> {
        #[cfg(unix)]
        {
            super::profiles::turns::claude::ClaudeSource::attach(self, options.into())
        }
        #[cfg(not(unix))]
        {
            let _ = options;
            Err(super::turns::AttachError::NoLaunch)
        }
    }

    /// Launch a trusted direct Codex, Pi or Cursor interactive CLI in a transcript
    /// namespace dedicated to this PTY. Requires `turns` on native macOS arm64.
    /// The matching profile, an absolute program and a leased directory are required.
    /// Admission validates bounded interactive argv, installs the provider's root
    /// environment variable and refuses pre-existing transcripts and competing launches.
    /// Pi and Cursor take the absolute Node executable and their packaged CLI script.
    /// Explicitly removed or conflicting provider-root environment settings are refused.
    /// Configure model access separately. See the host guide for the accepted options
    /// and the host's ongoing isolation responsibilities.
    #[cfg(feature = "turns")]
    pub fn spawn_with_transcript_turns(
        mut spec: SpawnSpec,
        agent: AgentSpec,
        source: super::turns::TranscriptSource,
        root: &super::turns::IsolatedTranscriptRoot,
    ) -> Result<(AgentTerminal, Receiver<Event>)> {
        if !cfg!(all(target_os = "macos", target_arch = "aarch64"))
            || !matches!(spec.mode, SpawnMode::Direct { .. })
        {
            return Err(crate::Error::Unsupported(
                "isolated transcript turns require a direct macOS arm64 launch".into(),
            ));
        }
        agent.limits.validate()?;
        let shell = Self::resolve_shell(&mut spec);
        let plan = agent.selected_launch(&spec, shell)?;
        Self::spawn_prepared(
            spec,
            agent,
            shell,
            plan,
            Some(Observation::Other(source, root)),
        )
    }

    /// Observe new turns beginning after verified EOF. Each nonempty pull rechecks
    /// file, process and writer isolation. Ambiguity, divergent history, or session change
    /// discards pending data and reports a persistent gap. Returns NoLaunch on an
    /// ordinary or Claude-only terminal. One active or retiring worker per terminal.
    #[cfg(feature = "turns")]
    pub fn attach_transcript_turns(
        &self,
        options: super::turns::TurnOptions,
    ) -> std::result::Result<super::turns::TranscriptReceiver, super::turns::AttachError> {
        #[cfg(unix)]
        {
            super::profiles::turns::isolated::attach(self, options)
        }
        #[cfg(not(unix))]
        {
            let _ = options;
            Err(super::turns::AttachError::NoLaunch)
        }
    }

    /// Starts a PTY agent with consumer-owned CLI/MCP tools scoped to this launch.
    /// Preserves the profile's permissions, trust and screen handling. Preparation
    /// validates descriptors and may write private temporary configuration files;
    /// it does not start a server or contact an agent. Unsupported MCP mappings fail
    /// before spawn. In Direct mode tool arguments are appended to the host's explicit
    /// argv; profile permission/trust arguments are still not added, matching ordinary
    /// Direct launches. Tool-configured Direct argv must be UTF-8. Unix login-shell
    /// startup files may change the child environment in Shell mode.
    ///
    /// For an explicit [`ModelChoice`], the profile arguments come first, then the
    /// model arguments, then any tool arguments. The selected plan must fit both
    /// the shell line limit and the PTY write budget before attachment preparation
    /// and again with the tool arguments included.
    /// Model selection errors are the same as for [`spawn`](Self::spawn).
    #[cfg(feature = "tools")]
    pub fn spawn_with_tools(
        mut spec: SpawnSpec,
        agent: AgentSpec,
        tools: &crate::tools::AgentTools,
    ) -> Result<(AgentTerminal, Receiver<Event>)> {
        let profile = agent
            .profile
            .as_ref()
            .ok_or_else(|| crate::Error::InvalidInput("tools require an agent profile".into()))?;
        agent.limits.validate()?;
        let shell = Self::resolve_shell(&mut spec);
        let plan = if let Some(plan) = agent.selected_launch(&spec, shell)? {
            plan.launch_line(shell, Some(spec.input_limits.max_write_bytes))?;
            tools.configure_pty(profile.as_ref(), plan)?
        } else if let SpawnMode::Direct { program, args } = &spec.mode {
            let mut launch = LaunchPlan::new(program.to_str().ok_or_else(|| {
                crate::Error::InvalidInput("tool-configured executable must be UTF-8".into())
            })?);
            launch.args = args
                .iter()
                .map(|arg| {
                    arg.to_str().map(str::to_owned).ok_or_else(|| {
                        crate::Error::InvalidInput("tool-configured arguments must be UTF-8".into())
                    })
                })
                .collect::<Result<_>>()?;
            // Ordinary Direct launches still inherit the profile's launch environment.
            let defaults = profile.launch(&agent.permissions, &agent.trust, &spec.cwd, shell);
            if defaults.tools.is_some() {
                return Err(crate::Error::InvalidInput(
                    "profile already has tool attachments".into(),
                ));
            }
            launch.env = defaults.env;
            launch.env_defaults = defaults.env_defaults;
            tools.configure_pty(profile.as_ref(), launch)?
        } else {
            tools.pty_launch(
                profile.as_ref(),
                &agent.permissions,
                &agent.trust,
                &spec.cwd,
                shell,
            )?
        };
        Self::spawn_prepared(
            spec,
            agent,
            shell,
            Some(plan),
            #[cfg(feature = "turns")]
            None,
        )
    }

    fn spawn_prepared(
        mut spec: SpawnSpec,
        agent: AgentSpec,
        shell: ShellKind,
        prepared: Option<LaunchPlan>,
        #[cfg(feature = "turns")] exclusive: Option<Observation<'_>>,
    ) -> Result<(AgentTerminal, Receiver<Event>)> {
        spec.screen.enabled = true;
        let direct = matches!(spec.mode, SpawnMode::Direct { .. });
        let mut launch_line = None;
        #[cfg(all(unix, any(test, feature = "turns")))]
        let mut observer_plan = None;
        #[cfg(feature = "tools")]
        let mut tool_attachment = None;
        if let Some(profile) = &agent.profile {
            if direct {
                if let Some(plan) = &prepared {
                    spec.mode = SpawnMode::Direct {
                        program: plan.program.clone().into(),
                        args: plan.args.iter().map(Into::into).collect(),
                    };
                }
            }
            let plan = prepared.unwrap_or_else(|| {
                profile.launch(&agent.permissions, &agent.trust, &spec.cwd, shell)
            });
            plan.merge_env_into(&mut spec.env);
            #[cfg(feature = "tools")]
            {
                tool_attachment = plan.tools.clone();
            }
            if !direct {
                let limit = agent
                    .model
                    .as_ref()
                    .map(|_| spec.input_limits.max_write_bytes);
                launch_line = Some(plan.launch_line(shell, limit)?);
            }
            #[cfg(all(unix, any(test, feature = "turns")))]
            {
                observer_plan = Some(plan);
            }
        }
        let trace = agent.trace.unwrap_or_else(|| {
            cfg!(debug_assertions) || std::env::var("AGENT_LINK_TRACE").is_ok_and(|v| v == "1")
        });
        let identity = spec.identity.clone();
        let reset_command = agent.profile.as_ref().and_then(|p| p.reset_command());
        // All profile metadata is collected before a child exists: a blocking host
        // callback must not strand a newly spawned process without a returned handle.
        let profile_id = agent
            .profile
            .as_ref()
            .map_or_else(String::new, |p| p.id().to_string());
        #[cfg(feature = "turns")]
        if exclusive.is_some_and(|observation| match observation {
            Observation::Claude(_) => profile_id != "claude",
            Observation::Other(source, _) => profile_id != source.profile(),
        }) {
            return Err(crate::Error::InvalidInput(
                "controlled transcript source requires its matching profile".into(),
            ));
        }
        #[cfg(all(unix, any(test, feature = "turns")))]
        let mut turn_claim = None;
        #[cfg(all(unix, any(test, feature = "turns")))]
        if let Some(plan) = observer_plan {
            #[cfg(feature = "turns")]
            if let Some(observation) = exclusive {
                turn_claim = Some(match observation {
                    Observation::Claude(root) => {
                        super::profiles::turns::claude::capture_exclusive(&mut spec, &plan, root)?
                    }
                    Observation::Other(source, root) => {
                        super::profiles::turns::isolated::capture(&mut spec, source, root)?
                    }
                });
            }
            #[cfg(test)]
            if let Some(capture) = agent.turn_launch.as_ref().filter(|_| turn_claim.is_none()) {
                turn_claim = Some(capture(&mut spec, &plan)?);
            }
        }
        #[cfg(feature = "turns")]
        #[cfg(unix)]
        let reserved_claim = turn_claim.as_ref();
        #[cfg(feature = "turns")]
        let admit = exclusive.map(|observation| {
            move || {
                let guard = observation.root().admit_spawn()?;
                #[cfg(unix)]
                if let Some(claim) = reserved_claim {
                    if claim.source.is_some() {
                        super::profiles::turns::isolated::check_empty(claim)?;
                    }
                }
                Ok(guard)
            }
        });
        #[cfg(not(feature = "turns"))]
        let admit = None::<fn() -> Result<()>>;
        let (term, events, watch) = Terminal::spawn_watched(spec, admit, || {
            #[cfg(all(unix, any(test, feature = "turns")))]
            if let Some(writer) = turn_claim.as_ref().and_then(|claim| claim.writer.as_ref()) {
                writer.begin_spawn();
            }
        })?;
        // Wake notifications coalesce; the monitor always reads the current state.
        let (monitor_tx, monitor_rx) = bounded(1);
        let cfg = Config {
            id: term.id(),
            profile_id,
            profile: agent.profile,
            launch_line,
            has_model_choice: agent.model.is_some(),
            reset_command,
            shell,
            trust: agent.trust,
            timing: agent.timing.clamped(),
            limits: agent.limits,
            foreground_policy: agent.foreground_policy,
            trace,
            identity,
        };
        let has_profile = cfg.profile.is_some();
        let launches = cfg.launch_line.is_some();
        let ctl = Arc::new(Controller::new(cfg, term.handle(), monitor_tx));
        #[cfg(all(unix, any(test, feature = "turns")))]
        let turns = turn_claim.map(|claim| {
            if let Some(writer) = claim.writer.as_ref().filter(|_| direct) {
                writer.spawned(&term);
            }
            super::turns::Slot::new(claim, &ctl)
        });
        if direct && has_profile {
            // The child is the agent: armed from its first output on.
            ctl.update_quietly(|st, now| {
                st.trace("direct agent: startup gate armed");
                st.program = AgentProgram::Session;
                st.launch(now);
            });
        }
        if launches {
            // The agent is told from a program the user starts by the lines they submit.
            ctl.term().count_shell_lines();
        }
        let agent_term = AgentTerminal {
            term,
            ctl,
            #[cfg(all(unix, any(test, feature = "turns")))]
            turns,
            queueing: Mutex::new(()),
            #[cfg(feature = "tools")]
            _tools: tool_attachment,
        };
        // On an error `agent_term` drops: the controller is disposed, the session torn down.
        drop(monitor::spawn(
            Arc::clone(&agent_term.ctl),
            watch,
            monitor_rx,
        )?);
        drop(controller::spawn(Arc::clone(&agent_term.ctl))?);
        Ok((agent_term, events))
    }

    /// Starts the agent in the session's shell: arms the startup gate (the state is
    /// [`AgentState::Starting`] when this returns) and types the launch line with the
    /// shell's line terminator (`\n`, or `\r` for `cmd.exe`, where a bare LF does not
    /// complete a line) — in a POSIX shell after the keys that kill its input line
    /// ([`ShellKind::clear_line`]), so a single line left there (an unsubmitted paste, a
    /// draft) never runs with it. Whatever is in front of the terminal gets the line — call
    /// it once per shell, not while an agent runs.
    ///
    /// With the shell in front (the foreground, read now) a Ctrl-C goes first — once the
    /// crate has typed anything into this session (a launch line, a paste; before that the
    /// shell may still be reading its startup files, which a SIGINT would cut short): one
    /// line kill leaves the earlier lines of a multi-line buffer in zsh, which would run
    /// with the launch line, and the terminal flushes input the shell has not read yet. A
    /// draft the user left on the shell's line is discarded and `$?` changes (130 in zsh
    /// and bash 5). The line then follows once the shell took the Ctrl-C (its answer and
    /// 150 ms of quiet, at most 1 s: a shell handling an interrupt can drop input that
    /// arrives meanwhile) — typed by a crate thread, after this call returned. Right after
    /// a Ctrl-C of the crate's own that the shell is still handling (a delivery that found
    /// the shell before its Enter) the new one waits until that one settled: two in one
    /// handling made bash 3.2 drop the line's first character. The same goes for every
    /// launch line the crate types for a delivery. The startup it armed restarts when the
    /// held line is typed. What is held back (the line; that later Ctrl-C) is written only
    /// if the shell still owns the terminal then and the host submitted no line to it
    /// since; otherwise the launch is dropped — nothing more typed, state
    /// [`AgentState::ShellOnly`] — so a program the user started meanwhile gets neither a
    /// SIGINT nor the launch line.
    ///
    /// The screen the line is typed over is remembered: the startup gate (and a delivery's
    /// dialog and lull checks) read only the rows drawn since, so the last agent's frame
    /// left there — ready composer, a dialog — never reads as this one's. With the shell in
    /// front the agent's program is expected too: the first program other than the shell
    /// seen owning the terminal afterwards (unix: its process group) is the one deliveries
    /// type into — unless a line the host submitted to the shell came first
    /// ([`send`](Self::send) explains why).
    ///
    /// No-op without a profile, and for a direct child (it already is the agent).
    /// [`Error::Closed`](crate::Error::Closed) once the session ended; a write error (the
    /// Ctrl-C's, when the line is held back).
    pub fn launch(&self) -> Result<()> {
        self.ctl.launch()
    }

    /// Attempts to queue `text` and returns its [`Ticket`] at once. Admitted deliveries
    /// normally also publish [`Event::Delivery`] (without a profile too); immediate
    /// admission refusals do not. Deliveries run one at a time in ticket order, each to
    /// its end, so every message owns its Enter.
    ///
    /// Admission checks raw text size before copying or sanitizing it and reserves bytes
    /// and a job slot before any launch. Limits include running jobs and delivery events
    /// still waiting to publish. [`AbortReason::InputTooLarge`] and
    /// [`AbortReason::QueueFull`] resolve immediately **without an `Event::Delivery`**;
    /// nothing is queued or launched. Drain event receivers to release capacity.
    /// A closed session or failed monitor similarly refuses new work as
    /// [`AbortReason::NotConnected`] without queueing an event.
    ///
    /// A delivery waits until the agent is ready (a startup dialog left to the user keeps it
    /// waiting — until the agent leaves: the shell back in front ends the wait, though the
    /// dialog may stay on screen; readiness is read only from what the agent drew since its
    /// launch line), then checks that the agent, not the shell, owns the terminal — every send,
    /// a paste-only one too, before the lull, again right before the paste and right
    /// before every Enter. With the shell in front before the paste it launches the agent
    /// once more and starts over; if the shell is still there it takes the shell fallback
    /// (`opts.on_shell`). It waits for a lull in the output, checks the screen for a
    /// dialog, pastes the text as one bracketed paste (no newline), and for a submit writes the
    /// Enter only after observing the pasted text. Fresh, stable evidence that the text
    /// left a readable composer verifies submission. Silence, unrelated output and an
    /// unreadable composer end
    /// [`AbortReason::SubmitUnverified`](super::AbortReason::SubmitUnverified), possibly
    /// before Enter. Retries are disabled by default; opting into them can submit twice
    /// if the agent accepts input before redrawing its composer. An unverified outcome
    /// must not be automatically resent. An exact reset command gets the longer reset
    /// verification window and at most one Enter. Only [`reset`](Self::reset) and
    /// [`reset_and_send`](Self::reset_and_send) check the agent's response for refusal.
    ///
    /// A question recognized by [`AgentProfile::question_dialog`] at the commit point
    /// stops even a paste-only send with [`AbortReason::DialogBlocking`]. A match after
    /// the paste stops the next Enter and rechecks foreground ownership. The agent in
    /// front gives `DialogBlocking`. A shell that took back the terminal gets Ctrl-C to
    /// clear the inherited draft and the ticket reports [`AbortReason::ShellOnly`].
    /// Other or unknown owners give [`AbortReason::ForegroundUnproven`]. Inspect the
    /// session before resending because the paste and an earlier Enter attempt may have
    /// reached the agent. The screen and foreground can change between a check and a write.
    ///
    /// **The text never runs as a shell command** where the crate can tell the shell owns
    /// the terminal (an exact foreground answer, or a heuristic one under
    /// [`ForegroundPolicy::AcceptHeuristic`]): the shell fallback leaves only a single line,
    /// without Enter
    /// ([`DeliveryOutcome::PastedUnsubmitted`](super::DeliveryOutcome::PastedUnsubmitted)),
    /// on a fresh input line (a Ctrl-C first, once the crate typed into the session: a
    /// draft or the last such line is discarded, a line editor in vi mode starts it in
    /// insert mode) — as a bracketed paste where the shell turned that on after the Ctrl-C,
    /// else typed as is (bash 3.2 reads the paste marker's ESC as a key) — and every launch
    /// line the crate types later clears that line first (a Ctrl-C, see
    /// [`launch`](Self::launch), then the line kill); a multi-line text, a `cmd.exe` line
    /// with a launch line still to come, and
    /// [`ShellFallback::Fail`](super::ShellFallback::Fail) end
    /// [`AbortReason::ShellOnly`](super::AbortReason::ShellOnly) with nothing typed. The
    /// shell in front right before an Enter (the agent left after the paste) gets no Enter:
    /// its input line is interrupted (Ctrl-C, discarding a paste that landed there) and the
    /// delivery ends `ShellOnly`; so does a multi-line paste without submit that the shell
    /// holds once it settled (read then: an agent leaves only once the input is there).
    ///
    /// **Nor is it typed into another program**: a program other than the shell in front
    /// gets the text — and an Enter — only when it is the agent's own: the one a launch
    /// line this terminal typed into the shell started ([`launch`](Self::launch); unix: its
    /// process group, Windows under [`ForegroundPolicy::AcceptHeuristic`]: the shell's one
    /// child). Anything else — a nested shell, `ssh`, `nix develop`, an editor, `cat`,
    /// started after the agent exited (noticed or not) or after a launch that started
    /// nothing, or an agent the user started by hand — gets nothing, no launch line either:
    /// [`AbortReason::ForegroundUnproven`](super::AbortReason::ForegroundUnproven). The
    /// screen never decides it: an inline agent leaves its last frame, ready composer
    /// included, under whatever the user starts next, where a submit would run the text
    /// (on a remote host, for `ssh`). Telling the agent from a program the user started
    /// relies on [`Terminal::write`]: a line the host submits to the shell after the
    /// launch line, before the agent was seen, leaves the agent unknown until the next
    /// launch.
    ///
    /// What stays open: the instant between each foreground read and its write; on unix
    /// the moment between an agent's exit and its shell taking the terminal back (the dead
    /// job's group still reads as the agent); the agent's own program not reading its
    /// input (hung) — the terminal queues the text for whoever reads next, the shell
    /// included; a paste-only single line the agent left unread (`Delivered`; it sits
    /// inert on the shell's line). An owner the policy cannot name (Windows under
    /// `ExactOnly`, or a Windows shell running several programs) is refused with
    /// `ForegroundUnproven`, even after a launch armed the gate.
    ///
    /// With a profile and no agent running in the shell, the agent is launched first —
    /// when the shell is in front (this call reads the foreground then; a program the
    /// policy tells from the shell gets no launch line and the send ends
    /// `ForegroundUnproven`, see above; unknown ownership also gets no launch line). A bare
    /// shell (no profile) gets the same pipeline: the shell is in front, so a one-line send
    /// ends as `PastedUnsubmitted`, a multi-line one as `ShellOnly` — another program in
    /// front, `ForegroundUnproven`.
    ///
    /// The text is pasted with control characters dropped (LF and tab kept, CR turned into
    /// LF; the shell fallback's single line also turns tabs into spaces): an embedded paste
    /// end marker or a ^C would otherwise act as keys. A text with nothing left to paste
    /// (empty, or only whitespace and control characters) ends
    /// [`AbortReason::EmptyText`](super::AbortReason::EmptyText) with nothing typed and no
    /// launch: an Enter alone would submit the user's draft.
    ///
    /// **Strict** ([`SendOptions::strict`]): every irreversible step needs proof that the
    /// agent's own program owns the terminal — a foreground answer the
    /// [`ForegroundPolicy`] trusts (`ExactOnly`: unix; `AcceptHeuristic`: Windows too)
    /// naming it — read before the lull, at the commit point, right before the paste,
    /// before every Enter and after the verified submit. The shell in front ends it
    /// `ShellOnly` (before the lull only after the one relaunch; never an inert paste)
    /// and marks the agent gone; anything else ends it
    /// [`AbortReason::ForegroundUnproven`](super::AbortReason::ForegroundUnproven).
    /// Non-strict sends also refuse unknown ownership. **Revalidation**
    /// ([`SendOptions::revalidate`]): asked at the commit point, before the strict proof and
    /// the dialog check (and before an inert paste into the shell); `Handshake` publishes
    /// [`Event::CommitPoint`] and waits for [`confirm`](Self::confirm).
    ///
    /// A receiver of this terminal's events that is not drained stalls the session and the
    /// running delivery with it (back-pressure): its ticket resolves once the receiver has
    /// room.
    pub fn send(&self, text: &str, opts: SendOptions) -> Ticket {
        self.queue(text.len(), text.len(), |ticket, resolver| {
            let job = MessageJob::new(ticket, text, opts);
            let launch = job.refusal().is_none();
            (Job::Message { job, resolver }, launch)
        })
    }

    /// Requests a fresh conversation with the profile's reset command. A stopped agent
    /// is launched first. The default [`ShellFallback::PasteInert`] can leave a single
    /// editable line on the shell without Enter. Without a reset command the ticket ends
    /// [`AbortReason::Unsupported`] in queue order, with nothing typed or launched.
    ///
    /// After observing the command in the composer, the controller writes Enter once.
    /// It checks for a fresh refusal while verifying submission and after settling fresh
    /// output for up to [`Timing::startup_max`]. A recognized refusal yields
    /// [`AbortReason::ResetRejected`]. An ambiguous notice or an unavailable check yields
    /// [`AbortReason::SubmitUnverified`], including after Enter. A notice that scrolls
    /// away between observations can be missed. `Delivered` verifies submission only.
    pub fn reset(&self) -> Ticket {
        let Some(command) = self.ctl.cfg.reset_command else {
            return self.refuse(AbortReason::Unsupported);
        };
        self.queue(command.len(), command.len(), |ticket, resolver| {
            let job = MessageJob::reset(ticket, command, ShellFallback::PasteInert);
            let launch = job.refusal().is_none();
            (Job::Message { job, resolver }, launch)
        })
    }

    /// A reset, then `text`, as **one** delivery (one ticket and one [`Event::Delivery`]).
    /// 1. The reset follows [`reset`](Self::reset) with [`ShellFallback::Fail`] and no
    ///    strict proof. A failed reset ends the delivery before the message is pasted.
    /// 2. After submission, the reset checks its response across the fresh-output settle.
    ///    The follow-up checks again at its commit point, after any host revalidation.
    /// 3. The follow-up submits `text` with `Fail` on the shell and the caller's
    ///    `strict` and `revalidate` settings. Only the follow-up carries revalidation.
    ///
    /// Both halves check for questions. If the reset ends with [`AbortReason::DialogBlocking`],
    /// the composite ticket reports that reason and the message is skipped.
    /// A stopped agent is launched first (as for `send`). Ends
    /// [`AbortReason::Unsupported`](super::AbortReason::Unsupported) without a reset
    /// command and [`AbortReason::EmptyText`](super::AbortReason::EmptyText) for a text with
    /// nothing to paste — in its turn, nothing typed (no reset either), no launch.
    pub fn reset_and_send(&self, text: &str, opts: SendOptions) -> Ticket {
        let Some(command) = self.ctl.cfg.reset_command else {
            return self.refuse(AbortReason::Unsupported);
        };
        self.queue(
            text.len().saturating_add(command.len()),
            text.len().max(command.len()),
            |ticket, resolver| {
                let message = MessageJob::new(
                    ticket,
                    text,
                    SendOptions {
                        submit: true,
                        strict: opts.strict,
                        on_shell: ShellFallback::Fail,
                        revalidate: opts.revalidate,
                    },
                );
                if message.refusal().is_some() {
                    return (
                        Job::Message {
                            job: message,
                            resolver,
                        },
                        false,
                    );
                }
                let reset = MessageJob::reset(ticket, command, ShellFallback::Fail);
                let launch = reset.refusal().is_none();
                (
                    Job::ResetAndSend {
                        reset,
                        message,
                        resolver,
                    },
                    launch,
                )
            },
        )
    }

    /// The host's answer to [`Event::CommitPoint`] for `ticket` (a delivery sent with
    /// [`Revalidate::Handshake`](super::Revalidate::Handshake)): `proceed` pastes, `false` ends the delivery
    /// [`AbortReason::Revalidate`](super::AbortReason::Revalidate). Ignored (a `debug`
    /// line) when that delivery does not await an answer — an unknown or finished ticket,
    /// an answer after the timeout, a second answer. Never waits.
    pub fn confirm(&self, ticket: TicketId, proceed: bool) {
        self.ctl.confirm(ticket, proceed);
    }

    /// Writes Ctrl-C (`\x03`): the terminal turns it into `SIGINT` for whatever is in
    /// front — the agent (most interrupt their turn; some exit), or the shell. Like the
    /// crate's own Ctrl-C, a launch line typed right after it (a relaunch, a send's launch)
    /// waits until the shell took it. Changes no state: a delivery finds out who is in front
    /// by reading the foreground. [`Error::Closed`](crate::Error::Closed) once the session
    /// is gone.
    pub fn interrupt(&self) -> Result<()> {
        self.ctl.host_interrupt()
    }

    /// Reserve before cloning/sanitizing text or launching the agent. A rejected ticket
    /// has no event, so rejected calls cannot grow the monitor's outbox.
    fn queue(
        &self,
        bytes: usize,
        largest: usize,
        build: impl FnOnce(TicketId, Resolver) -> (Job, bool),
    ) -> Ticket {
        let _queueing = self.queueing.lock().unwrap_or_else(|e| e.into_inner());
        let (id, refusal) = self.ctl.reserve_delivery(bytes, largest);
        let (ticket, resolver) = delivery::ticket(id);
        if let Some(reason) = refusal {
            resolver.resolve(DeliveryOutcome::Aborted(reason));
            return ticket;
        }
        let mut reservation = Admission {
            ctl: &self.ctl,
            id,
            committed: false,
        };
        if !self.ctl.track_admission(id, resolver.resolution()) {
            resolver.resolve(DeliveryOutcome::Aborted(AbortReason::Disposed));
            return ticket;
        }
        let (job, launch) = build(id, resolver);
        if launch {
            self.ctl.launch_if_stopped();
        }
        let mut job = Some(job);
        self.ctl.update(|st, _| {
            if !st.disposed {
                st.jobs.extend(job.take());
                reservation.committed = true;
            }
        });
        // Host callbacks in a refused job are dropped outside the controller lock.
        drop(job);
        ticket
    }

    /// A bounded delivery that ends with `reason` in its turn, nothing typed.
    fn refuse(&self, reason: AbortReason) -> Ticket {
        self.queue(0, 0, |ticket, resolver| {
            (
                Job::Message {
                    job: MessageJob::refused(ticket, reason),
                    resolver,
                },
                false,
            )
        })
    }

    /// The agent's state now.
    pub fn state(&self) -> AgentState {
        self.ctl.read(|st| st.agent_state())
    }

    /// Whether the host should mask the terminal (a loading overlay): set while the agent
    /// starts, lifted when its startup first settles, set again while a dialog is steered.
    /// Lifted after monitor failure, queue overflow, or session teardown.
    pub fn blocking(&self) -> bool {
        self.ctl.read(|st| st.blocking())
    }

    /// The agent's activity now.
    pub fn activity(&self) -> Activity {
        self.ctl.read(|st| st.activity.state())
    }

    /// [`state`](Self::state), [`blocking`](Self::blocking) and
    /// [`activity`](Self::activity) read together under one lock as a consistent snapshot.
    /// This call does not synchronize with event receivers: events received afterward
    /// may describe older transitions that were queued or awaiting publication.
    ///
    /// To display current status, subscribe before taking the initial snapshot, then use
    /// state and activity events as prompts to read `status()` again instead of applying
    /// their potentially older values over the snapshot.
    pub fn status(&self) -> (AgentState, bool, Activity) {
        self.ctl
            .read(|st| (st.agent_state(), st.blocking(), st.activity.state()))
    }

    /// The underlying session (write, resize, foreground, screen, subscribe, …).
    pub fn terminal(&self) -> &Terminal {
        &self.term
    }

    #[cfg(all(unix, any(test, feature = "turns")))]
    pub(super) fn turn_claim(&self) -> Option<&super::turns::Claim> {
        self.turns.as_ref().map(|slot| slot.claim.as_ref())
    }

    #[cfg(all(unix, any(test, feature = "turns")))]
    pub(super) fn attach_turns(
        &self,
        source: impl super::turns::Source,
        parser: impl super::turns::Parser,
        options: super::turns::Options,
    ) -> std::result::Result<super::turns::TurnReceiver, super::turns::AttachError> {
        self.turns
            .as_ref()
            .ok_or(super::turns::AttachError::NoLaunch)?
            .attach(&self.ctl, source, parser, options)
    }
}

impl Drop for AgentTerminal {
    fn drop(&mut self) {
        // Resolve tickets and close event publication without waiting for consumers;
        // then `term` drops and kills/reaps the session.
        self.ctl.dispose();
    }
}

impl fmt::Debug for AgentTerminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentTerminal")
            .field("term", &self.term)
            .field("profile", &self.ctl.cfg.profile_id)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

/// Releases capacity if job construction or launch unwinds before enqueueing it.
struct Admission<'a> {
    ctl: &'a Controller,
    id: TicketId,
    committed: bool,
}

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.ctl.cancel_admission(self.id);
        }
    }
}

#[cfg(all(unix, test))]
mod limit_tests {
    use super::*;
    use crate::agent::Dialog;
    use crate::{Error, PtySize};

    struct Profile;
    impl AgentProfile for Profile {
        fn id(&self) -> &str {
            "agent-link-limit-test"
        }
        fn dialog(&self, _: &str) -> Option<Dialog> {
            None
        }
        fn input_ready(&self, _: &str) -> bool {
            false
        }
        fn reset_command(&self) -> Option<&'static str> {
            Some("/new")
        }
    }

    // No controller threads: jobs remain queued until the test disposes the terminal.
    fn paused(limits: AgentLimits) -> (AgentTerminal, Receiver<Event>) {
        let _lock = crate::pty::test_spawn_lock();
        let (term, events) = Terminal::spawn(SpawnSpec::direct(
            "/bin/sleep",
            ["60"],
            std::env::temp_dir(),
            PtySize { rows: 24, cols: 80 },
        ))
        .unwrap();
        let (tx, _rx) = bounded(1);
        let cfg = Config {
            id: term.id(),
            profile: Some(Arc::new(Profile)),
            profile_id: "limit-test".into(),
            launch_line: Some("echo should-not-launch\n".into()),
            has_model_choice: false,
            reset_command: Some("/new"),
            shell: ShellKind::Posix,
            trust: TrustPolicy::default(),
            timing: Timing::default(),
            limits,
            foreground_policy: ForegroundPolicy::default(),
            trace: false,
            identity: crate::TerminalIdentity::default(),
        };
        let ctl = Arc::new(Controller::new(cfg, term.handle(), tx));
        (
            AgentTerminal {
                term,
                ctl,
                turns: None,
                queueing: Mutex::new(()),
                #[cfg(feature = "tools")]
                _tools: None,
            },
            events,
        )
    }

    #[test]
    fn raw_size_refusal_precedes_sanitizing_and_launch_and_has_no_event() {
        let (term, events) = paused(AgentLimits {
            max_message_bytes: 3,
            max_pending_bytes: 8,
            max_pending_jobs: 2,
        });
        // Sanitizing the send first would make it empty. The profile's /new command
        // exceeds the per-message cap on its own, including as the first leg.
        for ticket in [
            term.send("\x03\x03\x03\x03\x03", SendOptions::default()),
            term.reset(),
            term.reset_and_send("x", SendOptions::default()),
        ] {
            assert_eq!(
                ticket.try_outcome(),
                Some(DeliveryOutcome::Aborted(AbortReason::InputTooLarge))
            );
        }
        assert_eq!(term.state(), AgentState::ShellOnly);
        assert!(term.ctl.read(|st| st.jobs.is_empty()));
        assert!(events.is_empty());
    }

    #[test]
    fn composite_bytes_and_job_count_are_reserved_before_queueing() {
        let (term, _events) = paused(AgentLimits {
            max_message_bytes: 4,
            max_pending_bytes: 8,
            max_pending_jobs: 2,
        });
        let accepted = term.reset_and_send("text", SendOptions::default());
        assert!(accepted.try_outcome().is_none());
        assert_eq!(term.ctl.read(|st| st.jobs.len()), 1);
        for _ in 0..10_000 {
            let rejected = term.send("x", SendOptions::default());
            assert_eq!(
                rejected.try_outcome(),
                Some(DeliveryOutcome::Aborted(AbortReason::QueueFull))
            );
        }
        // Empty jobs consume a slot too, despite having no bytes.
        let empty = term.send("", SendOptions::default());
        assert!(empty.try_outcome().is_none());
        assert_eq!(
            term.send("", SendOptions::default()).try_outcome(),
            Some(DeliveryOutcome::Aborted(AbortReason::QueueFull))
        );
        assert_eq!(term.ctl.read(|st| st.jobs.len()), 2);
        drop(term);
        assert_eq!(
            accepted.try_outcome(),
            Some(DeliveryOutcome::Aborted(AbortReason::Disposed))
        );
        assert_eq!(
            empty.try_outcome(),
            Some(DeliveryOutcome::Aborted(AbortReason::Disposed))
        );
    }

    #[test]
    fn concurrent_sends_keep_ticket_and_queue_order_through_construction() {
        let (term, _events) = paused(AgentLimits::default());
        let (entered_tx, entered_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let (second_tx, second_rx) = bounded(1);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                term.queue(1, 1, |id, resolver| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    (
                        Job::Message {
                            job: MessageJob::new(id, "a", SendOptions::default()),
                            resolver,
                        },
                        false,
                    )
                })
            });
            entered_rx.recv().unwrap();
            let second = scope.spawn(|| {
                let ticket = term.send("b", SendOptions::default());
                second_tx.send(()).unwrap();
                ticket
            });
            let overtook = second_rx
                .recv_timeout(std::time::Duration::from_millis(30))
                .is_ok();
            release_tx.send(()).unwrap();
            let first = first.join().unwrap();
            let second = second.join().unwrap();
            assert!(
                !overtook,
                "later send overtook construction of the earlier ticket"
            );
            assert!(first.id() < second.id());
            assert_eq!(
                term.ctl
                    .read(|st| st.jobs.iter().filter_map(Job::ticket).collect::<Vec<_>>()),
                vec![first.id(), second.id()]
            );
        });
    }

    #[test]
    fn construction_unwind_releases_admission_and_shutdown_resolves_it() {
        let (term, _events) = paused(AgentLimits {
            max_pending_jobs: 1,
            ..AgentLimits::default()
        });
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            term.queue(1, 1, |_, _| panic!("construction failed"))
        }));
        assert!(unwind.is_err());
        // A cancellation during construction resolves the registered ticket; it cannot
        // enqueue a job after disposal. Public kill_async consumes self, but internal
        // controller shutdown is safe at this boundary too.
        let ticket = term.queue(1, 1, |id, resolver| {
            term.ctl.dispose();
            (
                Job::Message {
                    job: MessageJob::new(id, "a", SendOptions::default()),
                    resolver,
                },
                false,
            )
        });
        assert_eq!(
            ticket.try_outcome(),
            Some(DeliveryOutcome::Aborted(AbortReason::Disposed))
        );
        assert!(term.ctl.read(|st| st.jobs.is_empty()));
    }

    #[test]
    fn invalid_limits_are_refused_before_profile_or_child_launch() {
        for limits in [
            AgentLimits {
                max_pending_jobs: usize::MAX,
                ..AgentLimits::default()
            },
            AgentLimits {
                max_message_bytes: usize::MAX,
                max_pending_bytes: usize::MAX,
                ..AgentLimits::default()
            },
            AgentLimits {
                max_pending_bytes: 1,
                ..AgentLimits::default()
            },
            AgentLimits {
                max_pending_jobs: 0,
                ..AgentLimits::default()
            },
        ] {
            let spec = SpawnSpec::direct(
                "/nonexistent-agent-link-limit-test",
                [] as [&str; 0],
                std::env::temp_dir(),
                PtySize { rows: 24, cols: 80 },
            );
            assert!(matches!(
                AgentTerminal::spawn(
                    spec,
                    AgentSpec {
                        limits,
                        ..AgentSpec::default()
                    }
                ),
                Err(Error::InvalidInput(_))
            ));
        }
    }
}
