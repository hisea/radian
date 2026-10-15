//! Shared helpers for the integration tests that drive the `radian` binary (ADR 0032).
//!
//! Every test file includes this module and uses a subset of it, so unused helpers are
//! expected (a test binary compiles the whole module).
#![allow(dead_code)]
//!
//! The tests used to drive the `agent` binary, which is gone: `radian exec` is the headless
//! command now. `radian` is built by the workspace build, and `cargo test -p agent-cli` alone
//! does **not** rebuild `radian-tui` - so [`radian_bin`] refuses a stale binary the same way
//! the mcp-fixture helper refuses a missing one. That trap fooled a mutation run once: the
//! test passed against the previous build.

use std::path::{Path, PathBuf};

/// `target/debug` (or `target/release`), where the workspace build puts the binaries.
///
/// The test binary itself lives in `<target>/<profile>/deps/`, so two levels up is the
/// profile directory. This works for `cargo test` and `cargo test --release` alike, and it
/// does not depend on `CARGO_BIN_EXE_*` (which only exists for the crate's own bins).
pub fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary's path");
    exe.parent()
        .and_then(Path::parent)
        .expect("the test binary lives in <target>/<profile>/deps")
        .to_path_buf()
}

/// The repository root (`crates/agent-cli` two levels down).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate lives in <root>/crates/agent-cli")
        .to_path_buf()
}

/// The newest modification time under `dir`, recursively.
fn newest_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest: Option<std::time::SystemTime> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(path);
            } else if let Ok(modified) = meta.modified() {
                newest = Some(match newest {
                    Some(previous) => previous.max(modified),
                    None => modified,
                });
            }
        }
    }
    newest
}

/// The newest source mtime among the crates the `radian` binary is built from: every
/// `crates/*/src` and `third_party/*/src` tree, plus each crate's `build.rs`.
///
/// Not just `radian-tui`: `radian` links the `agent-cli` library and every other path
/// dependency, and `cargo test -p agent-cli` rebuilds only the library - so a change in any of
/// them leaves the binary stale.
fn newest_source_mtime() -> Option<std::time::SystemTime> {
    let root = repo_root();
    let mut newest: Option<std::time::SystemTime> = None;
    for base in ["crates", "third_party"] {
        let Ok(entries) = std::fs::read_dir(root.join(base)) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            // Manifests are deliberately not checked: a comment-only `Cargo.toml` edit does
            // not make the artifact stale and cargo does not rebuild for it, so including
            // them would refuse a perfectly current binary. A manifest change that alters the
            // build (a feature or a dependency) is the gap - rebuild before trusting the
            // tests when you make one.
            for candidate in [dir.join("src"), dir.join("build.rs")] {
                let found = if candidate.is_dir() {
                    newest_mtime(&candidate)
                } else {
                    std::fs::metadata(&candidate)
                        .and_then(|meta| meta.modified())
                        .ok()
                };
                if let Some(found) = found {
                    newest = Some(match newest {
                        Some(previous) => previous.max(found),
                        None => found,
                    });
                }
            }
        }
    }
    newest
}

/// [`radian_bin`] as a string, for the pty helper (which builds an argv of `&str`s).
pub fn radian_bin_str() -> String {
    radian_bin().to_string_lossy().into_owned()
}

/// The `radian` binary, refusing a missing **or stale** one.
///
/// A stale binary is worse than a missing one: `cargo test -p agent-cli` does not rebuild
/// `radian-tui`, so the tests would silently exercise the previous build. When any source the
/// binary is built from is newer than the binary, this says what to run - the same shape as
/// the mcp-fixture helper's "not built" message.
pub fn radian_bin() -> PathBuf {
    let path = target_dir().join("radian");
    assert!(
        path.exists(),
        "radian is not built; run: cargo build -p radian-tui --bin radian"
    );
    let binary_mtime = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .expect("the radian binary's mtime");
    if let Some(newest) = newest_source_mtime() {
        assert!(
            newest <= binary_mtime,
            "radian is older than its sources: the tests would exercise a stale binary \
             (cargo test -p agent-cli does not rebuild radian-tui). Run: cargo build -p \
             radian-tui --bin radian"
        );
    }
    path
}
