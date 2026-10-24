//! Every environment variable the code reads appears in `docs/user/environment.md`.
//!
//! Why a source scan is the source that cannot drift: the test collects the names from
//! the code itself — `std::env::var(_os)("LITERAL")` reads and the literal names passed to an
//! env-lookup closure (`get("LITERAL")`) — so a new variable fails here with no list to
//! update. A scan (rather than one list the code reads from) was chosen because the reads live
//! in four crates with different shapes (literals, constants, closures); one list would be a
//! second source to drift. Dynamic, user-configured names (MCP `inherit_env` entries, hook
//! environments, `auth.json` entry `env` keys) are not literals, so they stay out: the manual
//! documents them through the rules that read them, not by name. Test-only names are excluded
//! by name with the reason beside each. Rows are read only from the variable tables under the
//! section headings the test names; the `## Files that change behavior` table names variables
//! in passing and never satisfies a check. Mutation: delete one variable row from
//! `environment.md` → this fails naming it.
//!
//! A `pub const NAME … = "LITERAL"` (or `pub(crate) const`) only names a variable when the
//! code uses it as one: `NAME` is the first argument of an environment call — `env::var(`,
//! `env::var_os(`, `var_os(`, `env::set_var(`, `env::remove_var(`, `.env(`, `.env_remove(`,
//! or the env-lookup `get(` — with an optional path prefix (`crate::home::NAME`,
//! `home::NAME`). A constant whose identifier is never used that way names nothing, whatever
//! its value: `pub const COLUMN_HEADINGS: [&str; N] = ["TODO", …]` is a board heading, not
//! three variables. A constant that holds a *list* of names (`pub const INHERITED_ENV: &[&str]
//! = &["A", "B"]`) is never passed to a call itself; each entry counts only when an
//! environment call names that same literal, so a list of names the code sets is not mistaken
//! for a value list.

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

/// The calls whose first argument is an environment variable name: the read forms, the write
/// forms, and the env-lookup closure `get(`.
const ENV_CALLS: &[&str] = &[
    "env::var(",
    "env::var_os(",
    "var_os(",
    "env::set_var(",
    "env::remove_var(",
    ".env(",
    ".env_remove(",
    "get(",
];

/// The line without its `//` comment: a name stays out of prose.
fn code_of(line: &str) -> &str {
    line.split("//").next().unwrap_or("")
}

/// Whether this line holds a literal environment read: `env::var(_os)(…)` or an env-lookup
/// closure call `get("…")`.
fn is_env_read(code: &str) -> bool {
    code.contains("env::var") || code.contains("var_os(") || code.contains("get(\"")
}

/// A `SHOUTING_NAME`: an environment variable's shape, so a constant's own identifier is not
/// mistaken for the variable it holds.
fn is_env_name(name: &str) -> bool {
    name.len() > 3
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
}

/// Every quoted literal on the line, in order.
fn string_literals(code: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut rest = code;
    while let Some(start) = rest.find('"') {
        let tail = &rest[start + 1..];
        let end = tail.find('"').unwrap_or(tail.len());
        literals.push(tail[..end].to_owned());
        rest = &tail[end.min(tail.len())..];
        if rest.starts_with('"') {
            rest = &rest[1..];
        }
    }
    literals
}

fn is_identifier(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The first argument of every environment call on this line: an identifier (a path prefix
/// such as `crate::home::RADIAN_HOME_ENV` reduced to its last segment) or a quoted literal.
fn env_call_arguments(code: &str, identifiers: &mut Vec<String>, literals: &mut Vec<String>) {
    for call in ENV_CALLS {
        let mut rest = code;
        while let Some(at) = rest.find(call) {
            rest = &rest[at + call.len()..];
            let end = rest.find([',', ')']).unwrap_or(rest.len());
            let argument = rest[..end].trim();
            if let Some(inner) = argument.strip_prefix('"').and_then(|a| a.strip_suffix('"')) {
                literals.push(inner.to_owned());
            } else {
                let last = argument.rsplit("::").next().unwrap_or(argument);
                if is_identifier(last) {
                    identifiers.push(last.to_owned());
                }
            }
        }
    }
}

/// A `pub const` (or `pub(crate) const`) definition on this line: its identifier and the
/// literals of its value.
struct ConstDefinition {
    name: String,
    literals: Vec<String>,
}

fn const_definition(code: &str) -> Option<ConstDefinition> {
    let start = ["pub const ", "pub(crate) const "]
        .iter()
        .filter_map(|marker| code.find(marker).map(|at| at + marker.len()))
        .min()?;
    let rest = code[start..].trim_start();
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let after = rest[name.len()..].trim_start();
    // `pub const fn …` is a function, not a constant.
    if name.is_empty() || !after.starts_with(':') {
        return None;
    }
    let equals = start + code[start..].find('=')?;
    Some(ConstDefinition {
        name,
        literals: string_literals(&code[equals..]),
    })
}

fn push_name(names: &mut Vec<String>, name: &str) {
    if is_env_name(name) && !names.iter().any(|known| known == name) {
        names.push(name.to_owned());
    }
}

/// Every environment variable name in `sources`. Whole files are passed at once because a
/// constant is defined in one module and used in another.
fn env_names(sources: &[&str]) -> Vec<String> {
    // Pass one: the arguments the environment calls name — an identifier means a constant
    // holds a variable's name; a literal means a listed entry is used as one.
    let mut identifiers: Vec<String> = Vec::new();
    let mut literals: Vec<String> = Vec::new();
    for source in sources {
        for line in source.lines() {
            env_call_arguments(code_of(line), &mut identifiers, &mut literals);
        }
    }
    let mut names: Vec<String> = Vec::new();
    for source in sources {
        for line in source.lines() {
            let code = code_of(line);
            if is_env_read(code) {
                for literal in string_literals(code) {
                    push_name(&mut names, &literal);
                }
            }
            let Some(definition) = const_definition(code) else {
                continue;
            };
            let named_by_identifier = identifiers.contains(&definition.name);
            for literal in &definition.literals {
                if named_by_identifier || literals.contains(literal) {
                    push_name(&mut names, literal);
                }
            }
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
    (
        "RADIAN_ARC_GATE_TEST_EXIT_WITHOUT_RECORD",
        "read in radian-arc/src/gate/background.rs: makes the background gate exit before it writes any record, to exercise the no-record guard",
    ),
    (
        "RADIAN_ARC_GATE_TEST_DIE_AFTER_RUNNING",
        "read in radian-arc/src/gate/background.rs: makes the background gate exit right after it wrote `running`, to exercise the death count and `died`",
    ),
    (
        "RADIAN_ARC_GATE_TEST_SLOW_START_MS",
        "read in radian-arc/src/gate/background.rs: milliseconds the background gate waits before its `running` record, so a test can stop it while only the pass's `started` record exists",
    ),
    (
        "RADIAN_ARC_GATE_TEST_REVIEW_PROGRAM",
        "read in radian-arc/src/gate/background.rs: runs this program instead of `radian exec` for the review stage, so a test drives the background gate's `commands`/`review` pair without a model",
    ),
];

/// The headings whose tables list environment variables. Each heading is an anchor: a row
/// in another table on the page (the paths table, prose that names a variable in passing)
/// never satisfies a check.
const VARIABLE_HEADINGS: &[&str] = &[
    "## Home and paths",
    "## Credentials",
    "## Catalog and model",
    "## Terminal",
    "## Sandboxed children",
    "## Herdr",
];

#[test]
fn a_heading_constant_is_not_environment_variables() {
    let source = "pub const COLUMN_HEADINGS: [&str; COLUMNS] = \
                  [\"TODO\", \"DOING\", \"VERIFYING\", \"NEEDS YOU\"];";
    assert_eq!(env_names(&[source]), Vec::<String>::new());
}

#[test]
fn a_constant_used_as_an_environment_name_yields_its_literal() {
    let source = "pub const RADIAN_HOME_ENV: &str = \"RADIAN_HOME\";\n\
                  fn radian_home() {\n\
                      std::env::var_os(RADIAN_HOME_ENV)\n\
                  }";
    assert_eq!(env_names(&[source]), vec!["RADIAN_HOME".to_owned()]);
}

#[test]
fn a_path_prefixed_constant_use_yields_its_literal() {
    let source = "pub const RADIAN_HOME_ENV: &str = \"RADIAN_HOME\";\n\
                  fn radian_home() {\n\
                      std::env::var_os(crate::home::RADIAN_HOME_ENV)\n\
                  }";
    assert_eq!(env_names(&[source]), vec!["RADIAN_HOME".to_owned()]);
}

#[test]
fn a_constant_set_on_a_child_yields_its_literal() {
    let source = "pub const CHILD_HEADERS_ENV: &str = \"RADIAN_CHILD_HEADERS\";\n\
                  fn setup(command: &mut Command) {\n\
                      command.env(CHILD_HEADERS_ENV, value);\n\
                  }";
    assert_eq!(
        env_names(&[source]),
        vec!["RADIAN_CHILD_HEADERS".to_owned()]
    );
}

#[test]
fn a_constant_used_outside_an_environment_call_yields_nothing() {
    let source = "pub const GREETING: &str = \"HELLO_WORLD\";\n\
                  fn greet() {\n\
                      println!(\"{}\", GREETING);\n\
                  }";
    assert_eq!(env_names(&[source]), Vec::<String>::new());
}

#[test]
fn literal_reads_and_closure_reads_still_yield_their_names() {
    let source = "fn read() {\n\
                      let x = std::env::var(\"RADIAN_X\");\n\
                      let y = env.get(\"HERDR_ENV\");\n\
                  }";
    assert_eq!(
        env_names(&[source]),
        vec!["RADIAN_X".to_owned(), "HERDR_ENV".to_owned()]
    );
}

#[test]
fn every_env_var_appears_in_environment_md() {
    let root = workspace_root();
    let doc = std::fs::read_to_string(root.join("docs/user/environment.md"))
        .expect("environment.md exists");
    // Every table under the variable headings, flattened into one row list. The headings
    // are the anchors; the `## Files that change behavior` table names variables only in
    // passing and is never read.
    let mut rows: Vec<Vec<String>> = Vec::new();
    for heading in VARIABLE_HEADINGS {
        for table in radian_coding::drift::tables_under(&doc, "environment.md", heading) {
            rows.extend(table);
        }
    }
    assert!(!rows.is_empty(), "environment.md has no variable tables");
    let mut files = Vec::new();
    let crates = std::fs::read_dir(root.join("crates")).expect("crates dir reads");
    for entry in crates.flatten() {
        let src = entry.path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    assert!(!files.is_empty(), "no source files found");
    let texts: Vec<String> = files
        .iter()
        .map(|file| std::fs::read_to_string(file).expect("source reads"))
        .collect();
    let sources: Vec<&str> = texts.iter().map(String::as_str).collect();
    let names = env_names(&sources);
    assert!(!names.is_empty(), "no env reads found");
    let mut missing: Vec<String> = Vec::new();
    // The Files section stays a separate table: a variable's own entry lives in the
    // variable sections above it, never in this one.
    assert!(
        doc.contains("## Files that change behavior"),
        "environment.md lost its Files section"
    );
    for name in &names {
        if TEST_ONLY.iter().any(|(only, _)| only == name) {
            continue;
        }
        // The variable needs its own table row (first cell), not just a mention in
        // another row's prose: a dropped row must fail here.
        let has_row = rows
            .iter()
            .any(|row| row.first().is_some_and(|cell| cell.contains(name.as_str())));
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
