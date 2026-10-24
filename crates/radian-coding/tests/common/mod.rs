#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once};

use radian_coding::{CodingTools, ReadRoots};
use radian_core::{
    NoopSink, ToolCall, ToolContext, ToolFailure, ToolOutput, ToolProvider, ToolSession,
};
use serde_json::{Value, json};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique directory under `std::env::temp_dir()`, removed on drop.
///
/// There is no shared per-process root: under nextest every test is its own process, so a
/// root would outlive the test that created it and leak one directory per test. The prefix
/// keeps a leftover traceable to this crate, and the pid plus counter keep parallel tests
/// (under plain `cargo test`) apart.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
        Self::named_with("radian-coding-tests", label)
    }

    /// A named scratch directory whose own prefix identifies the test family (a leftover in the
    /// system temp dir still names the test that made it).
    pub fn named(prefix: &str) -> Self {
        Self::named_with(prefix, "scratch")
    }

    fn named_with(prefix: &str, label: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("{prefix}-{}-{label}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lets a test keep its existing `dir.join(..)` / `&dir` calls when a helper returns a
    /// guard instead of a `PathBuf`.
    pub fn as_path(&self) -> &Path {
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

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// A temporary file (a Unix socket leaves its inode behind) removed on drop.
pub struct TempPath(PathBuf);

impl TempPath {
    pub fn new(path: PathBuf) -> Self {
        let _ = std::fs::remove_file(&path);
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A unique directory under the system temp dir that is removed when the test process exits.
///
/// A `OnceLock` fixture is `'static`, so a `Drop` guard cannot own its directory. Under
/// nextest every test is its own process, so process exit is the end of the test; a plain
/// `cargo test` removes the fixture after the last test of the binary. The prefix keeps a
/// leftover traceable to this crate.
pub fn process_dir(prefix: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    EXIT_DIRS.lock().unwrap().push(path.clone());
    EXIT_HOOK.call_once(|| unsafe {
        // SAFETY: the callback only touches this module's registry.
        atexit(remove_exit_dirs);
    });
    path
}

static EXIT_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static EXIT_HOOK: Once = Once::new();

extern "C" fn remove_exit_dirs() {
    if let Ok(dirs) = EXIT_DIRS.lock() {
        for dir in dirs.iter().rev() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn atexit(callback: extern "C" fn()) -> i32;
}

#[cfg(not(unix))]
unsafe fn atexit(_callback: extern "C" fn()) -> i32 {
    // Tests run on macOS and Linux; elsewhere the directory is left to the OS.
    0
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
