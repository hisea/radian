//! End to end: a `cwd` in a config file reaches the server process, so the server starts in
//! that directory, not in radian's own working directory.
//!
//! The `mcp-fixture` binary belongs to `agent-extensions` (it is that crate's test server),
//! so this test looks for it beside the workspace target directory. A workspace-wide run
//! builds it; a lone `cargo test -p radian-host` needs
//! `cargo build -p agent-extensions --bin mcp-fixture` first. The message says so.

use std::path::{Path, PathBuf};

use agent_extensions::McpToolProvider;
use radian_core::{
    CancellationToken, ContentPart, NoopSink, ToolCall, ToolContext, ToolProvider, ToolSession,
};
use serde_json::json;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "radian-host-mcp-cwd-{}-{}-{}",
        std::process::id(),
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The fixture beside this test binary: `<target>/<profile>/deps/<test exe>` and
/// `<target>/<profile>/mcp-fixture`.
fn fixture() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    let path = exe
        .parent()
        .and_then(Path::parent)
        .expect("the test binary lives in <target>/<profile>/deps")
        .join("mcp-fixture");
    assert!(
        path.is_file(),
        "mcp-fixture is not built; run: cargo build -p agent-extensions --bin mcp-fixture"
    );
    path
}

#[tokio::test]
async fn the_server_process_starts_in_the_configured_directory() {
    let dir = temp_dir("e2e");
    // A path that is not radian's own working directory and not the config file's
    // directory, so the answer cannot come from either by accident.
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let file = dir.join("mcp.json");
    std::fs::write(
        &file,
        format!(
            r#"{{"mcpServers":{{"fx":{{"command":{:?},"cwd":"work"}}}}}}"#,
            fixture().to_str().unwrap()
        ),
    )
    .unwrap();

    let load = radian_host::load_mcp_config(file.to_str().unwrap()).unwrap();
    assert!(load.warnings.is_empty(), "{:?}", load.warnings);
    assert_eq!(load.servers.len(), 1);
    assert_eq!(load.servers[0].cwd.as_deref(), Some(work.as_path()));

    let mut session = McpToolProvider::new(load.servers)
        .open_session()
        .await
        .expect("the fixture starts");
    let call = ToolCall {
        id: "t".into(),
        name: "fx__cwd".into(),
        arguments: json!({}),
    };
    let cx = ToolContext {
        cancellation: CancellationToken::new(),
        events: &NoopSink,
        index: 0,
    };
    let output = session.execute(&call, cx).await.expect("cwd answers");
    let text: String = output
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    // The server reports its working directory as the OS resolves it; on macOS the temp
    // directory under `/var` is a symlink to `/private/var`, so both sides are resolved.
    assert_eq!(
        std::fs::canonicalize(Path::new(&text)).expect("the reported cwd exists"),
        std::fs::canonicalize(&work).unwrap(),
        "the server must run in the configured cwd"
    );
    session.close().await.expect("the fixture closes");
    let _ = std::fs::remove_dir_all(&dir);
}
