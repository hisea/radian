use std::collections::{HashMap, HashSet};

use crate::message::{Message, ToolCall, ToolResult};
use crate::outcome::SessionError;
use crate::tool::{ToolConcurrency, ToolSpec};

/// Error messages quote at most this many schema violations.
const MAX_REPORTED_VIOLATIONS: usize = 3;

/// The tool declarations of one session, with compiled argument validators.
pub(crate) struct ToolRegistry {
    specs: Vec<ToolSpec>,
    by_name: HashMap<String, usize>,
    validators: Vec<jsonschema::Validator>,
}

impl ToolRegistry {
    /// Compiles every input schema. `jsonschema` is built without HTTP or file resolvers, so a
    /// schema with an external `$ref` fails here instead of triggering I/O.
    pub(crate) fn new(specs: &[ToolSpec]) -> Result<Self, SessionError> {
        let mut by_name = HashMap::new();
        let mut validators = Vec::with_capacity(specs.len());
        for (i, spec) in specs.iter().enumerate() {
            if by_name.insert(spec.name.clone(), i).is_some() {
                return Err(SessionError::ToolSpec {
                    tool: spec.name.clone(),
                    message: "duplicate tool name".into(),
                });
            }
            let validator = jsonschema::validator_for(&spec.input_schema).map_err(|e| {
                SessionError::ToolSpec {
                    tool: spec.name.clone(),
                    message: format!("invalid input schema: {e}"),
                }
            })?;
            validators.push(validator);
        }
        Ok(Self {
            specs: specs.to_vec(),
            by_name,
            validators,
        })
    }

    pub(crate) fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Checks the name and arguments. On failure, returns the error result for the model.
    pub(crate) fn check(&self, call: &ToolCall) -> Result<ToolConcurrency, ToolResult> {
        let Some(&i) = self.by_name.get(&call.name) else {
            return Err(ToolResult::error(
                &call.id,
                "unknown_tool",
                format!("There is no tool named `{}`.", call.name),
            ));
        };
        if !call.arguments.is_object() {
            return Err(ToolResult::error(
                &call.id,
                "invalid_arguments",
                "Tool arguments must be a JSON object.",
            ));
        }
        let violations: Vec<String> = self.validators[i]
            .iter_errors(&call.arguments)
            .take(MAX_REPORTED_VIOLATIONS)
            .map(|e| e.to_string())
            .collect();
        if !violations.is_empty() {
            return Err(ToolResult::error(
                &call.id,
                "invalid_arguments",
                format!(
                    "Arguments do not match the schema: {}",
                    violations.join("; ")
                ),
            ));
        }
        Ok(self.specs[i].concurrency)
    }
}

/// A history is valid when every tool call is answered by exactly one result before the next
/// user or assistant message, and every result answers a call.
pub(crate) fn validate_history(history: &[Message]) -> Result<(), SessionError> {
    let invalid = |m: String| Err(SessionError::InvalidHistory(m));
    let mut seen_ids = HashSet::new();
    let mut open: Vec<&str> = Vec::new();
    for (i, message) in history.iter().enumerate() {
        match message {
            Message::User { .. } | Message::Assistant { .. } if !open.is_empty() => {
                return invalid(format!("message {i}: tool call {} has no result", open[0]));
            }
            Message::User { .. } => {}
            Message::Assistant { tool_calls, .. } => {
                for call in tool_calls {
                    if !seen_ids.insert(call.id.as_str()) {
                        return invalid(format!("message {i}: duplicate tool call id {}", call.id));
                    }
                    open.push(&call.id);
                }
            }
            Message::Tool { result, .. } => {
                let Some(pos) = open.iter().position(|id| *id == result.call_id) else {
                    return invalid(format!(
                        "message {i}: result for unknown or already answered call {}",
                        result.call_id
                    ));
                };
                open.remove(pos);
            }
        }
    }
    match open.first() {
        Some(id) => invalid(format!("tool call {id} has no result")),
        None => Ok(()),
    }
}
