use std::io::Write;
use std::ops::Range;

use agent_core::{ContentPart, ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::args;
use crate::fail;
use crate::provider::Limits;
use crate::workspace::{WorkspaceRoot, edit_distance_ci, io_failure};

#[derive(Deserialize)]
struct PatchArgs {
    path: String,
    old: String,
    new: String,
    /// Replace every located occurrence instead of refusing when there is more than one.
    #[serde(default)]
    replace_all: bool,
}

/// How many lines of the closest region a not-found result shows.
const CLOSEST_REGION_LINES: usize = 8;

pub(crate) fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Workspace-relative file path."
            },
            "old": {
                "type": "string",
                "description": "Text to find; must locate exactly one occurrence unless replace_all is true."
            },
            "new": {
                "type": "string",
                "description": "Replacement text."
            },
            "replace_all": {
                "type": "boolean",
                "description": "Replace every located occurrence instead of refusing when there is more than one; defaults to false."
            }
        },
        "required": ["path", "old", "new"],
        "additionalProperties": false
    })
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "patch".into(),
        description: "Replace `old` with `new` in an existing workspace file. Matching is \
                      layered, and every layer only *locates* text, it never invents any: \
                      (1) exact; (2) with typographic quotes, dashes and no-break spaces \
                      normalised to ASCII on both sides; (3) line by line, ignoring trailing \
                      whitespace. A match at any layer must be unique, or the edit is refused \
                      and the file is unchanged; set replace_all: true to replace every \
                      located occurrence instead. A CRLF file is matched on an LF view, the \
                      offsets are mapped back, and the file stays CRLF. When nothing matches, \
                      the error names the closest region of the file, with line numbers, so \
                      you do not need another read."
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
    if args.old.is_empty() {
        return Err(fail(
            "patch_invalid",
            "`old` must not be empty; use write to create or replace a whole file",
        ));
    }

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

    let view = LfView::new(&content);
    let (layer, ranges) = match locate(&view, &args.old) {
        Located::Found(layer, ranges) => (layer, ranges),
        // Nothing matched at any layer: say where the closest text is.
        Located::None => return Err(not_found(&view, &args.path, &args.old)),
    };
    if ranges.len() > 1 && !args.replace_all {
        return Err(fail(
            "patch_ambiguous",
            format!(
                "`old` appears {} times in `{}` (matched by {}); refusing to guess which one; \
                 the file is unchanged",
                ranges.len(),
                args.path,
                layer.name()
            ),
        ));
    }

    let replacement = line_ending_new(&args.new, content.contains("\r\n"));
    let mut updated = content;
    // Replace from the end so the earlier ranges' offsets stay valid.
    for range in ranges.iter().rev() {
        updated.replace_range(range.clone(), &replacement);
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

    let word = if ranges.len() == 1 {
        "occurrence"
    } else {
        "occurrences"
    };
    Ok(ToolOutput {
        content: vec![ContentPart::text(format!(
            "replaced {} {} in {} (matched by {})",
            ranges.len(),
            word,
            args.path,
            layer.name()
        ))],
        structured: Some(json!({
            "path": args.path,
            "replaced": ranges.len(),
            "matched": layer.name(),
        })),
    })
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
