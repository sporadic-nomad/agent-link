use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::Duration;

use crate::agent::Timing;
use crate::agent::compatibility::Compatibility;

use super::ProbeResult;

/// How probe commands are launched. Choose the mode used by the application.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProbeMode {
    /// Unix: interactive login shell, which can modify environment/cwd and resolve
    /// functions or aliases. Windows: direct PATH resolution (no login shell).
    #[default]
    LoginShell,
    /// Direct executable resolution under the policy's PATH and configured cwd.
    /// No shell startup files; an executable script may still modify its own context.
    /// Windows skips empty PATH entries, matching the shared RPC resolver; Unix
    /// treats them as cwd. Windows suffix expansion inspects only the filename.
    Direct,
}

/// Context for detailed probes (`probe` feature). No model request is made, but the
/// CLI's version/login commands may read configuration. Environment comes from the
/// separate [`EnvPolicy`](crate::env::EnvPolicy) argument, including all sets/removals.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ProbeOptions {
    /// Per-command deadline, capped at one day; default 8 seconds.
    pub timeout: Duration,
    /// Default login shell mode preserves the convenience API's launcher behavior.
    pub mode: ProbeMode,
    /// Unix login shell override; ignored in direct mode and on Windows.
    /// Used as given, with `-c` for a filename of `sh` and `-l -i -c` otherwise.
    /// A nonexistent shell produces an I/O failure, without substituting another shell.
    pub shell: Option<String>,
    /// Working directory. `None` snapshots the host's current directory; relative
    /// paths resolve against it. If that directory cannot be read, no commands run:
    /// readiness is [`RuntimeReadiness::Untested`] with
    /// [`ContextDiagnostic::WorkingDirectoryUnavailable`]. An explicit absolute
    /// path does not require reading the host's current directory.
    pub cwd: Option<PathBuf>,
}
impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            timeout: Timing::default().probe_timeout,
            mode: ProbeMode::default(),
            shell: None,
            cwd: None,
        }
    }
}

/// Readiness of the *version command*, not authentication or protocol compatibility.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeReadiness {
    /// A launcher returned exit zero for `--version`.
    Available,
    /// Direct resolution found none of the configured launchers.
    Missing,
    /// The profile supplies no launchers for probing.
    Unsupported,
    /// No command was tested (for example the worker could not start).
    #[default]
    Untested,
    /// A failure, timeout or inconclusive shell result prevents a stronger claim.
    Unknown,
}

/// The particular operation observed; neither operation measures an agent turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProbeOperation {
    /// `--version`.
    Version,
    /// The profile's login-status command.
    LoginStatus,
}

/// Command outcome without sensitive command output or OS error text.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProbeOutcome {
    /// Process completed; `code: None` can indicate termination by signal.
    Exited {
        /// True only for exit zero.
        success: bool,
        /// Native exit code, absent for a signal or unknown termination.
        code: Option<i32>,
    },
    /// Command/output did not finish before the deadline; teardown was requested.
    TimedOut,
    /// OS error while spawning, capturing, or waiting.
    IoError(ErrorKind),
    /// No executable resolved in direct mode; no command was spawned.
    NotFound,
    /// Shell quoting rejected an unsafe word; no command was spawned.
    InvalidCommand,
}

/// One attempted command. Captured login text is never retained in a report.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProbeObservation {
    /// Measured operation.
    pub operation: ProbeOperation,
    /// Configured launcher, possibly an alias/function in login-shell mode.
    pub launcher: String,
    /// Absolute resolved executable in direct mode; unknown for shell resolution.
    /// Identifies the path invoked, not a binary digest or a race-free file identity.
    pub executable: Option<PathBuf>,
    /// Exit/error outcome.
    pub outcome: ProbeOutcome,
    /// At least one captured stream exceeded 8 KiB.
    pub output_truncated: bool,
    /// At least one stream was invalid UTF-8; replacement was used for display only.
    pub output_lossy: bool,
}

/// Context caveats that must not silently become readiness claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContextDiagnostic {
    /// Shell startup files can change the supplied environment, cwd and resolution.
    ShellMayOverrideContext,
    /// The host working directory could not be read; no commands were attempted.
    WorkingDirectoryUnavailable,
    /// A profile callback panicked; no complete report is available.
    ProfilePanicked,
    /// The worker thread could not start.
    WorkerUnavailable,
    /// Windows executable suffix resolution uses the host's PATHEXT, as does the
    /// existing RPC resolver; a differing child PATHEXT is not modeled.
    HostPathExt,
}

/// Detailed, operation-scoped diagnostics. Caller-owned; version output is explicitly
/// accessible but redacted from `Debug` and never logged by the probe.
#[derive(Clone)]
#[non_exhaustive]
pub struct ProbeReport {
    /// Backward-compatible install and three-way sign-in projection.
    pub result: ProbeResult,
    /// Whether the version command could be measured successfully.
    pub readiness: RuntimeReadiness,
    /// Static profile declarations; never upgraded by a runtime version check.
    pub compatibility: Compatibility,
    /// Host OS and architecture, not necessarily the platform of a wrapped CLI.
    pub platform: (&'static str, &'static str),
    /// Actual selected command mode.
    pub mode: ProbeMode,
    /// Selected shell program, passed unchanged to process creation when attempted.
    /// Present even if no command could be started.
    /// Preserves the selected path/name; it is not canonicalized or resolved against
    /// PATH in this field. Use an absolute override to identify a particular shell path.
    pub shell: Option<String>,
    /// Requested absolute working directory, before shell startup files.
    pub cwd: Option<PathBuf>,
    /// Successful `--version` stdout then stderr, trimmed (at most 16 KiB + separator).
    /// Unparsed: banners and future version formats remain visible. Empty output is
    /// `None`. This may contain sensitive CLI output; persist only with host consent.
    pub version_output: Option<String>,
    /// Attempts in order, including the login check only after a successful version.
    pub observations: Vec<ProbeObservation>,
    /// Context limitations and failures.
    pub diagnostics: Vec<ContextDiagnostic>,
}
impl std::fmt::Debug for ProbeReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeReport")
            .field("result", &self.result)
            .field("readiness", &self.readiness)
            .field("compatibility", &self.compatibility)
            .field("platform", &self.platform)
            .field("mode", &self.mode)
            .field("shell", &self.shell)
            .field("cwd", &self.cwd)
            .field(
                "version_output",
                &self.version_output.as_ref().map(|_| "[redacted]"),
            )
            .field("observations", &self.observations)
            .field("diagnostics", &self.diagnostics)
            .finish()
    }
}
