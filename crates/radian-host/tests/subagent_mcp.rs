//! ADR 0043: the `explore` subagent is read-only — exactly `read list glob grep` — so
//! it must not receive the parent's `--mcp-config`, while `general` keeps it.
//!
//! The child here is a stub shell script rather than a `radian` binary: the fact each test
//! pins — whether the argv the tool built carries `--mcp-config` — is exactly the child's own
//! input, and a stub makes it observable without a provider. The mutation (passing
//! `--mcp-config` for `explore` again) fails the explore assertion; dropping it for `general`
//! too fails the general assertion.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_core::{CancellationToken, NoopSink, ToolCall, ToolContext, ToolProvider, ToolSession};
use radian_host::{SubagentParts, SubagentTools};
use radian_permissions::{ApprovalPolicy, NoopAudit, Profile};
use radian_sandbox::SandboxSlot;
use serde_json::{Value, json};

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "radian-subagent-mcp-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An executable stub that records its argv (one argument per line), then exits.
fn stub(dir: &Path, argv_path: &Path) -> PathBuf {
    let path = dir.join("child.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
            argv_path.display(),
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

fn flag_value(argv: &[String], flag: &str) -> Option<String> {
    let index = argv.iter().position(|arg| arg == flag)?;
    argv.get(index + 1).cloned()
}

async fn run_child(arguments: Value, coding_root: Option<PathBuf>) -> Vec<String> {
    let label = if arguments.get("agent").and_then(Value::as_str) == Some("explore") {
        "explore"
    } else {
        "general"
    };
    let dir = scratch(label);
    let workspace = dir.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let argv_path = dir.join("argv.txt");
    let child = stub(&dir, &argv_path);
    let coding_root = coding_root.or(Some(workspace.clone()));

    static SINK: NoopSink = NoopSink;
    let permissions = Arc::new(radian_permissions::PermissionHooks::new(
        Profile::safe(&workspace)
            .unwrap()
            .with_approval(ApprovalPolicy::Auto),
        None,
        Arc::new(NoopAudit),
    ));
    let tools = SubagentTools::new(
        SubagentParts {
            executable: Some(child),
            mcp_config: Some("parent-mcp.json".into()),
            ..SubagentParts::default()
        },
        dir.clone(),
        "parent",
        workspace,
        coding_root,
        SandboxSlot::empty(),
        radian_runtime::ConfigSlot::new(Value::Null),
        permissions,
    );
    let session = tools.open_session().await.expect("open the tool session");
    let call = ToolCall {
        id: "c1".into(),
        name: "subagent".into(),
        arguments,
    };
    let cx = ToolContext {
        cancellation: CancellationToken::new(),
        events: &SINK,
        index: 0,
    };
    // The stub exits 0 without writing a journal; the tool still returns its
    // "(wrote no assistant message)" result, which is all this test needs.
    session.execute(&call, cx).await.expect("the stub runs");
    std::fs::read_to_string(&argv_path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// An `explore` child starts without `--mcp-config` even when the parent has one: it mounts
/// exactly the four read-only tools, and MCP tools are never filtered by `--tools`.
#[tokio::test]
async fn the_explore_child_gets_no_mcp_config() {
    let argv = run_child(json!({"agent": "explore", "task": "MCP-TASK"}), None).await;
    assert!(
        !argv.iter().any(|arg| arg == "--mcp-config"),
        "an explore child must not receive --mcp-config: {argv:?}"
    );
    // The read-only allow-list is still there.
    assert_eq!(
        flag_value(&argv, "--tools").as_deref(),
        Some("read,list,glob,grep"),
        "{argv:?}"
    );
}

/// A `general` child keeps the parent's `--mcp-config`: it runs the parent session's tools.
#[tokio::test]
async fn the_general_child_keeps_mcp_config() {
    let argv = run_child(json!({"agent": "general", "task": "MCP-TASK"}), None).await;
    assert_eq!(
        flag_value(&argv, "--mcp-config").as_deref(),
        Some("parent-mcp.json"),
        "a general child must keep the parent's --mcp-config: {argv:?}"
    );
}
