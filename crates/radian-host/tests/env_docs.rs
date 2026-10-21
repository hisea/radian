//! Every `RADIAN_*` variable the code reads appears in `docs/user/environment.md`.
//!
//! Why a source scan is the source that cannot drift: the test collects the names from
//! the code itself — `std::env::var(_os)("LITERAL")` reads and `pub const … = "RADIAN_…"`
//! definitions across `crates/*/src` — so a new variable fails here with no list to
//! update. Test-only names never match those two shapes (they are set, compared, or
//! carried dynamically), so they stay out. Mutation: delete one variable row from
//! `environment.md` → this fails naming it.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("src dir reads");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// A `RADIAN_*` name this line actually reads (`env::var(_os)`) or defines (`pub const`).
/// Only quoted literals count, so a constant's own name (`RADIAN_CONFIG_ENV`) is not
/// mistaken for the variable it holds (`RADIAN_CONFIG`).
fn read_names(line: &str) -> Vec<String> {
    let code = line.split("//").next().unwrap_or("");
    if !(code.contains("env::var") || code.contains("pub const")) {
        return Vec::new();
    }
    let mut names = Vec::new();
    let mut rest = code;
    while let Some(start) = rest.find("\"RADIAN_") {
        let tail = &rest[start + 1..];
        let end = tail
            .find(|c: char| !c.is_ascii_uppercase() && !c.is_ascii_digit() && c != '_')
            .unwrap_or(tail.len());
        let name = &tail[..end];
        if name.len() > "RADIAN_".len() {
            names.push(name.to_owned());
        }
        rest = &tail[end.min(tail.len())..];
    }
    names
}

#[test]
fn every_radian_env_var_appears_in_environment_md() {
    let root = workspace_root();
    let doc = std::fs::read_to_string(root.join("docs/user/environment.md"))
        .expect("environment.md exists");
    let mut files = Vec::new();
    let crates = std::fs::read_dir(root.join("crates")).expect("crates dir reads");
    for entry in crates.flatten() {
        let src = entry.path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    assert!(!files.is_empty(), "no source files found");
    let mut names: Vec<String> = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("source reads");
        for line in text.lines() {
            for name in read_names(line) {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    assert!(!names.is_empty(), "no RADIAN_* reads found");
    let missing: Vec<_> = names
        .iter()
        .filter(|name| !doc.contains(name.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "these variables are missing from docs/user/environment.md: {missing:?}"
    );
}
