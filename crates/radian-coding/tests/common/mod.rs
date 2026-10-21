#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use radian_coding::{CodingTools, ReadRoots};
use radian_core::{
    NoopSink, ToolCall, ToolContext, ToolFailure, ToolOutput, ToolProvider, ToolSession,
};
use serde_json::{Value, json};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique directory under `std::env::temp_dir()`, removed on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!("radian-coding-tests-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = root.join(format!("{label}-{n}"));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Writes a file inside the temp dir, creating parent directories.
    pub fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.file(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        // The shared parent is deliberately left in place. Removing it while another test
        // thread sits between `create_dir_all(parent)` and `create_dir_all(child)` makes that
        // child creation fail with a spurious `AlreadyExists` (the parent vanished between
        // `mkdir`'s EEXIST and the `is_dir()` check) or `InvalidInput`. That race is
        // pre-existing and unrelated to any one test; the empty directory lives in the
        // system temp dir, which the OS cleans up.
    }
}

/// A sentinel directory that lives *outside* any workspace root, used as the escape target.
/// Nothing outside this directory (and the temp dir) is ever touched by a test.
pub fn sentinel_outside(dir: &TempDir) -> PathBuf {
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("sentinel.txt"), "SENTINEL-CONTENT").unwrap();
    outside
}

/// The workspace tools with **no** read-only roots: the boundary tests are hermetic and never
/// read the developer's real `~/.cargo`/`~/.rustup`. `tests/read_roots.rs` builds tools with
/// `CodingTools::new` (which resolves the roots from `CARGO_HOME`/`RUSTUP_HOME`) instead.
pub fn tools(root: &Path) -> CodingTools {
    CodingTools::with_read_roots(root, ReadRoots::none()).unwrap()
}

pub async fn call(
    tools: &CodingTools,
    name: &str,
    arguments: Value,
) -> Result<ToolOutput, ToolFailure> {
    let session = tools.open_session().await.unwrap();
    let call = ToolCall {
        id: "t".into(),
        name: name.into(),
        arguments,
    };
    let sink = NoopSink;
    let cx = ToolContext {
        cancellation: radian_core::CancellationToken::new(),
        events: &sink,
        index: 0,
    };
    session.execute(&call, cx).await
}

pub fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|p| match p {
            radian_core::ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Asserts the failure carries the single boundary code.
pub fn assert_outside(failure: &ToolFailure, user_path: &str) {
    assert_eq!(
        failure.code, "path_outside_workspace",
        "expected a boundary refusal for {user_path}, got {failure:?}"
    );
}

pub fn structured(output: &ToolOutput) -> Value {
    output.structured.clone().unwrap_or(json!(null))
}
