use std::io::{BufRead, BufReader, Seek, SeekFrom};

use radian_core::{ContentPart, ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
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
                "description": "File or directory to read. Use a workspace-relative path. For \
                                cargo registry or Rust toolchain sources, use an absolute path or \
                                a `~` path. Manual: `radian://docs/<page>.md`."
            },
            "max_bytes": {
                "type": "integer",
                "minimum": 1,
                "maximum": 1048576,
                "description": "Maximum bytes to read. Default: 65536."
            },
            "offset": {
                "type": "integer",
                "description": "First line to read (1-based). Default: 1. Negative counts from the end."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": "Maximum lines to read. For a directory, it caps entries \
                                instead. Default: 500. Maximum: 5000."
            }
        },
        "required": ["path"],
        "additionalProperties": false
    })
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "read".into(),
        description: "Read a UTF-8 text file or list a directory. Each line starts with \
                      its line number and `\": \"`. This prefix is not part of the file. \
                      Remove it before you use the text in `patch`. For a large file, read one \
                      part at a time with `offset` and `limit`. Do not increase `max_bytes` \
                      to get a later part."
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

/// Lists the bundled manual root from memory, in the same shape as a directory `read`:
/// `kind`, size and name rows sorted by name, `limit` as the entry cap, with the same
/// truncation note. `offset` and `max_bytes` are file arguments and are ignored, said once.
fn run_docs_dir(
    limits: &Limits,
    user_path: &str,
    limit: Option<usize>,
    offset_given: bool,
    max_bytes_given: bool,
) -> Result<ToolOutput, ToolFailure> {
    let cap = match limit {
        Some(n) if n > limits.max_list_entries => {
            return Err(fail(
                "read_limit_exceeded",
                format!(
                    "`limit` {n} exceeds the {} entry cap for a directory",
                    limits.max_list_entries
                ),
            ));
        }
        Some(n) => n,
        None => limits.default_list_entries,
    };
    let mut rows: Vec<(String, u64)> = crate::docs::DOCS
        .iter()
        .map(|(name, text)| (name.to_string(), text.len() as u64))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let total = rows.len();
    let truncated = total > cap;
    rows.truncate(cap);
    let mut text = String::new();
    for (name, size) in &rows {
        text.push_str(&format!("file\t{size}\t{name}\n"));
    }
    if truncated {
        text.push_str(&format!(
            "[truncated: showing {cap} of {total} entries; narrow it with `glob` or `grep`]\n"
        ));
    }
    if offset_given || max_bytes_given {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(
            "[offset and max_bytes are file arguments and were ignored for this \
             directory; `limit` caps entries]\n",
        );
    }
    Ok(ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(serde_json::json!({
            "path": user_path,
            "entries": rows.len(),
            "total_entries": total,
            "truncated": truncated,
        })),
    })
}

pub(crate) async fn run(
    root: &WorkspaceRoot,
    limits: &Limits,
    arguments: &serde_json::Value,
) -> Result<ToolOutput, ToolFailure> {
    let args: ReadArgs = args::parse("read", &schema(), arguments)?;
    let offset = args.offset.unwrap_or(1);
    if offset == 0 {
        return Err(args::invalid("read", arguments, ["`offset` must not be 0"]));
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
    // The bundled manual root (`radian://docs/`) has no fd and is listed from memory.
    if crate::docs::is_root(&path) {
        return run_docs_dir(limits, &path, limit, offset_given, max_bytes_given);
    }
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

    match opened.source {
        crate::workspace::OpenedSource::File(file) => {
            let mut reader = BufReader::new(file);
            finish_read(&mut reader, &path, total, budget, offset, limit)
        }
        crate::workspace::OpenedSource::Embedded(bytes) => {
            let mut reader = std::io::Cursor::new(bytes);
            finish_read(&mut reader, &path, total, budget, offset, limit)
        }
    }
}

/// The tail of a bounded read once the budget and total are known: negative-offset
/// resolution, the bounded stream, and the tool output. Generic over any readable
/// and seekable source, so real files and embedded manual pages share the logic.
fn finish_read<R: BufRead + Seek>(
    reader: &mut R,
    path: &str,
    total: u64,
    budget: usize,
    offset: i64,
    limit: Option<usize>,
) -> Result<ToolOutput, ToolFailure> {
    // The count scans newlines with `fill_buf`/`consume` only, so it never holds the
    // file in memory; the bounded `read_stream` below still does the actual read.
    let offset = if offset < 0 {
        let total_lines = count_lines(&mut *reader).map_err(|e| io_failure(path, e))?;
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|e| io_failure(path, e))?;
        resolve_from_end(total_lines, offset)
    } else {
        offset
    };
    let outcome = read_stream(&mut *reader, path, total, budget, offset, limit)?;

    let structured = outcome.structured(path, total);
    Ok(ToolOutput {
        content: vec![ContentPart::text(outcome.content)],
        structured: Some(structured),
    })
}

/// Counts the lines `read_stream` would report: each `\\n` ends a line, and a non-empty
/// tail without a trailing `\\n` is one more line. Scans with `fill_buf`/`consume` only,
/// so the file is never held in memory. An empty file counts 0 lines.
fn count_lines<R: BufRead>(reader: &mut R) -> std::io::Result<i64> {
    let mut newlines = 0i64;
    let mut seen = 0u64;
    let mut last_is_newline = true;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        newlines += bytecount_newlines(available);
        seen += available.len() as u64;
        last_is_newline = available[available.len() - 1] == b'\n';
        let take = available.len();
        reader.consume(take);
    }
    if seen > 0 && !last_is_newline {
        newlines += 1;
    }
    Ok(newlines)
}

/// Counts `\\n` bytes in one buffer fill. `\\n` never appears inside a multi-byte UTF-8
/// sequence, so a byte scan agrees with the line scan in `skip_lines`.
fn bytecount_newlines(buf: &[u8]) -> i64 {
    buf.iter().filter(|&&byte| byte == b'\n').count() as i64
}

/// Turns a negative `offset` (`-N`, N >= 1) into the absolute 1-based start line:
/// `total - N + 1`, clamped to 1 when N runs past the start. An empty file resolves
/// to 1, which the read below reports as past-the-end.
fn resolve_from_end(total_lines: i64, offset: i64) -> i64 {
    let want_last = offset.unsigned_abs() as i128;
    let start = total_lines as i128 - want_last + 1;
    start.max(1) as i64
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

    /// `resolve_from_end(-N)` is `total - N + 1`: one off in either direction reads the
    /// wrong first line, which the tail tests above pin down.
    #[test]
    fn resolving_from_the_end_counts_exactly() {
        assert_eq!(resolve_from_end(1000, -5), 996);
        assert_eq!(resolve_from_end(1000, -1), 1000);
        assert_eq!(resolve_from_end(3, -3), 1);
        // Past the start clamps to the first line; an empty file reads from line 1.
        assert_eq!(resolve_from_end(1000, -2000), 1);
        assert_eq!(resolve_from_end(0, -3), 1);
    }

    /// The line count treats a non-empty tail without `\\n` as one more line, matching the
    /// lines `read_stream` reports: `-1` is the last line in both shapes.
    #[test]
    fn counting_lines_agrees_with_and_without_a_trailing_newline() {
        for content in ["a\nb\nc\n", "a\nb\nc"] {
            let mut reader = BufReader::new(content.as_bytes());
            assert_eq!(count_lines(&mut reader).unwrap(), 3, "{content:?}");
        }
        let mut reader = BufReader::new("".as_bytes());
        assert_eq!(count_lines(&mut reader).unwrap(), 0);
        // One newline alone is one (empty) line, not zero.
        let mut reader = BufReader::new("\n".as_bytes());
        assert_eq!(count_lines(&mut reader).unwrap(), 1);
    }

    /// The negative-offset path streams through `fill_buf`/`consume` only and never calls
    /// `Read::read`: a reader whose `read` panics still counts and reads. A mutation that
    /// holds the whole file (`read_to_string`/`read_to_end`/collecting lines) calls `read`
    /// and fails this test instead of silently allocating the file.
    struct FillOnly<'a> {
        data: &'a [u8],
    }

    impl std::io::Read for FillOnly<'_> {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            panic!("the negative-offset path must not call Read::read");
        }
    }

    impl BufRead for FillOnly<'_> {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Ok(self.data)
        }

        fn consume(&mut self, amount: usize) {
            self.data = &self.data[amount..];
        }
    }

    #[test]
    fn the_from_end_path_never_calls_read() {
        let content = "a\nb\nc\nd\n";
        let mut reader = FillOnly {
            data: content.as_bytes(),
        };
        assert_eq!(count_lines(&mut reader).unwrap(), 4);
        let mut reader = FillOnly {
            data: content.as_bytes(),
        };
        let start = resolve_from_end(4, -2);
        assert_eq!(start, 3);
        let outcome = read_stream(&mut reader, "f.txt", content.len() as u64, 64, start, None)
            .expect("streaming read works on fill_buf alone");
        assert_eq!(outcome.start_line, 3);
        assert_eq!(outcome.end_line, 4);
        // Absolute, not relative to the tail: the continuation names a file line.
        assert!(outcome.next_offset.is_none(), "{:?}", outcome.next_offset);
        let mut reader = FillOnly {
            data: content.as_bytes(),
        };
        let limited = read_stream(
            &mut reader,
            "f.txt",
            content.len() as u64,
            64,
            start,
            Some(1),
        )
        .expect("limited tail reads one line");
        assert_eq!(limited.end_line, 3);
        assert_eq!(limited.next_offset, Some(4));
    }
}
