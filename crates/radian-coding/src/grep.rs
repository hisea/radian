use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

use agent_core::{ContentPart, ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use regex::Regex;
use serde::Deserialize;
use serde_json::json;

use crate::args;
use crate::fail;
use crate::provider::Limits;
use crate::search;
use crate::workspace::{WorkspaceRoot, display_path};

/// Bytes inspected to decide a file is binary: a NUL byte in the head means "skip".
const SNIFF_BYTES: usize = 8 << 10;

/// A single line is read, matched and reported on at most this many bytes. A longer line is
/// matched on its prefix and its reported text carries a `[truncated]` marker, so one enormous
/// line cannot grow the result without bound. The rest of the line is consumed, never stored.
pub(crate) const MAX_LINE_BYTES: usize = 8 << 10;

/// Reserve kept for the trailing truncation notes while the body grows, so the final output
/// (body plus notes) never passes the byte cap. The longest suffix is the context-cut line,
/// the match-truncation line and the byte-limit note together, well under this.
const BYTE_NOTE_RESERVE: usize = 512;

/// `grep`'s output shape. `Content` is today's behavior and the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Content,
    Files,
    Count,
}

impl OutputMode {
    fn as_str(self) -> &'static str {
        match self {
            OutputMode::Content => "content",
            OutputMode::Files => "files",
            OutputMode::Count => "count",
        }
    }
}

/// Whether `line` still fits in a body of `body_len` with room left for the trailing notes.
fn fits(body_len: usize, line_len: usize, byte_cap: usize) -> bool {
    body_len
        .checked_add(line_len)
        .and_then(|n| n.checked_add(BYTE_NOTE_RESERVE))
        .is_some_and(|n| n <= byte_cap)
}

/// The one trailing note when the byte cap stopped the output in `files`/`count` mode:
/// the limit in bytes, how many matches in how many files are shown, and how to narrow
/// the search. `content` mode uses [`byte_note_content`], which additionally names what was
/// dropped (context lines first, then matches).
fn byte_note(byte_cap: usize, total: usize, files_matched: usize, mode: OutputMode) -> String {
    let narrow = match mode {
        OutputMode::Content => {
            "narrow with a more specific `pattern`, `path` or `glob`, or `output: \"files\"`"
        }
        OutputMode::Files => {
            "narrow with a more specific `pattern`, `path` or `glob`, or `output: \"count\"`"
        }
        OutputMode::Count => "narrow with a more specific `pattern`, `path` or `glob`",
    };
    format!(
        "[truncated: output hit {byte_cap} byte limit; showing the first {total} matches \
         in {files_matched} files; {narrow}]\n"
    )
}

/// The one trailing note when the byte cap stopped a `content` result: like [`byte_note`],
/// plus what was dropped to fit. Context lines go first (every match line is kept), so the
/// note always names the dropped context lines and additionally the dropped matches when
/// the match lines alone did not fit.
fn byte_note_content(
    byte_cap: usize,
    shown: usize,
    files_matched: usize,
    dropped_context: usize,
    dropped_matches: usize,
    mode: OutputMode,
) -> String {
    let narrow = match mode {
        OutputMode::Content => {
            "narrow with a more specific `pattern`, `path` or `glob`, or `output: \"files\"\""
        }
        OutputMode::Files => {
            "narrow with a more specific `pattern`, `path` or `glob`, or `output: \"count\"\""
        }
        OutputMode::Count => "narrow with a more specific `pattern`, `path` or `glob`",
    };
    let dropped = if dropped_matches > 0 {
        format!("dropped {dropped_context} context lines and {dropped_matches} matches; ")
    } else {
        format!("dropped {dropped_context} context lines; ")
    };
    format!(
        "[truncated: output hit {byte_cap} byte limit; {dropped}showing the first {shown} matches \
         in {files_matched} files; {narrow}]\n"
    )
}
#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    case_insensitive: Option<bool>,
    context: Option<u64>,
    max_matches: Option<u64>,
    output: Option<String>,
}

pub(crate) fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Rust regex to search for in each line."
            },
            "path": {
                "type": "string",
                "description": "Workspace-relative directory (or file) to search; a read-only \
                                absolute or `~` directory under the cargo registry sources or a \
                                toolchain's `lib/rustlib/src` is also accepted (its results are \
                                absolute). Defaults to the workspace root."
            },
            "glob": {
                "type": "string",
                "description": "Only search files whose workspace-relative path matches this glob, e.g. `**/*.rs`."
            },
            "case_insensitive": {
                "type": "boolean",
                "description": "Match without regard to case; defaults to false."
            },
            "context": {
                "type": "integer",
                "minimum": 0,
                "description": "Lines of context to show before and after each match; defaults to 0. A larger value than the limit (30) is cut to it, not refused."
            },
            "max_matches": {
                "type": "integer",
                "minimum": 1,
                "maximum": 5000,
                "description": "Maximum total matches to return; defaults to 200."
            },
            "output": {
                "type": "string",
                "enum": ["content", "files", "count"],
                "description": "`content` (default) prints each match as `path:line:text`; \
                                `files` prints one line per matching file with its match count; \
                                `count` prints only the totals."
            }
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "grep".into(),
        description: "Search file contents under the workspace (or under path) with a regex. \
                      Each match is reported as `path:line:text`; with `context` the lines \
                      around it are reported too, as `path-line-text`, and groups are separated \
                      by `--`. `.gitignore` is respected, `.git` and symlinks are skipped, and \
                      binary files are skipped. `glob` limits the files searched, \
                      `case_insensitive` ignores case. Matches are capped per file and in \
                      total, total output is bounded by bytes, each cut named in the text; \
                      `output` selects `content` (default), `files` (one line per matching file) \
                      or `count` (totals only). Use `output: \"files\"` for which files contain \
                      a pattern and `output: \"count\"` for how many matches there are. \
                      An invalid regex is an error."
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
    let args: GrepArgs = args::parse("grep", &schema(), arguments)?;

    let regex = regex::RegexBuilder::new(&args.pattern)
        .case_insensitive(args.case_insensitive.unwrap_or(false))
        .build()
        .map_err(|error| fail("invalid_regex", format!("`{}`: {error}", args.pattern)))?;
    let file_filter = match &args.glob {
        Some(pattern) => Some(search::compile_glob(pattern)?),
        None => None,
    };
    // A model asking for more context than the limit gets the limit, not a refused call: the
    // schema's old `maximum` made 7 dogfood calls fail validation on 2026-10-17 (asks of 12-30),
    // each costing a model round trip. The cut is said in the output.
    let requested = usize::try_from(args.context.unwrap_or(0)).unwrap_or(usize::MAX);
    let context = requested.min(limits.max_grep_context);
    let context_cut = requested > context;
    let cap = match args.max_matches {
        Some(0) => {
            return Err(fail(
                "grep_limit_exceeded",
                "max_matches must be at least 1",
            ));
        }
        Some(n) if n as usize > limits.max_grep_matches => {
            return Err(fail(
                "grep_limit_exceeded",
                format!(
                    "max_matches {n} exceeds the {} limit",
                    limits.max_grep_matches
                ),
            ));
        }
        Some(n) => n as usize,
        None => limits.default_grep_matches,
    };

    // `resolve_existing` canonicalizes, so a symlinked `path` that escapes is refused and the
    // base is always inside the workspace.
    let requested = args.path.as_deref().unwrap_or(".");

    // ADR 0039 decision 2 and revision 1: the base and every file are opened through the
    // root's directory fd; the walk hands back open fds, never paths to re-open. An absolute
    // or `~` base must be under a read-only root, and its results are rendered with that
    // root's prefix so they can be passed to `read` again.
    let mut files: Vec<(File, String)> = Vec::new();
    match root.read_base(requested) {
        Ok(base) => {
            let prefix = base.prefix.clone();
            search::walk(&base.dir, &base.relative, &mut |found| {
                let rel = found.relative;
                if let Some(filter) = &file_filter
                    && !filter.is_match(&rel)
                {
                    return true;
                }
                files.push((found.file, display_path(prefix.as_deref(), &rel)));
                true
            });
            files.sort_by(|a, b| a.1.cmp(&b.1));
        }
        Err(failure) if failure.code == "not_a_directory" => {
            // A file base: the pattern is matched against its own root-relative path.
            let opened = root.open_read(requested)?;
            if opened.stat.kind != crate::safe_fs::EntryKind::File {
                return Err(fail(
                    "not_a_file",
                    format!("`{requested}` is not a regular file"),
                ));
            }
            let (rel, prefix) = root.read_relative(requested)?;
            if file_filter
                .as_ref()
                .is_none_or(|filter| filter.is_match(&rel))
            {
                files.push((opened.file, display_path(prefix.as_deref(), &rel)));
            }
        }
        Err(failure) => return Err(failure),
    }

    let mode = match args.output.as_deref() {
        None | Some("content") => OutputMode::Content,
        Some("files") => OutputMode::Files,
        Some("count") => OutputMode::Count,
        Some(other) => {
            return Err(args::invalid(
                "grep",
                arguments,
                [format!(
                    "`output` must be one of \"content\", \"files\" or \"count\", got \"{other}\""
                )],
            ));
        }
    };
    // `context` only shapes `content` lines; `files` and `count` never show them.
    let scan_context = match mode {
        OutputMode::Content => context,
        OutputMode::Files | OutputMode::Count => 0,
    };
    let byte_cap = limits.max_grep_output_bytes;

    let per_file_cap = limits.default_grep_matches_per_file;
    let mut body = String::new();
    let mut total = 0usize;
    let mut files_matched = 0usize;
    let mut truncated = false;
    let mut byte_truncated = false;
    // `content` mode only: what the byte cap dropped (context lines first, then matches).
    let mut dropped_context = 0usize;
    let mut dropped_matches = 0usize;
    if mode == OutputMode::Content {
        // Stream file by file with at most two bounded buffers, so peak memory is one
        // file's `scan_file` result plus at most twice the byte cap. `full` is the
        // complete body (match + context lines) while it still fits; the moment the
        // next file's lines would pass the cap it is discarded and only `matches_body`
        // (match lines + `--` separators + per-file notes) is kept, bounded by the cap.
        // Context lines are counted in `total_context` as they are scanned and never
        // stored once `full` is gone. When `full` never overflows the output is
        // byte-identical to building it all first; otherwise the matches-only body plus
        // the exact dropped counts is emitted. `available`/`truncated` match the old
        // in-order scan: at most `per_file_cap` matches per file, `cap` in total.
        let mut full: Option<String> = Some(String::new());
        let mut matches_body = String::new();
        let mut matches_accepting = true;
        let mut available = 0usize;
        let mut total_context = 0usize;
        let mut full_files = 0usize;
        let mut shown_total = 0usize;
        let mut shown_files = 0usize;
        for (file, rel) in files {
            let budget = per_file_cap.min(cap.saturating_sub(available));
            if budget == 0 {
                truncated = true;
                break;
            }
            let Scan::Matches {
                lines,
                matches,
                cut,
            } = scan_file(&regex, file, &rel, scan_context, budget)
            else {
                continue;
            };
            if matches == 0 {
                continue;
            }
            let new_available = available + matches;
            if cut || new_available >= cap {
                truncated = true;
            }
            total_context += lines
                .iter()
                .filter(|(is_match, line)| !*is_match && line.as_str() != "--\n")
                .count();
            if let Some(fb) = full.as_mut() {
                let mut add = 0usize;
                for (_, line) in &lines {
                    add += line.len();
                }
                if cut && new_available < cap {
                    add += format!(
                        "[{}: more than {} matches in one file]\n",
                        rel, per_file_cap
                    )
                    .len();
                }
                if fb
                    .len()
                    .checked_add(add)
                    .and_then(|n| n.checked_add(BYTE_NOTE_RESERVE))
                    .is_some_and(|n| n <= byte_cap)
                {
                    for (_, line) in &lines {
                        fb.push_str(line);
                    }
                    if cut && new_available < cap {
                        fb.push_str(&format!(
                            "[{}: more than {} matches in one file]\n",
                            rel, per_file_cap
                        ));
                    }
                    full_files += 1;
                } else {
                    // The next file's lines would pass the cap: discard the full body
                    // and keep only the bounded matches-only body from here on.
                    full = None;
                }
            }
            if matches_accepting {
                let mut shown_here = 0usize;
                let mut complete = true;
                for (is_match, line) in &lines {
                    if !*is_match && line.as_str() != "--\n" {
                        continue;
                    }
                    // A separator kept while the body is still empty has no group
                    // above it; the first match starts the output instead.
                    if !*is_match && matches_body.is_empty() {
                        continue;
                    }
                    if !fits(matches_body.len(), line.len(), byte_cap) {
                        complete = false;
                        break;
                    }
                    matches_body.push_str(line);
                    if *is_match {
                        shown_here += 1;
                    }
                }
                if shown_here == 0 {
                    // The first match already misses the cap: nothing to keep from
                    // here; further matches are only counted.
                    matches_accepting = false;
                } else {
                    shown_total += shown_here;
                    shown_files += 1;
                    if !complete {
                        // The byte note below covers the cut; no per-file note. Drop
                        // a dangling group separator at the very end.
                        if matches_body.ends_with("--\n") {
                            matches_body.truncate(matches_body.len() - "--\n".len());
                        }
                        matches_accepting = false;
                    } else if cut && shown_total < cap {
                        let note = format!(
                            "[{}: more than {} matches in one file]\n",
                            rel, per_file_cap
                        );
                        if !fits(matches_body.len(), note.len(), byte_cap) {
                            matches_accepting = false;
                        } else {
                            matches_body.push_str(&note);
                        }
                    }
                }
            }
            available = new_available;
            if available >= cap {
                break;
            }
        }
        if let Some(fb) = full {
            body = fb;
            total = available;
            files_matched = full_files;
        } else {
            // Over the cap: every context line was dropped first and only whole match
            // lines in order were kept; cut matches only when they alone did not fit.
            byte_truncated = true;
            dropped_context = total_context;
            body = matches_body;
            total = shown_total;
            files_matched = shown_files;
            dropped_matches = available.saturating_sub(shown_total);
        }
    } else {
        for (file, rel) in files {
            if byte_truncated {
                break;
            }
            let rel = &rel;
            let budget = per_file_cap.min(cap.saturating_sub(total));
            if budget == 0 {
                truncated = true;
                break;
            }
            let Scan::Matches {
                lines: _,
                matches,
                cut,
            } = scan_file(&regex, file, rel, scan_context, budget)
            else {
                continue;
            };
            if matches == 0 {
                continue;
            }
            match mode {
                OutputMode::Count => {
                    total += matches;
                    files_matched += 1;
                    if cut || total >= cap {
                        truncated = true;
                    }
                    if total >= cap {
                        break;
                    }
                }
                OutputMode::Files => {
                    let line = format!(
                        "{rel}: {matches} {}\n",
                        if matches == 1 { "match" } else { "matches" }
                    );
                    if !fits(body.len(), line.len(), byte_cap) {
                        byte_truncated = true;
                        break;
                    }
                    body.push_str(&line);
                    total += matches;
                    files_matched += 1;
                    if cut || total >= cap {
                        truncated = true;
                    }
                    if total >= cap {
                        break;
                    }
                }
                // `content` is handled above; kept so a future mode cannot silently fall
                // through the match without counting its matches.
                OutputMode::Content => {
                    debug_assert!(false, "content mode has its own emit path");
                }
            }
        }
    }
    if mode == OutputMode::Count {
        // Totals only, even when zero. The line is tiny against any realistic cap; when
        // even it does not fit, the byte note below is the whole output.
        let line = if total == 0 {
            "0 matches in 0 files\n".to_owned()
        } else {
            format!(
                "{total} {} in {files_matched} {}\n",
                if total == 1 { "match" } else { "matches" },
                if files_matched == 1 { "file" } else { "files" }
            )
        };
        if fits(body.len(), line.len(), byte_cap) {
            body.push_str(&line);
        } else {
            byte_truncated = true;
        }
    }
    let mut suffix = String::new();
    if context_cut && mode == OutputMode::Content {
        suffix.push_str(&format!(
            "[context cut to {} lines]\n",
            limits.max_grep_context
        ));
    }
    if truncated && !byte_truncated {
        suffix.push_str(&format!("[truncated: showing the first {total} matches]\n"));
    }
    if byte_truncated {
        if mode == OutputMode::Content {
            suffix.push_str(&byte_note_content(
                byte_cap,
                total,
                files_matched,
                dropped_context,
                dropped_matches,
                mode,
            ));
        } else {
            suffix.push_str(&byte_note(byte_cap, total, files_matched, mode));
        }
    }
    if total == 0 && !byte_truncated && mode != OutputMode::Count {
        suffix.push_str(&format!("[no matches for `{}`]\n", args.pattern));
    }
    debug_assert!(
        suffix.len() <= BYTE_NOTE_RESERVE,
        "grep suffix {}B exceeds the {BYTE_NOTE_RESERVE}B reserve",
        suffix.len()
    );
    let mut out = String::with_capacity(body.len() + suffix.len());
    out.push_str(&body);
    out.push_str(&suffix);
    if out.len() > byte_cap {
        // Pathological (a cap smaller than the reserve): keep the byte promise by cutting
        // on a character boundary. Only reachable with unrealistically tiny caps.
        let mut end = byte_cap;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }

    Ok(ToolOutput {
        content: vec![ContentPart::text(out)],
        structured: Some(json!({
            "pattern": args.pattern,
            "path": requested,
            "matches": total,
            "files": files_matched,
            "context": context,
            "truncated": truncated || byte_truncated,
            "output": mode.as_str(),
        })),
    })
}

/// What searching one file produced.
enum Scan {
    /// Nothing to report: the file is binary, could not be read, or had no match.
    Matches {
        /// Every rendered line (with its trailing newline); the flag says whether the line
        /// is a match (counted) or a context, separator or note line (not counted).
        lines: Vec<(bool, String)>,
        matches: usize,
        cut: bool,
    },
    Skipped,
}

/// Searches one file's lines, rendering at most `budget` matches. `cut` is set when a further
/// match exists that the budget excluded.
fn scan_file(regex: &Regex, mut file: File, rel: &str, context: usize, budget: usize) -> Scan {
    let mut sniff = [0u8; SNIFF_BYTES];
    let mut filled = 0usize;
    loop {
        match file.read(&mut sniff[filled..]) {
            Ok(0) => break,
            Ok(n) => {
                filled += n;
                if filled == SNIFF_BYTES {
                    break;
                }
            }
            Err(_) => return Scan::Skipped,
        }
    }
    if sniff[..filled].contains(&0) {
        return Scan::Skipped; // binary
    }
    if file.seek(SeekFrom::Start(0)).is_err() {
        return Scan::Skipped;
    }

    let mut reader = BufReader::new(file);
    let mut buf: Vec<u8> = Vec::new();
    let mut emitter = Emitter::new(context);
    // The last `context` non-matching lines that were **not** already shown, for the next
    // match's before-context. A match clears it: everything older is behind a shown line.
    let mut ring: VecDeque<(u64, String)> = VecDeque::new();
    let mut after_pending = 0usize;
    let mut lineno = 0u64;
    let mut matches = 0usize;
    let mut cut = false;

    // An io error ends the loop and keeps the matches found so far.
    while let Ok(state) = read_line_capped(&mut reader, &mut buf, MAX_LINE_BYTES) {
        if matches!(state, LineRead::Eof) {
            break;
        }
        lineno += 1;
        let content = strip_line_end(&buf);
        let matched = regex.is_match(&content);
        if matched {
            if matches == budget {
                cut = true;
                break;
            }
            for (line_number, line) in ring.drain(..) {
                emitter.line(rel, line_number, &line, false);
            }
            emitter.line(rel, lineno, &display(&content, state), true);
            matches += 1;
            after_pending = context;
        } else if after_pending > 0 {
            emitter.line(rel, lineno, &display(&content, state), false);
            after_pending -= 1;
        } else if context > 0 {
            ring.push_back((lineno, content));
            while ring.len() > context {
                ring.pop_front();
            }
        }
    }

    if matches == 0 {
        return Scan::Skipped;
    }
    Scan::Matches {
        lines: emitter.lines,
        matches,
        cut,
    }
}

/// The rendered text of a line that is too long for [`MAX_LINE_BYTES`] is marked, so a reader
/// knows the line continued past what was searched.
fn display(content: &str, state: LineRead) -> String {
    match state {
        LineRead::TooLong => format!("{content}[truncated]"),
        _ => content.to_owned(),
    }
}

/// Renders `path:line:text` for a match and `path-line-text` for a context line, with `--`
/// between non-contiguous groups (grep's own convention — only shown when `context` is on).
struct Emitter {
    lines: Vec<(bool, String)>,
    last: Option<u64>,
    emitted: bool,
    context: usize,
}

impl Emitter {
    fn new(context: usize) -> Self {
        Self {
            lines: Vec::new(),
            last: None,
            emitted: false,
            context,
        }
    }

    fn line(&mut self, rel: &str, lineno: u64, text: &str, matched: bool) {
        if self.context > 0
            && self.emitted
            && let Some(last) = self.last
            && lineno > last + 1
        {
            self.lines.push((false, "--\n".to_owned()));
        }
        let sep = if matched { ':' } else { '-' };
        let mut rendered = String::with_capacity(rel.len() + text.len() + 32);
        rendered.push_str(rel);
        rendered.push(sep);
        rendered.push_str(&lineno.to_string());
        rendered.push(sep);
        rendered.push_str(text);
        rendered.push('\n');
        self.lines.push((matched, rendered));
        self.last = Some(lineno);
        self.emitted = true;
    }
}

/// A line's text without its terminator, lossy-decoded so a non-UTF-8 byte cannot fail the
/// whole search.
pub(crate) fn strip_line_end(buf: &[u8]) -> String {
    let mut end = buf.len();
    if end > 0 && buf[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && buf[end - 1] == b'\r' {
        end -= 1;
    }
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// What [`read_line_capped`] did.
pub(crate) enum LineRead {
    /// A complete line (ending in `\n`, or the last line at EOF) is in the buffer.
    Complete,
    /// No more bytes: the buffer is empty.
    Eof,
    /// The line is longer than the cap; the buffer holds the first `cap` bytes and the rest of
    /// the line has been consumed.
    TooLong,
}

/// Reads one line into `buf`, never holding more than `cap` bytes. The remainder of an
/// over-long line is consumed and dropped, so the next call reads the following line.
pub(crate) fn read_line_capped<R: BufRead>(
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
            let room = cap.saturating_sub(buf.len());
            if room > 0 {
                let chunk = reader.fill_buf()?[..room].to_vec();
                buf.extend_from_slice(&chunk);
                reader.consume(room);
            }
            consume_to_newline(reader)?;
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

/// Consumes and drops bytes up to and including the next newline (constant memory). Used only
/// for the tail of a line already known to be over the cap.
fn consume_to_newline<R: BufRead>(reader: &mut R) -> std::io::Result<()> {
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
            return Ok(());
        }
        reader.consume(take);
        if has_newline {
            return Ok(());
        }
    }
}
