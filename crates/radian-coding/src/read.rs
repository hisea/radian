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

/// What one bounded read produced, before it becomes a [`ToolOutput`].
struct ReadOutcome {
    content: String,
    bytes_read: usize,
    truncated: bool,
    start_line: i64,
    end_line: i64,
    line_truncated: bool,
    next_offset: Option<i64>,
}

impl ReadOutcome {
    /// `offset` is past the last line: an empty read with a clear note, not an error.
    fn past_end(offset: i64, path: &str) -> Self {
        Self {
            content: format!("[offset {offset} is past the end of `{path}`]"),
            bytes_read: 0,
            truncated: false,
            start_line: offset,
            end_line: offset - 1,
            line_truncated: false,
            next_offset: None,
        }
    }

    fn structured(&self, path: &str, total: u64) -> serde_json::Value {
        let mut value = json!({
            "path": path,
            "bytes_read": self.bytes_read,
            "total_bytes": total,
            "truncated": self.truncated,
            "start_line": self.start_line,
            "end_line": self.end_line,
            "line_truncated": self.line_truncated,
        });
        if let Some(next) = self.next_offset {
            value["next_offset"] = json!(next);
        }
        value
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
    let outcome = read_stream(&mut reader, &args.path, total, budget, offset, limit)?;

    let structured = outcome.structured(&args.path, total);
    Ok(ToolOutput {
        content: vec![ContentPart::text(outcome.content)],
        structured: Some(structured),
    })
}

/// Reads `budget` bytes from line `offset` on, without ever holding more than about `budget`
/// bytes plus the reader's buffer.
///
/// The skip to `offset` scans newlines with [`BufRead::fill_buf`]/[`consume`] and stores
/// nothing (a file with one enormous line is never held in memory). The read phase reads at
/// most `budget - out.len() + 1` bytes of a line: enough to know whether it fits. A line that
/// does not fit is dropped whole and `next_offset` names it; only when the **first** line is
/// over the whole budget is its UTF-8 prefix returned, with `line_truncated`.
fn read_stream<R: BufRead>(
    reader: &mut R,
    path: &str,
    total: u64,
    budget: usize,
    offset: i64,
    limit: Option<usize>,
) -> Result<ReadOutcome, ToolFailure> {
    skip_lines(reader, offset - 1).map_err(|e| io_failure(path, e))?;
    // A `fill_buf` that is empty means line `offset` does not exist (the file ended at or
    // before the requested line). It is a 0-byte read, not an error.
    if reader
        .fill_buf()
        .map_err(|e| io_failure(path, e))?
        .is_empty()
    {
        return Ok(ReadOutcome::past_end(offset, path));
    }

    let start_line = offset;
    let mut out: Vec<u8> = Vec::with_capacity(budget.min(total as usize));
    let mut end_line = start_line - 1;
    let mut accepted = 0usize;
    let mut line_truncated = false;
    let mut next_offset: Option<i64> = None;
    let mut lineno = start_line;
    let mut line: Vec<u8> = Vec::new();
    loop {
        if let Some(limit) = limit
            && accepted >= limit
        {
            // The line is unread: it is the next read's start.
            next_offset = Some(lineno);
            break;
        }
        let room = budget.saturating_sub(out.len());
        match read_line_capped(reader, &mut line, room).map_err(|e| io_failure(path, e))? {
            LineRead::Eof => break,
            LineRead::Complete => {
                out.extend_from_slice(&line);
                end_line = lineno;
                accepted += 1;
            }
            LineRead::TooLong => {
                if out.is_empty() {
                    // The start line alone is longer than the whole budget: return a UTF-8
                    // prefix of it and continue at the next line.
                    out.extend_from_slice(utf8_prefix(&line, budget));
                    end_line = lineno;
                    line_truncated = true;
                    next_offset = Some(lineno + 1);
                } else {
                    // There is at least one complete line; drop this incomplete one.
                    next_offset = Some(lineno);
                }
                break;
            }
        }
        lineno += 1;
    }

    let bytes_read = out.len();
    let text = match std::str::from_utf8(&out) {
        Ok(text) => text.to_owned(),
        Err(error) if error.error_len().is_none() => {
            // An incomplete character at the very end (a budget cut): keep the prefix.
            std::str::from_utf8(&out[..error.valid_up_to()])
                .expect("valid_up_to bounds a valid prefix")
                .to_owned()
        }
        Err(error) => {
            return Err(fail(
                "not_utf8",
                format!("{} is not valid UTF-8 (byte {})", path, error.valid_up_to()),
            ));
        }
    };
    let truncated = next_offset.is_some();
    let mut content = text;
    if let Some(next) = next_offset {
        // The adapters never send `structured` to the model, so the continuation hint must be
        // in the body. A truncated first line is named as such (its prefix is a partial line,
        // not a clean line boundary).
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        if line_truncated {
            content.push_str(&format!(
                "[line {end_line} truncated at max_bytes; continue with offset={next}]"
            ));
        } else {
            content.push_str(&format!("[truncated: continue with offset={next}]"));
        }
    }
    Ok(ReadOutcome {
        content,
        bytes_read,
        truncated,
        start_line,
        end_line,
        line_truncated,
        next_offset,
    })
}

/// What [`read_line_capped`] did.
enum LineRead {
    /// A complete line (ending in `\n`, or the last line at EOF) is in the buffer.
    Complete,
    /// No more bytes: the buffer is empty.
    Eof,
    /// The line is longer than the cap; the buffer holds the first `cap` bytes.
    TooLong,
}

/// Reads one line into `buf`, never holding more than `cap` bytes. `cap` is the room left in
/// the caller's budget, so the caller learns "this line fits" (`Complete`) or "it does not"
/// (`TooLong`) without reading the rest of an arbitrarily long line.
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<LineRead> {
    buf.clear();
    loop {
        let (take, has_newline, at_eof) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                (0, false, true)
            } else {
                match available.iter().position(|&byte| byte == b'\n') {
                    Some(index) => (index + 1, true, false),
                    None => (available.len(), false, false),
                }
            }
        };
        if at_eof {
            return Ok(if buf.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Complete
            });
        }
        if buf.len() + take > cap {
            // The line does not fit: keep the room that is left and stop.
            let room = cap - buf.len();
            if room > 0 {
                let chunk = reader.fill_buf()?[..room].to_vec();
                buf.extend_from_slice(&chunk);
                reader.consume(room);
            }
            return Ok(LineRead::TooLong);
        }
        let chunk = reader.fill_buf()?[..take].to_vec();
        buf.extend_from_slice(&chunk);
        reader.consume(take);
        if has_newline {
            return Ok(LineRead::Complete);
        }
    }
}

/// Consumes up to `lines` complete lines, storing nothing: only newlines are counted. Constant
/// memory, so skipping to a line in a file whose lines are enormous stays cheap in memory (the
/// time is still O(bytes scanned), which the caller chose by asking for that offset).
fn skip_lines<R: BufRead>(reader: &mut R, lines: i64) -> std::io::Result<i64> {
    let mut count = 0i64;
    while count < lines {
        let (take, has_newline, at_eof) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                (0, false, true)
            } else {
                match available.iter().position(|&byte| byte == b'\n') {
                    Some(index) => (index + 1, true, false),
                    None => (available.len(), false, false),
                }
            }
        };
        if at_eof {
            break;
        }
        reader.consume(take);
        if has_newline {
            count += 1;
        }
    }
    Ok(count)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Read};

    /// Counts the bytes the `BufReader` pulls from the source, so a test can bound the read
    /// phase's memory (the skip phase is allowed to scan to `offset`).
    struct CountingRead<R> {
        inner: R,
        bytes: usize,
    }

    impl<R: Read> Read for CountingRead<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buf)?;
            self.bytes += read;
            Ok(read)
        }
    }

    /// The read phase must not pull more than the budget plus one reader buffer, even when the
    /// file is one enormous line with no newline (the OOM case `read_until` had). The source is
    /// an infinite repeat, so only a bounded implementation terminates at all.
    #[test]
    fn the_read_phase_never_reads_more_than_the_budget_plus_a_buffer() {
        const BUDGET: usize = 1024;
        const CAPACITY: usize = 8192;
        // Finite (64 MiB) so a mutation that reads a whole line terminates and fails the
        // count, instead of hanging on an infinite line.
        let counting = CountingRead {
            inner: io::repeat(b'x').take(64 << 20),
            bytes: 0,
        };
        let mut reader = BufReader::with_capacity(CAPACITY, counting);
        let outcome = read_stream(&mut reader, "big.js", 64 << 20, BUDGET, 1, None).unwrap();
        assert!(outcome.line_truncated, "{:?}", outcome.next_offset);
        assert_eq!(outcome.next_offset, Some(2));
        assert_eq!(outcome.bytes_read, BUDGET);
        let pulled = reader.into_inner().bytes;
        assert!(
            pulled <= BUDGET + CAPACITY + 1,
            "the read phase pulled {pulled} bytes for a {BUDGET}-byte budget"
        );
    }
}
