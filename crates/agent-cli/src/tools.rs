//! The CLI's tool provider. It is the shared host provider (`radian-host`), so `agent-cli`
//! and `radian-tui` mount exactly the same combination — built-in coding tools plus MCP
//! servers behind one `ToolProvider` — and the same duplicate-name rule.

pub use radian_host::HostTools as CliTools;

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{CancellationToken, ToolProvider, ToolSession};
    use agent_extensions::McpToolProvider;
    use radian_coding::CodingTools;

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

        let call = agent_core::ToolCall {
            id: "t".into(),
            name: "write".into(),
            arguments: serde_json::json!({"path": "f.txt", "content": "hi"}),
        };
        let sink = agent_core::NoopSink;
        let cx = agent_core::ToolContext {
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
