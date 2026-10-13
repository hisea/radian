use std::io::{BufRead, BufReader};

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
    /// 1-based first line to read.
    offset: Option<i64>,
    /// Maximum number of lines to read.
    limit: Option<i64>,
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "read".into(),
        description: "Read a UTF-8 text file from the workspace. For a large file, read it in \
                      sections with offset (the 1-based first line) and limit (how many lines): \
                      do NOT raise max_bytes to reach a later part, that rereads the prefix \
                      every time. max_bytes (default 65536, hard cap 1048576) bounds the bytes \
                      returned. A byte budget that runs out mid-line drops the incomplete line \
                      and the result names the line range and the next_offset to pass; the only \
                      exception is a first line longer than the whole budget, which is returned \
                      as a prefix with line_truncated. Binary (non-UTF-8) content is refused."
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
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based line to start reading at; defaults to 1."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of lines to read; no line limit if omitted."
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
    let offset = args.offset.unwrap_or(1);
    if offset < 1 {
        return Err(fail("invalid_arguments", "offset must be at least 1"));
    }
    let limit = match args.limit {
        Some(n) if n < 1 => return Err(fail("invalid_arguments", "limit must be at least 1")),
        Some(n) => Some(n as usize),
        None => None,
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

    let file = std::fs::File::open(&path).map_err(|e| io_failure(&args.path, e))?;
    let mut reader = BufReader::new(file);

    // Read whole lines until the requested start line; `line` then holds line `lineno`.
    let mut line: Vec<u8> = Vec::new();
    let mut lineno: i64 = 0;
    loop {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| io_failure(&args.path, e))?;
        if n == 0 {
            break;
        }
        lineno += 1;
        if lineno >= offset {
            break;
        }
    }
    if lineno < offset {
        // The file has fewer lines than `offset`: an empty read with a clear note, not an
        // error (a caller looping with `next_offset` naturally lands here once).
        let last = lineno;
        let note = if last == 0 {
            format!(
                "[offset {offset} is past the end of `{}` (the file is empty)]",
                args.path
            )
        } else {
            format!(
                "[offset {offset} is past the end of `{}` ({last} line(s))]",
                args.path
            )
        };
        return Ok(ToolOutput {
            content: vec![ContentPart::text(note)],
            structured: Some(json!({
                "path": args.path,
                "bytes_read": 0,
                "total_bytes": total,
                "truncated": false,
                "start_line": offset,
                "end_line": offset - 1,
                "line_truncated": false,
            })),
        });
    }

    let start_line = lineno;
    let mut out: Vec<u8> = Vec::with_capacity(budget.min(total as usize));
    let mut end_line = start_line - 1;
    let mut accepted = 0usize;
    let mut line_truncated = false;
    let mut next_offset: Option<i64> = None;
    loop {
        if line.is_empty() {
            // EOF: everything read was accepted.
            break;
        }
        if let Some(limit) = limit
            && accepted >= limit
        {
            // The line was read but not consumed: it is the next read's start.
            next_offset = Some(lineno);
            break;
        }
        if out.len() + line.len() <= budget {
            out.extend_from_slice(&line);
            end_line = lineno;
            accepted += 1;
        } else if out.is_empty() {
            // The start line alone is longer than the whole budget: return a UTF-8 prefix of
            // it and continue at the next line.
            out.extend_from_slice(utf8_prefix(&line, budget));
            end_line = lineno;
            line_truncated = true;
            next_offset = Some(lineno + 1);
            break;
        } else {
            // There is at least one complete line; drop this incomplete one and resume here.
            next_offset = Some(lineno);
            break;
        }
        // Read the next line.
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| io_failure(&args.path, e))?;
        if n == 0 {
            break;
        }
        lineno += 1;
    }

    let bytes_read = out.len();
    let text = match std::str::from_utf8(&out) {
        Ok(text) => text.to_owned(),
        Err(error) if error.error_len().is_none() => {
            // An incomplete character at the very end (a budget cut): keep the prefix. The
            // line-boundary rule already trims this, but keep it as a safety net.
            std::str::from_utf8(&out[..error.valid_up_to()])
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
    let truncated = next_offset.is_some();
    let mut content = text;
    if let Some(next) = next_offset {
        // A text-only provider sees the continuation hint; the structured fields carry the
        // same. A truncated *first line* is marked `line_truncated` instead: its prefix is a
        // partial line, and the existing single-line reads keep their exact text.
        if !line_truncated {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&format!("[truncated: continue with offset={next}]"));
        }
    }
    let mut structured = json!({
        "path": args.path,
        "bytes_read": bytes_read,
        "total_bytes": total,
        "truncated": truncated,
        "start_line": start_line,
        "end_line": end_line,
        "line_truncated": line_truncated,
    });
    if let Some(next) = next_offset {
        structured["next_offset"] = json!(next);
    }
    Ok(ToolOutput {
        content: vec![ContentPart::text(content)],
        structured: Some(structured),
    })
}

/// The longest prefix of `bytes` that is at most `budget` bytes and ends on a UTF-8 boundary.
/// The caller has already established that `bytes` is valid UTF-8, so the only failure is an
/// incomplete character cut by the budget.
fn utf8_prefix(bytes: &[u8], budget: usize) -> &[u8] {
    let end = budget.min(bytes.len());
    match std::str::from_utf8(&bytes[..end]) {
        Ok(_) => &bytes[..end],
        Err(error) => &bytes[..error.valid_up_to()],
    }
}
