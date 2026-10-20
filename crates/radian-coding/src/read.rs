use std::io::{BufRead, BufReader};

use agent_core::{ContentPart, ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::args;
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

pub(crate) fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Workspace-relative file path, or — read-only — an absolute or \
                                `~` path under the cargo registry sources or a toolchain's \
                                `lib/rustlib/src`."
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
                "description": "Maximum number of lines to read; no line limit if omitted. \
                                For a directory, caps entries instead (default 500, at most 5000)."
            }
        },
        "required": ["path"],
        "additionalProperties": false
    })
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "read".into(),
        description: "Read a UTF-8 text file from the workspace. Read-only Rust sources \
                      (cargo registry, toolchain `lib/rustlib/src`) are also readable when \
                      named as an absolute or `~` path. Every returned line is \
                      prefixed with its file line number as `N: `. That prefix is NOT part of \
                      the file: remove it before using the text anywhere else, in particular \
                      before passing it as the `old` text of `patch`. For a large file, read it \
                      in sections with offset (the 1-based first line) and limit (how many \
                      lines): do NOT raise max_bytes to reach a later part, that rereads the \
                      prefix every time. max_bytes (default 65536, hard cap 1048576) bounds the \
                      file bytes read. A byte budget that runs out mid-line drops the incomplete \
                      line and the result names the line range and the next_offset to pass; the \
                      only exception is a first line longer than the whole budget, which is \
                      returned as a prefix with line_truncated. Binary (non-UTF-8) content is \
                      refused. A directory path lists it (entries sorted with kind and size, \
                      symlinks never followed; `limit` caps entries)."
            .into(),
        input_schema: schema(),
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

/// How many similar names a missing-file error offers.
const MAX_SUGGESTIONS: usize = 3;

/// A missing file is the one place a typo is likelier than a wrong path: keep the `not_found`
/// code, but when the requested directory holds names close to the one asked for, append
/// `Did you mean …?` so the model can retry without another `read`/`glob`. Every suggestion is
/// a workspace-relative name inside the requested directory (see
/// [`WorkspaceRoot::suggest_similar`]).
fn missing_file(root: &WorkspaceRoot, user: &str, failure: ToolFailure) -> ToolFailure {
    if failure.code != "not_found" {
        return failure;
    }
    let suggestions = root.suggest_similar(user, MAX_SUGGESTIONS);
    if suggestions.is_empty() {
        return failure;
    }
    let names: Vec<String> = suggestions.iter().map(|name| format!("`{name}`")).collect();
    fail(
        "not_found",
        format!("{} Did you mean {}?", failure.message, names.join(", ")),
    )
}

pub(crate) async fn run(
    root: &WorkspaceRoot,
    limits: &Limits,
    arguments: &serde_json::Value,
) -> Result<ToolOutput, ToolFailure> {
    let args: ReadArgs = args::parse("read", &schema(), arguments)?;
    let offset = args.offset.unwrap_or(1);
    if offset < 1 {
        return Err(args::invalid(
            "read",
            arguments,
            ["`offset` must be at least 1"],
        ));
    }
    let limit = match args.limit {
        Some(n) if n < 1 => {
            return Err(args::invalid(
                "read",
                arguments,
                ["`limit` must be at least 1"],
            ));
        }
        Some(n) => Some(n as usize),
        None => None,
    };

    let scan = BlockingRead {
        root: root.clone(),
        limits: *limits,
        path: args.path,
        offset,
        limit,
        max_bytes: args.max_bytes,
        offset_given: args.offset.is_some(),
        max_bytes_given: args.max_bytes.is_some(),
    };
    // The opens, the read and the listing below are synchronous and never yield, so they
    // run on the blocking pool: `read` calls batched in one Core task overlap instead of
    // queueing behind each other. Cancellation drops this future; the blocking read then
    // runs on to the end in the background (read-only, so nothing is written) and its
    // result is never written back: Core's own `cancelled` wins the race.
    match tokio::task::spawn_blocking(move || read_blocking(scan)).await {
        Ok(result) => result,
        Err(error) => match error.try_into_panic() {
            Ok(payload) => std::panic::resume_unwind(payload),
            Err(_) => Err(fail("internal", "the read did not finish")),
        },
    }
}

/// Everything `run` does after argument parsing: the opens, the bounded read or the
/// directory listing, and the rendering. Synchronous by construction (see `run`); the body
/// is the old `run` tail, order of checks, limits, outputs and errors unchanged.
struct BlockingRead {
    root: WorkspaceRoot,
    limits: Limits,
    path: String,
    offset: i64,
    limit: Option<usize>,
    max_bytes: Option<u64>,
    offset_given: bool,
    max_bytes_given: bool,
}

fn read_blocking(scan: BlockingRead) -> Result<ToolOutput, ToolFailure> {
    let BlockingRead {
        root,
        limits,
        path,
        offset,
        limit,
        max_bytes,
        offset_given,
        max_bytes_given,
    } = scan;
    let limits = &limits;
    // A directory path lists it (ADR 0043 decision 1): the old `list` output, with
    // `limit` as the entry cap. `offset` and `max_bytes` are file arguments; for a
    // directory they are ignored, said once in the result. The directory is opened
    // through the root's directory fd and listed through that fd (ADR 0039 decision 2).
    // `open_dir` is tried first because `open_read` cannot open the root itself (an
    // empty relative path is not a file name); a file falls through to the read below.
    if let Ok(dir) = root.open_dir(&path) {
        return crate::list::run_dir(limits, &path, &dir, limit, offset_given, max_bytes_given);
    }
    // ADR 0039 decision 2: the file is opened through the root's directory fd, one
    // `openat(O_NOFOLLOW)` per component; it is never re-opened by path after a check.
    let opened = match root.open_read(&path) {
        Ok(opened) => opened,
        Err(failure) => return Err(missing_file(&root, &path, failure)),
    };
    if opened.stat.kind != crate::safe_fs::EntryKind::File {
        return Err(fail(
            "not_a_file",
            format!("`{path}` is not a regular file"),
        ));
    }
    let budget = match max_bytes {
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
    let total = opened.stat.size;

    let mut reader = BufReader::new(opened.file);
    let outcome = read_stream(&mut reader, &path, total, budget, offset, limit)?;

    let structured = outcome.structured(&path, total);
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
                    // prefix of it and continue at the next line. A definite invalid byte
                    // inside the budget is refused here, exactly like the normal path below.
                    match utf8_prefix(&line, budget) {
                        Ok(prefix) => out.extend_from_slice(prefix),
                        Err(position) => {
                            return Err(fail(
                                "not_utf8",
                                format!("{path} is not valid UTF-8 (byte {position})"),
                            ));
                        }
                    }
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
    // Number the file lines for the model: `N: ` before each. The prefix is not file content.
    // The body is numbered *before* the continuation hint is appended, so the hint carries no
    // prefix. An empty body with `next_offset` set is the truncated-first-line case where the
    // budget could not hold even one character; the number still names that line.
    let mut content = if text.is_empty() && line_truncated {
        format!("{start_line}: ")
    } else {
        number_lines(&text, start_line)
    };
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

/// Prefixes each line of `text` with its file line number as `N: `. `split_inclusive` keeps
/// the exact bytes, including each line's trailing newline and the absence of one on the last
/// line, so the only change is the prefix.
fn number_lines(text: &str, start_line: i64) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8 + 8);
    for (index, line) in text.split_inclusive('\n').enumerate() {
        out.push_str(&(start_line + index as i64).to_string());
        out.push_str(": ");
        out.push_str(line);
    }
    out
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
pub(crate) fn skip_lines<R: BufRead>(reader: &mut R, lines: i64) -> std::io::Result<i64> {
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
///
/// `Err(position)` when the range holds a **definite** invalid byte (the read must be refused,
/// like the normal path). An incomplete character *cut by the budget* is not an error: its
/// valid prefix is returned (that is the legitimate truncation the caller names
/// `line_truncated`).
fn utf8_prefix(bytes: &[u8], budget: usize) -> Result<&[u8], usize> {
    let end = budget.min(bytes.len());
    match std::str::from_utf8(&bytes[..end]) {
        Ok(_) => Ok(&bytes[..end]),
        // `error_len() == None`: the budget cut a multi-byte character. Keep the prefix.
        Err(error) if error.error_len().is_none() => Ok(&bytes[..error.valid_up_to()]),
        // A definite invalid byte inside the budget: the same refusal as the normal path.
        Err(error) => Err(error.valid_up_to()),
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
    /// Unwraps the failure side without needing `Debug` on `ReadOutcome`.
    fn expect_failure(outcome: Result<ReadOutcome, ToolFailure>) -> ToolFailure {
        match outcome {
            Ok(outcome) => panic!(
                "expected a failure, got content {:?} (truncated {})",
                outcome.content, outcome.line_truncated
            ),
            Err(error) => error,
        }
    }

    /// An over-long start line with a **definite** invalid byte inside the budget is refused,
    /// not silently cut into "looks-valid" text (the tool's description promises binary
    /// content is refused). `0xFF` can never appear in UTF-8.
    #[test]
    fn an_over_long_first_line_with_an_invalid_byte_is_refused() {
        // The invalid byte sits inside the budget (position 2 of an 8-byte budget).
        let bytes = b"ab\xFFcd".repeat(64);
        let mut reader = BufReader::new(&bytes[..]);
        let error = expect_failure(read_stream(
            &mut reader,
            "bad.bin",
            bytes.len() as u64,
            8,
            1,
            None,
        ));
        assert_eq!(error.code, "not_utf8", "{error:?}");
        assert!(
            error.message.contains("byte 2"),
            "the message names the byte position: {error:?}"
        );

        // At the very start of the line: refused too (the old code returned empty content).
        let bytes = b"\xFFfill".repeat(64);
        let mut reader = BufReader::new(&bytes[..]);
        let error = expect_failure(read_stream(
            &mut reader,
            "bad.bin",
            bytes.len() as u64,
            8,
            1,
            None,
        ));
        assert_eq!(error.code, "not_utf8", "{error:?}");
        assert!(error.message.contains("byte 0"), "{error:?}");
    }

    /// A budget that cuts a multi-byte character in half is the legitimate truncation: the
    /// prefix is returned, never half a character, with `line_truncated` and the hint.
    #[test]
    fn an_over_long_first_line_cut_mid_character_keeps_a_valid_prefix() {
        // "ab" + a 3-byte character + filler; budget 4 cuts inside the character.
        let mut bytes = b"ab".to_vec();
        bytes.extend_from_slice("€".as_bytes());
        bytes.extend_from_slice(&b"x".repeat(64));
        let mut reader = BufReader::new(&bytes[..]);
        let outcome = read_stream(&mut reader, "long.txt", bytes.len() as u64, 4, 1, None)
            .expect("a budget cut is not an error");
        assert!(
            outcome.content.starts_with("1: ab"),
            "only complete characters survive: {}",
            outcome.content
        );
        assert!(outcome.line_truncated, "truncated: {}", outcome.content);
        assert!(
            outcome.next_offset.is_some(),
            "next: {:?}",
            outcome.next_offset
        );
        assert!(
            outcome
                .content
                .contains("[line 1 truncated at max_bytes; continue with offset="),
            "content: {}",
            outcome.content
        );
    }

    /// An invalid byte **beyond** the budget is this read's business only in so far as it was
    /// not read: the read stops at the budget, so it succeeds. (Intentional: the tool answers
    /// for the bytes it read, not for the rest of the file.)
    #[test]
    fn an_invalid_byte_beyond_the_budget_does_not_fail_the_read() {
        let mut bytes = b"a".repeat(20);
        bytes.push(0xFF);
        bytes.extend_from_slice(&b"b".repeat(64));
        let mut reader = BufReader::new(&bytes[..]);
        let outcome = read_stream(&mut reader, "late-bad.bin", bytes.len() as u64, 8, 1, None)
            .expect("the invalid byte was never read");
        assert!(
            outcome.content.starts_with("1: aaaaaaaa"),
            "content: {}",
            outcome.content
        );
        assert!(outcome.line_truncated, "truncated: {}", outcome.content);
    }

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
