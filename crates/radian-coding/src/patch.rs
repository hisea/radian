use std::io::Write;
use std::ops::Range;

use agent_core::{ContentPart, ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::args;
use crate::fail;
use crate::provider::Limits;
use crate::workspace::{WorkspaceRoot, edit_distance_ci, io_failure};

/// The `patch` call: the legacy single-edit `old`/`new` pair, or the new `edits` array for
/// several edits to one file. Exactly one form is accepted (see [`edit_spec`]).
#[derive(Deserialize)]
struct PatchArgs {
    path: String,
    #[serde(default)]
    old: Option<String>,
    #[serde(default)]
    new: Option<String>,
    /// The legacy form's replace-all flag; ignored when `edits` is used.
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    edits: Option<Vec<PatchEdit>>,
}

/// One entry of the multi-edit form: the same fields the legacy form has, with its own
/// `replace_all` default (`false`).
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchEdit {
    old: String,
    new: String,
    #[serde(default)]
    replace_all: bool,
}

/// How many lines of the closest region a not-found result shows.
const CLOSEST_REGION_LINES: usize = 8;

/// How many ambiguous occurrences are listed with line numbers before the "and N more" line.
const MAX_AMBIGUOUS_LISTED: usize = 5;

/// How wide an ambiguous occurrence's line preview may be, in characters (an over-long line
/// carries a trailing ellipsis instead of growing the error without bound).
const AMBIGUOUS_PREVIEW_WIDTH: usize = 120;

pub(crate) fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File to change. Use a workspace-relative path."
            },
            "old": {
                "type": "string",
                "description": "Text to find. It must occur exactly once. To allow repeated matches, set `replace_all`. Use it with `new`."
            },
            "new": {
                "type": "string",
                "description": "Replacement text. Use it with `old`."
            },
            "replace_all": {
                "type": "boolean",
                "description": "Replace every match. Default: false."
            },
            "edits": {
                "type": "array",
                "minItems": 1,
                "description": "Several edits to one file. The tool applies them atomically. Every edit is matched in the original file. All ranges must be disjoint. A failed edit writes nothing. Each entry holds `{old, new, replace_all?}`. Use it instead of `old`/`new`.",
                "items": {
                    "type": "object",
                    "properties": {
                        "old": {"type": "string"},
                        "new": {"type": "string"},
                        "replace_all": {"type": "boolean"}
                    },
                    "required": ["old", "new"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["path"],
        "additionalProperties": false
    })
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "patch".into(),
        description: "Replace `old` with `new` in a file. Matching ignores typographic \
                      quotes, dashes, no-break spaces, and trailing whitespace. The match must \
                      be unique. To replace every match, set `replace_all`. To make several \
                      edits to one file, pass `edits` instead of `old`/`new`."
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
    let args: PatchArgs = args::parse("patch", &schema(), arguments)?;
    let form = edit_spec(&args, arguments)?;
    let multiple = matches!(form, EditSpec::Multiple(_));
    let specs: Vec<PatchEdit> = match form {
        EditSpec::Legacy {
            old,
            new,
            replace_all,
        } => vec![PatchEdit {
            old,
            new,
            replace_all,
        }],
        EditSpec::Multiple(edits) => edits,
    };

    // ADR 0039 decision 2: the target is opened read-write through the root's directory fd
    // (`O_NOFOLLOW` at every component, no `O_CREAT`: a patch target must exist) and the
    // regular-file / hard-link checks run on that fd. The same fd is read and later written;
    // nothing is re-opened by path.
    let opened = root.open_write(&args.path, false)?;
    let mut file = opened.file;
    if opened.stat.size as usize > limits.max_patch_bytes {
        return Err(fail(
            "patch_limit_exceeded",
            format!(
                "`{}` is {} bytes, over the {} byte patch limit",
                args.path, opened.stat.size, limits.max_patch_bytes
            ),
        ));
    }
    let content = crate::safe_fs::read_to_string(&file).map_err(|e| io_failure(&args.path, e))?;
    let crlf = content.contains("\r\n");

    // Every edit is located against the **original** contents before anything is written, so a
    // later edit can never match text an earlier one inserted, and a failure in any edit leaves
    // the file untouched.
    let view = LfView::new(&content);
    let mut resolved = Vec::with_capacity(specs.len());
    for (index, spec) in specs.iter().enumerate() {
        resolved.push(resolve_edit(
            &view, &content, &args.path, spec, index, multiple, crlf,
        )?);
    }
    check_conflicts(&args.path, &resolved)?;

    // All ranges are offsets into the original file. Sorting by start and applying from the end
    // keeps every earlier offset valid; the conflict check above guarantees they do not overlap.
    let mut replacements: Vec<(Range<usize>, String)> = resolved
        .iter()
        .flat_map(|edit| {
            edit.ranges
                .iter()
                .cloned()
                .map(|range| (range, edit.replacement.clone()))
        })
        .collect();
    replacements.sort_by_key(|(range, _)| range.start);
    // Where each replaced range sits in the old file and in the new one (ADR 0028
    // decision 5). The TUI draws one hunk block per range, with each row's line number; the
    // result **text** does not change.
    let hunks = replacement_hunks(&content, &replacements);
    let mut updated = content;
    for (range, replacement) in replacements.iter().rev() {
        updated.replace_range(range.clone(), replacement);
    }
    if updated.len() > limits.max_write_bytes {
        return Err(fail(
            "write_limit_exceeded",
            format!(
                "the result is {} bytes, over the {} byte write limit",
                updated.len(),
                limits.max_write_bytes
            ),
        ));
    }

    // Write back through the fd that was read: truncate, rewind, write.
    crate::safe_fs::truncate(&file).map_err(|e| io_failure(&args.path, e))?;
    file.write_all(updated.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|e| io_failure(&args.path, e))?;

    Ok(render_result(&args.path, multiple, &resolved, hunks))
}

/// The call's edits in one shape: the legacy `old`/`new` pair, or the new `edits` array.
enum EditSpec {
    Legacy {
        old: String,
        new: String,
        replace_all: bool,
    },
    Multiple(Vec<PatchEdit>),
}

/// Resolves the two accepted forms. `edits` and the legacy pair are mutually exclusive; the
/// legacy form keeps its exact behaviour (`old` and `new` are required, `old` must not be empty,
/// and an empty `new` is a legal deletion).
fn edit_spec(args: &PatchArgs, raw: &serde_json::Value) -> Result<EditSpec, ToolFailure> {
    if let Some(edits) = &args.edits {
        if raw.get("old").is_some() || raw.get("new").is_some() || raw.get("replace_all").is_some()
        {
            return Err(args::invalid(
                "patch",
                raw,
                ["use either `edits` or the `old`/`new` pair, not both"],
            ));
        }
        if edits.is_empty() {
            return Err(args::invalid("patch", raw, ["`edits` must not be empty"]));
        }
        for (index, edit) in edits.iter().enumerate() {
            if edit.old.is_empty() {
                return Err(fail(
                    "patch_invalid",
                    edit_message(
                        index,
                        "`old` must not be empty; use write to create or replace a whole file",
                    ),
                ));
            }
        }
        return Ok(EditSpec::Multiple(edits.clone()));
    }
    let old = legacy_field(args.old.as_deref(), raw, "old")?;
    let new = legacy_field(args.new.as_deref(), raw, "new")?;
    if old.is_empty() {
        return Err(fail(
            "patch_invalid",
            "`old` must not be empty; use write to create or replace a whole file",
        ));
    }
    Ok(EditSpec::Legacy {
        old,
        new,
        replace_all: args.replace_all,
    })
}

/// A required legacy field. `args::parse` drops an optional property sent as `""`, so a value
/// that came back `None` is either genuinely absent (an error) or was an empty string — legal
/// for `new` (a deletion), rejected for `old`. The raw call still carries the key, which tells
/// the two cases apart.
fn legacy_field(
    parsed: Option<&str>,
    raw: &serde_json::Value,
    name: &str,
) -> Result<String, ToolFailure> {
    if let Some(value) = parsed {
        return Ok(value.to_owned());
    }
    if raw.get(name).and_then(serde_json::Value::as_str).is_some() {
        return Ok(String::new());
    }
    Err(args::invalid(
        "patch",
        raw,
        [format!("missing required argument `{name}`")],
    ))
}

/// One edit after its range(s) were located in the **original** file.
struct ResolvedEdit {
    /// 0-based position in the call's edit list (the structured `index`).
    index: usize,
    layer: Layer,
    ranges: Vec<Range<usize>>,
    /// The replacement with the file's line endings, applied to every range above.
    replacement: String,
}

/// Locates one edit in the unchanged file and builds its replacement. A failure names the edit
/// by its 1-based number when the call carries more than one.
fn resolve_edit(
    view: &LfView,
    content: &str,
    path: &str,
    spec: &PatchEdit,
    index: usize,
    multiple: bool,
    crlf: bool,
) -> Result<ResolvedEdit, ToolFailure> {
    let (layer, ranges) = match locate(view, &spec.old) {
        Located::Found(layer, ranges) => (layer, ranges),
        // Nothing matched at any layer: say where the closest text is.
        Located::None => {
            return Err(number_edit(
                not_found(view, path, &spec.old),
                index,
                multiple,
            ));
        }
    };
    if ranges.len() > 1 && !spec.replace_all {
        return Err(number_edit(
            ambiguous(content, path, layer, &ranges),
            index,
            multiple,
        ));
    }
    Ok(ResolvedEdit {
        index,
        layer,
        ranges,
        replacement: line_ending_new(&spec.new, crlf),
    })
}

/// Prefixes a failure with its 1-based edit number for the multi-edit form; the legacy form's
/// messages are unchanged.
fn number_edit(mut failure: ToolFailure, index: usize, multiple: bool) -> ToolFailure {
    if multiple {
        failure.message = edit_message(index, &failure.message);
    }
    failure
}

fn edit_message(index: usize, message: &str) -> String {
    format!("edit {}: {message}", index + 1)
}

/// The refusal for text every layer located more than once: the existing first sentence and
/// the file-is-unchanged guarantee, plus up to [`MAX_AMBIGUOUS_LISTED`] occurrence line
/// numbers with each line's own text, then an "and N more" line when more remain.
fn ambiguous(content: &str, path: &str, layer: Layer, ranges: &[Range<usize>]) -> ToolFailure {
    let mut message = format!(
        "`old` appears {} times in `{path}` (matched by {}); refusing to guess which one; \
         the file is unchanged",
        ranges.len(),
        layer.name()
    );
    message.push_str("\nOccurrences:");
    let shown = ranges.len().min(MAX_AMBIGUOUS_LISTED);
    for range in &ranges[..shown] {
        let (lineno, preview) = occurrence_preview(content, range.start);
        message.push_str(&format!("\n{lineno}: {preview}"));
    }
    if ranges.len() > shown {
        message.push_str(&format!("\n... and {} more", ranges.len() - shown));
    }
    fail("patch_ambiguous", message)
}

/// The 1-based line number holding `offset` and that line's own text, trimmed to
/// [`AMBIGUOUS_PREVIEW_WIDTH`] characters with a trailing ellipsis when longer.
fn occurrence_preview(content: &str, offset: usize) -> (usize, String) {
    let clamped = offset.min(content.len());
    let lineno = line_of_offset(content, clamped);
    let start = content[..clamped].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = content[clamped..]
        .find('\n')
        .map(|i| clamped + i)
        .unwrap_or(content.len());
    let mut line = content[start..end].to_owned();
    if line.ends_with('\r') {
        line.pop();
    }
    if line.chars().count() <= AMBIGUOUS_PREVIEW_WIDTH {
        return (lineno, line);
    }
    let mut preview: String = line.chars().take(AMBIGUOUS_PREVIEW_WIDTH).collect();
    preview.push('\u{2026}');
    (lineno, preview)
}

/// Rejects any two edits whose selected ranges overlap, including an identical range chosen by
/// two edits, naming both 1-based edit numbers. Adjacent, non-overlapping ranges are allowed:
/// `a.end == b.start` is not an overlap.
fn check_conflicts(path: &str, edits: &[ResolvedEdit]) -> Result<(), ToolFailure> {
    for (position, first) in edits.iter().enumerate() {
        for second in &edits[position + 1..] {
            for left in &first.ranges {
                for right in &second.ranges {
                    if left.start < right.end && right.start < left.end {
                        return Err(fail(
                            "patch_conflict",
                            format!(
                                "edits {} and {} select overlapping text in `{path}`; the file \
                                 is unchanged",
                                first.index + 1,
                                second.index + 1
                            ),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// The tool result. The legacy form keeps its exact `{path, replaced, matched, hunks}` shape
/// and text; the multi form adds `edits` and a per-edit `results` array. Both carry the
/// flattened, file-order `hunks` the TUI numbers rows from.
fn render_result(
    path: &str,
    multiple: bool,
    edits: &[ResolvedEdit],
    hunks: serde_json::Value,
) -> ToolOutput {
    let replaced: usize = edits.iter().map(|edit| edit.ranges.len()).sum();
    let word = if replaced == 1 {
        "occurrence"
    } else {
        "occurrences"
    };
    if !multiple {
        let edit = &edits[0];
        return ToolOutput {
            content: vec![ContentPart::text(format!(
                "replaced {replaced} {word} in {path} (matched by {})",
                edit.layer.name()
            ))],
            structured: Some(json!({
                "path": path,
                "replaced": replaced,
                "matched": edit.layer.name(),
                "hunks": hunks,
            })),
        };
    }
    let results: Vec<serde_json::Value> = edits
        .iter()
        .map(|edit| {
            json!({
                "edit": edit.index + 1,
                "index": edit.index,
                "replaced": edit.ranges.len(),
                "matched": edit.layer.name(),
            })
        })
        .collect();
    ToolOutput {
        content: vec![ContentPart::text(format!(
            "replaced {replaced} {word} across {} edits in {path}",
            edits.len()
        ))],
        structured: Some(json!({
            "path": path,
            "edits": edits.len(),
            "replaced": replaced,
            "hunks": hunks,
            "results": results,
        })),
    }
}

/// One hunk per replaced range, in file order: the range's 1-based start line and line count in
/// the old file, and its replacement's in the new file.
///
/// `replacements` are ascending by start offset (the caller sorts the original file's ranges), so
/// a hunk's new-file start is its old-file start plus the line delta every **earlier** hunk
/// introduced: deleting lines moves the later hunks up, inserting them moves the later hunks
/// down. The TUI numbers the rows from these (ADR 0028). A line count comes
/// from `str::lines`, which counts a trailing newline as the end of the last line (`a\nb\n` is
/// two lines) and strips a `\r` (so a CRLF file counts the same as an LF one).
fn replacement_hunks(content: &str, replacements: &[(Range<usize>, String)]) -> serde_json::Value {
    let mut delta: i64 = 0;
    let hunks: Vec<serde_json::Value> = replacements
        .iter()
        .map(|(range, replacement)| {
            let new_lines = replacement.lines().count();
            let old_start = line_of_offset(content, range.start);
            let old_lines = content[range.clone()].lines().count();
            let new_start = old_start as i64 + delta;
            delta += new_lines as i64 - old_lines as i64;
            json!({
                "old_start": old_start,
                "old_lines": old_lines,
                // Always >= 1: the lines before a hunk are unchanged, so the new file has at
                // least the old file's line 1 before it.
                "new_start": new_start.max(1) as usize,
                "new_lines": new_lines,
            })
        })
        .collect();
    serde_json::Value::Array(hunks)
}

/// The 1-based line number of byte `offset` in `content`: the newlines before it, plus one.
fn line_of_offset(content: &str, offset: usize) -> usize {
    content[..offset.min(content.len())].matches('\n').count() + 1
}

/// Which layer located the edit. Each layer is a *locator*: the replacement text is `new`,
/// and the replaced bytes are the matched range of the file, never a normalised rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layer {
    /// The needle appears byte for byte.
    Exact,
    /// The needle and the file agree after typographic quotes, dashes and no-break spaces
    /// are mapped to ASCII on both sides.
    Normalised,
    /// A block of whole lines agrees after trailing whitespace is stripped from each line.
    LineTrimmed,
}

impl Layer {
    fn name(self) -> &'static str {
        match self {
            Layer::Exact => "exact",
            Layer::Normalised => "typographic normalisation",
            Layer::LineTrimmed => "line-by-line, trailing whitespace ignored",
        }
    }
}

/// The result of [`locate`].
enum Located {
    /// The layer that matched and the byte ranges of the original content to replace.
    Found(Layer, Vec<Range<usize>>),
    None,
}

/// The file as matched: CRLF is removed, so a needle written with `\n` matches a CRLF file.
/// `to_content[k]` maps view boundary `k` back to a byte offset in the original content, so a
/// replacement stays CRLF.
struct LfView {
    text: String,
    to_content: Vec<usize>,
}

impl LfView {
    fn new(content: &str) -> Self {
        if !content.contains("\r\n") {
            return Self {
                text: content.to_owned(),
                to_content: (0..=content.len()).collect(),
            };
        }
        let bytes = content.as_bytes();
        let mut text = Vec::with_capacity(bytes.len());
        let mut to_content = Vec::with_capacity(bytes.len() + 1);
        to_content.push(0);
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
                index += 1; // drop the `\r`; the `\n` stays
                continue;
            }
            text.push(bytes[index]);
            to_content.push(index + 1);
            index += 1;
        }
        Self {
            text: String::from_utf8(text).expect("the source is valid UTF-8"),
            to_content,
        }
    }

    /// Maps a byte range of the view to the corresponding range of the original content.
    fn content_range(&self, range: Range<usize>) -> Range<usize> {
        self.to_content[range.start]..self.to_content[range.end]
    }
}

/// Locates `old` in the file, trying the layers in order and stopping at the first that has
/// any match. A layer only locates; it never rewrites the file text.
fn locate(view: &LfView, old: &str) -> Located {
    let needle = old.replace("\r\n", "\n");

    let hits = find_all(&view.text, &needle);
    if !hits.is_empty() {
        let ranges = hits
            .into_iter()
            .map(|range| view.content_range(range))
            .collect();
        return Located::Found(Layer::Exact, ranges);
    }

    let (normalised, boundary) = normalise_typographic(&view.text);
    let needle_normalised = normalise_typographic(&needle).0;
    let hits = find_all(&normalised, &needle_normalised);
    if !hits.is_empty() {
        let ranges = hits
            .into_iter()
            .map(|range| view.content_range(boundary[range.start]..boundary[range.end]))
            .collect();
        return Located::Found(Layer::Normalised, ranges);
    }

    let hits = line_trimmed_matches(&view.text, &needle);
    if !hits.is_empty() {
        let ranges = hits
            .into_iter()
            .map(|(start, end)| view.content_range(start..end))
            .collect();
        return Located::Found(Layer::LineTrimmed, ranges);
    }

    Located::None
}

/// The failure for a needle that no layer located: it names the closest region of the file,
/// with line numbers, so the model can fix the needle without another `read`.
fn not_found(view: &LfView, path: &str, old: &str) -> ToolFailure {
    let mut message = format!("`old` does not appear in `{path}`; the file is unchanged");
    if let Some((first, lines)) = closest_region(&view.text, old) {
        let last = first + lines.len() as i64 - 1;
        message.push_str(&format!("\nThe closest region is lines {first}-{last}:"));
        for (offset, line) in lines.iter().enumerate() {
            message.push_str(&format!("\n{}: {}", first + offset as i64, line));
        }
    }
    fail("patch_not_found", message)
}

/// The window of file lines closest to the needle, as `(first 1-based line number, lines)`.
/// The window has the needle's line count (capped), and its score is the summed character
/// edit distance, so the earliest window wins a tie.
fn closest_region<'a>(view: &'a str, old: &str) -> Option<(i64, Vec<&'a str>)> {
    let spans = line_spans(view);
    if spans.is_empty() {
        return None;
    }
    let needle = old.replace("\r\n", "\n");
    let needle_lines = needle_lines(&needle);
    let width = needle_lines
        .len()
        .clamp(1, CLOSEST_REGION_LINES)
        .min(spans.len());
    let mut best: Option<(usize, usize)> = None; // (distance, start line index)
    for start in 0..=spans.len() - width {
        let distance: usize = (0..width)
            .map(|offset| {
                let (line_start, line_end) = spans[start + offset];
                let needle_line = needle_lines.get(offset).copied().unwrap_or("");
                edit_distance_ci(
                    view[line_start..line_end].trim_end(),
                    needle_line.trim_end(),
                )
            })
            .sum();
        match best {
            Some((best_distance, _)) if best_distance <= distance => {}
            _ => best = Some((distance, start)),
        }
    }
    let (_, start) = best?;
    let lines = (0..width)
        .map(|offset| {
            let (line_start, line_end) = spans[start + offset];
            &view[line_start..line_end]
        })
        .collect();
    Some((start as i64 + 1, lines))
}

/// Every non-overlapping occurrence of `needle` in `hay`, as byte ranges.
fn find_all(hay: &str, needle: &str) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    hay.match_indices(needle)
        .map(|(start, _)| start..start + needle.len())
        .collect()
}

/// Maps typographic quotes, dashes and no-break spaces to their ASCII equivalents.
fn normalise_char(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2043}' | '\u{2212}' | '\u{FE58}' | '\u{FE63}' | '\u{FF0D}' => '-',
        '\u{00A0}' | '\u{2007}' | '\u{202F}' => ' ',
        other => other,
    }
}

/// Normalises `text` and returns it together with a map from each normalised byte boundary
/// back to a byte offset in `text`, so a located match maps to the original bytes.
fn normalise_typographic(text: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(text.len());
    let mut boundary = vec![0usize];
    for (start, c) in text.char_indices() {
        boundary[out.len()] = start;
        out.push(normalise_char(c));
        while boundary.len() < out.len() + 1 {
            boundary.push(start + c.len_utf8());
        }
    }
    (out, boundary)
}

/// The byte spans `(start, end-without-newline)` of every line of `text`. A file ending in a
/// newline does not get a phantom empty final line.
fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            spans.push((start, index));
            start = index + 1;
        }
    }
    if start < text.len() {
        spans.push((start, text.len()));
    }
    spans
}

/// The needle's lines. A trailing newline is part of the block, not an extra empty line.
fn needle_lines(needle: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = needle.split('\n').collect();
    if needle.ends_with('\n') {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push("");
    }
    lines
}

/// Blocks of whole lines whose trailing whitespace is equal, as view byte ranges. A needle
/// ending in a newline has that newline included in the matched range.
fn line_trimmed_matches(view: &str, needle: &str) -> Vec<(usize, usize)> {
    let spans = line_spans(view);
    let needle_lines = needle_lines(needle);
    let count = needle_lines.len();
    if count == 0 || spans.len() < count {
        return Vec::new();
    }
    let include_newline = needle.ends_with('\n');
    let mut hits = Vec::new();
    let mut index = 0;
    while index + count <= spans.len() {
        let matches = (0..count).all(|offset| {
            let (start, end) = spans[index + offset];
            view[start..end].trim_end() == needle_lines[offset].trim_end()
        });
        if matches {
            let start = spans[index].0;
            let mut end = spans[index + count - 1].1;
            if include_newline && view.as_bytes().get(end) == Some(&b'\n') {
                end += 1;
            }
            hits.push((start, end));
            index += count;
        } else {
            index += 1;
        }
    }
    hits
}

/// Maps `new`'s line endings to the file's: a CRLF file gets CRLF, an LF file gets LF.
fn line_ending_new(new: &str, crlf: bool) -> String {
    let lf = new.replace("\r\n", "\n");
    if crlf { lf.replace('\n', "\r\n") } else { lf }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_normalised_map_points_back_at_the_original_bytes() {
        let (out, boundary) = normalise_typographic("‘a’\u{00A0}–");
        assert_eq!(out, "'a' -");
        // Each normalised boundary maps to an original byte offset, and the last is the end.
        assert_eq!(boundary[0], 0);
        assert_eq!(*boundary.last().unwrap(), "‘a’\u{00A0}–".len());
        assert!(boundary.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn the_lf_view_maps_offsets_and_stays_crlf() {
        let view = LfView::new("a\r\nb\r\n");
        assert_eq!(view.text, "a\nb\n");
        assert_eq!(view.content_range(0..3), 0..4); // "a\r\nb"
        assert_eq!(view.content_range(2..4), 3..6); // "\nb\r\n"
    }
}
