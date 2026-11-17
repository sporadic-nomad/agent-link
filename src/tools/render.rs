use super::*;
use crate::agent::launch::toml_basic_string;
use std::collections::BTreeMap;
use std::io::Write;

pub(super) struct Prepared {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub attachment: ToolAttachment,
    pub has_mcp: bool,
}

pub(super) fn prepare(
    tools: &AgentTools,
    backend: Option<McpBackend>,
    has_mcp: bool,
) -> Result<Prepared> {
    let mut args = Vec::new();
    let mut env = tools.env.clone();
    let mut directory = None;
    let mut acp_servers = json!([]);
    if has_mcp {
        match backend.ok_or_else(|| Error::Unsupported("MCP mapping unavailable".into()))? {
            McpBackend::Codex => codex(tools, &mut args, &mut env)?,
            McpBackend::Claude | McpBackend::Pi => {
                validate_literal_config(tools, backend == Some(McpBackend::Pi))?;
                let mut builder = tempfile::Builder::new();
                builder.prefix("agent-link-tools-");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    builder.permissions(std::fs::Permissions::from_mode(0o700));
                }
                let dir = builder.tempdir().map_err(|source| Error::Io {
                    context: "create private tool configuration directory".into(),
                    source,
                })?;
                let mut servers = serde_json::Map::new();
                for server in &tools.servers {
                    servers.insert(
                        server.name.clone(),
                        server_json(server, backend == Some(McpBackend::Claude)),
                    );
                }
                let config = dir.path().join("mcp.json");
                write_private(&config, &json!({"mcpServers":servers}).to_string())?;
                if backend == Some(McpBackend::Claude) {
                    args.extend(["--mcp-config".into(), path_string(&config)?]);
                    if tools.policy == McpPolicy::Exclusive {
                        args.push("--strict-mcp-config".into());
                    }
                } else {
                    let extension = dir.path().join("mcp-extension.mjs");
                    // Fixed source; the only interpolation is a JSON-encoded path. No
                    // host tool value is evaluated as JavaScript or shell source.
                    let source = format!(
                        "import {{ readFileSync }} from 'node:fs';\nexport default function(pi) {{\n  if (typeof pi.registerMcpServer !== 'function') throw new Error('agent-link requires Pi registerMcpServer support');\n  const servers = JSON.parse(readFileSync({}, 'utf8')).mcpServers;\n  for (const [name, config] of Object.entries(servers)) pi.registerMcpServer(name, config);\n}}\n",
                        json!(path_string(&config)?)
                    );
                    write_private(&extension, &source)?;
                    args.extend(["--extension".into(), path_string(&extension)?]);
                }
                directory = Some(dir);
            }
            McpBackend::Acp => {
                acp_servers = Value::Array(tools.servers.iter().map(acp_server).collect());
            }
        }
    }
    let mut path_prepend = Vec::new();
    for cli in &tools.cli {
        if let Some(parent) = cli.executable.parent() {
            if !path_prepend.iter().any(|p| p == parent) {
                path_prepend.push(parent.to_path_buf());
            }
        }
    }
    Ok(Prepared {
        args,
        env,
        has_mcp,
        attachment: ToolAttachment(Arc::new(Attachment {
            _directory: directory,
            path_prepend,
            backend,
            has_mcp,
            acp_servers,
        })),
    })
}

fn codex(
    tools: &AgentTools,
    args: &mut Vec<String>,
    env: &mut Vec<(String, String)>,
) -> Result<()> {
    // Unlike headers, Codex env_vars forwards names without renaming. Refuse
    // conflicting server environments instead of silently changing another server.
    let mut shared_env: BTreeMap<String, String> = tools
        .env
        .iter()
        .map(|(k, v)| (k.to_ascii_uppercase(), v.clone()))
        .collect();
    for server in &tools.servers {
        for (key, value) in &server.env {
            if shared_env
                .get(&key.to_ascii_uppercase())
                .is_some_and(|v| v != value)
            {
                return Err(invalid(
                    "Codex MCP servers require consistent values for shared environment names",
                ));
            }
            shared_env.insert(key.to_ascii_uppercase(), value.clone());
        }
    }
    let identity = crate::SessionId::next();
    for (index, server) in tools.servers.iter().enumerate() {
        let prefix = format!("mcp_servers.{}", server.name);
        // One override per named server. Inherited configuration merging and naming
        // conflicts are provider policy; hosts should choose application-specific names.
        let mut fields = Vec::new();
        match &server.connection {
            Connection::Stdio(command) => {
                fields.push(format!(
                    "command={}",
                    toml_basic_string(validate::path(command)?)
                ));
                fields.push(format!(
                    "args={}",
                    toml_array(server.args.iter().map(String::as_str))
                ));
                fields.push(format!(
                    "env_vars={}",
                    toml_array(server.env.iter().map(|(key, _)| key.as_str()))
                ));
                env.extend(server.env.clone());
            }
            Connection::Http(url) => {
                fields.push(format!("url={}", toml_basic_string(url)));
                let mut headers = Vec::new();
                for (h, (name, value)) in server.headers.iter().enumerate() {
                    let key = format!("AGENT_LINK_MCP_{identity}_{index}_{h}");
                    headers.push(format!(
                        "{}={}",
                        toml_basic_string(name),
                        toml_basic_string(&key)
                    ));
                    env.push((key, value.clone()));
                }
                fields.push(format!("env_http_headers={{{}}}", headers.join(",")));
            }
        }
        fields.push("enabled=true".into());
        args.extend(["-c".into(), format!("{prefix}={{{}}}", fields.join(","))]);
    }
    Ok(())
}

fn toml_array<'a>(values: impl Iterator<Item = &'a str>) -> String {
    format!(
        "[{}]",
        values.map(toml_basic_string).collect::<Vec<_>>().join(",")
    )
}

fn server_json(server: &McpServer, typed: bool) -> Value {
    let mut value = match &server.connection {
        Connection::Stdio(command) => {
            json!({"command":command,"args":server.args,"env":server.env.iter().cloned().collect::<BTreeMap<_,_>>()})
        }
        Connection::Http(url) => {
            json!({"url":url,"headers":server.headers.iter().cloned().collect::<BTreeMap<_,_>>()})
        }
    };
    if typed {
        value["type"] = json!(match server.connection {
            Connection::Stdio(_) => "stdio",
            Connection::Http(_) => "http",
        });
    }
    value
}

fn acp_server(server: &McpServer) -> Value {
    match &server.connection {
        Connection::Stdio(command) => {
            json!({"name":server.name,"command":command,"args":server.args,"env":server.env.iter().map(|(name,value)|json!({"name":name,"value":value})).collect::<Vec<_>>()})
        }
        Connection::Http(url) => {
            json!({"name":server.name,"type":"http","url":url,"headers":server.headers.iter().map(|(name,value)|json!({"name":name,"value":value})).collect::<Vec<_>>()})
        }
    }
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("temporary tool configuration path is not UTF-8"))
}

fn write_private(path: &Path, body: &str) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| Error::Io {
        context: "create private tool configuration".into(),
        source,
    })?;
    file.write_all(body.as_bytes()).map_err(|source| Error::Io {
        context: "write private tool configuration".into(),
        source,
    })
}

pub(super) fn check_conflicts(
    backend: Option<McpBackend>,
    args: &[String],
    env: &[(String, String)],
    has_mcp: bool,
) -> Result<()> {
    if !has_mcp {
        return Ok(());
    }
    if backend != Some(McpBackend::Acp) && args.iter().any(|arg| arg == "--") {
        return Err(invalid(
            "cannot append MCP options after an end-of-options marker",
        ));
    }
    if backend == Some(McpBackend::Claude)
        && args.iter().any(|a| {
            a == "--mcp-config" || a.starts_with("--mcp-config=") || a == "--strict-mcp-config"
        })
    {
        return Err(invalid(
            "launch already configures Claude MCP; combine connections in AgentTools",
        ));
    }
    let mut config_value = false;
    let overrides_mcp = args.iter().take_while(|a| a.as_str() != "--").any(|arg| {
        let value = if config_value {
            config_value = false;
            Some(arg.as_str())
        } else if arg == "-c" || arg == "--config" {
            config_value = true;
            None
        } else {
            arg.strip_prefix("--config=")
                .or_else(|| arg.strip_prefix("-c").map(|s| s.trim_start_matches('=')))
        };
        value.is_some_and(|v| {
            let key = v.split('=').next().unwrap_or("").trim();
            key == "mcp_servers" || key.starts_with("mcp_servers.")
        })
    });
    if backend == Some(McpBackend::Codex) && overrides_mcp {
        return Err(invalid(
            "launch already overrides Codex MCP; combine connections in AgentTools",
        ));
    }
    if env
        .iter()
        .any(|(key, _)| key.to_ascii_uppercase().starts_with("AGENT_LINK_MCP_"))
    {
        return Err(invalid(
            "reserved tool environment prefix already configured",
        ));
    }
    Ok(())
}

fn validate_literal_config(tools: &AgentTools, pi: bool) -> Result<()> {
    let mut pi_names = BTreeSet::new();
    for server in &tools.servers {
        if pi && !pi_names.insert(server.name.replace('-', "_")) {
            return Err(invalid("Pi MCP names collide after hyphen normalization"));
        }
        let connection = match &server.connection {
            Connection::Stdio(path) => validate::path(path)?,
            Connection::Http(url) => url,
        };
        if std::iter::once(connection)
            .chain(server.args.iter().map(String::as_str))
            .chain(
                server
                    .env
                    .iter()
                    .chain(&server.headers)
                    .map(|(_, v)| v.as_str()),
            )
            .any(|value| value.contains("${"))
            || (pi
                && server
                    .env
                    .iter()
                    .chain(&server.headers)
                    .any(|(_, v)| pi_expands_value(v)))
        {
            return Err(invalid(
                "MCP descriptor would trigger native configuration expansion",
            ));
        }
    }
    Ok(())
}

fn pi_expands_value(value: &str) -> bool {
    // Pi's config-value resolver recognizes bare $NAME references and also reduces
    // $$ / $! to literal $ / !. Backslashes do not escape these forms. Braced
    // references are already refused by the shared Claude/Pi guard above.
    value.trim_start().starts_with('!')
        || value.as_bytes().windows(2).any(|pair| {
            pair[0] == b'$'
                && (pair[1].is_ascii_alphabetic() || matches!(pair[1], b'_' | b'$' | b'!'))
        })
}
