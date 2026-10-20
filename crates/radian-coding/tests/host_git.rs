//! ADR 0039 decision 3: every host `git` invocation is built from the one shared helper
//! (`radian_coding::host_git`), so it carries the no-filter overrides. A new raw `git` command in
//! production source must fail this test — and a `git` call is exactly what was missed before
//! (`prompt.rs`, `contrib.rs` and these tools ran `git status`/`git diff` unsandboxed).

use std::path::{Path, PathBuf};

/// A command that constructs a `git` process (any path prefix: `std::process::`, `tokio::…`).
const NEEDLE: &str = "Command::new(\"git\")";

/// Production files that may contain the needle, with the exact number of times, and why. Test
/// modules (a `#[cfg(test)]` line and everything after it) are ignored, so their setup helpers
/// need no entry here.
const ALLOWED: &[(&str, usize)] = &[
    // The helper itself: the two command builders and the two config reads.
    ("crates/radian-coding/src/host_git.rs", 4),
    // The sandbox escape-matrix harness, not a host runtime path; it only creates test repos.
    ("crates/radian-sandbox/src/harness.rs", 1),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("the workspace root")
        .to_path_buf()
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("a source dir") {
        let path = entry.expect("a dir entry").path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// The occurrences of [`NEEDLE`] in the part of `text` before its first `#[cfg(test)]` line.
fn production_occurrences(text: &str) -> usize {
    text.split("#[cfg(test)]")
        .next()
        .unwrap_or(text)
        .matches(NEEDLE)
        .count()
}

#[test]
fn every_host_git_call_is_built_from_the_shared_helper() {
    let root = workspace_root();
    let mut files = Vec::new();
    let crates = root.join("crates");
    for entry in std::fs::read_dir(&crates).expect("crates/") {
        let src = entry.expect("a crate").path().join("src");
        if src.is_dir() {
            rs_files(&src, &mut files);
        }
    }

    let mut problems = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("a source file");
        let count = production_occurrences(&text);
        if count == 0 {
            continue;
        }
        let relative = file
            .strip_prefix(&root)
            .expect("under the workspace root")
            .to_string_lossy()
            .replace('\\', "/");
        match ALLOWED.iter().find(|(path, _)| *path == relative) {
            Some((_, allowed)) if *allowed == count => {}
            Some((_, allowed)) => problems.push(format!(
                "{relative}: {count} raw git commands before #[cfg(test)], expected {allowed}"
            )),
            None => problems.push(format!(
                "{relative}: {count} raw git command(s) not routed through radian_coding::host_git"
            )),
        }
    }
    assert!(
        problems.is_empty(),
        "every host git call must be built from radian_coding::host_git:\n{}",
        problems.join("\n")
    );

    // An allowlist that names a file which no longer exists (or was renamed) is a stale guard.
    for (path, _) in ALLOWED {
        assert!(
            files.iter().any(|file| file
                .strip_prefix(&root)
                .expect("under the workspace root")
                .to_string_lossy()
                .replace('\\', "/")
                == *path),
            "allowlisted file is gone: {path}"
        );
    }
}
