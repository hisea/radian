//! The `[sandbox]` section (ADR 0016, ADR 0034):
//! which Unix sockets the sandbox may connect to and which environment variable
//! names are passed through to a sandboxed command.
//!
//! Split from `config/mod.rs` by pure move (ADR 0043 decision 5): no behaviour change.
//! Only the user-level configuration may set it: a cloned repository must not be able
//! to open a socket for itself or pass an environment variable to a model-run command.

use std::path::{Path, PathBuf};

use toml::Value;

use super::{Config, ConfigError, wrong_type};

/// `[sandbox]` (ADR 0016, ADR 0034). Only the user-level
/// configuration may set it: a cloned repository must not be able to open a socket for itself
/// or pass an environment variable to a model-run command. The project-level
/// `<workspace>/.radian/config.toml` refuses both keys by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxConfig {
    /// Unix sockets the sandbox may connect to, as written (`~` is expanded when the policy
    /// is built; the path is canonicalized then). A path that does not resolve is skipped and
    /// named on the startup line.
    pub allow_unix_sockets: Vec<String>,
    /// Environment variable **names** passed through to a sandboxed command (ADR 0034
    /// revision 1 decision 7). Names only: a value cannot be set here.
    pub allow_env: Vec<String>,
}

pub(super) fn parse_sandbox(
    value: &Value,
    path: &Path,
    config: &mut Config,
) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "sandbox", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("allow_unix_sockets", Value::Array(items)) => {
                for (index, item) in items.iter().enumerate() {
                    match item {
                        Value::String(entry) if !entry.trim().is_empty() => {
                            config.sandbox.allow_unix_sockets.push(entry.clone())
                        }
                        _ => {
                            return Err(wrong_type(
                                path,
                                &format!("sandbox.allow_unix_sockets[{index}]"),
                                "a non-empty string",
                            ));
                        }
                    }
                }
            }
            ("allow_unix_sockets", _) => {
                return Err(wrong_type(
                    path,
                    "sandbox.allow_unix_sockets",
                    "an array of strings",
                ));
            }
            // ADR 0034: names only, so a value can never be smuggled in
            // through `NAME=value`. A bad entry is a structural error, like a bad socket path.
            ("allow_env", Value::Array(items)) => {
                for (index, item) in items.iter().enumerate() {
                    match item {
                        Value::String(name) if valid_env_name(name) => {
                            config.sandbox.allow_env.push(name.clone())
                        }
                        _ => {
                            return Err(wrong_type(
                                path,
                                &format!("sandbox.allow_env[{index}]"),
                                "an environment variable name (no `=`)",
                            ));
                        }
                    }
                }
            }
            ("allow_env", _) => {
                return Err(wrong_type(path, "sandbox.allow_env", "an array of strings"));
            }
            (other, _) => config.skipped.push(format!("sandbox.{other}")),
        }
    }
    Ok(())
}

/// An environment variable **name**: non-empty, no `=` (it sets no value) and no whitespace.
fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('=') && !name.chars().any(char::is_whitespace)
}

/// Expands `~` and canonicalizes `[sandbox] allow_unix_sockets` (ADR 0016). Only
/// absolute paths are accepted (a relative one is skipped, never joined to the config dir:
/// the sandbox rule must name exactly one socket). Returns the canonical paths and one
/// warning per entry that was skipped. Pure over `home`, so the rules are testable.
pub fn resolve_allow_unix_sockets(
    raw: &[String],
    home: Option<&Path>,
) -> (Vec<PathBuf>, Vec<String>) {
    let mut paths = Vec::new();
    let mut warnings = Vec::new();
    for entry in raw {
        let expanded = if entry == "~" {
            home.map(Path::to_path_buf)
        } else if let Some(rest) = entry.strip_prefix("~/") {
            home.map(|home| home.join(rest))
        } else if entry.starts_with('~') {
            // `~user` is not expanded.
            None
        } else {
            Some(PathBuf::from(entry))
        };
        let Some(expanded) = expanded else {
            warnings.push(format!(
                "sandbox.allow_unix_sockets: {entry}: cannot expand the home directory"
            ));
            continue;
        };
        if !expanded.is_absolute() {
            warnings.push(format!(
                "sandbox.allow_unix_sockets: {entry}: must be an absolute path"
            ));
            continue;
        }
        match std::fs::canonicalize(&expanded) {
            Ok(path) => {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
            Err(error) => warnings.push(format!("sandbox.allow_unix_sockets: {entry}: {error}")),
        }
    }
    (paths, warnings)
}
