use std::io::Write;

use agent_core::{ContentPart, ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::fail;
use crate::provider::Limits;
use crate::workspace::{WorkspaceRoot, io_failure};
use crate::write::write_error;

#[derive(Deserialize)]
struct PatchArgs {
    path: String,
    old: String,
    new: String,
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "patch".into(),
        description: "Replace one exact occurrence of `old` with `new` in an existing \
                      workspace file. There is no fuzzy matching: if `old` is missing or \
                      appears more than once the edit is refused and the file is untouched."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative file path."
                },
                "old": {
                    "type": "string",
                    "description": "Exact text to find; must appear exactly once."
                },
                "new": {
                    "type": "string",
                    "description": "Replacement text."
                }
            },
            "required": ["path", "old", "new"],
            "additionalProperties": false
        }),
        concurrency: ToolConcurrency::Exclusive,
    }
}

pub(crate) async fn run(
    root: &WorkspaceRoot,
    limits: &Limits,
    arguments: &serde_json::Value,
) -> Result<ToolOutput, ToolFailure> {
    let args: PatchArgs = serde_json::from_value(arguments.clone())
        .map_err(|e| fail("invalid_arguments", e.to_string()))?;
    if args.old.is_empty() {
        return Err(fail(
            "patch_invalid",
            "`old` must not be empty; use write to create or replace a whole file",
        ));
    }

    // Patch targets must already exist, so canonicalize and require the result inside.
    let target = root.resolve_existing(&args.path)?;
    root.guard_mutation_target(&args.path, &target)?;
    let metadata = std::fs::metadata(&target).map_err(|e| io_failure(&args.path, e))?;
    if !metadata.is_file() {
        return Err(fail(
            "not_a_file",
            format!("`{}` is not a regular file", args.path),
        ));
    }
    if metadata.len() as usize > limits.max_patch_bytes {
        return Err(fail(
            "patch_limit_exceeded",
            format!(
                "`{}` is {} bytes, over the {} byte patch limit",
                args.path,
                metadata.len(),
                limits.max_patch_bytes
            ),
        ));
    }
    let content = std::fs::read_to_string(&target).map_err(|e| io_failure(&args.path, e))?;

    match content.matches(&args.old).count() {
        0 => {
            return Err(fail(
                "patch_not_found",
                format!(
                    "`old` does not appear in `{}`; the file is unchanged",
                    args.path
                ),
            ));
        }
        1 => {}
        count => {
            return Err(fail(
                "patch_ambiguous",
                format!(
                    "`old` appears {count} times in `{}`; refusing to guess which one; the \
                     file is unchanged",
                    args.path
                ),
            ));
        }
    }

    let updated = content.replacen(&args.old, &args.new, 1);
    if updated.len() > limits.max_write_bytes {
        return Err(fail(
            "write_limit_exceeded",
            format!(
                "the result is {} bytes, over the {} byte write limit",
                updated.len(),
                limits.max_write_bytes
            ),
        ));
    }

    // Write the canonical target: it is already known to be inside the root, and
    // `O_NOFOLLOW` still refuses a link that appeared at that exact path meanwhile.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&target)
        .map_err(|e| write_error(&args.path, e))?;
    file.write_all(updated.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|e| io_failure(&args.path, e))?;

    Ok(ToolOutput {
        content: vec![ContentPart::text(format!(
            "replaced 1 occurrence in {}",
            args.path
        ))],
        structured: Some(json!({
            "path": args.path,
            "replaced": 1,
        })),
    })
}
