use radian_core::{ContentPart, ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::args;
use crate::fail;
use crate::provider::Limits;
use crate::search;
use crate::workspace::{WorkspaceRoot, display_path};

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
                "description": "Glob to match against each workspace-relative path. Example: `src/**/*.rs`."
            },
            "path": {
                "type": "string",
                "description": "Directory or file to search under. Default: the workspace \
                                root. A workspace-relative path, or an absolute path inside the \
                                workspace, searches the workspace and gives relative results. \
                                A read-only absolute or `~` directory under a read-only root \
                                (cargo registry or Rust toolchain sources, or a discovered \
                                skill's directory) is also accepted; in the `full` tier any other \
                                absolute or `~` path is searched too, except the sandbox's deny \
                                set, and its results are absolute."
            },
            "max_results": {
                "type": "integer",
                "minimum": 1,
                "maximum": 5000,
                "description": "Maximum paths to return. Default: 500."
            }
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "glob".into(),
        description: "Find files with a glob. `*` stops at `/`. `**` crosses `/`. \
                      The tool sorts the results. `.gitignore` and `.ignore` files apply \
                      from the workspace root down to the search directory, and the global \
                      gitignore is not read. The tool skips `.git`. The tool never follows \
                      a symlink."
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
    let scan = BlockingScan {
        root: root.clone(),
        matcher,
        pattern: args.pattern,
        path: args.path.unwrap_or_else(|| ".".to_owned()),
        cap,
    };
    // The walk below is synchronous and never yields, so it runs on the blocking pool:
    // `glob` calls batched in one Core task overlap instead of queueing behind each
    // other. Cancellation drops this future; the blocking walk then runs on to the end
    // in the background (read-only, so nothing is written) and its result is never
    // written back: Core's own `cancelled` wins the race.
    match tokio::task::spawn_blocking(move || scan_blocking(scan)).await {
        Ok(result) => result,
        Err(error) => match error.try_into_panic() {
            Ok(payload) => std::panic::resume_unwind(payload),
            Err(_) => Err(fail("internal", "the glob scan did not finish")),
        },
    }
}

/// Everything `run` does after argument parsing: the fd walk and the rendering.
/// Synchronous by construction (see `run`); the body is the old `run` tail, limits,
/// output and errors unchanged.
struct BlockingScan {
    root: WorkspaceRoot,
    matcher: globset::GlobMatcher,
    pattern: String,
    path: String,
    cap: usize,
}

fn scan_blocking(scan: BlockingScan) -> Result<ToolOutput, ToolFailure> {
    let BlockingScan {
        root,
        matcher,
        pattern,
        path,
        cap,
    } = scan;
    // ADR 0039 decision 2: the base is opened through the root's directory fd
    // (no symlink in any component) and the walk descends by fd; only regular files can match.
    // An absolute or `~` base must be under a read-only root, and its results are rendered
    // with that root's prefix so they can be passed to `read` again.
    // Several whitespace-separated bases in one `path`: each resolved as a lone call
    // would resolve it, the matches merged with duplicates removed and the cap shared
    // across the whole call. Every walk stays fd-relative (ADR 0039 decision 2).
    let requested: &str = &path;
    let resolved = search::resolve_bases(&root, requested)?;
    let mut matches: Vec<String> = Vec::new();
    for base in resolved.bases {
        match base {
            search::SingleBase::Dir(dir_base) => {
                let prefix = dir_base.prefix.clone();
                search::walk(
                    &dir_base.dir,
                    &dir_base.relative,
                    dir_base.ancestors,
                    &mut |found| {
                        let rel = found.relative;
                        if matcher.is_match(&rel) {
                            matches.push(display_path(prefix.as_deref(), &rel));
                        }
                        true
                    },
                );
            }
            search::SingleBase::File {
                opened,
                relative,
                prefix,
                ..
            } => {
                // A file base: the pattern is matched against its own root-relative path.
                if opened.stat.kind == crate::safe_fs::EntryKind::File
                    && matcher.is_match(&relative)
                {
                    matches.push(display_path(prefix.as_deref(), &relative));
                }
            }
        }
    }

    matches.sort();
    matches.dedup();
    let total = matches.len();
    let truncated = total > cap;
    matches.truncate(cap);

    let mut text = String::new();
    if let Some(header) = resolved.header {
        text.push_str(&header);
    }
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
        text.push_str(&format!("[no files matched `{pattern}`]\n"));
    }

    Ok(ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(json!({
            "pattern": pattern,
            "path": requested,
            "matches": matches.len(),
            "total_matches": total,
            "truncated": truncated,
        })),
    })
}
