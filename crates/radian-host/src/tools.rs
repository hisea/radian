//! The combined tool provider: the built-in coding tools, the agent memory tools and the MCP
//! servers, behind one `ToolProvider` so `Agent` still has a single provider type.
//!
//! Built-in names (coding and memory) are checked first and may not collide with each other or
//! with an MCP tool name; a collision fails `open_session` rather than silently shadowing one
//! of them.

use std::collections::HashSet;

use agent_core::{
    ToolCall, ToolContext, ToolError, ToolFailure, ToolOutput, ToolProvider, ToolSession, ToolSpec,
};
use agent_extensions::{McpSession, McpToolProvider};
use radian_coding::{AskSession, AskTools, CodingSession, CodingTools};

use crate::memory::{MemorySession, MemoryTools};

pub struct HostTools {
    coding: Option<CodingTools>,
    memory: Option<MemoryTools>,
    /// `ask_user` (ADR 0024 Q5). Separate from the coding tools: a host with no workspace
    /// still has a human to ask.
    ask: Option<AskTools>,
    mcp: McpToolProvider,
}

impl HostTools {
    pub fn new(
        coding: Option<CodingTools>,
        memory: Option<MemoryTools>,
        mcp: McpToolProvider,
    ) -> Self {
        Self {
            coding,
            memory,
            ask: None,
            mcp,
        }
    }

    /// Mounts `ask_user`. The host calls this only when a human is there to answer; without it
    /// the model's tool list has no `ask_user` at all.
    pub fn with_ask(mut self, ask: AskTools) -> Self {
        self.ask = Some(ask);
        self
    }
}

pub struct HostSession {
    coding: Option<CodingSession>,
    memory: Option<MemorySession>,
    ask: Option<AskSession>,
    mcp: McpSession,
    specs: Vec<ToolSpec>,
    coding_names: HashSet<String>,
    memory_names: HashSet<String>,
    ask_names: HashSet<String>,
}

impl ToolProvider for HostTools {
    type Session = HostSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        // Open MCP first: if it fails, no built-in state needs unwinding.
        let mcp = self.mcp.open_session().await?;
        let coding = match &self.coding {
            Some(coding) => Some(coding.open_session().await?),
            None => None,
        };
        let memory = match &self.memory {
            Some(memory) => Some(memory.open_session().await?),
            None => None,
        };
        let ask = match &self.ask {
            Some(ask) => Some(ask.open_session().await?),
            None => None,
        };

        let mut coding_names = HashSet::new();
        let mut memory_names = HashSet::new();
        let mut ask_names = HashSet::new();
        let mut built_in_specs: Vec<ToolSpec> = Vec::new();
        if let Some(coding) = &coding {
            for spec in coding.tool_specs() {
                coding_names.insert(spec.name.clone());
                built_in_specs.push(spec.clone());
            }
        }
        if let Some(memory) = &memory {
            for spec in memory.tool_specs() {
                memory_names.insert(spec.name.clone());
                built_in_specs.push(spec.clone());
            }
        }
        if let Some(ask) = &ask {
            for spec in ask.tool_specs() {
                ask_names.insert(spec.name.clone());
                built_in_specs.push(spec.clone());
            }
        }
        let specs = merge_tool_specs(built_in_specs, mcp.tool_specs().to_vec())?;

        Ok(HostSession {
            coding,
            memory,
            ask,
            mcp,
            specs,
            coding_names,
            memory_names,
            ask_names,
        })
    }
}

impl ToolSession for HostSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        if self.ask_names.contains(&call.name) {
            match &self.ask {
                Some(ask) => ask.execute(call, cx).await,
                None => Err(ToolFailure::new(
                    "unknown_tool",
                    format!("`{}` has no question session", call.name),
                )),
            }
        } else if self.memory_names.contains(&call.name) {
            match &self.memory {
                Some(memory) => memory.execute(call, cx).await,
                None => Err(ToolFailure::new(
                    "unknown_tool",
                    format!("`{}` has no memory session", call.name),
                )),
            }
        } else if self.coding_names.contains(&call.name) {
            match &self.coding {
                Some(coding) => coding.execute(call, cx).await,
                None => Err(ToolFailure::new(
                    "unknown_tool",
                    format!("`{}` has no built-in session", call.name),
                )),
            }
        } else {
            self.mcp.execute(call, cx).await
        }
    }

    async fn close(&mut self) -> Result<(), ToolError> {
        self.mcp.close().await
    }
}

/// Merges the built-in specs (coding then memory) with the MCP specs, refusing a name that is
/// defined twice — including two built-ins.
///
/// Split out and public so the rule is testable on its own: with the current alias scheme
/// (`server__tool`) a real MCP server cannot produce a built-in name, so a silent override
/// would be the only way this could ever go wrong — and an error is the safe end of that.
pub fn merge_tool_specs(
    built_in: Vec<ToolSpec>,
    mcp: Vec<ToolSpec>,
) -> Result<Vec<ToolSpec>, ToolError> {
    let mut specs: Vec<ToolSpec> = Vec::with_capacity(built_in.len() + mcp.len());
    for spec in built_in {
        if specs.iter().any(|existing| existing.name == spec.name) {
            return Err(ToolError(format!(
                "duplicate built-in tool name `{}`: two built-in providers define it",
                spec.name
            )));
        }
        specs.push(spec);
    }
    for spec in mcp {
        if specs.iter().any(|existing| existing.name == spec.name) {
            return Err(ToolError(format!(
                "duplicate tool name `{}`: the built-in tools and an MCP server both define it",
                spec.name
            )));
        }
        specs.push(spec);
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ToolConcurrency;
    use serde_json::json;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            concurrency: ToolConcurrency::Exclusive,
        }
    }

    #[test]
    fn a_duplicate_tool_name_is_refused_not_shadowed() {
        let error = merge_tool_specs(vec![spec("read")], vec![spec("read")]).unwrap_err();
        assert!(
            error.0.contains("duplicate tool name `read`"),
            "{}",
            error.0
        );
    }

    #[test]
    fn two_built_in_providers_cannot_define_the_same_name() {
        let error = merge_tool_specs(vec![spec("read"), spec("read")], vec![]).unwrap_err();
        assert!(error.0.contains("two built-in providers"), "{}", error.0);
    }

    #[test]
    fn distinct_names_merge_in_order() {
        let merged = merge_tool_specs(vec![spec("read")], vec![spec("fx__echo")]).unwrap();
        let names: Vec<&str> = merged.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["read", "fx__echo"]);
    }
}
