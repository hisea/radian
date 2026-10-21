//! ADR 0043: the `explore` subagent is read-only — exactly `read glob grep` — so
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
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "radian-subagent-mcp-{}-{label}-{unique}",
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

async fn run_child(arguments: Value, coding_root: Option<PathBuf>, trust: bool) -> Vec<String> {
    // One test binary, one stub writer and spawner at a time. The stub is written
    // to disk just before the child `exec`s it, and this binary's tests run on
    // parallel threads: another thread's `fork` (for its own child) can inherit
    // the still-open write descriptor, and the `exec` of the stub in that window
    // fails with `ETXTBSY` ("Text file busy", os error 26) on Linux — the same
    // race `command_footer` retries in product. The product's own subagent spawn
    // starts the installed `radian` binary, which nobody writes at run time, so
    // no product path needs this; serializing the fixture's write-then-spawn is
    // enough. The three tests stay fast: each stub exits at once.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = SERIAL.lock().await;

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
        Profile::workspace_write(&workspace)
            .unwrap()
            .with_approval(ApprovalPolicy::Auto),
        None,
        Arc::new(NoopAudit),
    ));
    let tools = SubagentTools::new(
        SubagentParts {
            executable: Some(child),
            mcp_config: Some("parent-mcp.json".into()),
            trust,
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
/// exactly the three read-only tools, and MCP tools are never filtered by `--tools`. It
/// also ignores the config file's `[mcp]` (`--no-config-mcp`), so it starts no MCP
/// server from either source.
#[tokio::test]
async fn the_explore_child_gets_no_mcp_config() {
    let argv = run_child(json!({"agent": "explore", "task": "MCP-TASK"}), None, false).await;
    assert!(
        !argv.iter().any(|arg| arg == "--mcp-config"),
        "an explore child must not receive --mcp-config: {argv:?}"
    );
    assert!(
        argv.iter().any(|arg| arg == "--no-config-mcp"),
        "an explore child must ignore the config [mcp]: {argv:?}"
    );
    // The read-only allow-list is still there.
    assert_eq!(
        flag_value(&argv, "--tools").as_deref(),
        Some("read,glob,grep"),
        "{argv:?}"
    );
}

/// B2: a `general` child of a trusted parent inherits `--trust`, so it loads the
/// workspace's `.mcp.json` like the parent; an `explore` child of a trusted parent still
/// passes `--no-config-mcp` (extended to the project file), so it loads none. The
/// mutation (trust is not inherited, or explore drops the flag) fails this test.
#[tokio::test]
async fn trust_reaches_the_general_child_but_explore_still_loads_none() {
    let general = run_child(json!({"agent": "general", "task": "MCP-TASK"}), None, true).await;
    assert!(
        general.iter().any(|arg| arg == "--trust"),
        "a general child of a trusted parent must inherit --trust: {general:?}"
    );
    assert!(
        !general.iter().any(|arg| arg == "--no-config-mcp"),
        "a general child still reads every implicit source: {general:?}"
    );
    let explore = run_child(json!({"agent": "explore", "task": "MCP-TASK"}), None, true).await;
    assert!(
        explore.iter().any(|arg| arg == "--no-config-mcp"),
        "an explore child must skip the project file even when trusted: {explore:?}"
    );
}

/// A `general` child keeps the parent's `--mcp-config`: it runs the parent session's tools.
/// It reads the same config file and merges it the same way (no `--no-config-mcp`), so
/// it ends up with the parent's own server set.
#[tokio::test]
async fn the_general_child_keeps_mcp_config() {
    let argv = run_child(json!({"agent": "general", "task": "MCP-TASK"}), None, false).await;
    assert_eq!(
        flag_value(&argv, "--mcp-config").as_deref(),
        Some("parent-mcp.json"),
        "a general child must keep the parent's --mcp-config: {argv:?}"
    );
    assert!(
        !argv.iter().any(|arg| arg == "--no-config-mcp"),
        "a general child must still read the config [mcp]: {argv:?}"
    );
}
