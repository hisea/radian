//! Loading the `--mcp-config` server config file into [`McpServerConfig`]s.
//!
//! Two JSON forms are accepted, and both normalise to the one internal
//! [`McpServerConfig`]:
//!
//! - Radian's own `{"servers": [...]}`: `id`, `command`, `args`, `env`, `inherit_env`,
//!   `shared_tools`, `call_timeout_secs`, `shutdown_timeout_secs`. Unknown fields are
//!   rejected, so a typo cannot silently drop a setting. `inherit_env` defaults to empty.
//! - The shared `{"mcpServers": {"<name>": {...}}}` object other clients write: the name is
//!   the server id, `command`/`args`/`env` mean the same, and Radian's own extra keys
//!   (`inherit_env`, `shared_tools`, `call_timeout_secs`, `shutdown_timeout_secs`) are
//!   accepted in both forms. Foreign keys (`disabled`, `autoApprove`, `timeout`, …) are
//!   ignored with one warning each entry: this is a foreign format, not a typo. Entries
//!   default to `inherit_env = ["HOME"]` (an unverified inference about `npx`/`uvx`
//!   servers; see the report for the one real-server check).
//!
//! A file with both keys, or with neither, is an error. Only stdio servers run: an entry
//! with a `type` other than `stdio` (or none), or with a `url`, is skipped with one
//! warning line; the session does not fail.
//!
//! `${VAR}` and `${VAR:-default}` in `command`, `args` and `env` values are expanded at
//! load time with Claude Code's semantics. An unset variable with no default expands to
//! empty and gives one warning line. A `${VAR:-default}` uses the default when the
//! variable is unset **or empty** (shell `:-`); a set-but-empty `${VAR}` expands to empty
//! with no warning.

use std::collections::BTreeMap;
use std::time::Duration;

use agent_extensions::McpServerConfig;
use serde::Deserialize;

/// What loading one `--mcp-config` file produced: the servers to start, and the warning
/// lines the host prints once at startup (`[mcp] …` on the CLI, the startup log in the
/// TUI). A bad file is still an error, never a warning: it fails before any process
/// starts.
#[derive(Debug, Default)]
pub struct McpLoad {
    pub servers: Vec<McpServerConfig>,
    pub warnings: Vec<String>,
}

/// The `--mcp-config` file in Radian's own form. Unknown fields are rejected so a typo
/// cannot silently drop a setting.
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
    /// How long `close()` waits for the server to exit once its stdin is closed.
    #[serde(default)]
    shutdown_timeout_secs: Option<u64>,
}

/// One entry of the shared `mcpServers` object. `url` is only read to decide the
/// stdio-only skip; anything not listed here lands in `unknown` and is ignored with a
/// warning.
#[derive(Deserialize)]
struct ForeignEntry {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    url: Option<serde_json::Value>,
    #[serde(default)]
    inherit_env: Option<Vec<String>>,
    #[serde(default)]
    shared_tools: Vec<String>,
    #[serde(default)]
    call_timeout_secs: Option<u64>,
    #[serde(default)]
    shutdown_timeout_secs: Option<u64>,
    #[serde(flatten)]
    unknown: BTreeMap<String, serde_json::Value>,
}

/// Parses and validates the file. Duplicate ids fail here, before any process starts: the
/// id is the tool alias prefix, so two servers sharing one would collide mid-session.
/// (`mcpServers` names are object keys, so they cannot repeat in valid JSON; the ids are
/// used as-is, with no normalisation, and the check below still guards both forms.)
pub fn load_mcp_config(path: &str) -> Result<McpLoad, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("--mcp-config {path}: {e}"))?;
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("--mcp-config {path}: not valid MCP config JSON: {e}"))?;
    let top = parsed
        .as_object()
        .ok_or_else(|| format!("--mcp-config {path}: expected an object at the top level"))?;
    let has_servers = top.contains_key("servers");
    let has_mcp_servers = top.contains_key("mcpServers");
    if has_servers && has_mcp_servers {
        return Err(format!(
            "--mcp-config {path}: `servers` and `mcpServers` cannot be combined; use one form"
        ));
    }
    if has_servers {
        let parsed: ConfigFile = serde_json::from_value(parsed)
            .map_err(|e| format!("--mcp-config {path}: not valid MCP config JSON: {e}"))?;
        let mut load = McpLoad::default();
        let mut seen = std::collections::BTreeSet::new();
        for entry in parsed.servers {
            if !seen.insert(entry.id.clone()) {
                return Err(format!(
                    "--mcp-config {path}: duplicate server id `{}`; ids prefix tool names \
                     and must be unique",
                    entry.id
                ));
            }
            load.servers.push(build_config(
                entry.id,
                entry.command,
                entry.args,
                entry.env,
                entry.inherit_env,
                entry.shared_tools,
                entry.call_timeout_secs,
                entry.shutdown_timeout_secs,
                &mut load.warnings,
            ));
        }
        return Ok(load);
    }
    if has_mcp_servers {
        let mut load = McpLoad::default();
        let mut seen = std::collections::BTreeSet::new();
        let entries = top["mcpServers"]
            .as_object()
            .ok_or_else(|| format!("--mcp-config {path}: `mcpServers` must be an object"))?;
        for (name, raw) in entries {
            if !seen.insert(name.clone()) {
                return Err(format!(
                    "--mcp-config {path}: duplicate server id `{name}`; ids prefix tool names \
                     and must be unique"
                ));
            }
            if let Some(config) = build_foreign(path, name, raw, &mut load.warnings)? {
                load.servers.push(config);
            }
        }
        return Ok(load);
    }
    Err(format!(
        "--mcp-config {path}: expected `servers` or `mcpServers` at the top level"
    ))
}

/// A `servers` entry, with `${VAR}` expansion applied.
#[allow(clippy::too_many_arguments)]
fn build_config(
    id: String,
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    inherit_env: Vec<String>,
    shared_tools: Vec<String>,
    call_timeout_secs: Option<u64>,
    shutdown_timeout_secs: Option<u64>,
    warnings: &mut Vec<String>,
) -> McpServerConfig {
    let command = expand_env(&command, &id, warnings);
    let args: Vec<String> = args
        .iter()
        .map(|arg| expand_env(arg, &id, warnings))
        .collect();
    let env: BTreeMap<String, String> = env
        .into_iter()
        .map(|(key, value)| (key, expand_env(&value, &id, warnings)))
        .collect();
    let mut config = McpServerConfig::new(id, command);
    for arg in args {
        config = config.arg(arg);
    }
    for (key, value) in env {
        config = config.env(key, value);
    }
    config.inherit_env = inherit_env.into_iter().map(Into::into).collect();
    config.shared_tools = shared_tools;
    if let Some(secs) = call_timeout_secs {
        config.call_timeout = Duration::from_secs(secs);
    }
    if let Some(secs) = shutdown_timeout_secs {
        config.shutdown_timeout = Duration::from_secs(secs);
    }
    config
}

/// A `mcpServers` entry: `None` when it is a remote server that is skipped with a
/// warning, an error when a stdio entry is malformed.
fn build_foreign(
    path: &str,
    name: &str,
    raw: &serde_json::Value,
    warnings: &mut Vec<String>,
) -> Result<Option<McpServerConfig>, String> {
    let entry: ForeignEntry = serde_json::from_value(raw.clone())
        .map_err(|e| format!("--mcp-config {path}: server `{name}` is not a valid entry: {e}"))?;
    let is_remote = !matches!(entry.r#type.as_deref(), None | Some("stdio")) || entry.url.is_some();
    if is_remote {
        let detail = match (&entry.r#type, &entry.url) {
            (Some(kind), _) => format!("type `{kind}`"),
            (None, Some(_)) => "has `url`".to_owned(),
            (None, None) => unreachable!("is_remote implies a type or a url"),
        };
        warnings.push(format!(
            "Radian supports only stdio; skipped `{name}` ({detail})"
        ));
        return Ok(None);
    }
    if !entry.unknown.is_empty() {
        let keys: Vec<&str> = entry.unknown.keys().map(String::as_str).collect();
        warnings.push(format!(
            "server `{name}`: ignoring unknown keys: {}",
            keys.join(", ")
        ));
    }
    let Some(command) = entry.command else {
        return Err(format!(
            "--mcp-config {path}: server `{name}` has no `command`; only stdio entries can run"
        ));
    };
    // `mcpServers` entries default to `inherit_env = ["HOME"]`; an explicit list is kept
    // exactly as written. The `servers` form keeps today's default (empty).
    let inherit_env = entry.inherit_env.unwrap_or_else(|| vec!["HOME".to_owned()]);
    Ok(Some(build_config(
        name.to_owned(),
        command,
        entry.args,
        entry.env,
        inherit_env,
        entry.shared_tools,
        entry.call_timeout_secs,
        entry.shutdown_timeout_secs,
        warnings,
    )))
}

/// Expands `${VAR}` and `${VAR:-default}` with Claude Code's semantics: an unset variable
/// with no default expands to empty and gives one warning line. A default containing `$`
/// is used literally (one pass, no recursion); an unterminated `${` is kept literally.
fn expand_env(raw: &str, server: &str, warnings: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let inner = &after[..end];
        rest = &after[end + 1..];
        let (name, default) = match inner.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (inner, None),
        };
        if name.is_empty() {
            out.push_str("${");
            out.push_str(inner);
            out.push('}');
            continue;
        }
        match std::env::var(name) {
            Ok(value) if default.is_none() || !value.is_empty() => out.push_str(&value),
            _ => {
                if let Some(default) = default {
                    out.push_str(default);
                } else {
                    warnings.push(format!(
                        "server `{server}`: environment variable `{name}` is not set; \
                         expanding to empty"
                    ));
                }
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, name: &str, text: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "radian-host-mcp-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_duplicate_server_id_is_refused_before_any_process_starts() {
        let dir = temp_dir("dup");
        let path = write(
            &dir,
            "dup.json",
            r#"{"servers":[{"id":"a","command":"true"},{"id":"a","command":"true"}]}"#,
        );
        let error = load_mcp_config(path.to_str().unwrap()).unwrap_err();
        assert!(error.contains("duplicate server id"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_servers_form_keeps_todays_defaults_and_rejects_unknown_fields() {
        let dir = temp_dir("servers-form");
        let path = write(
            &dir,
            "servers.json",
            r#"{"servers":[{"id":"a","command":"true","args":["x"],"env":{"K":"v"},
                "shared_tools":["echo"],
                "call_timeout_secs":7,"shutdown_timeout_secs":8}]}"#,
        );
        let load = load_mcp_config(path.to_str().unwrap()).unwrap();
        assert!(load.warnings.is_empty(), "{:?}", load.warnings);
        assert_eq!(load.servers.len(), 1);
        let server = &load.servers[0];
        assert_eq!(server.id, "a");
        assert!(server.inherit_env.is_empty(), "{:?}", server.inherit_env);
        assert_eq!(server.call_timeout, Duration::from_secs(7));
        assert_eq!(server.shutdown_timeout, Duration::from_secs(8));

        let bad = write(
            &dir,
            "bad.json",
            r#"{"servers":[{"id":"a","command":"true","bogus":1}]}"#,
        );
        let error = load_mcp_config(bad.to_str().unwrap()).unwrap_err();
        assert!(error.contains("bogus"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_with_both_forms_or_neither_is_an_error() {
        let dir = temp_dir("both-neither");
        let both = write(&dir, "both.json", r#"{"servers":[],"mcpServers":{}}"#);
        let error = load_mcp_config(both.to_str().unwrap()).unwrap_err();
        assert!(error.contains("cannot be combined"), "{error}");
        let neither = write(&dir, "neither.json", r#"{"other":[]}"#);
        let error = load_mcp_config(neither.to_str().unwrap()).unwrap_err();
        assert!(
            error.contains("expected `servers` or `mcpServers`"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_claude_docs_fixture_starts_stdio_and_skips_http_with_a_warning() {
        // `RADIAN_MCP_SLICE_A_SET` is set below; `RADIAN_MCP_SLICE_A_MISSING` is never set.
        unsafe {
            std::env::set_var("RADIAN_MCP_SLICE_A_SET", "from-env");
        }
        let dir = temp_dir("claude-docs");
        let path = write(
            &dir,
            "claude.json",
            r#"{"mcpServers":{
                "filesystem": {"command": "npx", "args": ["-y", "server-filesystem", "${RADIAN_MCP_SLICE_A_SET}"],
                    "env": {"WITH_DEFAULT": "${RADIAN_MCP_SLICE_A_MISSING:-fallback}", "EMPTY_OK": "x"}},
                "web": {"type": "http", "url": "https://example.com/mcp"}}}"#,
        );
        let load = load_mcp_config(path.to_str().unwrap()).unwrap();
        assert_eq!(load.servers.len(), 1);
        let server = &load.servers[0];
        assert_eq!(server.id, "filesystem");
        assert_eq!(
            server.inherit_env,
            vec![std::ffi::OsString::from("HOME")],
            "{:?}",
            server.inherit_env
        );
        assert_eq!(
            server.args,
            vec![
                std::ffi::OsString::from("-y"),
                std::ffi::OsString::from("server-filesystem"),
                std::ffi::OsString::from("from-env"),
            ]
        );
        let env: BTreeMap<String, String> = server
            .env
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert_eq!(env["WITH_DEFAULT"], "fallback");
        assert_eq!(
            load.warnings
                .iter()
                .filter(|w| w.contains("skipped"))
                .count(),
            1,
            "{:?}",
            load.warnings
        );
        assert!(
            load.warnings
                .iter()
                .any(|w| w.contains("skipped `web`") && w.contains("only stdio")),
            "{:?}",
            load.warnings
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unset_variable_expands_to_empty_with_one_warning() {
        let dir = temp_dir("unset-var");
        let path = write(
            &dir,
            "unset.json",
            r#"{"mcpServers":{"a":{"command":"echo ${RADIAN_MCP_SLICE_A_NEVER_SET_1}",
                "args":["${RADIAN_MCP_SLICE_A_NEVER_SET_2}"],
                "env":{"K":"pre-${RADIAN_MCP_SLICE_A_NEVER_SET_3}-post"}}}}"#,
        );
        let load = load_mcp_config(path.to_str().unwrap()).unwrap();
        assert_eq!(load.servers.len(), 1);
        let server = &load.servers[0];
        assert_eq!(server.command, std::path::PathBuf::from("echo "));
        assert_eq!(server.args, vec![std::ffi::OsString::from("")]);
        let value = server.env[0].1.to_string_lossy().into_owned();
        assert_eq!(value, "pre--post");
        let var_warnings: Vec<&String> = load
            .warnings
            .iter()
            .filter(|w| w.contains("expanding to empty"))
            .collect();
        assert_eq!(var_warnings.len(), 3, "{:?}", load.warnings);
        assert!(
            var_warnings.iter().all(|w| w.contains("server `a`")),
            "{:?}",
            load.warnings
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_default_applies_when_unset_or_empty_and_a_set_value_wins() {
        unsafe {
            std::env::set_var("RADIAN_MCP_SLICE_A_WINS", "kept");
            std::env::set_var("RADIAN_MCP_SLICE_A_EMPTY", "");
        }
        let dir = temp_dir("defaults");
        let path = write(
            &dir,
            "defaults.json",
            r#"{"mcpServers":{"a":{"command":"${RADIAN_MCP_SLICE_A_NEVER_SET_4:-dflt}",
                "args": ["${RADIAN_MCP_SLICE_A_WINS:-dflt}", "${RADIAN_MCP_SLICE_A_EMPTY:-dflt}"],
                "env": {"K": "${RADIAN_MCP_SLICE_A_WINS:-dflt}"}}}}"#,
        );
        let load = load_mcp_config(path.to_str().unwrap()).unwrap();
        assert!(
            load.warnings.is_empty(),
            "defaults must not warn: {:?}",
            load.warnings
        );
        let server = &load.servers[0];
        assert_eq!(server.command, std::path::PathBuf::from("dflt"));
        assert_eq!(
            server.args,
            vec![
                std::ffi::OsString::from("kept"),
                std::ffi::OsString::from("dflt"),
            ]
        );
        assert_eq!(server.env[0].1.to_string_lossy(), "kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn foreign_keys_are_ignored_with_one_warning_and_explicit_inherit_env_is_exact() {
        let dir = temp_dir("foreign-keys");
        let path = write(
            &dir,
            "foreign.json",
            r#"{"mcpServers":{"a":{"command":"true","disabled":false,
                "autoApprove":["x"],"timeout":30,"inherit_env":["FOO"]}}}"#,
        );
        let load = load_mcp_config(path.to_str().unwrap()).unwrap();
        assert_eq!(load.servers.len(), 1);
        assert_eq!(
            load.servers[0].inherit_env,
            vec![std::ffi::OsString::from("FOO")],
            "an explicit list is kept exactly, with no HOME added"
        );
        let unknown: Vec<&String> = load
            .warnings
            .iter()
            .filter(|w| w.contains("unknown keys"))
            .collect();
        assert_eq!(unknown.len(), 1, "{:?}", load.warnings);
        assert!(unknown[0].contains("disabled"), "{:?}", load.warnings);
        assert!(unknown[0].contains("autoApprove"), "{:?}", load.warnings);
        assert!(unknown[0].contains("timeout"), "{:?}", load.warnings);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_types_and_urls_are_skipped_while_stdio_runs() {
        let dir = temp_dir("remote-skip");
        let path = write(
            &dir,
            "remote.json",
            r#"{"mcpServers":{
                "plain": {"command": "true"},
                "typed": {"command": "true", "type": "stdio"},
                "http": {"type": "http"},
                "sse": {"type": "sse", "url": "https://example.com/sse"},
                "ws": {"type": "ws"},
                "url_only": {"url": "https://example.com/mcp"}}}"#,
        );
        let load = load_mcp_config(path.to_str().unwrap()).unwrap();
        let ids: Vec<&str> = load.servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["plain", "typed"]);
        assert_eq!(
            load.warnings
                .iter()
                .filter(|w| w.contains("skipped"))
                .count(),
            4,
            "{:?}",
            load.warnings
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stdio_entry_without_a_command_is_an_error() {
        let dir = temp_dir("no-command");
        let path = write(
            &dir,
            "no-command.json",
            r#"{"mcpServers":{"a":{"args":[]}}}"#,
        );
        let error = load_mcp_config(path.to_str().unwrap()).unwrap_err();
        assert!(
            error.contains("`a`") && error.contains("command"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn radian_keys_are_accepted_in_both_forms() {
        let dir = temp_dir("radian-keys");
        let path = write(
            &dir,
            "both-forms.json",
            r#"{"mcpServers":{"a":{"command":"true","shared_tools":["echo"],
                "call_timeout_secs":9,"shutdown_timeout_secs":10}}}"#,
        );
        let load = load_mcp_config(path.to_str().unwrap()).unwrap();
        assert!(load.warnings.is_empty(), "{:?}", load.warnings);
        assert_eq!(load.servers[0].shared_tools, vec!["echo".to_owned()]);
        assert_eq!(load.servers[0].call_timeout, Duration::from_secs(9));
        assert_eq!(load.servers[0].shutdown_timeout, Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
