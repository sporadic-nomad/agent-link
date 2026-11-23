//! Agent CLIs on a PTY: what the crate knows about each one, and the controller that drives
//! one.
//!
//! - [`AgentTerminal`] runs a session with an agent controller: it types the launch line,
//!   answers startup dialogs, tells when the agent is ready for programmatic input
//!   ([`AgentState`], with a `blocking` flag for the host's overlay) and follows its
//!   [`Activity`]. Configured by an [`AgentSpec`] and its [`Timing`].
//! - [`AgentTerminal::send`] delivers a message: queued in order, pasted when the agent is
//!   ready and its output paused, submitted with a verified Enter ([`SendOptions`]: also
//!   strict foreground proofs and a commit-point check, [`Revalidate`], and an opt-in
//!   unverified Enter for unreadable composers, [`UnreadableComposer`]); the [`Ticket`]
//!   and an `Event::Delivery` report the [`DeliveryOutcome`].
//!   [`AgentTerminal::reset`] sends the profile's fresh-conversation command,
//!   [`AgentTerminal::reset_and_send`] a reset and then a message as one delivery.
//! - [`AgentProfile`] answers three questions about screen text: is a startup dialog up
//!   (and which key moves toward the profile's answer — [`Dialog`]), is the ready composer
//!   up, and does the composer still hold text just sent. [`AgentProfile::classify`] puts a
//!   dialog before readiness.
//! - It also says how to start the agent: [`AgentProfile::launch`] builds a [`LaunchPlan`]
//!   for a [`Permissions`] posture and a [`TrustPolicy`]. [`AgentProfile::model_args`]
//!   maps an optional [`ModelChoice`] onto that plan before [`LaunchPlan::command_line`]
//!   quotes it for the PTY's [`ShellKind`]. The profile also supplies the reset command,
//!   `PATH` launchers, [`SignalHints`], a [`LoginCheck`] and an [`RpcLaunch`].
//! - [`profiles`] holds the built-ins (claude, codex, pi, opencode, cursor) and
//!   [`ProfileRegistry`]; [`detect`] holds the screen helpers they share, for a host
//!   writing its own profile.
//! - Markers match normalised text ([`normalize`](crate::screen::normalize)): escapes
//!   removed, whitespace collapsed, lowercase. Dialog detection also accepts a raw output
//!   buffer; readiness and pending input read the current screen only.
//!
//! Rules a host keeps: drain every receiver of the terminal or drop it (the controller
//! publishes on them, so an unread one stalls its deliveries too); drop the terminal outside
//! your own locks; treat only [`DeliveryOutcome::Delivered`] as delivered.
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use agent_link::agent::{AgentSpec, AgentState, AgentTerminal, ProfileRegistry, SendOptions};
//! use agent_link::{Event, PtySize, SpawnSpec};
//!
//! let profile = ProfileRegistry::builtin().get("codex").expect("a built-in profile");
//! let spec = SpawnSpec::shell("/path/to/project", PtySize { rows: 40, cols: 120 });
//! // `AgentSpec::new`: unattended (YOLO or equivalent). Configure `trust` explicitly
//! // when startup dialogs should be answered automatically.
//! let (agent, events) = AgentTerminal::spawn(spec, AgentSpec::new(profile))?;
//! agent.launch()?; // types `codex …` into the login shell
//! for event in &events {
//!     match event {
//!         Event::AgentState { state: AgentState::Ready, .. } => break,
//!         Event::AgentState { state: AgentState::Exited, .. } => return Ok(()),
//!         _ => {}
//!     }
//! }
//! drop(events); // not read any more
//! let opts = SendOptions { submit: true, ..SendOptions::default() };
//! let ticket = agent.send("Summarise README.md in one line", opts);
//! println!("{:?}", ticket.wait(Duration::from_secs(120))); // Some(Delivered)
//! # Ok::<(), agent_link::Error>(())
//! ```

mod activity;
pub mod compatibility;
mod controller;
mod delivery;
pub mod detect;
// `pub(crate)`: `rpc` and `probe` quote their one-shot shell commands with it.
pub(crate) mod launch;
mod leftover;
mod limits;
// Bounded loopback HTTP/1.1: the OpenCode ambient feed and question connections.
#[cfg(feature = "turns")]
mod loopback;
mod monitor;
mod permissions;
mod profile;
pub mod profiles;
mod proof;
#[cfg(feature = "questions")]
pub mod questions;
mod settle;
mod startup;
mod state;
mod terminal;
mod timing;
mod turn_gate;
#[cfg(all(unix, any(test, feature = "turns")))]
pub mod turns;
#[cfg(all(not(unix), feature = "turns"))]
#[path = "turns/unsupported.rs"]
pub mod turns;
mod verify;

pub use activity::Activity;
pub use delivery::{
    AbortReason, DeliveryOutcome, Revalidate, SendOptions, ShellFallback, Ticket, TicketId,
    UnreadableComposer,
};
pub use launch::{LaunchPlan, ModelChoice, ShellKind};
pub use limits::AgentLimits;
pub use permissions::{ForegroundPolicy, Permissions, PreTrust, TrustPolicy};
pub use profile::{
    AgentProfile, Dialog, DialogKind, LoginCheck, RpcLaunch, RpcLaunchOptions, ScreenState,
    SignalHints, TrustKey, Unlock,
};
pub use profiles::ProfileRegistry;
pub use terminal::{AgentSpec, AgentState, AgentTerminal};
pub use timing::Timing;
