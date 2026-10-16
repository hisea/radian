//! One place a tool turns raw arguments into its typed struct.
//!
//! A weak model often sends arguments that are almost right: a string where a number was
//! expected, an `""` for "use the default", a misspelled key, or two mistakes at once. This
//! module reports *every* problem in one failure, echoes the arguments the model sent, and
//! ends with a fixed instruction, so one round trip is enough to fix a call.
//!
//! The value is checked against the tool's own published `input_schema` (its `properties`,
//! `required` and `additionalProperties`), which is why each tool keeps its schema in a
//! `schema()` function the spec and the parser share.

use agent_core::ToolFailure;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::fail;

/// How much of the received arguments is echoed back. A wrong `path` on a large `write` must
/// not paste the 8 MiB `content` into the transcript.
const MAX_ECHO_BYTES: usize = 4096;

/// Parses `raw` into `T` against `schema`. On any problem (a required argument missing, the
/// wrong type, an unknown argument, a value `serde` still cannot read) the failure lists all
/// of them. An optional property sent as `""` is treated as absent.
pub(crate) fn parse<T: DeserializeOwned>(
    tool: &str,
    schema: &Value,
    raw: &Value,
) -> Result<T, ToolFailure> {
    let cleaned = clean(schema, raw);
    let mut problems = validate(schema, &cleaned);
    if problems.is_empty() {
        match serde_json::from_value::<T>(cleaned) {
            Ok(value) => return Ok(value),
            Err(error) => problems.push(error.to_string()),
        }
    }
    Err(invalid(tool, raw, problems))
}

/// The one failure shape for bad arguments.
pub(crate) fn invalid(
    tool: &str,
    raw: &Value,
    problems: impl IntoIterator<Item = impl Into<String>>,
) -> ToolFailure {
    let mut message = format!("invalid arguments for `{tool}`:");
    for problem in problems {
        message.push_str("\n- ");
        message.push_str(&problem.into());
    }
    message.push_str("\narguments received: ");
    message.push_str(&echo(raw));
    message.push_str("\nUpdate the arguments and call the tool again");
    fail("invalid_arguments", message)
}

/// Drops an optional property sent as `""` (a frequent DeepSeek shape for "the default") and
/// treats `null` as an empty object. A **required** property sent as `""` is kept, because
/// `write`'s `content` and `patch`'s `new` may legitimately be empty.
fn clean(schema: &Value, raw: &Value) -> Value {
    let mut object = match raw {
        Value::Object(map) => map.clone(),
        Value::Null => Map::new(),
        other => return other.clone(),
    };
    let required = required_names(schema);
    object.retain(|key, value| {
        !(value.as_str() == Some("") && !required.iter().any(|name| name == key))
    });
    Value::Object(object)
}

/// Every problem with `raw` against the schema's `properties`, `required` and
/// `additionalProperties`. `minimum`/`maximum` and the `type`: `integer` checks with a
/// semantic meaning (a limit exceeded) stay in the tool, which reports those as their own
/// error codes.
fn validate(schema: &Value, raw: &Value) -> Vec<String> {
    let Some(object) = raw.as_object() else {
        return vec![format!(
            "arguments must be an object, got {}",
            type_name(raw)
        )];
    };
    let mut problems = Vec::new();
    for name in required_names(schema) {
        if !object.contains_key(&name) {
            problems.push(format!("missing required argument `{name}`"));
        }
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    for (key, value) in object {
        let Some(property) = properties.and_then(|properties| properties.get(key)) else {
            problems.push(format!("unknown argument `{key}`"));
            continue;
        };
        if let Some(expected) = property.get("type").and_then(Value::as_str)
            && !type_matches(expected, value)
        {
            problems.push(format!(
                "`{key}` must be {expected}, got {}",
                type_name(value)
            ));
        }
    }
    problems
}

fn required_names(schema: &Value) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|required| {
            required
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The received arguments as JSON, bounded so a large value cannot dominate the failure.
fn echo(raw: &Value) -> String {
    let text = raw.to_string();
    if text.len() <= MAX_ECHO_BYTES {
        return text;
    }
    let mut end = MAX_ECHO_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}… [echo truncated at {MAX_ECHO_BYTES} bytes]",
        &text[..end]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[derive(serde::Deserialize, Debug)]
    struct Toy {
        path: String,
        offset: Option<i64>,
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    #[test]
    fn every_problem_is_listed_and_the_arguments_are_echoed() {
        let raw = json!({"offset": "soon", "extra": true});
        let failure = parse::<Toy>("toy", &schema(), &raw).unwrap_err();
        assert_eq!(failure.code, "invalid_arguments");
        assert!(
            failure.message.contains("missing required argument `path`"),
            "{}",
            failure.message
        );
        assert!(
            failure
                .message
                .contains("`offset` must be integer, got string"),
            "{}",
            failure.message
        );
        assert!(
            failure.message.contains("unknown argument `extra`"),
            "{}",
            failure.message
        );
        assert!(
            failure
                .message
                .contains(r#"arguments received: {"offset":"soon","extra":true}"#)
                || failure
                    .message
                    .contains("arguments received: {\"extra\":true,\"offset\":\"soon\"}"),
            "{}",
            failure.message
        );
        assert!(
            failure
                .message
                .ends_with("Update the arguments and call the tool again"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn an_empty_optional_string_is_absent_but_a_required_one_is_kept() {
        let raw = json!({"path": "f.txt", "offset": ""});
        let toy: Toy = parse("toy", &schema(), &raw).unwrap();
        assert_eq!(toy.path, "f.txt");
        assert_eq!(toy.offset, None);

        let required = json!({"path": ""});
        let toy: Toy = parse("toy", &schema(), &required).unwrap();
        assert_eq!(toy.path, "");
    }

    #[test]
    fn the_echo_is_bounded_on_a_character_boundary() {
        let raw = json!({"path": "é".repeat(MAX_ECHO_BYTES), "offset": "soon"});
        let failure = parse::<Toy>("toy", &schema(), &raw).unwrap_err();
        assert!(failure.message.contains("echo truncated"));
        assert!(
            failure
                .message
                .ends_with("Update the arguments and call the tool again")
        );
    }

    #[test]
    fn a_valid_call_is_untouched() {
        let raw = json!({"path": "f.txt", "offset": 3});
        let toy: Toy = parse("toy", &schema(), &raw).unwrap();
        assert_eq!(toy.path, "f.txt");
        assert_eq!(toy.offset, Some(3));
    }

    #[test]
    fn an_unexpected_shape_is_a_problem_not_a_panic() {
        // A non-object and `null` both produce a failure, never a panic.
        let failure =
            parse::<BTreeMap<String, String>>("toy", &schema(), &json!([1, 2])).unwrap_err();
        assert!(
            failure.message.contains("must be an object"),
            "{}",
            failure.message
        );
    }
}
