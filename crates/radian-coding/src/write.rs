use std::io::Write;

use agent_core::{ContentPart, ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::args;
use crate::fail;
use crate::provider::Limits;
use crate::workspace::{WorkspaceRoot, io_failure};

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub(crate) fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Workspace-relative file path."
            },
            "content": {
                "type": "string",
                "description": "Exact bytes to write (UTF-8)."
            }
        },
        "required": ["path", "content"],
        "additionalProperties": false
    })
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "write".into(),
        description: "Create or overwrite a workspace file with exactly `content` (no trailing \
                      newline is added). The parent directory must exist. The destination is \
                      opened without following a symlink and is refused if it is hard-linked."
            .into(),
        input_schema: schema(),
        concurrency: ToolConcurrency::Exclusive,
    }
}

pub(crate) async fn run(
    root: &WorkspaceRoot,
    limits: &Limits,
    arguments: &serde_json::Value,
) -> Result<ToolOutput, ToolFailure> {
    let args: WriteArgs = args::parse("write", &schema(), arguments)?;
    if args.content.len() > limits.max_write_bytes {
        return Err(fail(
            "write_limit_exceeded",
            format!(
                "content is {} bytes, over the {} byte limit",
                args.content.len(),
                limits.max_write_bytes
            ),
        ));
    }

    // ADR 0039 decision 2: parents are walked by fd with `O_NOFOLLOW | O_DIRECTORY`, the
    // destination is opened with `O_CREAT | O_NOFOLLOW` relative to its parent's fd, and the
    // regular-file and hard-link checks run on the open fd **before** it is truncated. Nothing
    // is re-opened by path, so a parent swapped for a link mid-call is refused, not followed.
    let opened = root.open_write(&args.path, true)?;
    let mut file = opened.file;
    crate::safe_fs::truncate(&file).map_err(|e| io_failure(&args.path, e))?;
    file.write_all(args.content.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|e| io_failure(&args.path, e))?;

    Ok(ToolOutput {
        content: vec![ContentPart::text(format!(
            "wrote {} bytes to {}",
            args.content.len(),
            args.path
        ))],
        structured: Some(json!({
            "path": args.path,
            "bytes_written": args.content.len(),
        })),
    })
}
