//! The `[model]` section and `[providers.<name>]` (ADR 0008 §2, ADR 0026 D2/D10,
//! ADR 0014 R5, ADR 0015, ADR 0017): the wired model route.
//!
//! Split from `config/mod.rs` by pure move (ADR 0043 decision 5): no behaviour change.
//! Every field is `None` when the file did not set it; the host then keeps its own
//! default and the command line always wins.

use std::path::Path;

use toml::Value;

use super::{Config, ConfigError, wrong_type};

/// The wired model route (ADR 0008 §2). Every field is `None` when the file did not set it;
/// the host then keeps its own default and the command line always wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelConfig {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// `[model.smol]` (ADR 0008 §1, ADR 0043 §1): the cheap route slot. Today only the
    /// `explore` subagent reads it; `None` means unset (and a half-set table warns as
    /// `model.smol` and is ignored, like any unknown key).
    pub smol: Option<SmolConfig>,
    pub headers: Vec<(String, String)>,
    pub context_window: Option<u64>,
    /// `[model] retries` (ADR 0017): how many times a transient model failure is
    /// retried inside one model call. `None` means the host's default
    /// ([`radian_core::DEFAULT_MODEL_RETRIES`]); `0` disables retrying.
    pub retries: Option<u32>,
}

/// `[model.smol]` (ADR 0008 §1): the cheap route slot's `provider` + `model`, the same
/// meaning as the `[model]` route keys, carried verbatim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmolConfig {
    pub provider: String,
    pub model: String,
}

pub(super) fn parse_providers(
    value: &Value,
    path: &Path,
    config: &mut Config,
) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "providers", "a table"));
    };
    for (name, value) in table {
        let Some(spec) = value.as_table() else {
            return Err(wrong_type(path, &format!("providers.{name}"), "a table"));
        };
        if let Some(reason) = crate::provider::name_error(name) {
            return Err(ConfigError::InvalidProviderName {
                path: path.to_path_buf(),
                name: name.clone(),
                reason,
            });
        }
        let mut decl = crate::provider::ProviderDecl {
            kind: String::new(),
            base_url: String::new(),
            reasoning_replay: None,
        };
        for (field, value) in spec {
            match (field.as_str(), value) {
                ("kind", Value::String(kind))
                    if matches!(kind.as_str(), "openai-compatible" | "anthropic") =>
                {
                    decl.kind = kind.clone();
                }
                // D10: only the two wire protocols exist in this build; anything else is a
                // structural error, not a display preference to skip.
                ("kind", _) => {
                    return Err(wrong_type(
                        path,
                        &format!("providers.{name}.kind"),
                        "one of openai-compatible|anthropic",
                    ));
                }
                ("base_url", Value::String(base_url)) if !base_url.is_empty() => {
                    decl.base_url = base_url.clone();
                }
                ("base_url", Value::String(_)) => {
                    return Err(ConfigError::InvalidProviderName {
                        path: path.to_path_buf(),
                        name: name.clone(),
                        reason: "base_url must not be empty".into(),
                    });
                }
                ("base_url", _) => {
                    return Err(wrong_type(
                        path,
                        &format!("providers.{name}.base_url"),
                        "a string",
                    ));
                }
                // ADR 0014 R5: a field name overrides the catalog, `false` turns
                // replay off. `true` names no field, so it is refused rather than guessed.
                ("reasoning_replay", Value::String(field)) if !field.is_empty() => {
                    decl.reasoning_replay = Some(crate::provider::ReasoningReplayOverride::Field(
                        field.clone(),
                    ));
                }
                ("reasoning_replay", Value::Boolean(false)) => {
                    decl.reasoning_replay = Some(crate::provider::ReasoningReplayOverride::Off);
                }
                ("reasoning_replay", _) => {
                    return Err(wrong_type(
                        path,
                        &format!("providers.{name}.reasoning_replay"),
                        "a non-empty string or false",
                    ));
                }
                (other, _) => config.skipped.push(format!("providers.{name}.{other}")),
            }
        }
        config.providers.insert(name.clone(), decl);
    }
    Ok(())
}

pub(super) fn parse_model(
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
            // ADR 0015 (with ADR 0018's "0 or missing is unknown"): a zero window means
            // **unset**, not a real zero-token window — the guard stays off exactly as if the
            // key were absent. It is a valid "off" value in the same shape as `retries = 0`
            // below. A negative or non-integer value is still refused rather than guessed
            // (fail-closed).
            ("context_window", Value::Integer(window)) if *window >= 0 => {
                config.model.context_window = (*window > 0).then_some(*window as u64);
            }
            ("context_window", _) => {
                return Err(wrong_type(
                    path,
                    "model.context_window",
                    "a non-negative integer",
                ));
            }
            // ADR 0017: `[model] retries = 3`; `0` disables retrying. A negative or
            // non-integer value is refused rather than guessed (fail-closed).
            ("retries", Value::Integer(n)) if *n >= 0 => {
                config.model.retries = Some(*n as u32);
            }
            ("retries", _) => {
                return Err(wrong_type(path, "model.retries", "a non-negative integer"));
            }
            // `[model.smol]` (ADR 0008 §1, ADR 0043 §1): the cheap route slot. Both keys
            // carry the same meaning as the `[model]` route keys; a half-set table warns
            // (`model.smol`) and is ignored, like any unknown key.
            ("smol", Value::Table(smol)) => {
                let mut provider = None;
                let mut model = None;
                for (key, value) in smol {
                    match (key.as_str(), value) {
                        ("provider", Value::String(provider_name)) => {
                            provider = Some(provider_name.clone());
                        }
                        ("model", Value::String(model_name)) => {
                            model = Some(model_name.clone());
                        }
                        (other, _) => {
                            config.skipped.push(format!("model.smol.{other}"));
                        }
                    }
                }
                match (provider, model) {
                    (Some(provider), Some(model)) => {
                        config.model.smol = Some(SmolConfig { provider, model });
                    }
                    (None, None) => {}
                    _ => config.skipped.push("model.smol".into()),
                }
            }
            ("smol", _) => {
                config.skipped.push("model.smol".into());
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
