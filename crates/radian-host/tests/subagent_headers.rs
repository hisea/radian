//! ADR 0043 decision 1a (gap 1): the parent's `--header` values reach the child, and never
//! through its argv.
//!
//! The child here is a stub shell script rather than a `radian` binary: the two facts this test
//! pins — the argv the tool built carries no header value, and the environment it set does — are
//! exactly the child's own two inputs, and a stub makes both observable without a provider. The
//! end-to-end half (the child's request carries the header; its journal and the parent's output
//! do not) is `agent-cli`'s `the_parents_header_reaches_the_child_and_nothing_else_writes_it`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_core::{CancellationToken, NoopSink, ToolCall, ToolContext, ToolProvider, ToolSession};
use radian_host::{SubagentParts, SubagentTools};
use radian_permissions::{ApprovalPolicy, NoopAudit, Profile};
use radian_sandbox::SandboxSlot;
use serde_json::{Value, json};

const SECRET: &str = "argv-header-secret-7";

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "radian-subagent-headers-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An executable stub that records its argv (one argument per line) and the child header
/// variable, then exits.
fn stub(dir: &Path, argv_path: &Path, env_path: &Path) -> PathBuf {
    let path = dir.join("child.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' \"$RADIAN_CHILD_HEADERS\" > '{}'\n",
            argv_path.display(),
            env_path.display(),
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

/// The parent's `--header` value reaches the child through the environment and the child's own
/// startup re-parses it (`radian_host::take_child_headers_from_env`). The mutation (not forwarding
/// it) leaves the variable unset and fails the environment assertion; a forwarding that put the
/// value in argv would fail the argv assertion.
#[tokio::test]
async fn the_header_value_reaches_the_child_through_the_environment_not_its_argv() {
    let dir = scratch("forward");
    let workspace = dir.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let argv_path = dir.join("argv.txt");
    let env_path = dir.join("env.txt");
    let child = stub(&dir, &argv_path, &env_path);

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
            headers: vec![format!("X-Test={SECRET}")],
            ..SubagentParts::default()
        },
        dir.clone(),
        "parent",
        workspace,
        None,
        SandboxSlot::empty(),
        radian_runtime::ConfigSlot::new(Value::Null),
        permissions,
    );
    let session = tools.open_session().await.expect("open the tool session");
    let call = ToolCall {
        id: "c1".into(),
        name: "subagent".into(),
        arguments: json!({"task": "HEADER-TASK"}),
    };
    let cx = ToolContext {
        cancellation: CancellationToken::new(),
        events: &SINK,
        index: 0,
    };
    session.execute(&call, cx).await.expect("the stub runs");

    // The environment the child inherits carries the header, in the encoded form its startup
    // reads; the argv — visible to every user through `ps` — does not.
    let env = std::fs::read_to_string(&env_path).unwrap_or_default();
    let decoded: Vec<String> = serde_json::from_str(&env).expect("a JSON array of NAME=VALUE");
    assert_eq!(decoded, vec![format!("X-Test={SECRET}")]);
    let argv = std::fs::read_to_string(&argv_path).unwrap_or_default();
    assert!(
        !argv.contains(SECRET),
        "the header value must not be in the child's argv: {argv}"
    );
}
