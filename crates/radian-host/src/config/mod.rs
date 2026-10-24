//! The configuration file (ADR 0008; its location is ADR 0020's `~/.radian/config/config.toml`).
//!
//! Reading is the main job; the reader never rewrites a file it read. The one writer is
//! [`theme_write::write_theme`] (`/theme`, ADR 0008): it edits **only** `tui.theme`
//! through `toml_edit` and refuses when the result would differ in anything else (ADR 0008
//! §5.4's "faithful or refused").
//!
//! The rules the reader enforces (each one is a test):
//!
//! - `schema_version` is required and an integer; `1` is current, a **higher** version is read
//!   in read-only mode with a warning (unknown keys are ignored), a **lower** version is
//!   refused (there is no v0→v1 migration path);
//! - a secret slot's value must be a `SecretRef` table (`from`/`name`); a bare string is
//!   refused, and **only `from = "env"` is supported** this version;
//! - an obviously secret-looking string anywhere is refused (a heuristic, not a boundary);
//! - an unknown `permissions` value is refused (fail-closed); an unknown `tui.*` value is
//!   ignored with a warning;
//! - a missing file is an empty configuration (never created).
//!
//! No error message ever carries a value that could be a secret: the plaintext and heuristic
//! errors name the key path and nothing else.

pub mod default;
pub mod migrate_claude;
pub mod migrate_pi;
pub mod model_write;
pub mod preserve;
pub mod theme_write;

mod error;
mod hooks;
mod model;
mod sandbox;
mod tui;

use std::path::{Path, PathBuf};

pub use error::ConfigError;
pub use hooks::{
    DEFAULT_HOOK_TIMEOUT_SECS, DEFAULT_INTENT_STOP, HOOK_EVENTS, HookCommand, HookMatcher,
    HooksConfig, POST_TOOL_USE, PRE_TOOL_USE, STOP, matcher_effective,
};
pub use model::{ModelConfig, PROVIDER_KINDS, SmolConfig};
pub use sandbox::{SandboxConfig, resolve_allow_unix_sockets, unix_sockets_ignored_warning};
pub use tui::{
    BELL_MODES, CLIPBOARD_MODES, DISPLAY_REASONING_MODES, DISPLAY_TOOL_OUTPUT_MODES,
    TuiColorOverride, TuiColorValue, TuiConfig, TuiFooterConfig, TuiFooterItemConfig,
    TuiFooterRowConfig, TuiWidgetsConfig,
};

use toml::Value;
use toml::map::Map;

/// The `schema_version` this reader understands.
pub const SUPPORTED_SCHEMA_VERSION: u64 = 1;

/// The environment variable that overrides the config path.
pub const RADIAN_CONFIG_ENV: &str = "RADIAN_CONFIG";

/// The file name inside `~/.radian/config/`.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Keys whose bare (string) value is a plaintext secret (ADR 0008 §3.2 rule 1).
const SENSITIVE_KEYS: [&str; 7] = [
    "api_key",
    "apikey",
    "token",
    "secret",
    "password",
    "passwd",
    "authorization",
];

/// The obvious secret prefixes (ADR 0008 §3.2 rule 2, matching the reference checker).
const SECRET_PREFIXES: [&str; 6] = ["sk-", "sk_", "xoxb-", "ghp_", "AKIA", "Bearer "];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionsConfig {
    pub profile: Option<String>,
    /// `[permissions] approval` (ADR 0012, ADR 0026 D9): how an `ask` resolves. The name
    /// set is `ApprovalPolicy::parse`'s; an unknown value is refused (fail-closed, the
    /// `permissions` rule).
    pub approval: Option<String>,
}

/// The `[compaction] auto` default: compact past the input budget unless turned off.
pub const DEFAULT_COMPACTION_AUTO: bool = true;

/// The `[compaction] thinking` default: the summary call's reasoning level.
pub const DEFAULT_COMPACTION_THINKING: &str = "low";

/// The `[skills] builtin` default: built-in skills are on unless turned off.
pub const DEFAULT_SKILLS_BUILTIN: bool = true;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionConfig {
    pub auto: Option<bool>,
    /// `[compaction] thinking` (ADR 0018): the reasoning level the
    /// **summary** call runs at, one of the seven unified levels, or `"current"` to use the
    /// conversation's level (no override). `None` means the default, `low`. The host clamps it
    /// against the route's map and converts it to the provider's wire value; the core only
    /// sees the converted object.
    pub thinking: Option<String>,
}

/// `[skills] extra_paths` (ADR 0025 slice 4, ruling 2): more skill roots, **as written** in the
/// file. The reader keeps the strings; the host resolves them with
/// [`resolve_extra_paths`] (the resolution needs the config file's directory and the user's
/// home, neither of which belongs in a TOML value). A directory that is not listed here is
/// never scanned — in particular `~/.claude/skills` is read only when it appears in this list
/// (ruling 2).
/// `[shell]` (ADR 0037): the section is kept only so an old `auto_background_secs` key
/// warns once and is ignored, never an error. There are no live keys: background happens
/// only with an explicit `background: true`, and the foreground timeout is the built-in
/// default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellConfig {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillsConfig {
    pub extra_paths: Vec<String>,
    /// `[skills] builtin` (ADR 0025): include the compiled-in skills. `None` means
    /// the default, **on**; the host reads `unwrap_or(true)`.
    pub builtin: Option<bool>,
    /// `[skills] disabled`: built-in skill names to leave out.
    pub disabled: Vec<String>,
}

/// `[memory]` (ADR 0021). Only the two keys this revision adds are read;
/// the rest of the D6 injection table (`enabled`, `max_tokens`, `global_budget`,
/// `project_budget`, `overflow`) is still the documented default and is named in
/// [`Config::skipped`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryConfig {
    /// `[memory.injection] additions`: append later turns' new hits as one System message.
    /// `None` means the default, **on**; the host reads `unwrap_or(true)`.
    pub additions: Option<bool>,
    /// `[memory.injection] addition_budget`: the token cap on one turn's addition. `None`
    /// means the default (500).
    pub addition_budget: Option<u64>,
}

impl MemoryConfig {
    /// The injection configuration this section describes, with the D6 defaults for everything
    /// the file does not set.
    pub fn injection(&self) -> crate::memory::InjectionConfig {
        let defaults = crate::memory::InjectionConfig::default();
        crate::memory::InjectionConfig {
            additions: self.additions.unwrap_or(defaults.additions),
            addition_budget: self.addition_budget.unwrap_or(defaults.addition_budget),
            ..defaults
        }
    }
}

/// `[integrations]` (ADR 0034 H6): which local integrations are on. `herdr` defaults to
/// **true**: it only takes effect inside a herdr pane (ADR 0034 H1), and `RADIAN_HERDR=0`
/// turns it off for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationsConfig {
    pub herdr: bool,
}

impl Default for IntegrationsConfig {
    fn default() -> Self {
        Self { herdr: true }
    }
}

/// `[control]` (ADR 0043): the process control port. `enabled` defaults to
/// **true** — every hosting process binds `$RADIAN_HOME/run/<pid>.sock` — and `false` turns
/// the socket off for this process. The socket carries the same verbs O6 lists; a client can
/// never declare the `User` source and there is no approval verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlConfig {
    pub enabled: bool,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// One parsed configuration file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// The path actually read, or `None` when there was no file.
    pub source: Option<PathBuf>,
    /// The file's `schema_version`, when read.
    pub schema_version: Option<u64>,
    /// True when the file's version is **higher** than supported: the known keys are
    /// interpreted, unknown ones ignored, and the file must never be rewritten.
    pub read_only: bool,
    pub model: ModelConfig,
    pub permissions: PermissionsConfig,
    pub sandbox: SandboxConfig,
    pub integrations: IntegrationsConfig,
    /// `[control]` (ADR 0043): the process control port, on by default.
    pub control: ControlConfig,
    pub compaction: CompactionConfig,
    pub skills: SkillsConfig,
    /// `[memory]` (ADR 0021): the injection keys this revision adds.
    pub memory: MemoryConfig,
    /// `[hooks]` (ADR 0045 slice A): user-level external command hooks.
    pub hooks: HooksConfig,
    /// `[shell]` (ADR 0037): kept only for the removed-key warning; there are no live keys.
    pub shell: ShellConfig,
    pub tui: TuiConfig,
    /// `[providers.<name>]`: named custom services (ADR 0026 D10). Carried raw; the hosts
    /// resolve them through `radian_host::provider::resolve_provider`.
    pub providers: std::collections::BTreeMap<String, crate::provider::ProviderDecl>,
    /// `[mcp]` (ADR 0008 §8): the user's MCP servers, in the same two forms as
    /// `--mcp-config` (`[mcp.mcpServers.<name>]` tables or `[[mcp.servers]]` entries).
    /// Parsed by the same loader the file uses, so the rules cannot drift; the hosts
    /// merge these with the `--mcp-config` file's servers (the command line wins).
    /// Warnings print as `[mcp] …` like the file's.
    pub mcp: crate::mcp::McpLoad,
    /// Known-but-unwired and unknown keys, as dotted paths, for the startup line. Never a
    /// value.
    pub skipped: Vec<String>,
}

/// The path a configuration is read from: `explicit` > `RADIAN_CONFIG` > `<root>/config/config.toml`.
/// Pure, so the precedence is testable without the environment.
pub fn resolve_path(
    explicit: Option<&Path>,
    env: Option<&str>,
    radian_home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(path) = explicit.filter(|path| !path.as_os_str().is_empty()) {
        return Some(path.to_path_buf());
    }
    if let Some(path) = env.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    radian_home.map(|root| crate::home::config_dir_for(root).join(CONFIG_FILE_NAME))
}

/// Resolves the path for this process and reads it. A missing file is an empty configuration
/// (it is never created). The command line's `--config` wins over `RADIAN_CONFIG`, which wins
/// over `~/.radian/config/config.toml`.
pub fn load(explicit: Option<&Path>) -> Result<Config, ConfigError> {
    let env = std::env::var(RADIAN_CONFIG_ENV).ok();
    let home = crate::home::radian_home();
    let path = resolve_path(explicit, env.as_deref(), home.as_deref());
    match path {
        None => Ok(Config::default()),
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(text) => parse(&path, &text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(ConfigError::Read {
                path,
                source: error,
            }),
        },
    }
}

/// The path this process reads its configuration from (`--config` > `RADIAN_CONFIG` >
/// `<root>/config/config.toml`), even when the file does not exist yet. [`Config::source`]
/// is `None` in that case; the `/theme` write-back needs the path to create it (ADR 0008
/// revision).
pub fn resolved_path(explicit: Option<&Path>) -> Option<PathBuf> {
    let env = std::env::var(RADIAN_CONFIG_ENV).ok();
    let home = crate::home::radian_home();
    resolve_path(explicit, env.as_deref(), home.as_deref())
}

/// Parses one configuration document. Split out so the fixture consistency test can feed the
/// `docs/schema/config/` files directly.
pub fn parse(path: &Path, text: &str) -> Result<Config, ConfigError> {
    let doc: Value = toml::from_str(text).map_err(|error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let table = doc.as_table().ok_or_else(|| ConfigError::Parse {
        path: path.to_path_buf(),
        message: "the top level is not a table".into(),
    })?;

    // §5.1/§5.2/§5.3: the version is checked before anything else.
    let version = match table.get("schema_version") {
        Some(Value::Integer(version)) => *version,
        _ => {
            return Err(ConfigError::MissingVersion {
                path: path.to_path_buf(),
            });
        }
    };
    if version < SUPPORTED_SCHEMA_VERSION as i64 {
        return Err(ConfigError::VersionTooOld {
            path: path.to_path_buf(),
            supported: SUPPORTED_SCHEMA_VERSION,
            found: version,
        });
    }
    let read_only = version > SUPPORTED_SCHEMA_VERSION as i64;

    // §4.2: plaintext (and lookalike) secrets are refused wherever they appear.
    check_no_plaintext(table, "", path)?;
    check_secret_refs(table, "", path)?;

    let mut config = Config {
        source: Some(path.to_path_buf()),
        schema_version: Some(version as u64),
        read_only,
        ..Config::default()
    };

    for (key, value) in table {
        match key.as_str() {
            "schema_version" => {}
            "model" => model::parse_model(value, path, read_only, &mut config)?,
            "permissions" => parse_permissions(value, path, read_only, &mut config)?,
            "sandbox" => sandbox::parse_sandbox(value, path, &mut config)?,
            "integrations" => parse_integrations(value, path, &mut config)?,
            "control" => parse_control(value, path, &mut config)?,
            "compaction" => parse_compaction(value, path, &mut config)?,
            "hooks" => hooks::parse_hooks(value, &mut config)?,
            "skills" => parse_skills(value, path, &mut config)?,
            "memory" => parse_memory(value, path, &mut config)?,
            "shell" => parse_shell(value, &mut config)?,
            "tui" => tui::parse_tui(value, path, &mut config)?,
            "providers" => model::parse_providers(value, path, &mut config)?,
            "mcp" => parse_mcp(value, path, &mut config)?,
            other => config.skipped.push(other.to_owned()),
        }
    }
    config.skipped.sort();
    config.skipped.dedup();
    Ok(config)
}

/// `[providers.<name>]`: named custom services (ADR 0026 D10). `kind` must be one of the two
/// wire protocols, `base_url` a non-empty string; the name must be valid and must not shadow
/// a built-in id — a bad name is refused, never warned around (the hosts would silently
/// resolve the built-in instead).
/// The profile names ADR 0008 §2 lists (`ask`/`allow`/`deny`) plus the tiers ADR 0012/0016
/// define. A value outside this set is refused rather than guessed (fail-closed).
const KNOWN_PROFILES: [&str; 7] = [
    "workspace",
    "workspace-write",
    "full",
    "full-access",
    "ask",
    "allow",
    "deny",
];

fn parse_permissions(
    value: &Value,
    path: &Path,
    read_only: bool,
    config: &mut Config,
) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "permissions", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("profile", Value::String(profile)) => {
                if !KNOWN_PROFILES.contains(&profile.as_str()) {
                    if read_only {
                        // A higher schema_version may define a profile this version does not
                        // know; read-only mode ignores it instead of refusing the file.
                        config.skipped.push("permissions.profile".into());
                    } else {
                        return Err(ConfigError::UnknownProfile {
                            path: path.to_path_buf(),
                            value: profile.clone(),
                        });
                    }
                } else if matches!(
                    profile.as_str(),
                    "workspace" | "workspace-write" | "full" | "full-access"
                ) {
                    config.permissions.profile = Some(profile.clone());
                } else {
                    // The ADR's `ask`/`allow`/`deny` names have no tier in this build; ignoring
                    // them keeps the host's default (never widens permissions).
                    config.skipped.push("permissions.profile".into());
                }
            }
            ("approval", Value::String(approval)) => {
                if radian_permissions::ApprovalPolicy::parse(approval).is_none() {
                    if read_only {
                        // A higher schema_version may define a policy this version does not
                        // know; read-only mode ignores it instead of refusing the file.
                        config.skipped.push("permissions.approval".into());
                    } else {
                        // Fail-closed (ADR 0008 §6): a `permissions` value this build does
                        // not know is refused, never guessed at.
                        return Err(ConfigError::UnknownApproval {
                            path: path.to_path_buf(),
                            value: approval.clone(),
                        });
                    }
                } else {
                    config.permissions.approval = Some(approval.clone());
                }
            }
            ("approval", _) => {
                return Err(wrong_type(path, "permissions.approval", "a string"));
            }
            (other, _) => config.skipped.push(format!("permissions.{other}")),
        }
    }
    Ok(())
}

fn parse_integrations(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "integrations", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("herdr", Value::Boolean(on)) => config.integrations.herdr = *on,
            ("herdr", _) => {
                return Err(wrong_type(path, "integrations.herdr", "a boolean"));
            }
            (other, _) => config.skipped.push(format!("integrations.{other}")),
        }
    }
    Ok(())
}

/// Parses `[control]` (ADR 0043). Only the `enabled` key exists today; an unknown
/// key is reported and ignored (the `tui.*` rule, not refuse).
fn parse_control(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "control", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("enabled", Value::Boolean(on)) => config.control.enabled = *on,
            ("enabled", _) => {
                return Err(wrong_type(path, "control.enabled", "a boolean"));
            }
            (other, _) => config.skipped.push(format!("control.{other}")),
        }
    }
    Ok(())
}

fn parse_compaction(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "compaction", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("auto", Value::Boolean(auto)) => config.compaction.auto = Some(*auto),
            ("auto", _) => return Err(wrong_type(path, "compaction.auto", "a boolean")),
            ("thinking", Value::String(level))
                if level == crate::COMPACTION_THINKING_CURRENT
                    || crate::THINKING_LEVELS.contains(&level.as_str()) =>
            {
                config.compaction.thinking = Some(level.clone());
            }
            ("thinking", Value::String(_)) => {
                return Err(wrong_type(
                    path,
                    "compaction.thinking",
                    "one of off|minimal|low|medium|high|xhigh|max, or \"current\"",
                ));
            }
            ("thinking", _) => return Err(wrong_type(path, "compaction.thinking", "a string")),
            (other, _) => config.skipped.push(format!("compaction.{other}")),
        }
    }
    Ok(())
}

/// Parses `[shell]` (ADR 0037): the section has no live keys. Every key — including the
/// removed `auto_background_secs` — is skipped and named on the startup line, never a
/// refusal (the preferences rule: a bad limit must not block startup).
fn parse_shell(value: &Value, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Ok(());
    };
    for (key, _value) in table {
        config.skipped.push(format!("shell.{key}"));
    }
    Ok(())
}

fn parse_memory(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "memory", "a table"));
    };
    for (key, value) in table {
        match key.as_str() {
            "injection" => {
                let Some(injection) = value.as_table() else {
                    return Err(wrong_type(path, "memory.injection", "a table"));
                };
                for (key, value) in injection {
                    match (key.as_str(), value) {
                        ("additions", Value::Boolean(on)) => {
                            config.memory.additions = Some(*on);
                        }
                        ("additions", _) => {
                            return Err(wrong_type(
                                path,
                                "memory.injection.additions",
                                "a boolean",
                            ));
                        }
                        ("addition_budget", Value::Integer(budget)) if *budget > 0 => {
                            config.memory.addition_budget = Some(*budget as u64);
                        }
                        ("addition_budget", _) => {
                            return Err(wrong_type(
                                path,
                                "memory.injection.addition_budget",
                                "a positive integer",
                            ));
                        }
                        // The rest of the D6 table is known but not wired in this slice: it is
                        // interpreted as the documented default and named on the startup line.
                        (other, _) => config.skipped.push(format!("memory.injection.{other}")),
                    }
                }
            }
            other => config.skipped.push(format!("memory.{other}")),
        }
    }
    Ok(())
}

/// `[skills] extra_paths = ["…"]` (ADR 0025 slice 4). The strings are kept raw; the host
/// resolves them with [`resolve_extra_paths`]. A non-string or empty entry is a structural
/// error (an empty path would silently resolve to the config directory); an unknown key under
/// `[skills]` is skipped and named on the startup line, like every other unknown key.
fn parse_skills(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "skills", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("extra_paths", Value::Array(items)) => {
                for (index, item) in items.iter().enumerate() {
                    match item {
                        Value::String(entry) if !entry.trim().is_empty() => {
                            config.skills.extra_paths.push(entry.clone());
                        }
                        _ => {
                            return Err(wrong_type(
                                path,
                                &format!("skills.extra_paths[{index}]"),
                                "a non-empty string",
                            ));
                        }
                    }
                }
            }
            ("extra_paths", _) => {
                return Err(wrong_type(
                    path,
                    "skills.extra_paths",
                    "an array of strings",
                ));
            }
            ("builtin", Value::Boolean(on)) => config.skills.builtin = Some(*on),
            ("builtin", _) => {
                return Err(wrong_type(path, "skills.builtin", "a boolean"));
            }
            ("disabled", Value::Array(items)) => {
                for (index, item) in items.iter().enumerate() {
                    match item {
                        Value::String(name) if !name.trim().is_empty() => {
                            config.skills.disabled.push(name.clone());
                        }
                        _ => {
                            return Err(wrong_type(
                                path,
                                &format!("skills.disabled[{index}]"),
                                "a non-empty string",
                            ));
                        }
                    }
                }
            }
            ("disabled", _) => {
                return Err(wrong_type(path, "skills.disabled", "an array of strings"));
            }
            (other, _) => config.skipped.push(format!("skills.{other}")),
        }
    }
    Ok(())
}

/// Turns `[skills] extra_paths` into filesystem paths (ADR 0025 slice 4). In order:
///
/// - a leading `~` (or exactly `~`) is the user's home;
/// - a relative path is relative to the **config file's directory** — the list lives next to
///   the config that declares it, not next to the workspace;
/// - an absolute path is kept.
///
/// Pure, so the rules are testable without touching the environment. A `~` with no home is left
/// alone (it will then be reported as a missing directory, never silently expanded).
pub fn resolve_extra_paths(
    raw: &[String],
    config_dir: Option<&Path>,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    raw.iter()
        .map(|entry| resolve_extra_path(entry, config_dir, home))
        .collect()
}

fn resolve_extra_path(entry: &str, config_dir: Option<&Path>, home: Option<&Path>) -> PathBuf {
    let tilde = entry == "~" || entry.starts_with("~/");
    let expanded = if entry == "~" {
        home.map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(entry))
    } else if let Some(rest) = entry.strip_prefix("~/") {
        match home {
            Some(home) => home.join(rest),
            None => PathBuf::from(entry),
        }
    } else {
        PathBuf::from(entry)
    };
    // An unexpanded `~` is left as written (and reported as a missing directory), never joined
    // to the config directory: `~/.claude/skills` is a clearer warning than a path under `~/.radian/config/`.
    if expanded.is_absolute() || tilde {
        expanded
    } else if let Some(dir) = config_dir {
        dir.join(expanded)
    } else {
        expanded
    }
}

/// Parses `[mcp]` (ADR 0008 §8): the same two forms as `--mcp-config`, spelled in TOML
/// (`[mcp.mcpServers.<name>]` tables or `[[mcp.servers]]` entries). One normaliser: the
/// TOML value is converted to JSON and the file's loader runs on it, so the rules (both
/// forms or neither is an error, duplicate ids are refused, remote entries are skipped
/// with a warning, `${VAR}` is expanded) cannot drift. A structural problem is a config
/// error; the warnings ride in [`Config::mcp`] and the hosts print them as `[mcp] …`.
fn parse_mcp(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    if value.as_table().is_none() {
        return Err(wrong_type(path, "mcp", "a table"));
    }
    let origin = format!("{} [mcp]", path.display());
    match crate::mcp::load_mcp_from_toml(value, &origin, path.parent()) {
        Ok(load) => {
            config.mcp = load;
            Ok(())
        }
        Err(message) => Err(ConfigError::Parse {
            path: path.to_path_buf(),
            message,
        }),
    }
}

fn wrong_type(path: &Path, key: &str, expected: &'static str) -> ConfigError {
    ConfigError::WrongType {
        path: path.to_path_buf(),
        key: key.to_owned(),
        expected,
    }
}

/// The `path.key` spelling the checker uses, for error messages and the skipped list.
fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// §4.2: a bare string on a sensitive key, or an obviously secret-looking string anywhere, is
/// refused. The `key` in the error is a path, never the value.
fn check_no_plaintext(
    table: &Map<String, Value>,
    path: &str,
    file: &Path,
) -> Result<(), ConfigError> {
    for (key, value) in table {
        let here = join(path, key);
        match value {
            Value::Table(table) => check_no_plaintext(table, &here, file)?,
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    if let Value::Table(table) = item {
                        check_no_plaintext(table, &format!("{here}[{index}]"), file)?;
                    }
                }
            }
            Value::String(text) => {
                if SENSITIVE_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
                    return Err(ConfigError::PlaintextSecret {
                        path: file.to_path_buf(),
                        key: here,
                    });
                }
                if looks_like_secret(text) {
                    return Err(ConfigError::SecretLookalike {
                        path: file.to_path_buf(),
                        key: here,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The heuristic of ADR 0008 §3.2 rule 2, byte-for-byte the reference checker's.
fn looks_like_secret(text: &str) -> bool {
    if SECRET_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return true;
    }
    text.len() >= 40
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// §4.1: every sensitive key must be a `SecretRef` table. `from`/`name` are required, `name`
/// is a non-empty string, and **only `from = "env"` is supported by this version**.
fn check_secret_refs(
    table: &Map<String, Value>,
    path: &str,
    file: &Path,
) -> Result<(), ConfigError> {
    for (key, value) in table {
        let here = join(path, key);
        if SENSITIVE_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
            let Some(secret) = value.as_table() else {
                return Err(ConfigError::SecretRef {
                    path: file.to_path_buf(),
                    key: here,
                    message: "must be a SecretRef table ({ from = \"env\", name = \"…\" })".into(),
                });
            };
            let from = secret
                .get("from")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if from != "env" {
                let message = if from.is_empty() {
                    "SecretRef.from must be \"env\"".to_owned()
                } else {
                    format!("SecretRef.from = {from:?}; this version only supports \"env\"")
                };
                return Err(ConfigError::SecretRef {
                    path: file.to_path_buf(),
                    key: here,
                    message,
                });
            }
            match secret.get("name").and_then(Value::as_str) {
                Some(name) if !name.is_empty() => {}
                _ => {
                    return Err(ConfigError::SecretRef {
                        path: file.to_path_buf(),
                        key: here,
                        message: "SecretRef.name must be a non-empty string".into(),
                    });
                }
            }
            let extra: Vec<&String> = secret
                .keys()
                .filter(|key| !matches!(key.as_str(), "from" | "name"))
                .collect();
            if !extra.is_empty() {
                return Err(ConfigError::SecretRef {
                    path: file.to_path_buf(),
                    key: here,
                    message: format!("SecretRef has unknown fields {extra:?}"),
                });
            }
        } else {
            match value {
                Value::Table(table) => check_secret_refs(table, &here, file)?,
                Value::Array(items) => {
                    for (index, item) in items.iter().enumerate() {
                        if let Value::Table(table) = item {
                            check_secret_refs(table, &format!("{here}[{index}]"), file)?;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(text: &str) -> Result<Config, ConfigError> {
        parse(Path::new("/tmp/test/config.toml"), text)
    }

    /// ADR 0034 H6: `[integrations] herdr` defaults to true; `false` turns it off and a
    /// non-boolean is refused.
    #[test]
    fn integrations_herdr_defaults_on_and_can_be_turned_off() {
        assert!(
            parse_str("schema_version = 1\n")
                .unwrap()
                .integrations
                .herdr
        );
        assert!(
            !parse_str("schema_version = 1\n[integrations]\nherdr = false\n")
                .unwrap()
                .integrations
                .herdr
        );
        assert!(
            parse_str("schema_version = 1\n[integrations]\nherdr = \"no\"\n").is_err(),
            "a non-boolean is refused"
        );
        let config = parse_str("schema_version = 1\n[integrations]\nfuture = 1\n").unwrap();
        assert!(config.skipped.contains(&"integrations.future".to_owned()));
    }

    /// ADR 0016: `[sandbox] allow_unix_sockets` is an array of strings; a bad
    /// shape is refused and an unknown key under `[sandbox]` is only skipped.
    #[test]
    fn sandbox_allow_unix_sockets_is_parsed_and_a_bad_shape_is_refused() {
        let config = parse_str(
            "schema_version = 1\n[sandbox]\nallow_unix_sockets = [\"~/.config/herdr/herdr.sock\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.sandbox.allow_unix_sockets,
            vec!["~/.config/herdr/herdr.sock"]
        );
        assert!(
            parse_str("schema_version = 1\n[sandbox]\nallow_unix_sockets = \"x\"\n").is_err(),
            "a non-array is refused"
        );
        let config = parse_str("schema_version = 1\n[sandbox]\nfuture = true\n").unwrap();
        assert!(config.sandbox.allow_unix_sockets.is_empty());
        assert!(config.skipped.contains(&"sandbox.future".to_owned()));
    }

    /// ADR 0034: `[sandbox] allow_env` is a list of names, not
    /// `NAME=value` pairs. A value smuggled in as `NAME=value` or a non-string is refused.
    #[test]
    fn sandbox_allow_env_is_names_only() {
        let config =
            parse_str("schema_version = 1\n[sandbox]\nallow_env = [\"FOO\", \"BAR\"]\n").unwrap();
        assert_eq!(config.sandbox.allow_env, vec!["FOO", "BAR"]);
        assert!(
            parse_str("schema_version = 1\n[sandbox]\nallow_env = [\"FOO=bar\"]\n").is_err(),
            "a value must not be smuggled in"
        );
        assert!(
            parse_str("schema_version = 1\n[sandbox]\nallow_env = [3]\n").is_err(),
            "a non-string is refused"
        );
        assert!(
            parse_str("schema_version = 1\n[sandbox]\nallow_env = \"FOO\"\n").is_err(),
            "a non-array is refused"
        );
    }

    /// The host expands `~`, canonicalizes, and skips (with a warning) a relative or missing
    /// entry. Only absolute paths are accepted.
    #[test]
    fn resolve_allow_unix_sockets_expands_home_and_skips_bad_entries() {
        let dir = crate::test_support::TempDir::named("radian-host-sandbox-sock-test");
        let sock = dir.join("h.sock");
        std::fs::write(&sock, b"").unwrap();
        let raw = vec![
            "~/.config/herdr/herdr.sock".to_owned(),
            "relative.sock".to_owned(),
            sock.to_string_lossy().into_owned(),
        ];
        let (paths, warnings) = resolve_allow_unix_sockets(&raw, Some(Path::new("/home/u")));
        assert_eq!(paths.len(), 1, "only the existing absolute path resolves");
        assert_eq!(paths[0], std::fs::canonicalize(&sock).unwrap());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Slice 0: on Linux a non-empty `allow_unix_sockets` warns once that it is ignored
    /// (the seccomp filter cannot whitelist by path); an empty list never warns, and on
    /// macOS the list is honored so there is no warning either.
    #[test]
    fn the_linux_unix_socket_warning_fires_only_for_a_non_empty_list() {
        assert_eq!(
            unix_sockets_ignored_warning(&[]),
            None,
            "an empty list needs no warning"
        );
        let warning = unix_sockets_ignored_warning(&[PathBuf::from("/run/herdr.sock")]);
        if cfg!(target_os = "linux") {
            let warning = warning.expect("a non-empty list warns on Linux");
            assert!(
                warning.contains("ignored on Linux"),
                "the line says it is ignored: {warning}"
            );
        } else {
            assert_eq!(warning, None, "macOS honors the list");
        }
    }

    #[test]
    fn resolve_path_prefers_explicit_then_env_then_the_radian_home() {
        let home = Path::new("/home/u/.radian");
        assert_eq!(
            resolve_path(
                Some(Path::new("/etc/radian.toml")),
                Some("/env.toml"),
                Some(home)
            ),
            Some(PathBuf::from("/etc/radian.toml"))
        );
        assert_eq!(
            resolve_path(None, Some("/env.toml"), Some(home)),
            Some(PathBuf::from("/env.toml"))
        );
        assert_eq!(
            resolve_path(None, None, Some(home)),
            Some(PathBuf::from("/home/u/.radian/config/config.toml"))
        );
        assert_eq!(resolve_path(None, None, None), None);
    }

    /// ADR 0025 slice 4: `~` is the user's home, a relative entry is relative to the **config
    /// file's directory**, and an absolute entry is kept. No home means `~` is left alone.
    #[test]
    fn extra_paths_resolve_tilde_relative_and_absolute() {
        let config_dir = Path::new("/home/u/.radian/config");
        let home = Path::new("/home/u");
        let raw = vec![
            "~/.claude/skills".to_owned(),
            "~".to_owned(),
            "./team-skills".to_owned(),
            "shared".to_owned(),
            "/opt/shared/skills".to_owned(),
        ];
        assert_eq!(
            resolve_extra_paths(&raw, Some(config_dir), Some(home)),
            vec![
                PathBuf::from("/home/u/.claude/skills"),
                PathBuf::from("/home/u"),
                PathBuf::from("/home/u/.radian/config/team-skills"),
                PathBuf::from("/home/u/.radian/config/shared"),
                PathBuf::from("/opt/shared/skills"),
            ]
        );
        // No home: `~` is not guessed at (it will be reported as a missing directory).
        let no_home = resolve_extra_paths(&raw[..2], Some(config_dir), None);
        assert_eq!(
            no_home,
            vec![PathBuf::from("~/.claude/skills"), PathBuf::from("~"),]
        );
        // No config file: a relative path stays relative.
        assert_eq!(
            resolve_extra_paths(&["shared".to_owned()], None, Some(home)),
            vec![PathBuf::from("shared")]
        );
    }

    /// `[skills] extra_paths` must be an array of non-empty strings; a bad shape is refused
    /// (an empty string would silently mean the config directory), an unknown key is skipped.
    #[test]
    fn skills_extra_paths_validate_the_shape() {
        let config =
            parse_str("schema_version = 1\n[skills]\nextra_paths = [\"a\", \"b\"]\nfuture = 1\n")
                .unwrap();
        assert_eq!(
            config.skills.extra_paths,
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(config.skipped.contains(&"skills.future".to_owned()));
        assert!(parse_str("schema_version = 1\n[skills]\nextra_paths = \"a\"\n").is_err());
        assert!(parse_str("schema_version = 1\n[skills]\nextra_paths = [\"\"]\n").is_err());
        assert!(parse_str("schema_version = 1\n[skills]\nextra_paths = [1]\n").is_err());
    }

    #[test]
    fn a_minimal_file_wires_the_known_keys() {
        let config = parse_str(
            r#"
schema_version = 1
[model]
provider = "anthropic"
base_url = "https://example.com"
model = "m-1"
context_window = 200000
[model.headers]
x-a = "1"
[permissions]
profile = "full"
[compaction]
auto = false
thinking = "high"
[tui]
theme = "light"
"#,
        )
        .unwrap();
        assert_eq!(config.model.provider.as_deref(), Some("anthropic"));
        assert_eq!(
            config.model.base_url.as_deref(),
            Some("https://example.com")
        );
        assert_eq!(config.model.model.as_deref(), Some("m-1"));
        assert_eq!(config.model.context_window, Some(200_000));
        assert_eq!(
            config.model.headers,
            vec![("x-a".to_string(), "1".to_string())]
        );
        assert_eq!(config.permissions.profile.as_deref(), Some("full"));
        assert_eq!(config.compaction.auto, Some(false));
        assert_eq!(config.compaction.thinking.as_deref(), Some("high"));
        assert_eq!(config.tui.theme.as_deref(), Some("light"));
    }

    /// ADR 0018: `[compaction] thinking` accepts the seven levels or `"current"`;
    /// anything else is a config error, never a silent clamp from the default.
    #[test]
    fn compaction_thinking_accepts_the_levels_and_current_and_refuses_anything_else() {
        let config =
            parse_str("schema_version = 1\n[compaction]\nthinking = \"current\"\n").unwrap();
        assert_eq!(config.compaction.thinking.as_deref(), Some("current"));
        for level in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let config = parse_str(&format!(
                "schema_version = 1\n[compaction]\nthinking = \"{level}\"\n"
            ))
            .unwrap();
            assert_eq!(config.compaction.thinking.as_deref(), Some(level));
        }
        assert!(parse_str("schema_version = 1\n[compaction]\nthinking = \"loud\"\n").is_err());
        assert!(parse_str("schema_version = 1\n[compaction]\nthinking = 3\n").is_err());
    }

    /// `[model] retries` (ADR 0017) is carried; a negative or non-integer value is
    /// refused (fail-closed), and `0` means "no retrying".
    #[test]
    fn model_retries_must_be_a_non_negative_integer() {
        let config = parse_str("schema_version = 1\n[model]\nretries = 3\n").unwrap();
        assert_eq!(config.model.retries, Some(3));
        let config = parse_str("schema_version = 1\n[model]\nretries = 0\n").unwrap();
        assert_eq!(config.model.retries, Some(0));
        assert!(parse_str("schema_version = 1\n[model]\nretries = -1\n").is_err());
        assert!(parse_str("schema_version = 1\n[model]\nretries = \"3\"\n").is_err());
    }

    /// ADR 0015: `[model] context_window` is a non-negative integer, and `0` means
    /// **unset** — a valid "off" in the same shape as `retries = 0`, never a zero-token window.
    /// A negative or non-integer value is refused rather than guessed (fail-closed).
    #[test]
    fn context_window_zero_is_unset_and_a_positive_value_is_carried() {
        let config = parse_str("schema_version = 1\n[model]\ncontext_window = 200000\n").unwrap();
        assert_eq!(config.model.context_window, Some(200_000));
        let config = parse_str("schema_version = 1\n[model]\ncontext_window = 0\n").unwrap();
        assert_eq!(config.model.context_window, None);
        assert!(parse_str("schema_version = 1\n[model]\ncontext_window = -1\n").is_err());
        assert!(parse_str("schema_version = 1\n[model]\ncontext_window = \"200000\"\n").is_err());
    }

    /// `tui.input_style` is carried as a string; the TUI parses it (the removed `bar` and any
    /// unknown value warn there), and a non-string is a skipped key (the `tui.*` rule), not a
    /// refusal.
    #[test]
    fn the_tui_input_style_is_carried_and_a_non_string_is_skipped() {
        // ADR 0035's 2026-10-16 note removed `bar`; the reader still carries the spelling so the
        // TUI can warn with it. `lines` and `box` are the accepted styles.
        for style in ["bar", "lines", "box"] {
            let config = parse_str(&format!(
                "schema_version = 1\n[tui]\ninput_style = \"{style}\"\n"
            ))
            .unwrap();
            assert_eq!(config.tui.input_style.as_deref(), Some(style));
            assert!(config.skipped.is_empty(), "{style}");
        }

        // Any other string is carried too (the TUI validates it), never a refusal.
        let config = parse_str("schema_version = 1\n[tui]\ninput_style = \"future\"\n").unwrap();
        assert_eq!(config.tui.input_style.as_deref(), Some("future"));
        assert!(config.skipped.is_empty());

        // A non-string is skipped.
        let config = parse_str("schema_version = 1\n[tui]\ninput_style = 3\n").unwrap();
        assert_eq!(config.tui.input_style, None);
        assert!(config.skipped.contains(&"tui.input_style".to_string()));
    }

    /// `tui.display.mermaid` (ADR 0028 R8): a bool is carried; an unknown value is skipped.
    #[test]
    fn the_display_mermaid_flag_is_carried_and_a_non_bool_is_skipped() {
        let config = parse_str("schema_version = 1\n[tui.display]\nmermaid = false\n").unwrap();
        assert_eq!(config.tui.display_mermaid, Some(false));
        let config = parse_str("schema_version = 1\n[tui.display]\nmermaid = \"yes\"\n").unwrap();
        assert_eq!(config.tui.display_mermaid, None);
        assert!(config.skipped.contains(&"tui.display.mermaid".to_string()));
    }

    /// `tui.display.highlight` (ADR 0028 R2): a bool is carried; an unknown value is skipped.
    #[test]
    fn the_display_highlight_flag_is_carried_and_a_non_bool_is_skipped() {
        let config = parse_str("schema_version = 1\n[tui.display]\nhighlight = false\n").unwrap();
        assert_eq!(config.tui.display_highlight, Some(false));
        assert_eq!(
            parse_str("schema_version = 1\n")
                .unwrap()
                .tui
                .display_highlight,
            None
        );
        let config = parse_str("schema_version = 1\n[tui.display]\nhighlight = \"yes\"\n").unwrap();
        assert_eq!(config.tui.display_highlight, None);
        assert!(
            config
                .skipped
                .contains(&"tui.display.highlight".to_string())
        );
    }

    /// `tui.display.reasoning` (ADR 0028 R3): the three modes are carried; an unknown one is
    /// skipped with the `tui.*` warning and the TUI keeps its default.
    #[test]
    fn the_display_reasoning_mode_is_carried_and_an_unknown_value_is_skipped() {
        assert_eq!(
            parse_str("schema_version = 1\n")
                .unwrap()
                .tui
                .display_reasoning,
            None
        );
        for mode in ["collapsed", "expanded", "hidden"] {
            let config = parse_str(&format!(
                "schema_version = 1\n[tui.display]\nreasoning = \"{mode}\"\n"
            ))
            .unwrap();
            assert_eq!(config.tui.display_reasoning.as_deref(), Some(mode));
        }
        let config =
            parse_str("schema_version = 1\n[tui.display]\nreasoning = \"sometimes\"\n").unwrap();
        assert_eq!(config.tui.display_reasoning, None);
        assert!(
            config
                .skipped
                .contains(&"tui.display.reasoning".to_string())
        );
        // A `tui.display` that is not a table is skipped, not refused.
        let config = parse_str("schema_version = 1\n[tui]\ndisplay = 3\n").unwrap();
        assert!(config.skipped.contains(&"tui.display".to_string()));
    }

    /// `tui.mouse` is an optional tri-state: absent = the default (on), `true`/`false` are
    /// carried verbatim, and a wrong type is a display preference (skipped, never a refusal).
    #[test]
    fn the_tui_mouse_flag_is_carried_and_a_wrong_type_is_skipped() {
        assert_eq!(parse_str("schema_version = 1\n").unwrap().tui.mouse, None);
        assert_eq!(
            parse_str("schema_version = 1\n[tui]\nmouse = false\n")
                .unwrap()
                .tui
                .mouse,
            Some(false)
        );
        let config = parse_str("schema_version = 1\n[tui]\nmouse = \"yes\"\n").unwrap();
        assert_eq!(config.tui.mouse, None);
        assert!(config.skipped.contains(&"tui.mouse".to_string()));
    }

    #[test]
    fn a_missing_version_and_a_lower_version_are_refused() {
        assert!(matches!(
            parse_str("schema_version = \"1\"\n").unwrap_err(),
            ConfigError::MissingVersion { .. }
        ));
        assert!(matches!(
            parse_str("schema_version = 0\n").unwrap_err(),
            ConfigError::VersionTooOld { found: 0, .. }
        ));
    }

    #[test]
    fn a_higher_version_is_read_only_with_a_warning_not_a_refusal() {
        let config = parse_str("schema_version = 99\n[model]\nmodel = \"m-99\"\n").unwrap();
        assert!(config.read_only);
        assert_eq!(config.schema_version, Some(99));
        assert_eq!(config.model.model.as_deref(), Some("m-99"));
    }

    #[test]
    fn a_bare_secret_is_refused_without_echoing_the_value() {
        let error =
            parse_str("schema_version = 1\n[model]\napi_key = \"super-secret\"\n").unwrap_err();
        let text = error.to_string();
        assert!(!text.contains("super-secret"), "{text}");
        assert!(text.contains("api_key"), "{text}");
        assert!(matches!(error, ConfigError::PlaintextSecret { .. }));
    }

    #[test]
    fn a_secret_lookalike_anywhere_is_refused_without_echoing_it() {
        let error =
            parse_str("schema_version = 1\n[tui]\nstatus_template = \"sk-live-abcdef123456\"\n")
                .unwrap_err();
        let text = error.to_string();
        assert!(!text.contains("sk-live"), "{text}");
        assert!(matches!(error, ConfigError::SecretLookalike { .. }));
    }

    /// ADR 0026 D2: `model.api_key` is refused with a pointer at `auth.json`, never silently
    /// ignored.
    #[test]
    fn model_api_key_is_refused_with_an_auth_json_pointer() {
        let error =
            parse_str("schema_version = 1\n[model]\napi_key = { from = \"env\", name = \"K\" }\n")
                .unwrap_err();
        assert!(
            matches!(error, ConfigError::ApiKeyRemoved { .. }),
            "{error}"
        );
        let text = error.to_string();
        assert!(text.contains("auth.json"), "{text}");
        assert!(text.contains("/login"), "{text}");
    }

    #[test]
    fn a_secret_ref_with_a_non_env_source_is_refused() {
        let error = parse_str(
            "schema_version = 1\n[model]\napi_key = { from = \"keychain\", name = \"x\" }\n",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("only supports \"env\""),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_profile_is_refused_and_an_unknown_theme_is_carried() {
        let error =
            parse_str("schema_version = 1\n[permissions]\nprofile = \"future\"\n").unwrap_err();
        assert!(
            matches!(error, ConfigError::UnknownProfile { .. }),
            "{error}"
        );

        // ADR 0029: any theme name is carried; whether it resolves (built-in or
        // `~/.radian/themes/<name>.toml`) is the TUI's finding, a startup warning.
        let config = parse_str("schema_version = 1\n[tui]\ntheme = \"future\"\n").unwrap();
        assert_eq!(config.tui.theme.as_deref(), Some("future"));
        assert!(config.skipped.is_empty());
    }

    /// ADR 0026 D9: `[permissions] approval` carries the four known policy names; an
    /// unknown value is refused (fail-closed), a non-string is a type error, and in
    /// read-only mode an unknown future value is skipped instead.
    #[test]
    fn permissions_approval_carries_known_names_and_refuses_unknown_ones() {
        for name in ["on-ask", "always", "never", "auto"] {
            let config = parse_str(&format!(
                "schema_version = 1\n[permissions]\napproval = \"{name}\"\n"
            ))
            .unwrap();
            assert_eq!(config.permissions.approval.as_deref(), Some(name), "{name}");
            assert!(config.skipped.is_empty(), "{name}");
        }
        let error =
            parse_str("schema_version = 1\n[permissions]\napproval = \"yolo\"\n").unwrap_err();
        assert!(
            matches!(error, ConfigError::UnknownApproval { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("auto"), "{error}");
        let error = parse_str("schema_version = 1\n[permissions]\napproval = true\n").unwrap_err();
        assert!(error.to_string().contains("a string"), "{error}");
        // Read-only mode (a higher schema_version): ignored, not refused.
        let config =
            parse_str("schema_version = 99\n[permissions]\napproval = \"future\"\n").unwrap();
        assert_eq!(config.permissions.approval, None);
        assert!(config.skipped.contains(&"permissions.approval".to_string()));
    }

    /// ADR 0029 decision 5: `[tui.colors]` entries are carried raw; the TUI validates names
    /// and colors. A malformed entry is a skipped key, never a refusal.
    #[test]
    fn tui_colors_entries_are_carried_and_malformed_ones_skipped() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[tui]\n",
            "theme = \"kanagawa-wave\"\n",
            "[tui.colors]\n",
            "user = \"#0af\"\n",
            "tool_call = 39\n",
            "notice = { fg = \"oklch(62% 0.1 200)\", bold = true }\n",
            "hint = { fg = \"#fff\", neon = true }\n",
            "muted = 2.5\n",
        ))
        .unwrap();
        assert_eq!(config.tui.theme.as_deref(), Some("kanagawa-wave"));
        assert_eq!(config.tui.colors.len(), 3, "{:?}", config.tui.colors);
        let get = |key: &str| {
            config
                .tui
                .colors
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value)
                .unwrap()
        };
        assert_eq!(get("user").fg, Some(TuiColorValue::Text("#0af".to_owned())));
        assert_eq!(get("tool_call").fg, Some(TuiColorValue::Index(39)));
        let notice = get("notice");
        assert_eq!(
            notice.fg,
            Some(TuiColorValue::Text("oklch(62% 0.1 200)".to_owned()))
        );
        assert_eq!(notice.bold, Some(true));
        assert_eq!(
            config.skipped,
            vec!["tui.colors.hint".to_owned(), "tui.colors.muted".to_owned()]
        );

        // A non-table `tui.colors` is a wrong type (structural), like `tui.keybindings`.
        let error = parse_str("schema_version = 1\n[tui]\ncolors = 3\n").unwrap_err();
        assert!(matches!(error, ConfigError::WrongType { .. }), "{error}");
    }

    /// ADR 0026 D10: `[providers.<name>]` is carried into `Config.providers`; a bad or
    /// shadowing name refuses the file (fail-closed, like `permissions.profile`).
    #[test]
    fn named_providers_are_carried_and_bad_names_refused() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.corp]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://llm.example.com/v1\"\n",
            "[providers.relay]\n",
            "kind = \"anthropic\"\n",
            "base_url = \"https://relay.example\"\n",
        ))
        .unwrap();
        assert_eq!(config.providers.len(), 2);
        assert_eq!(config.providers["corp"].kind, "openai-compatible");
        assert_eq!(config.providers["relay"].base_url, "https://relay.example");
        assert!(config.skipped.is_empty(), "{:?}", config.skipped);

        // A name shadowing a built-in refuses the file, naming the rule.
        let error = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.anthropic]\n",
            "kind = \"anthropic\"\n",
            "base_url = \"https://x\"\n",
        ))
        .unwrap_err();
        assert!(
            matches!(error, ConfigError::InvalidProviderName { .. }),
            "{error}"
        );
        let text = error.to_string();
        assert!(text.contains("providers.anthropic"), "{text}");
        assert!(text.contains("built-in"), "{text}");

        // An invalid name too.
        let error = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.\"has space\"]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://x\"\n",
        ))
        .unwrap_err();
        assert!(
            matches!(error, ConfigError::InvalidProviderName { .. }),
            "{error}"
        );

        // `openai-compatible` is reserved.
        let error = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.openai-compatible]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://x\"\n",
        ))
        .unwrap_err();
        assert!(
            matches!(error, ConfigError::InvalidProviderName { .. }),
            "{error}"
        );

        // An unknown kind is refused (never guessed at the host).
        let error = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.ok-name]\n",
            "kind = \"grpc\"\n",
            "base_url = \"https://x\"\n",
        ))
        .unwrap_err();
        assert!(matches!(error, ConfigError::WrongType { .. }), "{error}");
        assert!(
            error.to_string().contains("openai-compatible|anthropic"),
            "{error}"
        );
    }

    /// The removed `[shell] auto_background_secs` warns once and is ignored, never an
    /// error: every value shape (an integer, `0`, a negative, a wrong type) parses and
    /// lands in `skipped`. Dropping the warning fails this test.
    #[test]
    fn the_removed_shell_auto_background_secs_warns_once_and_is_ignored() {
        for text in [
            "schema_version = 1\n[shell]\nauto_background_secs = 30\n",
            "schema_version = 1\n[shell]\nauto_background_secs = 0\n",
            "schema_version = 1\n[shell]\nauto_background_secs = -1\n",
            "schema_version = 1\n[shell]\nauto_background_secs = \"x\"\n",
        ] {
            let config = parse_str(text).unwrap();
            assert_eq!(
                config
                    .skipped
                    .iter()
                    .filter(|key| key.as_str() == "shell.auto_background_secs")
                    .count(),
                1,
                "{text}: {:?}",
                config.skipped
            );
        }

        // Unset: nothing warned.
        let config = parse_str("schema_version = 1\n").unwrap();
        assert!(config.skipped.is_empty(), "{:?}", config.skipped);
    }

    /// `[model.smol]` (ADR 0008 §1, ADR 0043 §1) carries the cheap route slot: both keys
    /// set means set; a half-set table warns (`model.smol`) and is ignored.
    #[test]
    fn the_model_smol_route_is_carried_and_a_half_set_table_is_ignored() {
        let config =
            parse_str("schema_version = 1\n[model.smol]\nprovider = \"cheap\"\nmodel = \"m-c\"\n")
                .unwrap();
        let smol = config.model.smol.as_ref().expect("smol is set");
        assert_eq!(smol.provider.as_str(), "cheap");
        assert_eq!(smol.model.as_str(), "m-c");
        assert!(config.skipped.is_empty(), "{:?}", config.skipped);

        // Only one of the two keys: warned and ignored, so `explore` follows the parent.
        for text in [
            "schema_version = 1\n[model.smol]\nprovider = \"cheap\"\n",
            "schema_version = 1\n[model.smol]\nmodel = \"m-c\"\n",
        ] {
            let config = parse_str(text).unwrap();
            assert_eq!(config.model.smol, None, "{text}");
            assert!(
                config.skipped.contains(&"model.smol".to_owned()),
                "{text}: {:?}",
                config.skipped
            );
        }

        // Unset: nothing carried, nothing warned.
        let config = parse_str("schema_version = 1\n").unwrap();
        assert_eq!(config.model.smol, None);
        assert!(config.skipped.is_empty());
    }

    /// Unknown `[model.smol]` keys and wrong types are skipped, never refused (the
    /// warn-and-skip style); a wrong-typed key cannot half-set the table.
    #[test]
    fn model_smol_unknown_keys_and_wrong_types_are_skipped() {
        let config = parse_str(
            "schema_version = 1\n[model]\nfuture = 1\n[model.smol]\nmodel = \"m-c\"\nprovider = 3\n",
        )
        .unwrap();
        assert_eq!(config.model.smol, None);
        for expected in ["model.future", "model.smol.provider", "model.smol"] {
            assert!(
                config.skipped.contains(&expected.to_owned()),
                "{expected}: {:?}",
                config.skipped
            );
        }

        let config = parse_str("schema_version = 1\n[model]\nsmol = 3\n").unwrap();
        assert_eq!(config.model.smol, None);
        assert!(
            config.skipped.contains(&"model.smol".to_owned()),
            "{:?}",
            config.skipped
        );
    }

    /// `[subagent.explore]` was never released: it is now an unknown section, skipped and
    /// warned like any unknown key, never read.
    #[test]
    fn subagent_explore_is_now_an_unknown_section() {
        let config = parse_str(
            "schema_version = 1\n[subagent.explore]\nprovider = \"cheap\"\nmodel = \"m-c\"\n",
        )
        .unwrap();
        assert_eq!(config.model.smol, None);
        assert!(
            config.skipped.contains(&"subagent".to_owned()),
            "{:?}",
            config.skipped
        );
    }

    /// `[shell]` has no live keys: any key is skipped and warned, never refused.
    #[test]
    fn the_shell_section_has_no_live_keys() {
        for text in [
            "schema_version = 1\n[shell]\nauto_background_secs = 30\n",
            "schema_version = 1\n[shell]\nfuture_key = 1\n",
        ] {
            let config = parse_str(text).unwrap();
            assert!(
                config.skipped.iter().any(|key| key.starts_with("shell.")),
                "{text}: {:?}",
                config.skipped
            );
        }
    }

    /// ADR 0014 R5: `[providers.<id>] reasoning_replay` overrides the catalog — a
    /// non-empty field name, or `false` to switch replay off. `true` names no field and is
    /// refused rather than guessed.
    #[test]
    fn a_named_providers_reasoning_replay_override_is_carried() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.corp]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://llm.example.com/v1\"\n",
            "reasoning_replay = \"reasoning_content\"\n",
            "[providers.relay]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://relay.example/v1\"\n",
            "reasoning_replay = false\n",
        ))
        .unwrap();
        assert_eq!(
            config.providers["corp"].reasoning_replay,
            Some(crate::provider::ReasoningReplayOverride::Field(
                "reasoning_content".into()
            ))
        );
        assert_eq!(
            config.providers["relay"].reasoning_replay,
            Some(crate::provider::ReasoningReplayOverride::Off)
        );
        // Absent: the catalog decides.
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.corp]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://llm.example.com/v1\"\n",
        ))
        .unwrap();
        assert_eq!(config.providers["corp"].reasoning_replay, None);

        // `true` is not a field name.
        let error = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.corp]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://llm.example.com/v1\"\n",
            "reasoning_replay = true\n",
        ))
        .unwrap_err();
        assert!(matches!(error, ConfigError::WrongType { .. }), "{error}");
        assert!(error.to_string().contains("reasoning_replay"), "{error}");
    }

    #[test]
    fn the_adrs_ask_allow_deny_profiles_are_known_and_ignored() {
        let config = parse_str("schema_version = 1\n[permissions]\nprofile = \"ask\"\n").unwrap();
        assert_eq!(config.permissions.profile, None, "no tier in this build");
        assert!(config.skipped.contains(&"permissions.profile".to_string()));
    }

    #[test]
    fn unknown_and_unwired_keys_are_collected() {
        let config = parse_str(
            r#"
schema_version = 1
future_top = 1
[model]
include_usage = true
future_option = "alpha"
"#,
        )
        .unwrap();
        assert!(config.skipped.contains(&"future_top".to_string()));
        assert!(config.skipped.contains(&"model.include_usage".to_string()));
        assert!(config.skipped.contains(&"model.future_option".to_string()));
        assert!(
            !config.skipped.iter().any(|key| key == "mcp"),
            "mcp is wired now, never skipped: {:?}",
            config.skipped
        );
    }

    /// `[mcp]` (ADR 0008 §8): both TOML forms reach the server list through the same
    /// loader the `--mcp-config` file uses; `mcp` is never a skipped key.
    #[test]
    fn the_mcp_servers_form_is_carried_and_never_skipped() {
        let config = parse_str(
            "schema_version = 1\n[[mcp.servers]]\nid = \"files\"\ncommand = \"/work/example/mcp-files\"\nargs = [\"--root\", \"/work/example\"]\n",
        )
        .unwrap();
        assert_eq!(config.mcp.servers.len(), 1);
        assert_eq!(config.mcp.servers[0].id, "files");
        assert!(config.mcp.warnings.is_empty(), "{:?}", config.mcp.warnings);
        assert!(
            !config.skipped.iter().any(|key| key == "mcp"),
            "{:?}",
            config.skipped
        );
    }

    /// `[mcp.mcpServers.<name>]` (ADR 0008 §8): the shared form normalises the same way,
    /// with the same `HOME` default the file form has.
    #[test]
    fn the_mcp_mcp_servers_form_is_carried_with_the_home_default() {
        let config = parse_str(
            "schema_version = 1\n[mcp.mcpServers.files]\ncommand = \"/work/example/mcp-files\"\n",
        )
        .unwrap();
        assert_eq!(config.mcp.servers.len(), 1);
        assert_eq!(config.mcp.servers[0].id, "files");
        assert_eq!(
            config.mcp.servers[0].inherit_env,
            vec![std::ffi::OsString::from("HOME")],
            "{:?}",
            config.mcp.servers[0].inherit_env
        );
        assert!(config.mcp.warnings.is_empty(), "{:?}", config.mcp.warnings);
    }

    /// A remote `[mcp.mcpServers]` entry is skipped with a warning and starts nothing;
    /// the stdio entry still lands.
    #[test]
    fn a_remote_mcp_entry_is_skipped_and_starts_nothing() {
        let config = parse_str(
            "schema_version = 1\n[mcp.mcpServers.web]\ntype = \"http\"\nurl = \"https://example.com/mcp\"\n[mcp.mcpServers.files]\ncommand = \"/work/example/mcp-files\"\n",
        )
        .unwrap();
        assert_eq!(config.mcp.servers.len(), 1);
        assert_eq!(config.mcp.servers[0].id, "files");
        assert_eq!(config.mcp.warnings.len(), 1, "{:?}", config.mcp.warnings);
        assert!(
            config.mcp.warnings[0].contains("skipped `web`"),
            "{:?}",
            config.mcp.warnings
        );
    }

    /// Both `[mcp]` forms at once, or neither, is a config error like it is for the file.
    #[test]
    fn both_mcp_forms_or_neither_is_a_config_error() {
        let error = parse_str(
            "schema_version = 1\n[[mcp.servers]]\nid = \"a\"\ncommand = \"true\"\n[mcp.mcpServers]\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot be combined"), "{error}");
        let error = parse_str("schema_version = 1\n[mcp]\nfuture = 1\n").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("expected `servers` or `mcpServers`"),
            "{error}"
        );
        let error = parse_str("schema_version = 1\n[mcp]\n").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("expected `servers` or `mcpServers`"),
            "{error}"
        );
    }

    /// `[mcp]` must be a table, and duplicate ids are refused before any process starts.
    #[test]
    fn a_non_table_mcp_and_a_duplicate_mcp_id_are_refused() {
        let error = parse_str("schema_version = 1\nmcp = 3\n").unwrap_err();
        assert!(
            error.to_string().contains("`mcp` must be a table"),
            "{error}"
        );
        let error = parse_str(
            "schema_version = 1\n[[mcp.servers]]\nid = \"a\"\ncommand = \"true\"\n[[mcp.servers]]\nid = \"a\"\ncommand = \"true\"\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate server id"), "{error}");
    }

    #[test]
    fn keybindings_are_carried_raw_for_the_tui() {
        let config =
            parse_str("schema_version = 1\n[tui.keybindings]\nsend = [\"ctrl-s\"]\n").unwrap();
        assert_eq!(
            config.tui.keybindings,
            vec![("send".to_string(), vec!["ctrl-s".to_string()])]
        );
    }

    /// Every configuration key the parser accepts appears in `docs/user/configuration.md`.
    /// Mutation: delete one key row from configuration.md → this fails naming it.
    /// Why this cannot drift: the list below is fed through the real reader (a renamed or
    /// removed key fails the parse or lands in `skipped`), and every
    /// `docs/schema/config/` fixture the reader accepts must resolve to the same list — a
    /// new accepted key needs a fixture by repo convention, so it fails here until the
    /// list and the page both name it. Fixture keys the reader ignores land in
    /// `skipped` instead and are exempt, exactly like unknown user keys.
    #[test]
    fn every_accepted_key_appears_in_configuration_md() {
        /// Full key paths, each one a literal string `configuration.md` must contain.
        const KEYS: &[&str] = &[
            "schema_version",
            "model.provider",
            "model.base_url",
            "model.model",
            "model.headers",
            "model.context_window",
            "model.retries",
            "model.smol",
            "model.smol.provider",
            "model.smol.model",
            "model.include_usage",
            "permissions.profile",
            "permissions.approval",
            "sandbox.allow_unix_sockets",
            "sandbox.allow_env",
            "integrations.herdr",
            "control.enabled",
            "compaction.auto",
            "compaction.thinking",
            "hooks.PreToolUse",
            "hooks.PostToolUse",
            "hooks.Stop",
            "hooks.intent_stop",
            "skills.extra_paths",
            "skills.builtin",
            "skills.disabled",
            "memory.injection.additions",
            "memory.injection.addition_budget",
            "shell",
            "tui.theme",
            "tui.colors",
            "tui.keybindings",
            "tui.mouse",
            "tui.input_style",
            "tui.clipboard",
            "tui.bell",
            "tui.tips",
            "tui.banner",
            "tui.brand",
            "tui.spinner",
            "tui.usage_period_secs",
            "tui.display.reasoning",
            "tui.display.tool_output",
            "tui.display.tool_preview_lines",
            "tui.display.mermaid",
            "tui.display.highlight",
            "tui.footer.preset",
            "tui.footer.separator",
            "tui.footer.separator_style",
            "tui.footer.glyphs",
            "tui.footer.context_line",
            "tui.footer.transparent",
            "tui.footer.items",
            "tui.footer.rows",
            "tui.widgets.above_input",
            "tui.widgets.below_input",
            "mcp.servers",
        ];
        /// Dynamic families: the doc names the section, fixture leaves may live under it.
        const SECTIONS: &[&str] = &[
            "providers",
            "model.headers",
            "model.smol",
            "tui.colors",
            "tui.keybindings",
            "tui.display",
            "tui.footer",
            "tui.footer.items",
            "tui.footer.rows",
            "tui.widgets",
            "hooks.PreToolUse",
            "hooks.PostToolUse",
            "hooks.Stop",
            "mcp.mcpServers",
            "memory.injection",
            "shell",
        ];
        fn covered(path: &str) -> bool {
            if KEYS.contains(&path) {
                return true;
            }
            if SECTIONS
                .iter()
                .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}.")))
            {
                return true;
            }
            let mut rest = path;
            while let Some((parent, _)) = rest.rsplit_once('.') {
                if KEYS.contains(&parent) || SECTIONS.contains(&parent) {
                    return true;
                }
                rest = parent;
            }
            false
        }

        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let doc = std::fs::read_to_string(manifest.join("../../docs/user/configuration.md"))
            .expect("configuration.md exists");
        for key in KEYS.iter().chain(SECTIONS.iter()) {
            assert!(
                doc.contains(key),
                "{key} is missing from docs/user/configuration.md"
            );
        }

        // The list parses through the real reader with nothing skipped.
        let path = std::path::Path::new("/tmp/test/config.toml");
        let parsed = parse(
            path,
            concat!(
                "schema_version = 1\n",
                "[model]\n",
                "provider = \"office\"\n",
                "base_url = \"https://models.example.test/v1\"\n",
                "model = \"m-1\"\n",
                "retries = 3\n",
                "context_window = 100\n",
                "[model.headers]\n",
                "x-tenant = \"team\"\n",
                "[model.smol]\n",
                "provider = \"openai-compatible\"\n",
                "model = \"m-1\"\n",
                "[providers.office]\n",
                "kind = \"openai-compatible\"\n",
                "base_url = \"https://models.example.test/v1\"\n",
                "reasoning_replay = \"reasoning_content\"\n",
                "[permissions]\n",
                "profile = \"workspace\"\n",
                "approval = \"on-ask\"\n",
                "[sandbox]\n",
                "allow_unix_sockets = [\"/tmp/x.sock\"]\n",
                "allow_env = [\"MY_NAME\"]\n",
                "[integrations]\n",
                "herdr = false\n",
                "[control]\n",
                "enabled = false\n",
                "[compaction]\n",
                "auto = false\n",
                "thinking = \"low\"\n",
                "[[hooks.PreToolUse]]\n",
                "matcher = \"shell\"\n",
                "[[hooks.PreToolUse.hooks]]\n",
                "type = \"command\"\n",
                "command = \"echo hi\"\n",
                "timeout = 10\n",
                "[[hooks.PostToolUse]]\n",
                "[[hooks.PostToolUse.hooks]]\n",
                "type = \"command\"\n",
                "command = \"echo hi\"\n",
                "[[hooks.Stop]]\n",
                "[[hooks.Stop.hooks]]\n",
                "type = \"command\"\n",
                "command = \"echo hi\"\n",
                "[hooks]\n",
                "intent_stop = false\n",
                "[skills]\n",
                "builtin = false\n",
                "disabled = [\"review\"]\n",
                "extra_paths = [\"skills\"]\n",
                "[memory.injection]\n",
                "additions = false\n",
                "addition_budget = 10\n",
                "[shell]\n",
                "[tui]\n",
                "theme = \"mono\"\n",
                "mouse = false\n",
                "input_style = \"box\"\n",
                "clipboard = \"off\"\n",
                "bell = \"all\"\n",
                "tips = false\n",
                "banner = false\n",
                "brand = \"R\"\n",
                "spinner = \"arc\"\n",
                "usage_period_secs = 60\n",
                "[tui.colors]\n",
                "user = \"#ffffff\"\n",
                "[tui.keybindings]\n",
                "send = [\"ctrl-s\"]\n",
                "[tui.display]\n",
                "reasoning = \"hidden\"\n",
                "tool_output = \"expanded\"\n",
                "tool_preview_lines = 3\n",
                "mermaid = false\n",
                "highlight = false\n",
                "[tui.footer]\n",
                "preset = \"pi\"\n",
                "separator = \"|\"\n",
                "separator_style = \"dim\"\n",
                "glyphs = \"ascii\"\n",
                "context_line = \"percentage\"\n",
                "transparent = true\n",
                "[tui.footer.items.status]\n",
                "symbol = \"S\"\n",
                "format = \"x\"\n",
                "disabled = true\n",
                "priority = 1\n",
                "width = 20\n",
                "[[tui.footer.rows]]\n",
                "left = [\"model\"]\n",
                "right = [\"usage\"]\n",
                "separator = \"|\"\n",
                "position = \"footer\"\n",
                "[tui.widgets]\n",
                "above_input = [\"status\"]\n",
                "below_input = [\"usage\"]\n",
                "[[mcp.servers]]\n",
                "id = \"s\"\n",
                "command = \"/bin/true\"\n",
            ),
        )
        .expect("the key list parses");
        assert!(
            parsed.skipped.is_empty(),
            "the key list must leave nothing skipped: {:?}",
            parsed.skipped
        );

        // Every fixture the reader accepts resolves to the same list.
        fn leaves(value: &Value, path: String, out: &mut Vec<String>) {
            match value {
                Value::Table(table) => {
                    let sensitive = path.rsplit('.').next().is_some_and(|last| {
                        SENSITIVE_KEYS.contains(&last.to_ascii_lowercase().as_str())
                    });
                    if table.len() == 2
                        && table.contains_key("from")
                        && table.contains_key("name")
                        && sensitive
                    {
                        out.push(path);
                        return;
                    }
                    for (key, value) in table {
                        let here = if path.is_empty() {
                            key.clone()
                        } else {
                            format!("{path}.{key}")
                        };
                        leaves(value, here, out);
                    }
                }
                Value::Array(items) => {
                    if items.iter().all(|item| item.is_table()) {
                        for item in items {
                            leaves(item, path.clone(), out);
                        }
                    } else {
                        out.push(path);
                    }
                }
                _ => out.push(path),
            }
        }
        let dir = manifest.join("../../docs/schema/config");
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("config fixtures exist")
            .flatten()
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("toml"))
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        assert!(!entries.is_empty(), "no fixtures found in {dir:?}");
        for entry in entries {
            let fixture = entry.path();
            let text = std::fs::read_to_string(&fixture).expect("fixture reads");
            let Ok(parsed) = parse(&fixture, &text) else {
                continue; // a reject-fixture: nothing to document.
            };
            let value: Value = toml::from_str(&text).expect("fixture parses as TOML");
            let mut paths = Vec::new();
            leaves(&value, String::new(), &mut paths);
            for leaf in paths {
                if leaf == "schema_version" {
                    continue;
                }
                let ignored = parsed
                    .skipped
                    .iter()
                    .any(|key| key == &leaf || leaf.starts_with(&format!("{key}.")));
                assert!(
                    covered(&leaf) || ignored,
                    "{}: {leaf} is neither documented nor skipped",
                    fixture.display()
                );
            }
        }
    }

    /// The `configuration.md` headings whose tables hold the keys this check reads. Each
    /// heading is an anchor: a row in another table on the page is never read.
    const CONFIGURATION_KEY_HEADINGS: &[&str] = &[
        "## `schema_version`",
        "## `[model]`",
        "## `[providers.<name>]`",
        "## `[permissions]`",
        "## `[hooks]`",
        "## `[skills]`",
        "## `[memory.injection]`",
        "## `[control]`",
        "## `[integrations]`",
        "## `[compaction]`",
        "## `[tui]`",
    ];

    /// Every closed value set the reader enforces appears in its key's own row of
    /// `docs/user/configuration.md`, and every documented default matches the code.
    /// Mutation: drop one value from a row (or change one default cell) → this fails
    /// naming the key. Why the code is the source that cannot drift: each value list is the
    /// `pub const` the parser's own guard reads, and each default is the effective value the
    /// host computes from an empty config. Each key's table is located by its section
    /// heading; a row in another table never counts.
    #[test]
    fn closed_value_sets_and_defaults_appear_in_configuration_md() {
        use crate::model::COMPACTION_THINKING_CURRENT;
        use crate::{GENERIC_ID, THINKING_LEVELS};
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let doc = std::fs::read_to_string(manifest.join("../../docs/user/configuration.md"))
            .expect("configuration.md exists");
        // Every table under the config-section headings, flattened into one row list. The
        // headings are the anchors; a table in any other section is never read.
        let key_rows: Vec<Vec<String>> = CONFIGURATION_KEY_HEADINGS
            .iter()
            .flat_map(|heading| {
                radian_coding::drift::tables_under(&doc, "configuration.md", heading)
            })
            .flatten()
            .collect();
        fn row<'a>(rows: &'a [Vec<String>], key: &str) -> Vec<&'a Vec<String>> {
            let keyed: Vec<&Vec<String>> = rows
                .iter()
                .filter(|row| row.first().is_some_and(|cell| cell.contains(key)))
                .collect();
            assert!(!keyed.is_empty(), "{key} has no row in configuration.md");
            keyed
        }
        // Each value of a closed set appears in its key's row, not just anywhere.
        let sets: &[(&str, Vec<String>)] = &[
            (
                "tui.clipboard",
                tui::CLIPBOARD_MODES.iter().map(|s| s.to_string()).collect(),
            ),
            (
                "tui.bell",
                tui::BELL_MODES.iter().map(|s| s.to_string()).collect(),
            ),
            (
                "tui.display.reasoning",
                tui::DISPLAY_REASONING_MODES
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            (
                "tui.display.tool_output",
                tui::DISPLAY_TOOL_OUTPUT_MODES
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            (
                "providers.<name>.kind",
                model::PROVIDER_KINDS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            (
                "permissions.approval",
                radian_permissions::APPROVAL_POLICIES
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
        ];
        for (key, values) in sets {
            for row in row(&key_rows, key) {
                for value in values {
                    assert!(
                        row.iter().any(|cell| cell.contains(value.as_str())),
                        "{key} row is missing value {value}: {row:?}"
                    );
                }
            }
        }
        // `compaction.thinking` is the seven levels plus `current`.
        for row in row(&key_rows, "compaction.thinking") {
            for level in THINKING_LEVELS {
                assert!(
                    row.iter().any(|cell| cell.contains(level)),
                    "compaction.thinking row misses {level}: {row:?}"
                );
            }
            assert!(
                row.iter()
                    .any(|cell| cell.contains(COMPACTION_THINKING_CURRENT)),
                "compaction.thinking row misses current: {row:?}"
            );
        }
        // `permissions.profile` is the tier names and their aliases (plus the ignored ADR
        // names, which the row names as ignored).
        for row in row(&key_rows, "permissions.profile") {
            for name in KNOWN_PROFILES {
                assert!(
                    row.iter().any(|cell| cell.contains(name)),
                    "permissions.profile row misses {name}: {row:?}"
                );
            }
        }
        // Every hook event has its own row.
        for event in HOOK_EVENTS {
            let key = format!("hooks.{event}");
            assert!(
                row(&key_rows, &key)
                    .iter()
                    .any(|row| row.iter().any(|cell| cell.contains(event))),
                "{key} row misses {event}"
            );
        }
        // Each default is the code's effective value for an empty config. The default must
        // sit in the row's Default cell (not just anywhere: the Allowed cell would mask a
        // changed default). The four-column `schema_version` row carries no Default cell:
        // there the whole row must contain it.
        fn default_in_row(row: &[String], default: &str) -> bool {
            if row.len() >= 5 {
                row[2].contains(default)
            } else {
                row.iter().any(|cell| cell.contains(default))
            }
        }
        let empty = parse(
            std::path::Path::new("/tmp/test/config.toml"),
            "schema_version = 1\n",
        )
        .expect("empty config parses");
        let defaults: &[(&str, String)] = &[
            ("schema_version", SUPPORTED_SCHEMA_VERSION.to_string()),
            ("model.provider", GENERIC_ID.to_string()),
            (
                "model.retries",
                radian_core::DEFAULT_MODEL_RETRIES.to_string(),
            ),
            ("compaction.auto", DEFAULT_COMPACTION_AUTO.to_string()),
            (
                "compaction.thinking",
                DEFAULT_COMPACTION_THINKING.to_string(),
            ),
            ("skills.builtin", DEFAULT_SKILLS_BUILTIN.to_string()),
            ("hooks.intent_stop", DEFAULT_INTENT_STOP.to_string()),
            (
                "memory.injection.additions",
                empty.memory.injection().additions.to_string(),
            ),
            (
                "memory.injection.addition_budget",
                empty.memory.injection().addition_budget.to_string(),
            ),
            (
                "integrations.herdr",
                IntegrationsConfig::default().herdr.to_string(),
            ),
            (
                "control.enabled",
                ControlConfig::default().enabled.to_string(),
            ),
            (
                "permissions.profile",
                radian_permissions::Preset::FullAccess.as_str().to_string(),
            ),
            (
                "permissions.approval",
                radian_permissions::ApprovalPolicy::OnAsk
                    .as_str()
                    .to_string(),
            ),
        ];
        for (key, default) in defaults {
            let lines = row(&key_rows, key);
            assert!(
                lines.iter().any(|line| default_in_row(line, default)),
                "{key} row misses its default {default}: {lines:?}"
            );
        }
    }
}
