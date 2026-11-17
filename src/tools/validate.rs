use super::*;

impl AgentTools {
    pub(super) fn validate(&self) -> Result<()> {
        if self.cli.len().saturating_add(self.servers.len()) > MAX_TOOLS {
            return Err(invalid("too many application tools (maximum 64)"));
        }
        let mut budget = Budget(0);
        pairs(&self.env, false, &mut budget)?;
        let mut cli_paths = BTreeSet::new();
        for cli in &self.cli {
            let executable = path(&cli.executable)?;
            if cli.executable.parent().is_some_and(|parent| {
                // EnvPolicy prepends the original directory verbatim. In particular,
                // Windows join_paths succeeds for semicolons by adding quotes, which
                // would be lost here. Require the encoded value to be unchanged.
                !std::env::join_paths(std::iter::once(parent))
                    .is_ok_and(|encoded| encoded == parent.as_os_str())
            }) {
                return Err(invalid(
                    "CLI parent directory cannot be prepended to PATH without escaping",
                ));
            }
            budget.text(executable)?;
            budget.text(&cli.description)?;
            if cli.description.trim().is_empty() {
                return Err(invalid("CLI description must not be empty"));
            }
            if !cli_paths.insert(executable) {
                return Err(invalid("duplicate CLI executable"));
            }
        }
        let mut names = BTreeSet::new();
        for server in &self.servers {
            if server.name.is_empty()
                || server.name.len() > 64
                || !server
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
            {
                return Err(invalid(
                    "MCP server name must contain 1..64 ASCII letters, digits, underscores or hyphens",
                ));
            }
            if !names.insert(&server.name) {
                return Err(invalid("duplicate MCP server name"));
            }
            budget.text(&server.name)?;
            if server.args.len() > 128 {
                return Err(invalid("too many MCP process arguments"));
            }
            for arg in &server.args {
                budget.text(arg)?;
            }
            pairs(&server.env, false, &mut budget)?;
            pairs(&server.headers, true, &mut budget)?;
            match &server.connection {
                Connection::Stdio(executable) => {
                    budget.text(path(executable)?)?;
                    if !server.headers.is_empty() {
                        return Err(invalid("stdio MCP does not accept HTTP headers"));
                    }
                }
                Connection::Http(url) => {
                    budget.text(url)?;
                    http_url(url)?;
                    if !server.env.is_empty() || !server.args.is_empty() {
                        return Err(invalid(
                            "HTTP MCP does not accept process arguments or environment",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

fn http_url(value: &str) -> Result<()> {
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"));
    // Keep restrictions on the original input: WHATWG parsing can discard whitespace,
    // repair a missing authority or erase empty userinfo. Backslashes in the authority
    // are also repaired to slashes, so do not let them bypass host validation.
    let allowed = rest.is_some_and(|s| {
        let authority = s.split(['/', '?']).next().unwrap_or("");
        !authority.is_empty() && !authority.contains(['@', '\\'])
    }) && !value.chars().any(|c| c.is_whitespace() || c.is_control())
        && !value.contains('#');
    // Validate host syntax (including bracketed IPv6) and a u16 port without DNS or
    // network access. Retain the original URL for rendering rather than normalizing it.
    if !allowed || !url::Url::parse(value).is_ok_and(|parsed| parsed.has_host()) {
        return Err(invalid(
            "MCP URL must be an absolute http(s) URL with a valid host and port, without userinfo, whitespace or a fragment",
        ));
    }
    Ok(())
}

pub(super) fn path(path: &Path) -> Result<&str> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(invalid("tool executable must be an absolute file path"));
    }
    let text = path
        .to_str()
        .ok_or_else(|| invalid("tool executable must be valid UTF-8"))?;
    if text.chars().any(char::is_control) {
        return Err(invalid("tool executable contains a control character"));
    }
    Ok(text)
}

struct Budget(usize);
impl Budget {
    fn text(&mut self, value: &str) -> Result<()> {
        self.0 = self.0.saturating_add(value.len());
        if self.0 > MAX_CONFIGURATION_BYTES {
            return Err(invalid("application tool configuration exceeds 64 KiB"));
        }
        if value.contains('\0') {
            return Err(invalid("application tool configuration contains NUL"));
        }
        Ok(())
    }
}

fn pairs(values: &[(String, String)], headers: bool, budget: &mut Budget) -> Result<()> {
    if values.len() > 64 {
        return Err(invalid("too many tool environment variables or headers"));
    }
    let mut seen = BTreeSet::new();
    for (key, value) in values {
        budget.text(key)?;
        budget.text(value)?;
        let valid = if headers {
            key.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        } else {
            key.bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
        };
        if key.is_empty() || !valid {
            return Err(invalid(
                "invalid tool environment variable or HTTP header name",
            ));
        }
        if !headers && key.to_ascii_uppercase().starts_with("AGENT_LINK_MCP_") {
            return Err(invalid("tool environment uses a reserved prefix"));
        }
        if !seen.insert(key.to_ascii_uppercase()) {
            return Err(invalid(
                "duplicate tool environment variable or HTTP header",
            ));
        }
        if headers && value.chars().any(|c| c.is_control() && c != '\t') {
            return Err(invalid("HTTP header value contains a control character"));
        }
    }
    Ok(())
}
