//! `RADIAN_EXTRA_BODY`: fields merged into every model request body (ADR 0026 D13).
//!
//! The host reads the variable once at startup and gives the same map to the startup model and
//! to every route a `/model` switch builds. Both hosts print the same two diagnostics — one
//! line naming the keys applied, one warning naming a key a request setting also sets — so the
//! wording lives here, beside the parser.
//!
//! Reserved keys are checked **per route**, never as a union: the caller names the route's
//! adapter and gets back only the keys that adapter owns. A key another adapter reserves is
//! accepted here and refused when a switch reaches that adapter.

use serde_json::{Map, Value};

use crate::model::Provider;

/// The variable both hosts read at startup. Public so a test can name it without a literal.
pub const EXTRA_BODY_ENV: &str = "RADIAN_EXTRA_BODY";

/// A parsed, validated `RADIAN_EXTRA_BODY`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtraBody {
    fields: Map<String, Value>,
}

impl ExtraBody {
    /// The fields, in insertion order (JSON object order).
    pub fn fields(&self) -> &Map<String, Value> {
        &self.fields
    }

    /// The fields to hand to a model builder.
    pub fn into_fields(self) -> Map<String, Value> {
        self.fields
    }

    /// The one startup line naming the keys, never their values. `None` when no field is set.
    pub fn applied_line(&self) -> Option<String> {
        (!self.fields.is_empty()).then(|| {
            format!(
                "[body] {EXTRA_BODY_ENV} sets: {}",
                self.fields.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }

    /// The keys this body sets that `adapter` reserves (ADR 0026 D13), in the adapter's own
    /// list order. Empty when the route accepts every key.
    pub fn refused_keys(&self, adapter: Provider) -> Vec<&'static str> {
        reserved_keys_for(adapter)
            .iter()
            .copied()
            .filter(|key| self.fields.contains_key(*key))
            .collect()
    }

    /// The one refusal line for `adapter` (ADR 0026 D13), or `None` when the adapter reserves
    /// none of the keys. The values never appear.
    pub fn refusal_for(&self, adapter: Provider) -> Option<String> {
        let refused = self.refused_keys(adapter);
        (!refused.is_empty()).then(|| {
            format!(
                "{EXTRA_BODY_ENV} sets `{}`, which the request builder owns and cannot be \
                 overridden; remove it",
                refused.join("`, `")
            )
        })
    }

    /// One warning line naming every key `settings` also sets: the request value wins (the
    /// adapters merge `extra_body` first, then the request's settings).
    pub fn settings_warning(&self, settings: &Value) -> Option<String> {
        let Value::Object(settings) = settings else {
            return None;
        };
        let mut overlapped: Vec<&str> = self
            .fields
            .keys()
            .filter(|key| settings.contains_key(*key))
            .map(String::as_str)
            .collect();
        if overlapped.is_empty() {
            return None;
        }
        overlapped.sort_unstable();
        Some(format!(
            "[body] {EXTRA_BODY_ENV} sets `{}`, which the request's own setting also sets; the \
             request value wins",
            overlapped.join("`, `")
        ))
    }
}

/// Reads and validates the variable. Empty counts as unset.
pub fn read_extra_body() -> Result<Option<ExtraBody>, String> {
    parse_extra_body(std::env::var(EXTRA_BODY_ENV).ok())
}

/// [`read_extra_body`] with the raw value passed in, so the rule is testable without touching
/// the process environment.
pub fn parse_extra_body(raw: Option<String>) -> Result<Option<ExtraBody>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&raw).map_err(|error| {
        format!(
            "{EXTRA_BODY_ENV} is not valid JSON ({error}); it must be a JSON object whose \
             top-level fields merge into every request body"
        )
    })?;
    let Value::Object(fields) = value else {
        return Err(format!(
            "{EXTRA_BODY_ENV} must be a JSON object whose top-level fields merge into every \
             request body, not {}",
            json_kind(&value)
        ));
    };
    Ok((!fields.is_empty()).then_some(ExtraBody { fields }))
}

/// The body keys one adapter owns (ADR 0026 D13). One source of truth per adapter: the list
/// lives in the adapter crate, whose request builder refuses the same keys again.
pub fn reserved_keys_for(adapter: Provider) -> &'static [&'static str] {
    match adapter {
        Provider::OpenAiCompatible => agent_provider_openai_compatible::reserved_body_keys(),
        Provider::ChatGpt => agent_provider_openai_responses::reserved_body_keys(),
        Provider::Anthropic => agent_provider_anthropic::reserved_body_keys(),
    }
}

/// The JSON kind of a value, for the one error message that names it.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parsed(raw: &str) -> ExtraBody {
        parse_extra_body(Some(raw.to_owned()))
            .expect("the value parses")
            .expect("a non-empty object")
    }

    #[test]
    fn unset_an_empty_value_and_an_empty_object_change_nothing() {
        assert_eq!(parse_extra_body(None), Ok(None));
        assert_eq!(parse_extra_body(Some(String::new())), Ok(None));
        assert_eq!(parse_extra_body(Some("   \n".to_owned())), Ok(None));
        assert_eq!(parse_extra_body(Some("{}".to_owned())), Ok(None));
    }

    #[test]
    fn a_non_object_is_refused_naming_the_variable_and_the_kind() {
        for (raw, kind) in [
            ("[1]", "an array"),
            ("\"x\"", "a string"),
            ("1", "a number"),
            ("true", "a boolean"),
            ("null", "null"),
        ] {
            let error = parse_extra_body(Some(raw.to_owned())).expect_err(raw);
            assert!(error.contains(EXTRA_BODY_ENV), "{raw}: {error}");
            assert!(error.contains(kind), "{raw}: {error}");
        }
        let error = parse_extra_body(Some("{not json".to_owned())).expect_err("invalid JSON");
        assert!(error.contains(EXTRA_BODY_ENV), "{error}");
    }

    #[test]
    fn a_reserved_key_is_refused_per_adapter_and_named_in_the_line() {
        // The chat route accepts `max_tokens` (its builder only inserts the catalog cap when
        // the body has none); the Anthropic route reserves it.
        let extra = parsed(r#"{"max_tokens":32000}"#);
        assert!(extra.refused_keys(Provider::OpenAiCompatible).is_empty());
        assert_eq!(extra.refused_keys(Provider::Anthropic), vec!["max_tokens"]);
        let line = extra
            .refusal_for(Provider::Anthropic)
            .expect("the Anthropic route refuses the key");
        assert!(line.contains(EXTRA_BODY_ENV), "{line}");
        assert!(line.contains("`max_tokens`"), "{line}");
        assert_eq!(extra.refusal_for(Provider::OpenAiCompatible), None);

        // Every key an adapter owns is refused on that adapter alone, and never silently
        // dropped: the line names each one.
        for adapter in [
            Provider::OpenAiCompatible,
            Provider::ChatGpt,
            Provider::Anthropic,
        ] {
            for &key in reserved_keys_for(adapter) {
                let raw = json!({key: true}).to_string();
                let extra = parsed(&raw);
                let line = extra
                    .refusal_for(adapter)
                    .unwrap_or_else(|| panic!("{key} must be refused on {adapter:?}"));
                assert!(line.contains(EXTRA_BODY_ENV), "{key}: {line}");
                assert!(line.contains(&format!("`{key}`")), "{key}: {line}");
            }
        }
    }

    #[test]
    fn the_keys_line_and_the_settings_warning_name_keys_only() {
        let extra = parsed(r#"{"thinking":{"type":"enabled"},"temperature":0.2}"#);
        let line = extra.applied_line().expect("a line for a non-empty object");
        assert!(line.contains("thinking"), "{line}");
        assert!(line.contains("temperature"), "{line}");
        assert!(!line.contains("enabled"), "the values never appear: {line}");
        assert!(!line.contains("0.2"), "the values never appear: {line}");

        let warning = extra
            .settings_warning(&json!({"thinking": {"type": "adaptive"}, "other": 1}))
            .expect("the overlap warns");
        assert!(warning.contains("thinking"), "{warning}");
        assert!(!warning.contains("temperature"), "{warning}");
        assert!(
            !warning.contains("adaptive"),
            "the request's value never appears: {warning}"
        );
        assert_eq!(extra.settings_warning(&json!({"unrelated": 1})), None);
        assert_eq!(extra.settings_warning(&Value::Null), None);
    }
}
