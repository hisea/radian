//! Loading MCP server configs into [`McpServerConfig`]s: the `--mcp-config` file, the
//! user's `[mcp]` section, and the project `.mcp.json`.
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
//!
//! The project `.mcp.json` (`Scope::Project`) blanks [`CREDENTIAL_VARS`] wherever they are
//! referenced — `command`, `args`, `env` values and `inherit_env` — with one warning per
//! reference, so a trusted project file cannot carry the user's shell credentials into its
//! servers. User entries (`Scope::User`) expand as today.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_extensions::McpServerConfig;
use serde::Deserialize;

/// Credential variables blanked in project `.mcp.json` entries (
/// revised round 1: the table protects the user's shell keys from project servers,
/// built-in or not): each built-in provider's customary key variable, plus the common
/// third-party provider keys. A fixed table, never a `*_API_KEY` pattern; synced when a
/// built-in provider is added. The exact list was meant to come from models.dev's
/// per-provider `env`, but the bundled snapshot carries none (unverified), so these names
/// come from this code base and its docs. Radian's own keys are never in process env
/// (ADR 0026 D2 reads `auth.json`); this table only stops a project file from inheriting
/// the user's shell keys. `chatgpt` is OAuth-only and has no key variable.
pub const CREDENTIAL_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENCODE_API_KEY",
    "OPENCODE_GO_API_KEY",
    "DEEPSEEK_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENROUTER_API_KEY",
    "GROQ_API_KEY",
    "XAI_API_KEY",
    "MISTRAL_API_KEY",
];

/// Which source an entry is loaded from. User sources (`--mcp-config`, `[mcp]`) expand
/// `${VAR}` against the process environment as today; project entries (`.mcp.json`)
/// additionally blank [`CREDENTIAL_VARS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoadScope {
    #[default]
    User,
    Project,
}

/// What loading one `--mcp-config` file produced: the servers to start, and the warning
/// lines the host prints once at startup (`[mcp] …` on the CLI, the startup log in the
/// TUI). A bad file is still an error, never a warning: it fails before any process
/// starts.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
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
    load_json_value(&parsed, &format!("--mcp-config {path}"), LoadScope::User)
}

/// Loads the `[mcp]` section of the user's `config.toml`: the same two forms as
/// `--mcp-config`, spelled in TOML (`[mcp.mcpServers.<name>]` tables or `[[mcp.servers]]`
/// entries). One normaliser: the TOML value is converted to JSON and runs through the
/// same loader the file uses, so the rules (both/neither is an error, duplicate ids are
/// refused, remote entries are skipped with a warning, `${VAR}` is expanded) cannot
/// drift. `origin` names the section in errors (the config path, with ` [mcp]`).
pub fn load_mcp_from_toml(mcp: &toml::Value, origin: &str) -> Result<McpLoad, String> {
    let json = serde_json::to_value(mcp)
        .map_err(|e| format!("{origin}: [mcp] is not valid MCP config: {e}"))?;
    load_json_value(&json, origin, LoadScope::User)
}

/// The project file: `<workspace>/.mcp.json`.
pub fn project_path(workspace: &Path) -> PathBuf {
    workspace.join(".mcp.json")
}

/// Loads the project `.mcp.json` at `path` in [`LoadScope::Project`]: `${VAR}` expands as
/// usual, except [`CREDENTIAL_VARS`] expand to empty. `origin` names the file in errors
/// (`<path>`), like `--mcp-config <path>` does for the flag's file.
pub fn load_project_file(path: &Path) -> Result<McpLoad, String> {
    let origin = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|e| format!("{origin}: {e}"))?;
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("{origin}: not valid MCP config JSON: {e}"))?;
    load_json_value(&parsed, &origin, LoadScope::Project)
}

/// Loads `<workspace>/.mcp.json` for a session that may start its servers (ADR 0031 T1:
/// only when the directory is trusted), and reports what the host prints as `[mcp] …`.
///
/// - No file: nothing loads, no lines.
/// - `ignore_implicit` (`--no-config-mcp`, the `explore` child's flag): nothing loads; one
///   line says the file was skipped.
/// - Untrusted: no process starts; one line says so, in the shape of ADR 0031 T3's
///   non-interactive line.
/// - Trusted but unreadable: nothing loads; one warning line (a project file must not break
///   startup, like `<workspace>/.radian/config.toml`).
/// - Trusted and readable: the servers plus one loaded line and the loader's warnings.
pub fn load_project(
    workspace: &Path,
    trusted: bool,
    ignore_implicit: bool,
) -> (McpLoad, Vec<String>) {
    let path = project_path(workspace);
    if !path.is_file() {
        return (McpLoad::default(), Vec::new());
    }
    let origin = path.display().to_string();
    if ignore_implicit {
        return (
            McpLoad::default(),
            vec![format!(
                "project .mcp.json skipped (--no-config-mcp): {origin}"
            )],
        );
    }
    if !trusted {
        return (
            McpLoad::default(),
            vec![format!(
                "project .mcp.json not loaded (untrusted): pass --trust to load {origin}, \
                 or run radian interactively"
            )],
        );
    }
    match load_project_file(&path) {
        Ok(load) => {
            let mut lines = vec![format!(
                "project .mcp.json: loaded {} server{} from {origin}",
                load.servers.len(),
                if load.servers.len() == 1 { "" } else { "s" },
            )];
            lines.extend(load.warnings.iter().cloned());
            (load, lines)
        }
        Err(error) => (
            McpLoad::default(),
            vec![format!(
                "project .mcp.json cannot be read ({error}); skipped"
            )],
        ),
    }
}

/// One project entry as written, for the trust question (ADR 0031 T3): the name, the
/// `command` + `args` **before** `${VAR}` expansion, and the `env` **key names only** —
/// never values. `skipped` names why a remote entry will not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPreviewEntry {
    pub name: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env_keys: Vec<String>,
    pub skipped: Option<String>,
}

/// The trust question's view of `<workspace>/.mcp.json`. `None` when there is no file;
/// `error` when the file exists but cannot be read or parsed (the loader's message, which
/// names the problem, never a value).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectPreview {
    pub entries: Vec<ProjectPreviewEntry>,
    pub error: Option<String>,
}

/// Renders a [`ProjectPreview`] as trust-question lines, shared by the `radian exec`
/// prompt and the TUI modal: one line per server (the name, the `command` + `args` as
/// written, the `env` key names only — never values), remote entries marked skipped, and
/// the read/parse error when the file cannot be used.
pub fn preview_lines(preview: &ProjectPreview) -> Vec<String> {
    if let Some(error) = &preview.error {
        return vec![format!("project .mcp.json: {error}")];
    }
    if preview.entries.is_empty() {
        return vec!["project .mcp.json: no servers to load".to_owned()];
    }
    let runnable = preview
        .entries
        .iter()
        .filter(|entry| entry.skipped.is_none())
        .count();
    let mut lines = vec![format!(
        "project .mcp.json: {} server{} to load once trusted:",
        runnable,
        if runnable == 1 { "" } else { "s" },
    )];
    for entry in &preview.entries {
        lines.push(preview_entry_line(entry));
    }
    lines
}

/// One trust-question line for a project entry. Values never appear: the command and
/// args are as written (before `${VAR}` expansion) and only env key names are named.
fn preview_entry_line(entry: &ProjectPreviewEntry) -> String {
    if let Some(detail) = &entry.skipped {
        return format!("mcp: {} skipped ({detail})", entry.name);
    }
    let mut command = entry
        .command
        .clone()
        .unwrap_or_else(|| "(no command)".to_owned());
    for arg in &entry.args {
        command.push(' ');
        command.push_str(arg);
    }
    if entry.env_keys.is_empty() {
        format!("mcp: {} `{command}`", entry.name)
    } else {
        format!(
            "mcp: {} `{command}` env: {}",
            entry.name,
            entry.env_keys.join(", ")
        )
    }
}

/// Reads the raw entries of `<workspace>/.mcp.json` for the trust question. The loader runs
/// first purely as the structural check, so the preview lists exactly what a trusted load
/// would start (plus the skipped remote entries, which the loader drops with a warning).
/// Values are never carried: commands and args as written, env key names only.
pub fn preview_project(workspace: &Path) -> Option<ProjectPreview> {
    let path = project_path(workspace);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            return Some(ProjectPreview {
                entries: Vec::new(),
                error: Some(format!("{}: {error}", path.display())),
            });
        }
    };
    let origin = path.display().to_string();
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Some(ProjectPreview {
                entries: Vec::new(),
                error: Some(format!("{origin}: not valid MCP config JSON: {error}")),
            });
        }
    };
    // The structural check: both/neither forms, duplicate ids, a stdio entry without a
    // command. Its message is the preview's error, so the question and the load agree.
    if let Err(error) = load_json_value(&parsed, &origin, LoadScope::Project) {
        return Some(ProjectPreview {
            entries: Vec::new(),
            error: Some(error),
        });
    }
    // The load succeeded, so the shapes below are known; anything unexpected here is
    // simply left out (the load is the authority, not the preview).
    let mut entries = Vec::new();
    let top = parsed.as_object().expect("the load succeeded on an object");
    if let Some(servers) = top.get("servers").and_then(|value| value.as_array()) {
        for entry in servers {
            entries.push(ProjectPreviewEntry {
                name: entry
                    .get("id")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                command: entry
                    .get("command")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned),
                args: raw_args(entry.get("args")),
                env_keys: raw_env_keys(entry.get("env")),
                skipped: None,
            });
        }
    }
    if let Some(servers) = top.get("mcpServers").and_then(|value| value.as_object()) {
        for (name, raw) in servers {
            let kind = raw.get("type").and_then(|value| value.as_str());
            let has_url = raw.get("url").is_some_and(|url| !url.is_null());
            entries.push(ProjectPreviewEntry {
                name: name.clone(),
                command: raw
                    .get("command")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned),
                args: raw_args(raw.get("args")),
                env_keys: raw_env_keys(raw.get("env")),
                skipped: remote_reason(kind, has_url)
                    .map(|detail| format!("Radian supports only stdio; skipped ({detail})",)),
            });
        }
    }
    Some(ProjectPreview {
        entries,
        error: None,
    })
}

/// `args` as written (non-string items are left out; the load is the authority).
fn raw_args(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|value| value.as_array())
        .map(|args| {
            args.iter()
                .filter_map(|arg| arg.as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// `env` key names only — never values.
fn raw_env_keys(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|value| value.as_object())
        .map(|env| env.keys().cloned().collect())
        .unwrap_or_default()
}

/// Merges the `[mcp]` servers with the `--mcp-config` file's: the command line wins. A
/// colliding id keeps the config's position with the file's entry, plus one warning
/// naming the id (printed as `[mcp] …` like every other MCP warning).
pub fn merge_mcp(config: McpLoad, file: McpLoad) -> McpLoad {
    merge_named(config, "[mcp]", file, "--mcp-config")
}

/// Merges the project `.mcp.json` under the user's own sources: `[mcp]` and `--mcp-config`
/// both win a collision with a project entry, with one warning naming the id and the
/// winning side. The user's sources keep their own precedence (the command line over the
/// config file, with [`merge_mcp`]'s wording), so a project file can never shadow what the
/// user wrote themselves.
pub fn merge_project_mcp(project: McpLoad, config: McpLoad, file: McpLoad) -> McpLoad {
    let mut warnings = project.warnings;
    warnings.extend(config.warnings);
    warnings.extend(file.warnings);
    let mut servers = project.servers;
    // Where each id came from, for the collision line. `&'static` because the only
    // sources are the three spellings below.
    let mut source: BTreeMap<String, &'static str> = BTreeMap::new();
    for server in &servers {
        source.insert(server.id.clone(), ".mcp.json");
    }
    for server in config.servers {
        if let Some(previous) = source.get(server.id.as_str()) {
            warnings.push(format!(
                "server `{}` is set in both {previous} and [mcp]; using [mcp]",
                server.id
            ));
            let pos = servers
                .iter()
                .position(|existing| existing.id == server.id)
                .expect("the source map and the servers agree");
            servers[pos] = server;
        } else {
            source.insert(server.id.clone(), "[mcp]");
            servers.push(server);
        }
    }
    for server in file.servers {
        if let Some(previous) = source.get(server.id.as_str()) {
            warnings.push(format!(
                "server `{}` is set in both {previous} and --mcp-config; using --mcp-config",
                server.id
            ));
            let pos = servers
                .iter()
                .position(|existing| existing.id == server.id)
                .expect("the source map and the servers agree");
            servers[pos] = server;
        } else {
            source.insert(server.id.clone(), "--mcp-config");
            servers.push(server);
        }
    }
    McpLoad { servers, warnings }
}

/// Merges two loads with the overlay winning: a colliding id keeps the base's position
/// with the overlay's entry, plus one warning naming the id and both sides.
fn merge_named(base: McpLoad, base_label: &str, overlay: McpLoad, overlay_label: &str) -> McpLoad {
    let mut warnings = base.warnings;
    warnings.extend(overlay.warnings);
    let mut servers = base.servers;
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    for (pos, server) in servers.iter().enumerate() {
        index.insert(server.id.clone(), pos);
    }
    for server in overlay.servers {
        if let Some(&pos) = index.get(server.id.as_str()) {
            warnings.push(format!(
                "server `{}` is set in both {base_label} and {overlay_label}; \
                 using {overlay_label}",
                server.id
            ));
            servers[pos] = server;
        } else {
            index.insert(server.id.clone(), servers.len());
            servers.push(server);
        }
    }
    McpLoad { servers, warnings }
}

/// The shared normaliser behind [`load_mcp_config`], [`load_mcp_from_toml`] and
/// [`load_project_file`]: `origin` is `--mcp-config <path>` for a file, `<path> [mcp]` for
/// the config section, and `<path>` for the project file, so all spellings fail with the
/// same words. `scope` decides whether [`CREDENTIAL_VARS`] are blanked.
fn load_json_value(
    parsed: &serde_json::Value,
    origin: &str,
    scope: LoadScope,
) -> Result<McpLoad, String> {
    let top = parsed
        .as_object()
        .ok_or_else(|| format!("{origin}: expected an object at the top level"))?;
    let has_servers = top.contains_key("servers");
    let has_mcp_servers = top.contains_key("mcpServers");
    if has_servers && has_mcp_servers {
        return Err(format!(
            "{origin}: `servers` and `mcpServers` cannot be combined; use one form"
        ));
    }
    if has_servers {
        let parsed: ConfigFile = serde_json::from_value(parsed.clone())
            .map_err(|e| format!("{origin}: not valid MCP config JSON: {e}"))?;
        let mut load = McpLoad::default();
        let mut seen = std::collections::BTreeSet::new();
        for entry in parsed.servers {
            if !seen.insert(entry.id.clone()) {
                return Err(format!(
                    "{origin}: duplicate server id `{}`; ids prefix tool names \
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
                scope,
            ));
        }
        return Ok(load);
    }
    if has_mcp_servers {
        let mut load = McpLoad::default();
        let mut seen = std::collections::BTreeSet::new();
        let entries = top["mcpServers"]
            .as_object()
            .ok_or_else(|| format!("{origin}: `mcpServers` must be an object"))?;
        for (name, raw) in entries {
            if !seen.insert(name.clone()) {
                return Err(format!(
                    "{origin}: duplicate server id `{name}`; ids prefix tool names \
                     and must be unique"
                ));
            }
            if let Some(config) = build_foreign(origin, name, raw, &mut load.warnings, scope)? {
                load.servers.push(config);
            }
        }
        return Ok(load);
    }
    Err(format!(
        "{origin}: expected `servers` or `mcpServers` at the top level"
    ))
}

/// A `servers` entry, with `${VAR}` expansion applied. In [`LoadScope::Project`],
/// [`CREDENTIAL_VARS`] expand to empty and are dropped from `inherit_env`.
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
    scope: LoadScope,
) -> McpServerConfig {
    let command = expand_env(&command, &id, warnings, scope);
    let args: Vec<String> = args
        .iter()
        .map(|arg| expand_env(arg, &id, warnings, scope))
        .collect();
    let env: BTreeMap<String, String> = env
        .into_iter()
        .map(|(key, value)| (key, expand_env(&value, &id, warnings, scope)))
        .collect();
    let inherit_env: Vec<String> = inherit_env
        .into_iter()
        .filter(|name| {
            if scope == LoadScope::Project && CREDENTIAL_VARS.contains(&name.as_str()) {
                warnings.push(format!(
                    "server `{id}`: credential variable `{name}` is not inherited by \
                     project servers; removed from inherit_env"
                ));
                return false;
            }
            true
        })
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
    origin: &str,
    name: &str,
    raw: &serde_json::Value,
    warnings: &mut Vec<String>,
    scope: LoadScope,
) -> Result<Option<McpServerConfig>, String> {
    let entry: ForeignEntry = serde_json::from_value(raw.clone())
        .map_err(|e| format!("{origin}: server `{name}` is not a valid entry: {e}"))?;
    if let Some(detail) = remote_reason(entry.r#type.as_deref(), entry.url.is_some()) {
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
            "{origin}: server `{name}` has no `command`; only stdio entries can run"
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
        scope,
    )))
}

/// Why a `mcpServers` entry will not start: `Some(detail)` for a remote entry (`type`
/// other than `stdio`, or a `url`), `None` for a stdio entry. Shared by the loader (which
/// skips with a warning) and the trust-question preview (which lists the skip).
fn remote_reason(kind: Option<&str>, has_url: bool) -> Option<String> {
    if matches!(kind, None | Some("stdio")) && !has_url {
        return None;
    }
    Some(match kind {
        Some(kind) => format!("type `{kind}`"),
        None => "has `url`".to_owned(),
    })
}

/// Expands `${VAR}` and `${VAR:-default}` with Claude Code's semantics: an unset variable
/// with no default expands to empty and gives one warning line. A default containing `$`
/// is used literally (one pass, no recursion); an unterminated `${` is kept literally.
/// In [`LoadScope::Project`], [`CREDENTIAL_VARS`] expand to empty with their own warning,
/// even when set (and even with a default): a project file never receives the user's
/// shell credentials.
fn expand_env(raw: &str, server: &str, warnings: &mut Vec<String>, scope: LoadScope) -> String {
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
        if scope == LoadScope::Project && CREDENTIAL_VARS.contains(&name) {
            warnings.push(format!(
                "server `{server}`: credential variable `{name}` is not passed to \
                 project servers; expanding to empty"
            ));
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

    fn server(id: &str, command: &str) -> McpServerConfig {
        McpServerConfig::new(id, command)
    }

    /// The merge the hosts run: both sources together, the `--mcp-config` file winning a
    /// collision with one warning that names the id. The winner keeps the config's
    /// position, so the order is stable no matter which side is read first.
    #[test]
    fn on_a_collision_the_command_line_wins_with_one_warning_naming_the_id() {
        let config = McpLoad {
            servers: vec![
                server("shared", "from-config"),
                server("only-config", "true"),
            ],
            warnings: vec!["config warning".to_owned()],
        };
        let file = McpLoad {
            servers: vec![server("shared", "from-file"), server("only-file", "true")],
            warnings: vec!["file warning".to_owned()],
        };
        let merged = merge_mcp(config, file);
        let ids: Vec<&str> = merged.servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["shared", "only-config", "only-file"]);
        let shared = &merged.servers[0];
        assert_eq!(
            shared.command,
            std::path::PathBuf::from("from-file"),
            "the file's entry must win"
        );
        assert_eq!(
            merged.warnings.len(),
            3,
            "both sources' warnings plus the one collision line: {:?}",
            merged.warnings
        );
        assert!(merged.warnings.contains(&"config warning".to_owned()));
        assert!(merged.warnings.contains(&"file warning".to_owned()));
        let collisions: Vec<&String> = merged
            .warnings
            .iter()
            .filter(|w| w.contains("--mcp-config"))
            .collect();
        assert_eq!(collisions.len(), 1, "{:?}", merged.warnings);
        assert!(collisions[0].contains("`shared`"), "{:?}", merged.warnings);
    }

    /// Without a collision the merge only concatenates, config first.
    #[test]
    fn without_a_collision_the_merge_concatenates_config_first() {
        let config = McpLoad {
            servers: vec![server("a", "true")],
            warnings: Vec::new(),
        };
        let file = McpLoad {
            servers: vec![server("b", "true")],
            warnings: Vec::new(),
        };
        let merged = merge_mcp(config, file);
        let ids: Vec<&str> = merged.servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert!(merged.warnings.is_empty());
    }

    /// `[mcp]` in TOML runs through the same loader as the file: a `mcpServers` table
    /// normalises with the `HOME` default, and both forms at once is an error.
    #[test]
    fn the_toml_section_runs_through_the_same_loader_as_the_file() {
        let value: toml::Value =
            toml::from_str("[mcpServers.files]\ncommand = \"/work/example/mcp-files\"\n").unwrap();
        let load = load_mcp_from_toml(&value, "test-config [mcp]").unwrap();
        assert_eq!(load.servers.len(), 1);
        assert_eq!(load.servers[0].id, "files");
        assert_eq!(
            load.servers[0].inherit_env,
            vec![std::ffi::OsString::from("HOME")]
        );
        let both: toml::Value =
            toml::from_str("[[servers]]\nid = \"a\"\ncommand = \"true\"\n[mcpServers]\n").unwrap();
        let error = load_mcp_from_toml(&both, "test-config [mcp]").unwrap_err();
        assert!(error.contains("cannot be combined"), "{error}");
        assert!(error.starts_with("test-config [mcp]:"), "{error}");
    }

    /// B2: a project entry blanks [`CREDENTIAL_VARS`] in `command`, `args` and `env`
    /// values — even when set — with one warning per reference; the same entry as a user
    /// source expands them as today. The mutation (project entries expand credentials)
    /// fails this test.
    #[test]
    fn project_entries_blank_credential_variables_but_user_entries_expand_them() {
        unsafe {
            std::env::set_var("RADIAN_MCP_B2_KEY", "from-env");
            std::env::set_var("ANTHROPIC_API_KEY", "sk-b2-live");
            // Round 1: a common third-party provider key is in the table too.
            std::env::set_var("DEEPSEEK_API_KEY", "sk-b2-deepseek");
        }
        let dir = temp_dir("credential-blank");
        let text = r#"{"mcpServers":{"a":{
            "command": "run ${ANTHROPIC_API_KEY}",
            "args": ["${RADIAN_MCP_B2_KEY}", "${ANTHROPIC_API_KEY:-dflt}"],
            "env": {"TOKEN": "${ANTHROPIC_API_KEY}", "OTHER": "${RADIAN_MCP_B2_KEY}",
                "ENGINE": "${DEEPSEEK_API_KEY}"}}}}"#;
        let user_path = write(&dir, "user.json", text);
        let user = load_mcp_config(user_path.to_str().unwrap()).unwrap();
        assert!(user.warnings.is_empty(), "{:?}", user.warnings);
        let server = &user.servers[0];
        assert_eq!(
            server.command,
            std::path::PathBuf::from("run sk-b2-live"),
            "user entries keep today's expansion"
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
        assert_eq!(env["TOKEN"], "sk-b2-live");
        assert_eq!(env["OTHER"], "from-env");
        assert_eq!(env["ENGINE"], "sk-b2-deepseek");

        let project_path = write(&dir, ".mcp.json", text);
        let project = load_project_file(&project_path).unwrap();
        let server = &project.servers[0];
        assert_eq!(
            server.command,
            std::path::PathBuf::from("run "),
            "project entries blank the credential even when it is set"
        );
        assert_eq!(
            server.args,
            vec![
                std::ffi::OsString::from("from-env"),
                std::ffi::OsString::from(""),
            ],
            "a plain variable still expands; a credential with a default is still blank"
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
        assert_eq!(env["TOKEN"], "");
        assert_eq!(env["OTHER"], "from-env");
        assert_eq!(env["ENGINE"], "", "third-party keys blank too");
        // One warning per credential reference: command, one arg, two env values.
        let blanked: Vec<&String> = project
            .warnings
            .iter()
            .filter(|w| w.contains("credential variable"))
            .collect();
        assert_eq!(blanked.len(), 4, "{:?}", project.warnings);
        assert!(
            blanked.iter().any(|w| w.contains("`ANTHROPIC_API_KEY`"))
                && blanked.iter().any(|w| w.contains("`DEEPSEEK_API_KEY`")),
            "{:?}",
            project.warnings
        );
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B2: `inherit_env` cannot smuggle a credential into a project server: the name is
    /// dropped with a warning, while a user entry keeps it exactly.
    #[test]
    fn project_inherit_env_drops_credential_variables() {
        let dir = temp_dir("credential-inherit");
        let text = r#"{"mcpServers":{"a":{"command":"true",
            "inherit_env":["HOME","OPENAI_API_KEY"]}}}"#;
        let user_path = write(&dir, "user.json", text);
        let user = load_mcp_config(user_path.to_str().unwrap()).unwrap();
        assert!(user.warnings.is_empty(), "{:?}", user.warnings);
        assert_eq!(
            user.servers[0].inherit_env,
            vec![
                std::ffi::OsString::from("HOME"),
                std::ffi::OsString::from("OPENAI_API_KEY"),
            ]
        );
        let project_path = write(&dir, ".mcp.json", text);
        let project = load_project_file(&project_path).unwrap();
        assert_eq!(
            project.servers[0].inherit_env,
            vec![std::ffi::OsString::from("HOME")],
            "the credential must not reach the child environment"
        );
        assert_eq!(project.warnings.len(), 1, "{:?}", project.warnings);
        assert!(
            project.warnings[0].contains("`OPENAI_API_KEY`")
                && project.warnings[0].contains("inherit"),
            "{:?}",
            project.warnings
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B2: `load_project` gates on trust. Untrusted: no servers, one line in ADR 0031 T3's
    /// shape. Trusted: the servers plus the loaded line. The mutation (untrusted loads too)
    /// fails the untrusted assertion; a malformed project file warns and loads nothing
    /// instead of breaking startup.
    #[test]
    fn load_project_gates_on_trust_and_never_breaks_startup() {
        let dir = temp_dir("project-gate");
        // No file: nothing loads, no lines.
        let (load, lines) = load_project(&dir, false, false);
        assert!(load.servers.is_empty());
        assert!(lines.is_empty(), "{lines:?}");

        write(
            &dir,
            ".mcp.json",
            r#"{"mcpServers":{"a":{"command":"true"}}}"#,
        );
        // Untrusted: no process may start; the line says so and names `--trust`.
        let (load, lines) = load_project(&dir, false, false);
        assert!(load.servers.is_empty(), "untrusted must start nothing");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("not loaded (untrusted)") && lines[0].contains("--trust"),
            "{lines:?}"
        );
        // `--no-config-mcp` (the `explore` child): skipped even when trusted.
        let (load, lines) = load_project(&dir, true, true);
        assert!(load.servers.is_empty());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("--no-config-mcp"), "{lines:?}");
        // Trusted: the server plus the loaded line.
        let (load, lines) = load_project(&dir, true, false);
        assert_eq!(load.servers.len(), 1);
        assert_eq!(load.servers[0].id, "a");
        assert!(
            lines.iter().any(|line| line.contains("loaded 1 server")),
            "{lines:?}"
        );
        // Malformed while trusted: a warning, no servers, no error.
        write(&dir, ".mcp.json", "{not json");
        let (load, lines) = load_project(&dir, true, false);
        assert!(load.servers.is_empty());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("skipped"), "{lines:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B2: the preview carries names, commands and args as written (no expansion) and env
    /// key names only — never values — and marks remote entries as skipped.
    #[test]
    fn the_preview_lists_raw_entries_and_marks_skips() {
        unsafe {
            std::env::set_var("RADIAN_MCP_B2_PREVIEW", "expanded-would-be-wrong");
        }
        let dir = temp_dir("preview");
        assert_eq!(preview_project(&dir), None, "no file means no preview");
        write(
            &dir,
            ".mcp.json",
            r#"{"mcpServers":{
                "fs": {"command": "npx", "args": ["-y", "${RADIAN_MCP_B2_PREVIEW}"],
                    "env": {"SECRET": "sekret-value", "PLAIN": "x"}},
                "web": {"type": "http", "url": "https://example.com/mcp"}}}"#,
        );
        let preview = preview_project(&dir).expect("the file exists");
        assert!(preview.error.is_none(), "{:?}", preview.error);
        assert_eq!(preview.entries.len(), 2);
        let fs = preview
            .entries
            .iter()
            .find(|entry| entry.name == "fs")
            .expect("the stdio entry");
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert!(
            fs.args
                .iter()
                .any(|arg| arg.contains("${RADIAN_MCP_B2_PREVIEW}")),
            "args are as written, before expansion: {:?}",
            fs.args
        );
        assert_eq!(fs.env_keys, vec!["PLAIN".to_owned(), "SECRET".to_owned()]);
        assert_eq!(fs.skipped, None);
        let rendered = format!("{preview:?}");
        assert!(
            !rendered.contains("sekret-value") && !rendered.contains("expanded-would-be-wrong"),
            "values never leak into the preview: {rendered}"
        );
        let web = preview
            .entries
            .iter()
            .find(|entry| entry.name == "web")
            .expect("the remote entry");
        let skipped = web.skipped.as_deref().expect("remote entries are marked");
        assert!(skipped.contains("stdio"), "{skipped}");
        // The shared question lines: the header counts the runnable server, entries show
        // commands as written with env key names only, skips are marked, values never leak.
        let lines = preview_lines(&preview);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("1 server to load once trusted")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(
                |line| line.contains("mcp: fs `npx -y ${RADIAN_MCP_B2_PREVIEW}`")
                    && line.contains("env: PLAIN, SECRET")
            ),
            "as written, keys only: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("mcp: web skipped") && line.contains("stdio")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("sekret-value")),
            "values never leak: {lines:?}"
        );
        // A broken file previews as an error, not entries.
        write(&dir, ".mcp.json", "{not json");
        let preview = preview_project(&dir).expect("the file exists");
        assert!(preview.entries.is_empty());
        assert!(preview.error.is_some(), "{preview:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B2: the project file loses every collision: `[mcp]` wins over `.mcp.json`, and
    /// `--mcp-config` wins over both, each with one warning naming the id and the winner.
    /// Without a project file the merge keeps [`merge_mcp`]'s exact wording.
    #[test]
    fn the_project_file_loses_every_collision() {
        let project = McpLoad {
            servers: vec![
                server("shared", "from-project"),
                server("only-project", "true"),
            ],
            warnings: vec!["project warning".to_owned()],
        };
        let config = McpLoad {
            servers: vec![
                server("shared", "from-config"),
                server("only-config", "true"),
            ],
            warnings: Vec::new(),
        };
        let file = McpLoad {
            servers: vec![server("only-config", "from-file")],
            warnings: Vec::new(),
        };
        let merged = merge_project_mcp(project, config, file);
        let ids: Vec<&str> = merged.servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["shared", "only-project", "only-config"]);
        assert_eq!(
            merged.servers[0].command,
            std::path::PathBuf::from("from-config"),
            "the user's own config wins over the project file"
        );
        assert_eq!(
            merged.servers[2].command,
            std::path::PathBuf::from("from-file"),
            "the command line wins over everything"
        );
        let collisions: Vec<&String> = merged
            .warnings
            .iter()
            .filter(|w| w.contains("is set in both"))
            .collect();
        assert_eq!(collisions.len(), 2, "{:?}", merged.warnings);
        assert!(
            collisions[0].contains("`shared`")
                && collisions[0].contains(".mcp.json")
                && collisions[0].contains("[mcp]"),
            "{:?}",
            merged.warnings
        );
        assert!(
            collisions[1].contains("`only-config`")
                && collisions[1].contains("[mcp]")
                && collisions[1].contains("--mcp-config"),
            "{:?}",
            merged.warnings
        );
        // Without a project file the config/file wording is exactly `merge_mcp`'s.
        let merged = merge_project_mcp(
            McpLoad::default(),
            McpLoad {
                servers: vec![server("a", "from-config")],
                warnings: Vec::new(),
            },
            McpLoad {
                servers: vec![server("a", "from-file")],
                warnings: Vec::new(),
            },
        );
        assert_eq!(
            merged.servers[0].command,
            std::path::PathBuf::from("from-file")
        );
        assert!(
            merged.warnings.iter().any(
                |w| w == "server `a` is set in both [mcp] and --mcp-config; using --mcp-config"
            ),
            "{:?}",
            merged.warnings
        );
    }
}
