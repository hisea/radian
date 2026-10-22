//! The six tools of `radian arc mcp` (ADR 0048 decision 11).
//!
//! One list is the source for both sides: `tools/list` builds its answer from it, the call
//! dispatch matches its names, and the manual's drift test reads the same names and
//! parameter names from it, so a renamed tool or parameter cannot drift from the page.
//!
//! There is deliberately no tool to confirm, start, merge, reject, change the auto level,
//! or approve: those are the owner's (decision 2).

use serde_json::{Value, json};

/// One MCP tool.
pub struct ToolDef {
    /// The MCP tool name, which is also the dispatch key.
    pub name: &'static str,
    /// What the tool does, shown by `tools/list`.
    pub description: &'static str,
    /// The JSON Schema of the arguments, as the text `tools/list` returns.
    pub input_schema: &'static str,
}

impl ToolDef {
    /// The schema as a JSON value. The text is a compile-time constant and a unit test
    /// parses every one of them, so an unparsable schema is a test failure, not a runtime
    /// surprise; the fallback keeps the server answering anyway.
    pub fn schema(&self) -> Value {
        serde_json::from_str(self.input_schema).unwrap_or_else(|_| json!({"type": "object"}))
    }

    /// The schema's property names and whether each one is required, sorted by name
    /// (`serde_json`'s map is a `BTreeMap`).
    pub fn parameters(&self) -> Vec<(String, bool)> {
        let schema = self.schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        schema["properties"]
            .as_object()
            .map(|properties| {
                properties
                    .keys()
                    .map(|name| (name.clone(), required.contains(&name.as_str())))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The tool names, in the order `tools/list` returns them.
pub fn names() -> Vec<&'static str> {
    TOOLS.iter().map(|tool| tool.name).collect()
}

/// The six tools, in the order `tools/list` returns them.
pub const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "add_plan",
        description: "Create a draft plan. Returns its new id.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "slug": {
      "type": "string",
      "description": "The slug after the plan's number: 1 to 48 characters from a-z, 0-9 and -, with no leading or trailing -."
    },
    "title": {
      "type": "string",
      "description": "The plan's title. It may not be empty."
    },
    "request": {
      "type": "string",
      "description": "The request text: what the plan delivers. It is never rewritten after the plan is confirmed."
    }
  },
  "required": ["slug", "title", "request"],
  "additionalProperties": false
}"#,
    },
    ToolDef {
        name: "add_task",
        description: "Add a task to a draft plan. Returns its new id.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "plan": {
      "type": "string",
      "description": "The id of the draft plan the task belongs to."
    },
    "slug": {
      "type": "string",
      "description": "The slug after the task's number: 1 to 48 characters from a-z, 0-9 and -, with no leading or trailing -."
    },
    "title": {
      "type": "string",
      "description": "The task's title. It may not be empty."
    },
    "brief": {
      "type": "string",
      "description": "The task's brief: what the worker is asked to do."
    },
    "after": {
      "type": "array",
      "items": {"type": "string"},
      "description": "Task ids that must be merged first: a task of the same plan, or a task whose state is done. Default: none."
    },
    "allow_noop": {
      "type": "boolean",
      "description": "The task may deliver no change on purpose. Default: false."
    },
    "extra_stages": {
      "type": "array",
      "items": {
        "type": "string",
        "enum": ["commands", "mutation", "review", "ci", "human"]
      },
      "description": "Gate stages this task adds to the plan's profile. Default: none."
    }
  },
  "required": ["plan", "slug", "title", "brief"],
  "additionalProperties": false
}"#,
    },
    ToolDef {
        name: "update_plan",
        description: "Change a plan. On a draft plan, replace its title or request text. On a confirmed plan, append a change record; the plan then returns to draft with auto off and keeps its tasks' state. Returns the plan.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "plan": {
      "type": "string",
      "description": "The plan id."
    },
    "title": {
      "type": "string",
      "description": "A new title. A draft plan only; refused on a confirmed plan."
    },
    "request": {
      "type": "string",
      "description": "New request text. A draft plan only; after confirmation the request text is never rewritten."
    },
    "change_record": {
      "type": "string",
      "description": "What changed and why. A confirmed plan only: it is appended under the plan's '## Change records' heading with the UTC time. Refused on a draft plan."
    }
  },
  "required": ["plan"],
  "additionalProperties": false
}"#,
    },
    ToolDef {
        name: "add_rework",
        description: "Give a task in review a rework brief. The brief is appended under '## Rework <n>' and the task returns to todo.ready. Returns the task.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "task": {
      "type": "string",
      "description": "The task id. Its state must be review."
    },
    "brief": {
      "type": "string",
      "description": "The rework brief. It may not be empty."
    }
  },
  "required": ["task", "brief"],
  "additionalProperties": false
}"#,
    },
    ToolDef {
        name: "list",
        description: "List plans, each with its tasks in order. Returns the same JSON as `radian arc list --json`.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "plan": {
      "type": "string",
      "description": "Only this plan. Default: every plan."
    }
  },
  "additionalProperties": false
}"#,
    },
    ToolDef {
        name: "show",
        description: "Show one plan or task with every field and its body. Returns the same JSON as `radian arc show --json`.",
        input_schema: r#"{
  "type": "object",
  "properties": {
    "id": {
      "type": "string",
      "description": "The plan or task id. A plan is looked up first."
    }
  },
  "required": ["id"],
  "additionalProperties": false
}"#,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_six_tools_are_the_lead_tools_and_nothing_else() {
        assert_eq!(
            names(),
            vec![
                "add_plan",
                "add_task",
                "update_plan",
                "add_rework",
                "list",
                "show"
            ]
        );
        for tool in TOOLS {
            for forbidden in ["confirm", "start", "merge", "reject", "auto", "approve"] {
                assert!(
                    !tool.name.contains(forbidden),
                    "`{}` looks like an owner action",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn every_schema_parses_and_names_its_parameters() {
        for tool in TOOLS {
            let schema = tool.schema();
            assert_eq!(schema["type"], "object", "{}", tool.name);
            assert_eq!(schema["additionalProperties"], false, "{}", tool.name);
            assert!(
                !tool.parameters().is_empty(),
                "{} has no parameter",
                tool.name
            );
            let required: Vec<&str> = schema["required"]
                .as_array()
                .map(|names| names.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            for name in &required {
                assert!(
                    schema["properties"].get(*name).is_some(),
                    "{} requires unknown `{name}`",
                    tool.name
                );
            }
        }
        let add_task = TOOLS
            .iter()
            .find(|tool| tool.name == "add_task")
            .expect("add_task exists");
        let parameters = add_task.parameters();
        let required: Vec<&str> = parameters
            .iter()
            .filter(|(_, required)| *required)
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(required, vec!["brief", "plan", "slug", "title"]);
        for optional in ["after", "allow_noop", "extra_stages"] {
            assert!(
                parameters
                    .iter()
                    .any(|(name, req)| name == optional && !req),
                "`{optional}` must be optional"
            );
        }
    }

    #[test]
    fn the_extra_stages_are_the_gate_stages() {
        let add_task = TOOLS
            .iter()
            .find(|tool| tool.name == "add_task")
            .expect("add_task exists");
        let schema = add_task.schema();
        let values: Vec<&str> = schema["properties"]["extra_stages"]["items"]["enum"]
            .as_array()
            .expect("the stage enum is a list")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(values, crate::store::STAGE_NAMES);
    }
}
