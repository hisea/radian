//! The `sandbox` nextest profile (`.config/nextest.toml`): every test it excludes must still
//! name a test, so a rename or a deletion cannot leave a stale exclusion behind.
//!
//! The exclusions are exact names (`test(=name)`), never a substring or a module wildcard.
//! The source scan is the same "no second list to drift" approach as `env_docs.rs`: an
//! integration test is a function in a crate's `tests/` directory, and a unit test is a
//! `#[test]`/`#[tokio::test]` function under `src/`, where a module prefix (`foo::tests::bar`)
//! must match the file's module path plus an inline `mod tests`.
//!
//! Mutation: add a made-up name to the profile → this test fails naming it. Rename or delete
//! an excluded test → it fails the same way.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const PROFILE: &str = "sandbox";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn config() -> toml::Value {
    let path = workspace_root().join(".config/nextest.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    toml::from_str(&text).unwrap_or_else(|error| panic!("{} is not TOML: {error}", path.display()))
}

/// Every `test(=name)` leaf in `profile.sandbox.default-filter`.
fn excluded_names() -> Vec<String> {
    let doc = config();
    let filter = doc
        .get("profile")
        .and_then(|profile| profile.get(PROFILE))
        .and_then(|profile| profile.get("default-filter"))
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("profile.{PROFILE}.default-filter is missing"));
    assert!(
        filter.trim_start().starts_with("not ("),
        "profile.{PROFILE}.default-filter must be one `not (...)` expression:\n{filter}"
    );
    let mut names = Vec::new();
    let mut rest = filter;
    while let Some(start) = rest.find("test(") {
        let after = &rest[start + "test(".len()..];
        let end = after
            .find(')')
            .unwrap_or_else(|| panic!("unclosed `test(` in profile.{PROFILE}.default-filter"));
        let leaf = &after[..end];
        let name = leaf.strip_prefix('=').unwrap_or_else(|| {
            panic!(
                "`test({leaf})` is not exact: write `test(={leaf})` so a substring or a \
                 wildcard cannot exclude a whole module"
            )
        });
        assert!(
            !name.is_empty()
                && name.split("::").all(|segment| {
                    !segment.is_empty()
                        && segment
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_')
                }),
            "`{name}` is not a test name"
        );
        names.push(name.to_owned());
        rest = &after[end..];
    }
    assert!(!names.is_empty(), "profile.{PROFILE} excludes no test");
    let unique: BTreeSet<&String> = names.iter().collect();
    assert_eq!(
        unique.len(),
        names.len(),
        "a name is listed twice: {names:?}"
    );
    names
}

/// A function that looks like a test: in a crate's `tests/` directory, or a `src/` function
/// with a test attribute above it.
struct TestFunction {
    name: String,
    /// A `src/` file's module path (`memory::identity`); `None` inside `tests/`.
    module: Option<String>,
    /// Whether that `src/` file declares an inline `mod tests`.
    declares_tests: bool,
}

impl TestFunction {
    /// Whether this function is the test named by `full` (`name` or `module::tests::name`).
    fn matches(&self, full: &str) -> bool {
        let mut segments: Vec<&str> = full.split("::").collect();
        let Some(name) = segments.pop() else {
            return false;
        };
        if name != self.name {
            return false;
        }
        if segments.is_empty() {
            // An integration test, or a unit test at a crate root.
            return true;
        }
        if segments.last() != Some(&"tests") {
            return false;
        }
        let parent = segments[..segments.len() - 1].join("::");
        self.module.as_deref() == Some(parent.as_str()) && self.declares_tests
    }
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("a source directory reads");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) != Some("target") {
                rust_files(&path, out);
            }
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// A `src/` file's module path: `src/lib.rs`/`src/main.rs` are the crate root (`""`),
/// `src/memory/identity.rs` is `memory::identity`, and `src/skills/mod.rs` is `skills`.
/// `None` for a file outside `src/` (a `tests/` integration test).
fn module_path(file: &Path) -> Option<String> {
    let parts: Vec<String> = file
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    let src = parts.iter().position(|part| part == "src")?;
    let mut rel = parts[src + 1..].to_vec();
    let last = rel.pop()?;
    match last.as_str() {
        "lib.rs" | "main.rs" => {}
        "mod.rs" => {}
        _ => rel.push(last.trim_end_matches(".rs").to_owned()),
    }
    Some(rel.join("::"))
}

/// `fn <name>` on this line, if any.
fn function_name(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let starts = [
        "fn ",
        "pub fn ",
        "async fn ",
        "pub async fn ",
        "pub(crate) fn ",
    ];
    if !starts.iter().any(|start| trimmed.starts_with(start)) {
        return None;
    }
    let start = trimmed.find("fn ")?;
    let rest = &trimmed[start + 3..];
    let name: &str = &rest[..rest
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map(|(index, _)| index)
        .unwrap_or(rest.len())];
    if !name.is_empty() && rest[name.len()..].trim_start().starts_with('(') {
        Some(name)
    } else {
        None
    }
}

fn test_functions() -> Vec<TestFunction> {
    let mut files = Vec::new();
    rust_files(&workspace_root().join("crates"), &mut files);
    let mut tests = Vec::new();
    for file in files {
        let module = module_path(&file);
        let integration = module.is_none();
        let text = std::fs::read_to_string(&file).expect("a Rust source file reads");
        let declares_tests = text.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("mod tests") || line.starts_with("pub mod tests")
        });
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let Some(name) = function_name(line) else {
                continue;
            };
            let attributes = lines[index.saturating_sub(10)..index].join("\n");
            let is_test = integration
                || attributes.contains("#[test")
                || attributes.contains("#[tokio::test")
                || attributes.contains("#[async_std::test");
            if is_test {
                tests.push(TestFunction {
                    name: name.to_owned(),
                    module: module.clone(),
                    declares_tests,
                });
            }
        }
    }
    tests
}

/// Every `sandbox` exclusion still names a test.
#[test]
fn every_sandbox_exclusion_names_a_test() {
    let names = excluded_names();
    let tests = test_functions();
    let missing: Vec<&String> = names
        .iter()
        .filter(|name| !tests.iter().any(|test| test.matches(name)))
        .collect();
    assert!(
        missing.is_empty(),
        "profile.{PROFILE} excludes tests that do not exist (renamed or deleted?):\n{}",
        missing
            .iter()
            .map(|name| format!("  {name}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The default profile still runs every test outside the sandbox (ADR 0048 §7: this is not a
/// loosening of the gate).
#[test]
fn the_default_profile_excludes_nothing() {
    let doc = config();
    let default = doc
        .get("profile")
        .and_then(|profile| profile.get("default"))
        .expect("profile.default exists");
    assert!(
        default.get("default-filter").is_none(),
        "profile.default must not set a default-filter; only profile.{PROFILE} excludes tests"
    );
}
