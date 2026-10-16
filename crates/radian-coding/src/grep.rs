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

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    case_insensitive: Option<bool>,
    context: Option<u64>,
    max_matches: Option<u64>,
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
                "maximum": 10,
                "description": "Lines of context to show before and after each match; defaults to 0."
            },
            "max_matches": {
                "type": "integer",
                "minimum": 1,
                "maximum": 5000,
                "description": "Maximum total matches to return; defaults to 200."
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
                      total, with a line naming what was cut; an invalid regex is an error."
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
    let context = args.context.unwrap_or(0);
    if context as usize > limits.max_grep_context {
        return Err(args::invalid(
            "grep",
            arguments,
            [format!(
                "`context` {context} exceeds the {} line limit",
                limits.max_grep_context
            )],
        ));
    }
    let context = context as usize;
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

    let per_file_cap = limits.default_grep_matches_per_file;
    let mut out = String::new();
    let mut total = 0usize;
    let mut files_matched = 0usize;
    let mut truncated = false;
    for (file, rel) in files {
        let rel = &rel;
        let budget = per_file_cap.min(cap.saturating_sub(total));
        if budget == 0 {
            truncated = true;
            break;
        }
        let Scan::Matches { text, matches, cut } = scan_file(&regex, file, rel, context, budget)
        else {
            continue;
        };
        if matches == 0 {
            continue;
        }
        out.push_str(&text);
        total += matches;
        files_matched += 1;
        if cut {
            if total >= cap {
                truncated = true;
                break;
            }
            out.push_str(&format!(
                "[{rel}: more than {per_file_cap} matches in one file]\n"
            ));
            truncated = true;
        }
        if total >= cap {
            truncated = true;
            break;
        }
    }
    if truncated {
        out.push_str(&format!("[truncated: showing the first {total} matches]\n"));
    }
    if total == 0 {
        out.push_str(&format!("[no matches for `{}`]\n", args.pattern));
    }

    Ok(ToolOutput {
        content: vec![ContentPart::text(out)],
        structured: Some(json!({
            "pattern": args.pattern,
            "path": requested,
            "matches": total,
            "files": files_matched,
            "context": context,
            "truncated": truncated,
        })),
    })
}

/// What searching one file produced.
enum Scan {
    /// Nothing to report: the file is binary, could not be read, or had no match.
    Matches {
        text: String,
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
        text: emitter.out,
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
    out: String,
    last: Option<u64>,
    emitted: bool,
    context: usize,
}

impl Emitter {
    fn new(context: usize) -> Self {
        Self {
            out: String::new(),
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
            self.out.push_str("--\n");
        }
        let sep = if matched { ':' } else { '-' };
        self.out.push_str(rel);
        self.out.push(sep);
        self.out.push_str(&lineno.to_string());
        self.out.push(sep);
        self.out.push_str(text);
        self.out.push('\n');
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
