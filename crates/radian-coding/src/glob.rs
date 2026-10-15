use std::path::Path;

use agent_core::{ContentPart, ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::args;
use crate::fail;
use crate::provider::Limits;
use crate::search;
use crate::workspace::WorkspaceRoot;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
    max_results: Option<u64>,
}

pub(crate) fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Glob matched against each workspace-relative path, e.g. `src/**/*.rs`."
            },
            "path": {
                "type": "string",
                "description": "Workspace-relative directory (or file) to search under; defaults to the workspace root."
            },
            "max_results": {
                "type": "integer",
                "minimum": 1,
                "maximum": 5000,
                "description": "Maximum paths to return; defaults to 500."
            }
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "glob".into(),
        description: "Find files under the workspace whose workspace-relative path matches a \
                      glob, such as `src/**/*.rs`, `**/Cargo.toml` or `tests/*.rs`. `*` does \
                      not cross a `/`; `**` does. Results are sorted. `.gitignore` is \
                      respected, `.git` is skipped, and a symlink is never followed (a \
                      symlinked file is not a match). Truncated past max_results with a line \
                      naming what was cut."
            .into(),
        input_schema: schema(),
        concurrency: ToolConcurrency::Shared,
    }
}

pub(crate) async fn run(
    root: &WorkspaceRoot,
    limits: &Limits,
    arguments: &serde_json::Value,
) -> Result<ToolOutput, ToolFailure> {
    let args: GlobArgs = args::parse("glob", &schema(), arguments)?;
    let matcher = search::compile_glob(&args.pattern)?;
    let cap = match args.max_results {
        Some(0) => {
            return Err(fail(
                "glob_limit_exceeded",
                "max_results must be at least 1",
            ));
        }
        Some(n) if n as usize > limits.max_glob_results => {
            return Err(fail(
                "glob_limit_exceeded",
                format!(
                    "max_results {n} exceeds the {} limit",
                    limits.max_glob_results
                ),
            ));
        }
        Some(n) => n as usize,
        None => limits.default_glob_results,
    };

    // `resolve_existing` canonicalizes, so a symlinked `path` that escapes is refused and the
    // base is always inside the workspace.
    let requested = args.path.as_deref().unwrap_or(".");
    let base = root.resolve_existing(requested)?;

    let mut matches: Vec<String> = Vec::new();
    if base.is_file() {
        // A file base: the pattern is matched against its own workspace-relative path.
        if let Some(rel) = search::relative(root.root(), &base) {
            let rel = rel.to_string_lossy().into_owned();
            if matcher.is_match(Path::new(&rel)) {
                matches.push(rel);
            }
        }
    } else {
        for entry in search::walk(&base).flatten() {
            // Directories and symlinks are skipped: only regular files can match.
            if !search::is_regular_file(&entry) {
                continue;
            }
            let Some(rel) = search::relative(root.root(), entry.path()) else {
                continue;
            };
            let rel = rel.to_string_lossy().into_owned();
            if matcher.is_match(Path::new(&rel)) {
                matches.push(rel);
            }
        }
    }

    matches.sort();
    let total = matches.len();
    let truncated = total > cap;
    matches.truncate(cap);

    let mut text = String::new();
    for path in &matches {
        text.push_str(path);
        text.push('\n');
    }
    if truncated {
        text.push_str(&format!(
            "[truncated: showing {} of {total} matches]\n",
            matches.len()
        ));
    }
    if matches.is_empty() {
        text.push_str(&format!("[no files matched `{}`]\n", args.pattern));
    }

    Ok(ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(json!({
            "pattern": args.pattern,
            "path": requested,
            "matches": matches.len(),
            "total_matches": total,
            "truncated": truncated,
        })),
    })
}
