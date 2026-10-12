use std::io::Read;

use agent_core::{ContentPart, ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::fail;
use crate::provider::Limits;
use crate::workspace::{WorkspaceRoot, io_failure};

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    max_bytes: Option<u64>,
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "read".into(),
        description: "Read a UTF-8 text file from the workspace. Large files are truncated \
                      at max_bytes (default 65536, hard cap 1048576); the result reports the \
                      total size and whether it was truncated. Binary (non-UTF-8) content is \
                      refused."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative file path."
                },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1048576,
                    "description": "Byte budget for this read; defaults to 65536."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        concurrency: ToolConcurrency::Shared,
    }
}

pub(crate) async fn run(
    root: &WorkspaceRoot,
    limits: &Limits,
    arguments: &serde_json::Value,
) -> Result<ToolOutput, ToolFailure> {
    let args: ReadArgs = serde_json::from_value(arguments.clone())
        .map_err(|e| fail("invalid_arguments", e.to_string()))?;
    let budget = match args.max_bytes {
        Some(0) => return Err(fail("read_limit_exceeded", "max_bytes must be at least 1")),
        Some(n) if n as usize > limits.max_read_bytes => {
            return Err(fail(
                "read_limit_exceeded",
                format!(
                    "max_bytes {n} exceeds the {} byte limit",
                    limits.max_read_bytes
                ),
            ));
        }
        Some(n) => n as usize,
        None => limits.default_read_bytes,
    };

    let path = root.resolve_existing(&args.path)?;
    let metadata = std::fs::metadata(&path).map_err(|e| io_failure(&args.path, e))?;
    if !metadata.is_file() {
        return Err(fail(
            "not_a_file",
            format!("`{}` is not a regular file", args.path),
        ));
    }
    let total = metadata.len();
    let mut bytes = Vec::with_capacity(budget.min(total as usize));
    std::fs::File::open(&path)
        .map_err(|e| io_failure(&args.path, e))?
        .take(budget as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| io_failure(&args.path, e))?;

    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text.to_owned(),
        Err(error) if error.error_len().is_none() => {
            // The only invalid bytes are an incomplete character at the end, i.e. the
            // budget cut a multi-byte character. Keep the complete prefix.
            std::str::from_utf8(&bytes[..error.valid_up_to()])
                .expect("valid_up_to bounds a valid prefix")
                .to_owned()
        }
        Err(error) => {
            return Err(fail(
                "not_utf8",
                format!(
                    "`{}` is not valid UTF-8 (byte {})",
                    args.path,
                    error.valid_up_to()
                ),
            ));
        }
    };
    let truncated = (bytes.len() as u64) < total;

    Ok(ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(json!({
            "path": args.path,
            "bytes_read": bytes.len(),
            "total_bytes": total,
            "truncated": truncated,
        })),
    })
}
