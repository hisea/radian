//! Loading the MCP server config file into [`McpServerConfig`]s.

use std::collections::BTreeMap;
use std::time::Duration;

use agent_extensions::McpServerConfig;
use serde::Deserialize;

/// The `--mcp-config` file. Unknown fields are rejected so a typo cannot silently drop a
/// setting.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    servers: Vec<ServerEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerEntry {
    id: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    inherit_env: Vec<String>,
    #[serde(default)]
    shared_tools: Vec<String>,
    #[serde(default)]
    call_timeout_secs: Option<u64>,
}

/// Parses and validates the file. Duplicate ids fail here, before any process starts: the
/// id is the tool alias prefix, so two servers sharing one would collide mid-session.
pub(crate) fn load_mcp_config(path: &str) -> Result<Vec<McpServerConfig>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("--mcp-config {path}: {e}"))?;
    let parsed: ConfigFile = serde_json::from_str(&text)
        .map_err(|e| format!("--mcp-config {path}: not valid MCP config JSON: {e}"))?;
    let mut seen = std::collections::BTreeSet::new();
    let mut servers = Vec::with_capacity(parsed.servers.len());
    for entry in parsed.servers {
        if !seen.insert(entry.id.clone()) {
            return Err(format!(
                "--mcp-config {path}: duplicate server id `{}`; ids prefix tool names \
                 and must be unique",
                entry.id
            ));
        }
        let mut config = McpServerConfig::new(entry.id, entry.command);
        for arg in entry.args {
            config = config.arg(arg);
        }
        for (key, value) in entry.env {
            config = config.env(key, value);
        }
        config.inherit_env = entry.inherit_env.into_iter().map(Into::into).collect();
        config.shared_tools = entry.shared_tools;
        if let Some(secs) = entry.call_timeout_secs {
            config.call_timeout = Duration::from_secs(secs);
        }
        servers.push(config);
    }
    Ok(servers)
}
