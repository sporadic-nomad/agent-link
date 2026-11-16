//! Install and sign-in probes for agent CLIs — what an agent picker shows. For each
//! [`AgentProfile`], [`probe_agents`] answers two separate questions ([`ProbeResult`]): is the
//! CLI installed (a launcher answers `--version` with exit 0), and is it signed in (the
//! profile's [`login_check`](AgentProfile::login_check), read by [`classify_login`]).
//!
//! **Sign-in is three-way; unknown is never "signed out".** `Some(false)` only when the check
//! exited non-zero with its signed-out marker in the output
//! ([`LoginCheck`](crate::agent::LoginCheck) has the verification table). No check, a CLI
//! that is not installed, a timeout, a spawn failure, an unsupported subcommand or a reworded
//! message all read `None`: "assume fine".
//!
//! **Context**: the entire [`EnvPolicy`] is applied, including configured values and
//! `inherit: false`. Unix defaults to the interactive login shell; its startup files
//! may override environment, cwd or launcher resolution. [`probe_agents_detailed`]
//! exposes this limitation and accepts [`ProbeOptions`] for direct resolution and an
//! explicit cwd. Windows resolves directly (including `.cmd` shims).
//!
//! **Bounds**: each command has a timeout and retains at most 8 KiB per output stream,
//! draining excess bytes. Truncated/invalid UTF-8 login output is inconclusive. Commands
//! have null stdin, Unix runs in a new session and Windows suppresses console windows.
//! Teardown kills the process group/tree; an escaped descendant may retain a detached
//! reader until it closes its pipe. Windows tree cleanup uses the existing best-effort
//! taskkill helper. Probes are synchronous and run one worker per supplied profile.
//!
//! **Logging**: every outcome, prefixed `[probe-agents]` (see [`probe_agents`]).
//!
//! ```no_run
//! use agent_link::agent::{profiles, Timing};
//! use agent_link::env::EnvPolicy;
//! use agent_link::probe::probe_agents;
//!
//! // The spawns' `PATH` and the keys the host keeps from its children.
//! let mut env = EnvPolicy::default();
//! env.remove("SECRET_TOKEN");
//! // Blocks for seconds: call it on a worker thread, never on a UI thread.
//! let results = probe_agents(&profiles::builtin(), &env, Timing::default().probe_timeout);
//! for (id, probe) in &results {
//!     // `signed_in: None` is unknown ("assume fine"), never "signed out".
//!     println!("{id}: installed={} signed_in={:?}", probe.installed, probe.signed_in);
//! }
//! ```

mod capture;
mod login;
mod report;
mod run;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub use login::classify_login;
pub use report::*;

use self::run::Probe;
use crate::agent::AgentProfile;
use crate::env::EnvPolicy;
use crate::screen::catch_panic;

/// One profile's answer: two facts, deliberately not folded into one — offering "Install"
/// for a CLI that is installed but signed out sends the user to reinstall it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ProbeResult {
    /// A launcher answered `--version` with exit 0.
    pub installed: bool,
    /// `Some(true)` signed in, `Some(false)` signed out — only when the profile's own check
    /// said so. `None` is unknown (no check, not installed, an inconclusive run): treat it as
    /// "assume fine", never as signed out.
    pub signed_in: Option<bool>,
}

/// Convenience projection of [`probe_agents_detailed`] using login-shell defaults.
/// Blocks until all probes finish. Timeout is per command (capped at one day), plus
/// up to two seconds to reap on Unix; Windows taskkill can add time. Launchers run
/// sequentially per profile; Unix tries until one succeeds, Windows stops at the first
/// resolved launcher. Duplicate IDs keep the later profile's answer.
///
/// The full environment policy now applies (including config directories and clearing
/// inheritance). Shell startup files can override it. Use detailed direct mode when
/// matching a direct spawn. Sign-in stays unknown for absent/failed/inconclusive checks.
/// A panicking profile or unavailable worker maps to the historical default result;
/// detailed reports preserve those failures. Panicking `id()` callbacks are skipped.
/// Logs use `[probe-agents]`; version/login payloads and environment values are not logged.
pub fn probe_agents(
    profiles: &[Arc<dyn AgentProfile>],
    env: &EnvPolicy,
    timeout: Duration,
) -> HashMap<String, ProbeResult> {
    let options = ProbeOptions {
        timeout,
        ..ProbeOptions::default()
    };
    probe_agents_detailed(profiles, env, &options)
        .into_iter()
        .map(|(id, report)| (id, report.result))
        .collect()
}

/// Probe installation and sign-in with explicit context and operation-scoped evidence.
/// See [`probe_agents`] for blocking, launcher fallback and duplicate-ID behavior.
/// The result records version output, platform, profile revision, command outcomes and
/// context caveats. It does not negotiate a protocol, run a model turn, or reject versions
/// outside a tested range. No login payload or environment value is retained.
///
/// ```no_run
/// use agent_link::{agent::profiles, env::EnvPolicy, probe::*};
/// let mut options = ProbeOptions::default();
/// options.mode = ProbeMode::Direct;
/// options.cwd = Some("/path/to/project".into());
/// let reports = probe_agents_detailed(&profiles::builtin(), &EnvPolicy::default(), &options);
/// for (id, report) in reports {
///     println!("{id}: {:?}, signed in {:?}", report.readiness, report.result.signed_in);
/// }
/// ```
pub fn probe_agents_detailed(
    profiles: &[Arc<dyn AgentProfile>],
    env: &EnvPolicy,
    options: &ProbeOptions,
) -> HashMap<String, ProbeReport> {
    let probe = Arc::new(Probe::with_options(env, options));
    let threads: Vec<_> = profiles
        .iter()
        .filter_map(|profile| {
            let id = catch_panic(|| profile.id().to_string())
                .inspect_err(|_| {
                    log::warn!("[probe-agents] a profile's id() panicked ⇒ skipped");
                })
                .ok()?;
            let (worker, profile) = (Arc::clone(&probe), Arc::clone(profile));
            let thread = std::thread::Builder::new()
                .name("agent-link-probe".into())
                .spawn(move || catch_panic(|| worker.profile(profile.as_ref())));
            Some((id, thread))
        })
        .collect();
    let mut results = HashMap::with_capacity(threads.len());
    for (id, thread) in threads {
        let answer = match thread {
            Ok(handle) => handle
                .join()
                .ok()
                .and_then(Result::ok)
                .ok_or(ContextDiagnostic::ProfilePanicked),
            Err(_) => Err(ContextDiagnostic::WorkerUnavailable),
        };
        let report = answer.unwrap_or_else(|diagnostic| {
            log::warn!("[probe-agents] {id}: {diagnostic:?} ⇒ reported as not installed");
            let mut report = probe.empty_report();
            // A later callback (such as login_check) can panic after version ran.
            // Do not claim that the runtime was never tested in that case.
            if diagnostic == ContextDiagnostic::ProfilePanicked {
                report.readiness = RuntimeReadiness::Unknown;
            }
            report.diagnostics.push(diagnostic);
            report
        });
        results.insert(id, report);
    }
    results
}

#[cfg(all(test, unix))]
fn probe_agents_in(
    profiles: &[Arc<dyn AgentProfile>],
    env: &EnvPolicy,
    timeout: Duration,
    shell: Option<&str>,
) -> HashMap<String, ProbeResult> {
    let options = ProbeOptions {
        timeout,
        shell: shell.map(str::to_owned),
        ..ProbeOptions::default()
    };
    probe_agents_detailed(profiles, env, &options)
        .into_iter()
        .map(|(id, report)| (id, report.result))
        .collect()
}
