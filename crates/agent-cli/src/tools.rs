//! The CLI's tool provider: the built-in coding tools plus the MCP servers, behind one
//! `ToolProvider` so `Agent` still has a single provider type.
//!
//! Built-in names are checked first and may not collide with an MCP tool name; a collision
//! fails `open_session` rather than silently shadowing one of them.

use std::collections::HashSet;

use agent_core::{
    ToolCall, ToolContext, ToolError, ToolFailure, ToolOutput, ToolProvider, ToolSession, ToolSpec,
};
use agent_extensions::{McpSession, McpToolProvider};
use radian_coding::{CodingSession, CodingTools};

pub struct CliTools {
    coding: Option<CodingTools>,
    mcp: McpToolProvider,
}

impl CliTools {
    pub fn new(coding: Option<CodingTools>, mcp: McpToolProvider) -> Self {
        Self { coding, mcp }
    }
}

pub struct CliSession {
    coding: Option<CodingSession>,
    mcp: McpSession,
    specs: Vec<ToolSpec>,
    built_in: HashSet<String>,
}

impl ToolProvider for CliTools {
    type Session = CliSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        // Open MCP first: if it fails, no built-in state needs unwinding.
        let mcp = self.mcp.open_session().await?;
        let coding = match &self.coding {
            Some(coding) => Some(coding.open_session().await?),
            None => None,
        };

        let mut specs: Vec<ToolSpec> = Vec::new();
        let mut built_in = HashSet::new();
        if let Some(coding) = &coding {
            for spec in coding.tool_specs() {
                built_in.insert(spec.name.clone());
                specs.push(spec.clone());
            }
        }
        for spec in mcp.tool_specs() {
            if specs.iter().any(|existing| existing.name == spec.name) {
                return Err(ToolError(format!(
                    "duplicate tool name `{}`: the built-in tools and an MCP server both \
                     define it",
                    spec.name
                )));
            }
            specs.push(spec.clone());
        }

        Ok(CliSession {
            coding,
            mcp,
            specs,
            built_in,
        })
    }
}

impl ToolSession for CliSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        if self.built_in.contains(&call.name) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::CancellationToken;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agent-cli-tools-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn built_in_tools_are_exposed_and_dispatched() {
        let ws = scratch("builtin");
        let coding = CodingTools::new(&ws).unwrap();
        let tools = CliTools::new(Some(coding), McpToolProvider::new(Vec::new()));
        let session = tools.open_session().await.unwrap();
        let names: Vec<&str> = session
            .tool_specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["read", "write", "patch", "list", "git_status", "git_diff"]
        );

        let call = ToolCall {
            id: "t".into(),
            name: "write".into(),
            arguments: serde_json::json!({"path": "f.txt", "content": "hi"}),
        };
        let sink = agent_core::NoopSink;
        let cx = ToolContext {
            cancellation: CancellationToken::new(),
            events: &sink,
            index: 0,
        };
        session.execute(&call, cx).await.unwrap();
        assert_eq!(std::fs::read_to_string(ws.join("f.txt")).unwrap(), "hi");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn without_a_workspace_only_mcp_tools_exist() {
        let tools = CliTools::new(None, McpToolProvider::new(Vec::new()));
        let session = tools.open_session().await.unwrap();
        assert!(session.tool_specs().is_empty());
    }
}
