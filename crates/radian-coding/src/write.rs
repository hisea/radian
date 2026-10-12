use std::io::Write;

use agent_core::{ContentPart, ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::fail;
use crate::provider::Limits;
use crate::workspace::{WorkspaceRoot, io_failure};

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "write".into(),
        description: "Create or overwrite a workspace file with exactly `content` (no trailing \
                      newline is added). The parent directory must exist. The destination is \
                      opened without following a symlink and is refused if it is hard-linked."
            .into(),
        input_schema: json!({
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
        }),
        concurrency: ToolConcurrency::Exclusive,
    }
}

pub(crate) async fn run(
    root: &WorkspaceRoot,
    limits: &Limits,
    arguments: &serde_json::Value,
) -> Result<ToolOutput, ToolFailure> {
    let args: WriteArgs = serde_json::from_value(arguments.clone())
        .map_err(|e| fail("invalid_arguments", e.to_string()))?;
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

    let target = root.resolve_parent_for_write(&args.path)?;
    root.guard_mutation_target(&args.path, &target)?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // Never follow a symlink at the destination. The guard above already refuses links, so
    // this is defence in depth against the destination being swapped for a link between the
    // guard and this open.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&target)
        .map_err(|e| write_error(&args.path, e))?;
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

/// `ELOOP` here means the destination grew a symlink between the guard and the open; report
/// it as the symlink rule rather than a generic io error.
pub(crate) fn write_error(user: &str, error: std::io::Error) -> ToolFailure {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return fail(
            "path_is_symlink",
            format!("`{user}` is a symlink; this tool never writes through a link"),
        );
    }
    io_failure(user, error)
}
