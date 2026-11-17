# Application CLI and MCP Tools

Enable `tools` to attach tools implemented by your application to an agent run. The
facade prepares launch arguments, child environment and temporary configuration; it
does not implement a CLI, MCP server, IPC bridge or application actions. It installs
nothing globally and writes neither user nor project MCP configuration.

```toml
agent-link = { version = "0.2", features = ["tools", "codex"] }
```

## Describe the application's tools

```rust
use agent_link::tools::{AgentTools, CliTool, McpServer};

let tools = AgentTools::new()
    .with_cli(CliTool::new(
        bundled_cli_path, // Absolute path inside your application bundle
        "Use `context` to read the active document. Use `--help` for commands.",
    ))
    .with_env("MY_APP_ENDPOINT", application_ipc_endpoint)
    .with_mcp(
        McpServer::http("my_application", application_mcp_url)
            .header("Authorization", format!("Bearer {run_token}")),
    );
```

Use either CLI, MCP, or both. A bundled command's parent directory is prepended to
the agent child's PATH. `tools.instructions()` describes each absolute executable
path and its usage; `tools.prompt(message)` prefixes a message with those instructions.
Send this through the agent's ordinary prompt API. Preparation never sends a prompt
or changes a system message. MCP discovery is handled by the agent's MCP client.

For a stdio MCP server use
`McpServer::stdio("my_application", absolute_server_path).arg("--mcp").env("MY_APP_ENDPOINT", endpoint)`.
The agent starts that executable. An HTTP server is already running in your app (or
a consumer-owned sidecar); the facade only registers its endpoint. The CLI/server
must obtain live state and dispatch actions through your application's own code.

## Start an agent

For a profile-based stdio launch, replace the profile's launch builder:

```rust
use agent_link::agent::{ProfileRegistry, RpcLaunchOptions};
use agent_link::codex::{CodexAppServer, CodexLimits};
use agent_link::rpc::RpcSpawnSpec;

let profile = ProfileRegistry::builtin().get("codex").unwrap();
let launch = tools.rpc_launch(profile.as_ref(), &RpcLaunchOptions::default())?;
let spec = RpcSpawnSpec::new(launch, application_working_directory);
let (agent, events) = CodexAppServer::spawn(spec, CodexLimits::default())?;
// Drain events, initialize, create/restore a thread and send tools.prompt(...)
// through the usual adapter APIs. See the native adapter guide.
```

Move the returned `RpcLaunch` directly into `RpcSpawnSpec`. It retains the attachment's
metadata and temporary files. The matching native adapter consumes it automatically;
raw `RpcChild` hosts still implement their vendor protocol themselves.

For a custom launch use `tools.configure_rpc(McpBackend::Claude, launch)` (for example,
on the result of `claude::launch`). This explicitly selects a mapping without guessing
from an executable name. Custom `AgentProfile` implementations may instead opt in with
`mcp_backend(transport)`; its default is `None`. CLI-only profile attachments need no MCP
mapping; use `tools.configure_rpc(None, launch)` for a raw CLI-only launch. Native
adapters reject attachments for a different MCP backend before spawning.

For interactive PTY sessions:

```rust
use agent_link::agent::{AgentSpec, AgentTerminal};
use agent_link::{PtySize, SpawnSpec};

let spec = SpawnSpec::shell(application_working_directory, PtySize { rows: 24, cols: 100 });
let (terminal, events) = AgentTerminal::spawn_with_tools(spec, AgentSpec::new(profile), &tools)?;
terminal.launch()?;
// Drain events and wait for startup, then use terminal.send with tools.prompt(message)
// and your SendOptions, and observe its delivery ticket.
```

Shell mode uses the normal profile launch with appended tool options. Direct mode
appends tool options to the host's explicit executable/arguments; it does not add
profile permission or trust flags, matching normal Direct behavior. Profile launch
environment still applies. Tool-configured Direct argv must be UTF-8.
`tools.pty_launch(...)` is also available for hosts preparing their own launch plans;
such hosts must retain the plan or its `tools` attachment until the child exits.

Existing permission and trust choices remain in effect, including unattended defaults.
Tool registration does not grant execution permissions. An agent's shell-environment
policy or login-shell startup files can change PATH or remove variables before a tool
runs. Absolute paths in CLI instructions avoid reliance on PATH lookup, but sandbox
and network policy can still prevent access.

## Provider mappings

| Agent / surface | CLI | MCP registration |
| --- | --- | --- |
| Codex PTY and app-server | Yes | Invocation `-c mcp_servers.NAME=...` overrides; stdio env and HTTP header values travel through child environment variables |
| Claude PTY and stream-json | Yes | Private JSON file passed with `--mcp-config` |
| Pi PTY and raw RPC | Yes | Temporary `--extension` calling `pi.registerMcpServer`; requires a Pi version exposing that API |
| OpenCode ACP / another ACP agent | Yes | `mcpServers` in `session/new`, `session/load`, and `session/resume` |
| OpenCode PTY; Cursor | Yes | Unsupported; requesting MCP returns `Error::Unsupported` |

ACP always supports stdio definitions at the protocol level; HTTP requires the
initialized agent to advertise `agentCapabilities.mcpCapabilities.http`. Otherwise
session admission fails without enqueueing a request or consuming a session. With
raw `RpcChild`, read `launch.tools.as_ref().and_then(|t| t.acp_servers())` before
moving the launch, then implement these messages yourself. These definitions contain
credentials and should not be logged.

`McpPolicy::Additive` is the default: ordinary MCP configuration is preserved. Use
application-specific names; duplicate names across inherited configuration follow
the provider's precedence. In Pi, file-configured entries override extension
registrations. Pi names that differ only by `-` versus `_` are rejected within one
attachment. `McpPolicy::Exclusive` requests Claude's `--strict-mcp-config`, including
an empty list; other backends return `Unsupported`. Managed policy can still apply.

These mappings follow [Codex MCP configuration](https://developers.openai.com/codex/mcp/),
[Claude MCP configuration](https://code.claude.com/docs/en/mcp),
[Pi's extension API](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/extensions.md)
and [ACP session setup](https://agentclientprotocol.com/protocol/session-setup)
(source review: 2026-11-17; preparation does not negotiate CLI versions).
Evidence for this addition is offline serialization, admission and native child
lifetime tests on macOS. No new live CLI interoperability or Linux/Windows runtime
validation is claimed. Older Pi versions without the registration API cannot use
the MCP mapping; extension loading reports that missing API.

## Ownership, validation and limits

- Describe tools once and prepare separately for each agent run. Each preparation
  gets independent configuration; clones of a prepared launch share ownership.
- The PTY/RPC child retains temporary files until its owner is dropped, after child
  teardown. Extra launch clones prolong their lifetime. Native adapters tear down
  asynchronously, so file cleanup can follow adapter close/drop. Last-owner cleanup
  is best effort; a crash can leave files behind. Unix directories are created with
  mode `0700` and files with `0600`; Windows uses inherited temporary-directory ACLs.
- Your app owns endpoint readiness, server shutdown, authentication and revocation.
  Use a fresh run token and revoke it when the app/run ends. Child-scoped registration
  is not access isolation; it does not revoke a public endpoint or prevent an agent
  from learning an executable path. Existing native MCP configuration is not erased.
- Preparation accepts at most 64 combined CLI/MCP entries and 64 KiB of descriptor
  text. Per server: 128 arguments, 64 environment pairs and 64 headers. Paths must be
  absolute UTF-8; CLI parent directories must be safe to prepend to PATH without escaping
  (`;` on Windows and `:` on Unix are rejected). Names, duplicate fields, NULs and HTTP
  header controls are validated. MCP HTTP URLs are parsed with `url` before rendering:
  hosts, numeric ports in `0..=65535` and bracketed IPv6 syntax must be valid. Userinfo,
  whitespace, fragments, empty authorities and backslashes in the authority are rejected.
  Valid URL strings are preserved exactly; validation performs no DNS or network requests.
  Invalid descriptors return `InvalidInput`; unsupported mappings/policies return
  `Unsupported`; temporary-file failures return `Io`. Validation does not check whether
  executables exist or endpoints respond. Normal spawn, framing and delivery limits apply.
- Values are literal. Claude/Pi configuration strings containing `${` are rejected.
  Pi environment/header values also reject `$NAME`, `$$`, `$!`, and a leading `!`
  (including after whitespace), preventing native interpolation, escape decoding or
  command evaluation. Backslashes do not escape these forms in Pi. Dollar signs that
  do not start a template, such as `$9` or a trailing `$`, remain literal. Supply already
  resolved values that do not use these reserved forms. Codex forwards
  server env names through the agent process, so different values for the same env key
  across its servers are rejected. Use env/header fields for secrets, not arguments or
  URL query strings. Host environment removals win; PATH prepends follow EnvPolicy's
  existing rules. `AGENT_LINK_MCP_` is reserved for generated environment keys.
- Existing MCP flags, duplicate attachments and unusable end-of-options placement
  are rejected where detected. Tool descriptors and attachments redact Debug output;
  ordinary launch Debug/explicit tracing can still show arguments you supplied.
- Preparation and cleanup can perform synchronous filesystem I/O. Keep them off a
  UI/async executor thread when latency matters; no async runtime is required.

With `tools` enabled, `RpcLaunch` and `LaunchPlan` gain an optional `tools` field.
Constructors initialize it to `None`; full `RpcLaunch` struct literals need updating.
Prefer constructors and preserve that field when forwarding prepared launches.

The [offline preparation example](../examples/application_tools.rs) is runnable with
`cargo run --example application_tools --features tools -- codex /absolute/app-cli http://127.0.0.1:9123/mcp`.
It prepares a launch and shows CLI instructions; it does not start an agent or server.
