//! Check which built-in agent CLIs are installed, inspect their compatibility metadata,
//! and run the non-destructive three-way sign-in check.
//!
//! ```text
//! cargo run -p agent-link --example probe_agents --features probe
//! cargo run -p agent-link --example probe_agents --features probe -- --direct
//! ```

use std::error::Error;
use std::process::ExitCode;

use agent_link::agent::{Timing, profiles};
use agent_link::env::EnvPolicy;
use agent_link::probe::{ProbeMode, ProbeOptions, probe_agents_detailed};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("probe_agents: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let direct = std::env::args().skip(1).any(|arg| arg == "--direct");
    let mode = if direct {
        ProbeMode::Direct
    } else {
        ProbeMode::LoginShell
    };

    let builtin = profiles::builtin();
    let env = EnvPolicy::default();
    let mut options = ProbeOptions::default();
    options.timeout = Timing::default().probe_timeout;
    options.mode = mode;

    println!(
        "Probing {} built-in profiles ({:?}):",
        builtin.len(),
        options.mode
    );
    let reports = probe_agents_detailed(&builtin, &env, &options);

    for profile in &builtin {
        let id = profile.id();
        let Some(report) = reports.get(id) else {
            continue;
        };
        println!(
            "- {id}: readiness={:?}, installed={}, signed_in={:?} (pty={:?}, rpc={:?}, login={:?})",
            report.readiness,
            report.result.installed,
            report.result.signed_in,
            report.compatibility.pty,
            report.compatibility.rpc_launch,
            report.compatibility.login_check,
        );
        for obs in &report.observations {
            println!(
                "    {:?} `{}` -> {:?} (executable: {:?})",
                obs.operation, obs.launcher, obs.outcome, obs.executable
            );
        }
        if !report.diagnostics.is_empty() {
            println!("    diagnostics: {:?}", report.diagnostics);
        }
    }
    Ok(())
}
