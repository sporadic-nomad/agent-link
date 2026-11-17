//! Attach consumer-owned CLI tools and MCP servers to one agent run (`tools` feature).
//!
//! [`AgentTools`] prepares launch arguments, child environment and private temporary
//! configuration. It never installs software, edits global/project configuration,
//! implements a tool server or sends a prompt. Use [`AgentTools::prompt`] to include
//! CLI discovery instructions in a message. MCP discovery belongs to the agent.
//!
//! Prepared launches and their children retain temporary files through shared ownership;
//! the last owner removes them. Keep the original launch metadata when forwarding a
//! launch to a native adapter. Cleanup after an application crash is not guaranteed.
//! The host owns endpoint lifetime, authentication, revocation and action validation.
//! Child-only registration is not a sandbox or a guarantee of tool connectivity.
//!
//! Mappings: Codex config overrides, Claude invocation-specific MCP files, Pi's
//! `registerMcpServer` extension API, and ACP session parameters. OpenCode's built-in
//! stdio profile uses ACP. OpenCode PTY and Cursor MCP configuration are unsupported;
//! CLI attachments work with any profile. CLI versions and permissions may further
//! constrain availability. No live interoperability is implied by preparation.
//!
//! ```no_run
//! use agent_link::agent::{ProfileRegistry, RpcLaunchOptions};
//! use agent_link::tools::{AgentTools, CliTool, McpServer};
//!
//! # fn prepare(bundled_cli: std::path::PathBuf, mcp_url: String) -> agent_link::Result<()> {
//! let tools = AgentTools::new()
//!     .with_cli(CliTool::new(bundled_cli, "Use `context` to read application state."))
//!     .with_mcp(McpServer::http("my_application", mcp_url));
//! let profile = ProfileRegistry::builtin().get("codex").unwrap();
//! let launch = tools.rpc_launch(profile.as_ref(), &RpcLaunchOptions::default())?;
//! let first_message = tools.prompt("Explain the current document.")?;
//! // Move launch into RpcSpawnSpec and your matching adapter, initialize it,
//! // then submit first_message through its ordinary prompt API.
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::{
    AgentProfile, LaunchPlan, Permissions, RpcLaunch, RpcLaunchOptions, ShellKind, TrustPolicy,
};
use crate::env::EnvPolicy;
use crate::{Error, Result};
use serde_json::{Value, json};

mod render;
#[cfg(test)]
mod tests;
mod validate;

/// Maximum combined CLI/MCP descriptors accepted by one preparation.
pub const MAX_TOOLS: usize = 64;
/// Maximum aggregate descriptor strings, paths, arguments and environment bytes.
pub const MAX_CONFIGURATION_BYTES: usize = 64 * 1024;

/// Agent launch surface used to select a profile's MCP configuration mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolTransport {
    /// Interactive PTY launch.
    Pty,
    /// Structured stdio launch (including native adapters).
    Rpc,
}

/// Explicit provider configuration mapping, independent of executable/profile names.
/// Custom profiles may implement [`AgentProfile::mcp_backend`], or callers can use
/// [`AgentTools::configure_rpc`] with a raw launch. This is not version negotiation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpBackend {
    /// `-c mcp_servers...` overrides; credentials are carried in child environment.
    Codex,
    /// `--mcp-config` with an owned private JSON file.
    Claude,
    /// Temporary extension calling `pi.registerMcpServer`; requires that upstream API.
    Pi,
    /// MCP definitions sent by `acp::AcpClient` during new/load/resume.
    Acp,
}

/// Whether application MCP connections supplement other configured servers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpPolicy {
    /// Preserve native configuration, registering the explicitly named servers.
    /// Name collisions follow provider precedence; choose application-specific names.
    #[default]
    Additive,
    /// Request provider-supported exclusion of ordinary inherited MCP configuration.
    /// Currently Claude only (`--strict-mcp-config`); managed policy still applies.
    /// Other backends return `Error::Unsupported`, including an empty exclusive set.
    Exclusive,
}

/// A bundled command the agent may invoke through its own shell tool.
/// No executable is installed or copied. Its absolute parent directory is prepended
/// to the child PATH, and its absolute path appears in generated usage instructions.
/// The parent must be safe to prepend without escaping: `;` on Windows and `:` on
/// Unix are rejected, as are other characters invalid in a PATH entry.
#[derive(Clone)]
pub struct CliTool {
    executable: PathBuf,
    description: String,
}

impl CliTool {
    /// Declares an absolute executable path and host-authored usage instructions.
    /// Validation occurs when preparing a launch or generating instructions.
    pub fn new(executable: impl Into<PathBuf>, description: impl Into<String>) -> Self {
        Self {
            executable: executable.into(),
            description: description.into(),
        }
    }
}

impl fmt::Debug for CliTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CliTool(<redacted>)")
    }
}

#[derive(Clone)]
enum Connection {
    Stdio(PathBuf),
    Http(String),
}

/// A consumer-provided MCP connection. Payloads and credentials are omitted from Debug.
/// Stdio executables must be absolute UTF-8 paths; HTTP uses an explicit http(s) URL
/// with a valid host, port and bracketed IPv6 syntax. Userinfo, whitespace and fragments
/// are rejected. Validation preserves the original URL and never resolves DNS.
/// No executable availability, connection readiness or authentication is inferred.
/// Values are literal: Claude/Pi `${...}` expansion syntax is rejected. Pi also rejects
/// `$NAME`, `$$`, `$!` and command-valued `!` prefixes in environment/header values.
#[derive(Clone)]
pub struct McpServer {
    name: String,
    connection: Connection,
    args: Vec<String>,
    env: Vec<(String, String)>,
    headers: Vec<(String, String)>,
}

impl McpServer {
    /// A server the agent starts over stdio. Use `arg` and `env` for its process.
    pub fn stdio(name: impl Into<String>, executable: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            connection: Connection::Stdio(executable.into()),
            args: Vec::new(),
            env: Vec::new(),
            headers: Vec::new(),
        }
    }
    /// An HTTP MCP endpoint owned by the consumer. Use `header` for authentication.
    pub fn http(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            connection: Connection::Http(url.into()),
            args: Vec::new(),
            env: Vec::new(),
            headers: Vec::new(),
        }
    }
    /// Appends one unquoted stdio argument. Invalid on HTTP; preparation rejects it.
    #[must_use]
    pub fn arg(mut self, value: impl Into<String>) -> Self {
        self.args.push(value.into());
        self
    }
    /// Sets one stdio environment variable. Duplicate keys and HTTP use are rejected.
    /// Codex forwards these through its parent agent environment and `env_vars`;
    /// conflicting values for the same key across servers are therefore rejected.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
    /// Sets one HTTP header. Duplicate names and stdio use are rejected.
    #[must_use]
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }
}

impl fmt::Debug for McpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("McpServer(<redacted>)")
    }
}

/// Reusable descriptions of application tools; contains no processes or open servers.
/// Each preparation owns separate private configuration, while clones of a prepared
/// launch share that configuration. Up to 64 tools and 64 KiB of descriptor data are
/// accepted. Existing agent permission and trust settings are preserved.
#[derive(Clone, Default)]
pub struct AgentTools {
    cli: Vec<CliTool>,
    servers: Vec<McpServer>,
    env: Vec<(String, String)>,
    policy: McpPolicy,
}

impl fmt::Debug for AgentTools {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentTools")
            .field("cli_count", &self.cli.len())
            .field("mcp_count", &self.servers.len())
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl AgentTools {
    /// Empty tools, additive MCP configuration, no environment changes.
    pub fn new() -> Self {
        Self::default()
    }
    /// Adds a consumer-owned bundled command.
    #[must_use]
    pub fn with_cli(mut self, tool: CliTool) -> Self {
        self.cli.push(tool);
        self
    }
    /// Adds a consumer-owned MCP server connection.
    #[must_use]
    pub fn with_mcp(mut self, server: McpServer) -> Self {
        self.servers.push(server);
        self
    }
    /// Sets an agent-child variable, e.g. the current application's IPC endpoint.
    /// Does not modify the host process environment. Host EnvPolicy removals still win.
    /// As with EnvPolicy, PATH prepends are applied after removals. Agent shell policy
    /// and login-shell startup files may change what eventually reaches a command.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
    /// Selects additive (default) or explicitly exclusive MCP configuration.
    #[must_use]
    pub fn with_mcp_policy(mut self, policy: McpPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Produces CLI discovery text, without exposing MCP configuration or environment.
    /// Call before sending a prompt; attaching tools never sends a message implicitly.
    /// Empty CLI sets produce an empty string. Descriptor errors are `InvalidInput`.
    pub fn instructions(&self) -> Result<String> {
        self.validate()?;
        let mut text = String::new();
        if !self.cli.is_empty() {
            text.push_str(
                "Application commands available for this run (absolute executable paths):\n",
            );
        }
        for tool in &self.cli {
            text.push_str(&format!(
                "- {}: {}\n",
                json!(validate::path(&tool.executable)?),
                tool.description
            ));
        }
        Ok(text)
    }

    /// Prefixes a message with CLI usage instructions. This is ordinary host-supplied
    /// text, not a system-role injection or a grant of shell execution permissions.
    /// Normal transport/delivery limits still apply to the returned message.
    pub fn prompt(&self, message: &str) -> Result<String> {
        let instructions = self.instructions()?;
        Ok(if instructions.is_empty() {
            message.into()
        } else {
            format!("{instructions}\n{message}")
        })
    }

    /// Builds the profile's normal stdio launch and attaches tools. Use the returned
    /// launch in RpcSpawnSpec and the matching native adapter, or implement its native
    /// protocol yourself. ACP definitions remain in `launch.tools` until sent by ACP.
    /// Performs bounded validation and optional temporary-file I/O; never spawns.
    pub fn rpc_launch(
        &self,
        profile: &dyn AgentProfile,
        options: &RpcLaunchOptions,
    ) -> Result<RpcLaunch> {
        self.validate()?;
        let launch = profile.rpc_launch_with_options(options)?;
        self.configure_rpc_optional(profile.mcp_backend(ToolTransport::Rpc), launch)
    }

    /// Attaches tools to a raw/custom stdio launch using an explicit mapping.
    /// In particular, use Claude with `claude::launch`, or Acp with an arbitrary
    /// ACP executable. An already attached launch is rejected, not double-configured.
    /// Pass `None` for CLI-only attachments to an arbitrary executable.
    pub fn configure_rpc(
        &self,
        backend: impl Into<Option<McpBackend>>,
        launch: RpcLaunch,
    ) -> Result<RpcLaunch> {
        self.configure_rpc_optional(backend.into(), launch)
    }

    fn configure_rpc_optional(
        &self,
        backend: Option<McpBackend>,
        mut launch: RpcLaunch,
    ) -> Result<RpcLaunch> {
        if launch.tools.is_some() {
            return Err(invalid("launch already has tool attachments"));
        }
        let prepared = self.prepare(backend)?;
        render::check_conflicts(backend, &launch.args, &launch.env, prepared.has_mcp)?;
        launch.args.extend(prepared.args);
        launch.env.extend(prepared.env);
        launch.tools = Some(prepared.attachment);
        Ok(launch)
    }

    /// Builds an interactive profile launch with tools. Prefer
    /// `agent::AgentTerminal::spawn_with_tools` to keep resources owned automatically.
    /// Shell representability is checked by that spawn path before any child exists.
    /// CLI-only tools work with every profile; unsupported MCP mappings are refused.
    pub fn pty_launch(
        &self,
        profile: &dyn AgentProfile,
        permissions: &Permissions,
        trust: &TrustPolicy,
        cwd: &Path,
        shell: ShellKind,
    ) -> Result<LaunchPlan> {
        self.validate()?;
        let launch = profile.launch(permissions, trust, cwd, shell);
        self.configure_pty(profile, launch)
    }

    pub(crate) fn configure_pty(
        &self,
        profile: &dyn AgentProfile,
        mut launch: LaunchPlan,
    ) -> Result<LaunchPlan> {
        let backend = profile.mcp_backend(ToolTransport::Pty);
        if launch.tools.is_some() {
            return Err(invalid("launch already has tool attachments"));
        }
        if backend == Some(McpBackend::Acp)
            && (!self.servers.is_empty() || self.policy == McpPolicy::Exclusive)
        {
            return Err(Error::Unsupported(
                "ACP MCP requires structured-stdio session requests".into(),
            ));
        }
        let prepared = self.prepare(backend)?;
        render::check_conflicts(backend, &launch.args, &launch.env, prepared.has_mcp)?;
        launch.args.extend(prepared.args);
        launch.env.extend(prepared.env);
        launch.tools = Some(prepared.attachment);
        Ok(launch)
    }

    fn prepare(&self, backend: Option<McpBackend>) -> Result<render::Prepared> {
        self.validate()?;
        let has_mcp = !self.servers.is_empty() || self.policy == McpPolicy::Exclusive;
        if has_mcp && backend.is_none() {
            return Err(Error::Unsupported(
                "profile has no per-run MCP mapping for this transport".into(),
            ));
        }
        if self.policy == McpPolicy::Exclusive && backend != Some(McpBackend::Claude) {
            return Err(Error::Unsupported(
                "exclusive MCP configuration is supported only by Claude".into(),
            ));
        }
        render::prepare(self, backend, has_mcp)
    }
}

fn invalid(message: &str) -> Error {
    Error::InvalidInput(message.into())
}

/// Opaque shared ownership of prepared tool configuration and child PATH additions.
/// Created by AgentTools; retain it when moving/copying a prepared launch. Native
/// adapters and RPC/PTY child owners retain it automatically. Last-owner Drop removes
/// temporary files (best effort). Equality compares preparation identity, not payloads.
#[derive(Clone)]
pub struct ToolAttachment(Arc<Attachment>);

struct Attachment {
    _directory: Option<tempfile::TempDir>,
    path_prepend: Vec<PathBuf>,
    backend: Option<McpBackend>,
    has_mcp: bool,
    acp_servers: Value,
}

impl fmt::Debug for ToolAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolAttachment")
            .field("has_mcp", &self.0.has_mcp)
            .finish_non_exhaustive()
    }
}
impl PartialEq for ToolAttachment {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ToolAttachment {}

impl ToolAttachment {
    /// Native MCP server definitions for consumers implementing ACP on raw RpcChild.
    /// None means another mapping was selected. Contains potentially sensitive headers
    /// and environment values: do not log it. AcpClient sends these automatically.
    pub fn acp_servers(&self) -> Option<&Value> {
        (self.0.backend == Some(McpBackend::Acp)).then_some(&self.0.acp_servers)
    }
    pub(crate) fn apply_path(&self, policy: &mut EnvPolicy) {
        let mut paths = self.0.path_prepend.clone();
        for path in &policy.path_prepend {
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
        policy.path_prepend = paths;
    }
    #[cfg(any(feature = "codex", feature = "claude", feature = "acp"))]
    pub(crate) fn require_backend(&self, backend: McpBackend) -> Result<()> {
        if self.0.has_mcp && self.0.backend != Some(backend) {
            return Err(Error::Unsupported(
                "tool attachment does not match the native adapter".into(),
            ));
        }
        Ok(())
    }
}
