//! Every environment variable the code reads appears in `docs/user/environment.md`.
//!
//! Why a source scan is the source that cannot drift: the test collects the names from
//! the code itself — `std::env::var(_os)("LITERAL")` reads, `pub const … = "NAME"`
//! definitions, and the literal names passed to an env-lookup closure — so a new variable
//! fails here with no list to update. A scan (rather than one list the code reads from)
//! was chosen because the reads live in four crates with different shapes (literals,
//! constants, closures); one list would be a second source to drift. Dynamic,
//! user-configured names (MCP `inherit_env` entries, hook environments, `auth.json` entry
//! `env` keys) are not literals, so they stay out: the manual documents them through the
//! rules that read them, not by name. Test-only names are excluded by name with the reason
//! beside each. Mutation: delete one variable row from `environment.md` → this fails
//! naming it.

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

/// Whether this line is an environment read: a literal `env::var(_os)` call, a `pub const`
/// definition holding a variable name, or a literal passed to an env-lookup closure.
fn is_env_read(code: &str) -> bool {
    code.contains("env::var")
        || code.contains("var_os(")
        || code.contains("pub const")
        || code.contains("get(\"")
}

/// Every `SHOUTING_NAME` literal on an environment-read line. Quoted literals only, so a
/// constant's own name is not mistaken for the variable it holds.
fn read_names(line: &str) -> Vec<String> {
    let code = line.split("//").next().unwrap_or("");
    if !is_env_read(code) {
        return Vec::new();
    }
    let mut names = Vec::new();
    let mut rest = code;
    while let Some(start) = rest.find('"') {
        let tail = &rest[start + 1..];
        let end = tail.find('"').unwrap_or(tail.len());
        let name = &tail[..end];
        if name.len() > 3
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        {
            names.push(name.to_owned());
        }
        rest = &tail[end.min(tail.len())..];
        if rest.starts_with('"') {
            rest = &rest[1..];
        }
    }
    names
}

/// Inputs that exist only for tests or development: never set them by hand, so the manual
/// does not document them.
const TEST_ONLY: &[(&str, &str)] = &[
    (
        "RADIAN_SANDBOX_PROBE_FAIL",
        "read in radian-exec/src/profile.rs and radian-tui/src/profile.rs: exactly \"1\" injects a sandbox probe failure to exercise the refusal path",
    ),
    (
        "RADIAN_TURN_COMMIT_DELAY_MS",
        "read in radian-host/src/session/open.rs: milliseconds between a turn's finish and its journal commit, to exercise fork timing",
    ),
    (
        "RADIAN_TRACE_PHASES",
        "read in radian-session/src/trace.rs: prints a startup phase trace with timings, for development",
    ),
];

#[test]
fn every_env_var_appears_in_environment_md() {
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
    assert!(!names.is_empty(), "no env reads found");
    let mut missing: Vec<String> = Vec::new();
    // The "Files that change behavior" table names variables in passing; a variable's
    // own entry lives in the tables above it.
    let mut sections = doc.split("## Files that change behavior");
    let vars_doc = sections.next().unwrap_or(&doc);
    assert!(
        sections.next().is_some(),
        "environment.md lost its Files section; the drift test scopes rows above it"
    );
    for name in &names {
        if TEST_ONLY.iter().any(|(only, _)| only == name) {
            continue;
        }
        // The variable needs its own table row (first cell), not just a mention in
        // another row's prose: a dropped row must fail here.
        let has_row = vars_doc.lines().any(|line| {
            line.starts_with('|')
                && line
                    .split('|')
                    .nth(1)
                    .is_some_and(|cell| cell.contains(name.as_str()))
        });
        if !has_row {
            missing.push(name.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "these variables are missing from docs/user/environment.md: {missing:?}"
    );
    // The test-only exclusions must still be read by the code: a renamed seam fails here
    // instead of silently leaving the list.
    for (only, _reason) in TEST_ONLY {
        assert!(
            names.iter().any(|name| name == only),
            "{only} is no longer read; drop it from TEST_ONLY"
        );
    }
}
