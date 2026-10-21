//! Why the configuration was refused (ADR 0008): the error type for the config reader.
//!
//! Split from `config/mod.rs` by pure move (ADR 0043 decision 5): no behaviour change. The
//! `Display` never contains a secret value.

use std::fmt;
use std::path::PathBuf;

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
    /// A `[providers.<name>]` entry that can never be used (ADR 0026 D10): the name is not a
    /// valid provider name, or it shadows a built-in id. Refused, never guessed around.
    InvalidProviderName {
        path: PathBuf,
        name: String,
        reason: String,
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
                "{}: permissions.profile = {value:?} is not one of workspace|full \
                 (or the aliases); refusing rather than guessing",
                path.display()
            ),
            ConfigError::UnknownApproval { path, value } => write!(
                f,
                "{}: permissions.approval = {value:?} is not one of on-ask|always|never|auto; \
                 refusing rather than guessing",
                path.display()
            ),
            ConfigError::ApiKeyRemoved { path } => write!(
                f,
                "{}: `model.api_key` is no longer read; the API key comes only \
                 from ~/.radian/credentials/auth.json. Add an entry for the provider there, e.g. \
                 {{\"opencode-go\": {{\"type\": \"api_key\", \"key\": \"$MY_KEY\"}}}}, or use the \
                 `/login` command",
                path.display()
            ),
            ConfigError::InvalidProviderName { path, name, reason } => {
                write!(f, "{}: providers.{name}: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {}
