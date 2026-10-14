//! The **read-only** configuration file reader (ADR 0008; its location is ADR 0020's
//! `~/.radian/config/config.toml`).
//!
//! This slice only reads: there is no migration entry point and nothing is ever written back,
//! so ADR 0008 §5.4's "write-back must be faithful or refused" cannot be violated here (the
//! reader never writes).
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

pub mod migrate_pi;

use std::fmt;
use std::path::{Path, PathBuf};

use toml::Value;
use toml::map::Map;

/// The `schema_version` this reader understands.
pub const SUPPORTED_SCHEMA_VERSION: u64 = 1;

/// The environment variable that overrides the config path.
pub const RADIAN_CONFIG_ENV: &str = "RADIAN_CONFIG";

/// The file name inside `~/.radian/config/`.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Keys whose bare (string) value is a plaintext secret (ADR 0008 §4.2 rule 1).
const SENSITIVE_KEYS: [&str; 7] = [
    "api_key",
    "apikey",
    "token",
    "secret",
    "password",
    "passwd",
    "authorization",
];

/// The obvious secret prefixes (ADR 0008 §4.2 rule 2, matching the reference checker).
const SECRET_PREFIXES: [&str; 6] = ["sk-", "sk_", "xoxb-", "ghp_", "AKIA", "Bearer "];

/// Why the configuration was refused. The `Display` never contains a secret value.
#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        message: String,
    },
    MissingVersion {
        path: PathBuf,
    },
    VersionTooOld {
        path: PathBuf,
        supported: u64,
        found: i64,
    },
    PlaintextSecret {
        path: PathBuf,
        key: String,
    },
    SecretLookalike {
        path: PathBuf,
        key: String,
    },
    SecretRef {
        path: PathBuf,
        key: String,
        message: String,
    },
    WrongType {
        path: PathBuf,
        key: String,
        expected: &'static str,
    },
    UnknownProfile {
        path: PathBuf,
        value: String,
    },
    /// `[permissions] approval` has a value this build does not know (ADR 0008 §5.1:
    /// `permissions` is fail-closed). Refused, never guessed at.
    UnknownApproval {
        path: PathBuf,
        value: String,
    },
    /// `model.api_key` is no longer a key source (ADR 0026 D2): the key comes only from
    /// `auth.json`.
    ApiKeyRemoved {
        path: PathBuf,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, source } => {
                write!(f, "{}: cannot read: {source}", path.display())
            }
            ConfigError::Parse { path, message } => {
                write!(f, "{}: invalid TOML: {message}", path.display())
            }
            ConfigError::MissingVersion { path } => write!(
                f,
                "{}: missing or non-integer schema_version",
                path.display()
            ),
            ConfigError::VersionTooOld {
                path,
                supported,
                found,
            } => write!(
                f,
                "{}: schema_version {found} is older than the supported v{supported}; there is no \
                 migration for it in this version (the file is never rewritten)",
                path.display()
            ),
            ConfigError::PlaintextSecret { path, key } => write!(
                f,
                "{}: `{key}` must be a SecretRef table ({{ from = \"env\", name = \"…\" }}), \
                 never a plaintext value",
                path.display()
            ),
            ConfigError::SecretLookalike { path, key } => write!(
                f,
                "{}: `{key}` looks like a plaintext secret; configuration files must not contain \
                 secret values",
                path.display()
            ),
            ConfigError::SecretRef { path, key, message } => {
                write!(f, "{}: `{key}`: {message}", path.display())
            }
            ConfigError::WrongType {
                path,
                key,
                expected,
            } => write!(f, "{}: `{key}` must be {expected}", path.display()),
            ConfigError::UnknownProfile { path, value } => write!(
                f,
                "{}: permissions.profile = {value:?} is not one of safe|workspace|full \
                 (or the aliases); refusing rather than guessing",
                path.display()
            ),
            ConfigError::UnknownApproval { path, value } => write!(
                f,
                "{}: permissions.approval = {value:?} is not one of on-ask|always|never|auto \
                 (ADR 0026 D9); refusing rather than guessing",
                path.display()
            ),
            ConfigError::ApiKeyRemoved { path } => write!(
                f,
                "{}: `model.api_key` is no longer read (ADR 0026 D2); the API key comes only \
                 from ~/.radian/credentials/auth.json. Add an entry for the provider there, e.g. \
                 {{\"opencode-go\": {{\"type\": \"api_key\", \"key\": \"$MY_KEY\"}}}}, or use the \
                 `/login` command (next slice)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// The wired model route (ADR 0008 §2). Every field is `None` when the file did not set it;
/// the host then keeps its own default and the command line always wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelConfig {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub headers: Vec<(String, String)>,
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionsConfig {
    pub profile: Option<String>,
    /// `[permissions] approval` (ADR 0012, ADR 0026 D9): how an `ask` resolves. The name
    /// set is `ApprovalPolicy::parse`'s; an unknown value is refused (fail-closed, the
    /// `permissions` rule).
    pub approval: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionConfig {
    pub auto: Option<bool>,
}

/// One `[tui.colors]` color value (ADR 0029 decision 5): a color literal (`#hex`,
/// `oklch(..)`, `okhsl(..)` or `""` for the terminal default) or a 256-color index.
/// The literal text is resolved by the TUI, which owns the color grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiColorValue {
    Text(String),
    Index(u8),
}

/// One `[tui.colors]` entry (ADR 0029 decision 5): the foreground/background colors and the
/// modifier forces. An absent field leaves the theme's value alone; `bold = false` etc.
/// removes the modifier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiColorOverride {
    pub fg: Option<TuiColorValue>,
    pub bg: Option<TuiColorValue>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub dim: Option<bool>,
    pub underline: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiConfig {
    /// `tui.theme`: a built-in name (`dark|light|mono`) or a theme file name (ADR 0029). The
    /// reader accepts any string — whether the name resolves is the TUI's finding, reported
    /// as a startup warning.
    pub theme: Option<String>,
    /// `tui.colors`: per-role color overrides, raw. The TUI validates the role names and
    /// colors; unknown or malformed entries are startup warnings (the `tui.*` rule).
    pub colors: Vec<(String, TuiColorOverride)>,
    /// The raw `tui.keybindings` table: action name → key strings. The TUI parses and
    /// validates it (its key grammar belongs to the terminal layer).
    pub keybindings: Vec<(String, Vec<String>)>,
    /// `tui.mouse`: whether the terminal reports the mouse wheel (ADR 0023 U2, plan B).
    /// `None` means the default, **on**; the TUI reads `unwrap_or(true)`. `false` sends no
    /// mouse sequence at all and leaves the terminal's native selection/copy alone.
    pub mouse: Option<bool>,
    /// `tui.input_style`: `"lines"` (the default: a rule above and below, `> ` on the first
    /// line) or `"box"` (the bordered input box). `None` means the default; an unknown value is
    /// ignored with a warning (the `tui.*` rule), and the TUI falls back to `lines`.
    pub input_style: Option<String>,
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
    pub compaction: CompactionConfig,
    pub tui: TuiConfig,
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
            "model" => parse_model(value, path, read_only, &mut config)?,
            "permissions" => parse_permissions(value, path, read_only, &mut config)?,
            "compaction" => parse_compaction(value, path, &mut config)?,
            "tui" => parse_tui(value, path, &mut config)?,
            // Known by the ADR, not wired in this slice: the hosts take MCP servers from
            // `--mcp-config`. Ignored, and named on the startup line.
            "mcp" => config.skipped.push("mcp".into()),
            other => config.skipped.push(other.to_owned()),
        }
    }
    config.skipped.sort();
    config.skipped.dedup();
    Ok(config)
}

fn parse_model(
    value: &Value,
    path: &Path,
    read_only: bool,
    config: &mut Config,
) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "model", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("provider", Value::String(s)) => config.model.provider = Some(s.clone()),
            ("base_url", Value::String(s)) => config.model.base_url = Some(s.clone()),
            ("model", Value::String(s)) => config.model.model = Some(s.clone()),
            // ADR 0026 D2: `model.api_key` is refused, never silently ignored (a silent ignore
            // would make the user think the key took effect).
            ("api_key", _) => {
                return Err(ConfigError::ApiKeyRemoved {
                    path: path.to_path_buf(),
                });
            }
            ("headers", Value::Table(headers)) => {
                for (name, value) in headers {
                    if let Value::String(value) = value {
                        config.model.headers.push((name.clone(), value.clone()));
                    } else {
                        config.skipped.push(format!("model.headers.{name}"));
                    }
                }
            }
            ("context_window", Value::Integer(window)) if *window > 0 => {
                config.model.context_window = Some(*window as u64);
            }
            ("context_window", _) => {
                return Err(wrong_type(
                    path,
                    "model.context_window",
                    "a positive integer",
                ));
            }
            // Known by the ADR, not wired here (the host's `--include-usage` controls it).
            ("include_usage", _) => config.skipped.push("model.include_usage".into()),
            (other, _) => {
                if !read_only || table.contains_key(other) {
                    config.skipped.push(format!("model.{other}"));
                }
            }
        }
    }
    Ok(())
}

/// The profile names ADR 0008 §2 lists (`ask`/`allow`/`deny`) plus the tiers ADR 0012/0016
/// define. A value outside this set is refused rather than guessed (fail-closed).
const KNOWN_PROFILES: [&str; 8] = [
    "safe",
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
                    "safe" | "workspace" | "workspace-write" | "full" | "full-access"
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
                        // Fail-closed (ADR 0008 §5.1): a `permissions` value this build does
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

fn parse_compaction(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "compaction", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("auto", Value::Boolean(auto)) => config.compaction.auto = Some(*auto),
            ("auto", _) => return Err(wrong_type(path, "compaction.auto", "a boolean")),
            (other, _) => config.skipped.push(format!("compaction.{other}")),
        }
    }
    Ok(())
}

fn parse_tui(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "tui", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            // Any name is accepted (ADR 0029 decision 1): built-in names resolve directly,
            // anything else is looked up in `~/.radian/themes/<name>.json` by the TUI, which
            // warns and falls back when neither exists. A display preference never refuses
            // the file.
            ("theme", Value::String(theme)) => config.tui.theme = Some(theme.clone()),
            ("theme", _) => config.skipped.push("tui.theme".into()),
            ("colors", Value::Table(colors)) => {
                for (key, value) in colors {
                    match value {
                        // A bare value is the foreground color.
                        Value::String(text) => config.tui.colors.push((
                            key.clone(),
                            TuiColorOverride {
                                fg: Some(TuiColorValue::Text(text.clone())),
                                ..TuiColorOverride::default()
                            },
                        )),
                        Value::Integer(index) if *index >= 0 && *index <= 255 => {
                            config.tui.colors.push((
                                key.clone(),
                                TuiColorOverride {
                                    fg: Some(TuiColorValue::Index(*index as u8)),
                                    ..TuiColorOverride::default()
                                },
                            ))
                        }
                        Value::Table(spec) => {
                            let mut override_value = TuiColorOverride::default();
                            let mut malformed = false;
                            for (field, field_value) in spec {
                                match (field.as_str(), field_value) {
                                    ("fg" | "bg", Value::String(text)) => {
                                        let value = TuiColorValue::Text(text.clone());
                                        if field == "fg" {
                                            override_value.fg = Some(value);
                                        } else {
                                            override_value.bg = Some(value);
                                        }
                                    }
                                    ("fg" | "bg", Value::Integer(index))
                                        if *index >= 0 && *index <= 255 =>
                                    {
                                        let value = TuiColorValue::Index(*index as u8);
                                        if field == "fg" {
                                            override_value.fg = Some(value);
                                        } else {
                                            override_value.bg = Some(value);
                                        }
                                    }
                                    ("bold", Value::Boolean(flag)) => {
                                        override_value.bold = Some(*flag)
                                    }
                                    ("italic", Value::Boolean(flag)) => {
                                        override_value.italic = Some(*flag)
                                    }
                                    ("dim", Value::Boolean(flag)) => {
                                        override_value.dim = Some(*flag)
                                    }
                                    ("underline", Value::Boolean(flag)) => {
                                        override_value.underline = Some(*flag)
                                    }
                                    // An unknown field or a wrong-typed one makes the whole
                                    // entry a display preference: ignore it (and say so).
                                    _ => malformed = true,
                                }
                            }
                            if malformed {
                                config.skipped.push(format!("tui.colors.{key}"));
                            } else {
                                config.tui.colors.push((key.clone(), override_value));
                            }
                        }
                        // A non-string, non-table color is a display preference.
                        _ => config.skipped.push(format!("tui.colors.{key}")),
                    }
                }
            }
            ("colors", _) => return Err(wrong_type(path, "tui.colors", "a table")),
            ("keybindings", Value::Table(bindings)) => {
                for (action, keys) in bindings {
                    match keys.as_array() {
                        Some(keys) => {
                            let keys: Vec<String> = keys
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect();
                            config.tui.keybindings.push((action.clone(), keys));
                        }
                        None => config.skipped.push(format!("tui.keybindings.{action}")),
                    }
                }
            }
            ("keybindings", _) => return Err(wrong_type(path, "tui.keybindings", "a table")),
            ("mouse", Value::Boolean(mouse)) => config.tui.mouse = Some(*mouse),
            ("input_style", Value::String(style)) if matches!(style.as_str(), "lines" | "box") => {
                config.tui.input_style = Some(style.clone());
            }
            // An unknown input style is a display preference: ignore it (and say so), never
            // refuse the file. The TUI falls back to the default.
            ("input_style", _) => config.skipped.push("tui.input_style".into()),
            // A non-boolean mouse is a display preference: ignore it (and say so), never
            // refuse the file.
            ("mouse", _) => config.skipped.push("tui.mouse".into()),
            (other, _) => config.skipped.push(format!("tui.{other}")),
        }
    }
    Ok(())
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

/// The heuristic of ADR 0008 §4.2 rule 2, byte-for-byte the reference checker's.
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
        assert_eq!(config.tui.theme.as_deref(), Some("light"));
    }

    /// `tui.input_style` is carried as a string; the TUI parses it, and an unknown value is a
    /// skipped key (the `tui.*` rule), not a refusal.
    #[test]
    fn the_tui_input_style_is_carried_and_an_unknown_value_is_skipped() {
        let config = parse_str("schema_version = 1\n[tui]\ninput_style = \"box\"\n").unwrap();
        assert_eq!(config.tui.input_style.as_deref(), Some("box"));
        assert!(config.skipped.is_empty());

        let config = parse_str("schema_version = 1\n[tui]\ninput_style = \"future\"\n").unwrap();
        assert_eq!(config.tui.input_style, None);
        assert!(config.skipped.contains(&"tui.input_style".to_string()));
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
        // `~/.radian/themes/<name>.json`) is the TUI's finding, a startup warning.
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
[mcp.servers]
"#,
        )
        .unwrap();
        assert!(config.skipped.contains(&"future_top".to_string()));
        assert!(config.skipped.contains(&"model.include_usage".to_string()));
        assert!(config.skipped.contains(&"model.future_option".to_string()));
        assert!(config.skipped.contains(&"mcp".to_string()));
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
}
