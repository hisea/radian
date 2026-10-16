use std::fs::File;
use std::io::{Read, Write};

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
    let readable = opened.readable;
    let mut file = opened.file;
    // ADR 0028: the result carries the old file's line count so the TUI
    // can show `+M −N`. It is counted on the fd that is about to be truncated (never a second
    // open of the path), streamed in chunks so a large old file is not held in memory, and
    // before the truncate so the bytes are still there. An empty file reports nothing.
    //
    // The count is a display nicety and must never fail the write (review fix 1): a file the
    // user may write but not read opens write-only (`readable` is false), and a read error on a
    // readable fd is swallowed too — the write goes ahead with no `previous_lines` either way.
    let previous_lines = if readable {
        count_lines(&file).unwrap_or(0)
    } else {
        0
    };
    crate::safe_fs::truncate(&file).map_err(|e| io_failure(&args.path, e))?;
    file.write_all(args.content.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|e| io_failure(&args.path, e))?;

    let text = format!("wrote {} bytes to {}", args.content.len(), args.path);
    let mut structured = json!({
        "path": args.path,
        "bytes_written": args.content.len(),
    });
    if previous_lines > 0 {
        structured["previous_lines"] = json!(previous_lines);
    }

    Ok(ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(structured),
    })
}

/// The number of lines an open file holds, by the rule the TUI's own count of a tool's
/// `content` uses (`radian-tui`'s `diff_meta`): [`str::lines`] — lines split at `\n`, a `\r\n`
/// pair counting once, a final line ending adding no empty line, and an empty file counting
/// zero. Streamed in fixed chunks, so the count never depends on the file's size (the result's
/// `previous_lines`, ADR 0028).
fn count_lines(file: &File) -> std::io::Result<u64> {
    let mut handle = file;
    let mut buf = [0u8; 64 * 1024];
    let mut newlines = 0u64;
    let mut total = 0u64;
    let mut last = 0u8;
    loop {
        let read = handle.read(&mut buf)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        newlines += buf[..read].iter().filter(|&&byte| byte == b'\n').count() as u64;
        last = buf[read - 1];
    }
    if total == 0 {
        return Ok(0);
    }
    Ok(newlines + 1 - u64::from(last == b'\n'))
}
