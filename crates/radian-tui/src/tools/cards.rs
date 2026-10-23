//! The per-tool renderers: the head line and the body lines of every builtin tool, plus the
//! unified-diff machinery they share (the diff body, the line roles, the hunk header parsing and
//! the patch/diff metas). `tools.rs` owns the card model, the registry and the fold modes.

use super::*;

/// `MCP and other tools`: the name and the arguments' JSON summary.
pub(super) fn generic_head(card: &ToolCard, _text: &str) -> String {
    card.arguments_summary()
}

/// The generic body: the structured output pretty-printed when the tool reported one, the
/// plain text otherwise. The pretty print is synthesized (the text is a different rendering),
/// so its lines carry no source range — the title and a text-backed body stay selectable,
/// a structured-only pretty print does not (reported).
pub(super) fn generic_body(_card: &ToolCard, text: &str) -> Vec<BodyLine> {
    text_body_lines(text, |_| LineRole::Normal)
}

/// A pure text body: every line of `text`, with the byte range of each. `role` classifies the
/// line (the diff renderers pass a classifier; everyone else `Normal`).
pub(super) fn text_body_lines(text: &str, mut role: impl FnMut(&str) -> LineRole) -> Vec<BodyLine> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let range = offset..offset + stripped.len();
        out.push(BodyLine::Text {
            source: range,
            role: role(stripped),
        });
        offset += line.len();
    }
    // `split_inclusive` yields nothing for an empty text: one empty line, like a code block.
    if out.is_empty() {
        out.push(BodyLine::Text {
            source: 0..0,
            role: LineRole::Normal,
        });
    }
    out
}

/// The unified-diff line classifier for a row the renderer draws **without** line numbers (the
/// fallback: an older journal entry or an MCP tool's text): structure is weak, `+`/`-` carry the
/// diff roles, everything else is context. File headers are checked first, because `+++`/`---`
/// also start with the add/remove markers.
pub(super) fn diff_role(line: &str) -> LineRole {
    if diff_structure(line) {
        // `@@` headers and file headers are structure, not content (ADR 0028
        // decision 5: the `frame` role, the weak level).
        return LineRole::Frame;
    }
    if line.starts_with('+') {
        return LineRole::DiffAdded;
    }
    if line.starts_with('-') {
        return LineRole::DiffRemoved;
    }
    LineRole::DiffContext
}

/// A unified diff's structural lines: the `@@` hunk headers and the file headers
/// (`diff --git`, `index`, `---`, `+++`, `new file`, `deleted file`, `similarity`, `rename`).
pub(super) fn diff_structure(line: &str) -> bool {
    line.starts_with("@@")
        || line.starts_with("diff ")
        || line.starts_with("index ")
        || line.starts_with("+++")
        || line.starts_with("---")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
        || line.starts_with("similarity ")
        || line.starts_with("rename ")
}

/// Which side of the diff a row's sign puts it on, or `None` for a structural row (a `@@`
/// header, a file header, a `\ No newline at end of file` marker): those draw in the structure
/// role and carry no line number.
pub(super) fn diff_kind(line: &str) -> Option<DiffKind> {
    if diff_structure(line) {
        return None;
    }
    match line.as_bytes().first() {
        // A blank row (a tool that writes an empty line for an unchanged one) is context too:
        // git itself writes a single space.
        None | Some(b' ') => Some(DiffKind::Context),
        Some(b'+') => Some(DiffKind::Added),
        Some(b'-') => Some(DiffKind::Removed),
        _ => None,
    }
}

/// The old and new start lines a `@@ -a,b +c,d @@` header names, in that order, or `None` when
/// the line is not a hunk header. The counts are not read: the rows count themselves.
pub(super) fn parse_hunk_header(line: &str) -> Option<(u64, u64)> {
    let rest = line.strip_prefix("@@")?;
    let header = &rest[..rest.find("@@")?];
    let mut fields = header.split_whitespace();
    let old = fields.next()?.strip_prefix('-')?.split(',').next()?;
    let new = fields.next()?.strip_prefix('+')?.split(',').next()?;
    Some((old.parse().ok()?, new.parse().ok()?))
}

/// The path a file header names, so the rows under it highlight by the file's suffix:
/// `+++ b/src/main.rs` and `--- a/src/main.rs` give `src/main.rs`, and
/// `diff --git a/x b/x` gives the new side, `x`.
pub(super) fn file_header_path(line: &str) -> Option<&str> {
    let rest = if let Some(rest) = line.strip_prefix("+++ ") {
        rest
    } else if let Some(rest) = line.strip_prefix("--- ") {
        rest
    } else {
        line.strip_prefix("diff --git ")
            .and_then(|rest| rest.split_whitespace().nth(1))?
    };
    Some(
        rest.strip_prefix("a/")
            .or_else(|| rest.strip_prefix("b/"))
            .unwrap_or(rest),
    )
}

/// The path a card's call names, for the diff body's language (`patch` knows it from the call).
pub(super) fn call_path(card: &ToolCard) -> Option<String> {
    string_field(&card.arguments, "path").or_else(|| {
        card.structured
            .as_ref()
            .and_then(|value| string_field(value, "path"))
    })
}

/// `shell`'s title: the command verbatim as `$ <command>` (its own newlines kept — a
/// multi-line command wraps, continuation rows from the body column).
pub(super) fn shell_head(card: &ToolCard, _text: &str) -> String {
    let command = shell_command(card);
    if command.is_empty() {
        return card.arguments_summary();
    }
    format!("$ {command}")
}

/// The command a shell card runs, verbatim (its own newlines kept).
pub(crate) fn shell_command(card: &ToolCard) -> String {
    card.structured
        .as_ref()
        .and_then(|value| value.get("command"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            card.arguments
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// The head text of a tool **call** entry (ADR 0028): the tool name and
/// the renderer's title, `shell $ make test` — the text the head row draws and copies. The
/// `• ` marker, the meta and the status glyph are the block layer's and are never copied.
pub fn call_text(card: &ToolCard, text: &str) -> String {
    let title = head_text(card, text);
    if title.is_empty() {
        return card.name.clone();
    }
    format!("{} {}", card.name, title)
}

/// The exit code from the structured result (`shell`'s own JSON), when it has one.
pub(crate) fn exit_code_of(card: &ToolCard) -> Option<i64> {
    card.structured
        .as_ref()
        .and_then(|value| value.get("exit_code"))
        .and_then(Value::as_i64)
}

/// The rows a renderer's body lays out to, from the display text alone: the collapsed head's
/// `· N lines` (ADR 0028) and the pairing's [`ToolCard::result_lines`].
///
/// Every built-in renderer emits exactly one row per line of the display text (`patch`'s
/// synthesized diff included), which `the_line_count_mirrors_every_renderer`
/// pins; the push path therefore counts without building the rows, which a resume would
/// otherwise do for every tool result.
pub fn body_line_count(text: &str) -> usize {
    if text.is_empty() {
        1
    } else {
        text.lines().count()
    }
}

/// A millisecond duration as the head meta writes it: two decimals below a second (`0.01s`),
/// one above it (`12.4s`).
pub(super) fn duration_text(ms: u64) -> String {
    if ms < 1000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// The head's meta fields (ADR 0028): weak, ` · `-separated, and **only
/// the data that exists**. `shell` writes its outcome (`exit 0`, or `timeout 600s` when it
/// timed out); `patch`/`write` their `+N −M`; every tool its measured duration.
///
/// `cancelled` is the caller's: it also knows the entry's own error code, which the paired
/// result carries rather than the call entry.
pub fn meta_parts(card: &ToolCard, cancelled: bool) -> Vec<String> {
    let structured = card.structured.as_ref();
    let flag = |key: &str| {
        structured
            .and_then(|value| value.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    let mut parts: Vec<String> = Vec::new();
    if cancelled || flag("cancelled") {
        parts.push("cancelled".to_string());
    }
    if card.name == "shell" {
        if let Some(code) = exit_code_of(card) {
            parts.push(format!("exit {code}"));
        }
        if flag("timed_out") {
            let secs = structured
                .and_then(|value| value.get("timeout_secs"))
                .and_then(Value::as_u64);
            parts.push(match secs {
                Some(secs) => format!("timeout {secs}s"),
                None => "timeout".to_string(),
            });
        }
    }
    if let Some(diff) = diff_meta(card) {
        parts.push(diff);
    }
    // The measured duration: the card's own field, or the one a tool reports in its structured
    // output (`shell` writes `duration_ms`; see `radian-coding`'s `shell.rs`).
    let duration = card.duration_ms.or_else(|| {
        structured
            .and_then(|value| value.get("duration_ms"))
            .and_then(Value::as_u64)
    });
    if let Some(ms) = duration {
        parts.push(duration_text(ms));
    }
    parts
}

/// The `+N −M` a `patch` or `write` head shows: `patch` sums the result's structured `hunks`
/// (one `edit` may replace several ranges, and every range counts), falling back to the call's
/// edits when the result carries none (an older journal entry); `write` counts the content it
/// wrote (and the file's previous line count when the result carries one).
///
/// A failed `patch` changed nothing, so it shows no counts at all: the call's edits name text
/// the file still has.
pub(super) fn diff_meta(card: &ToolCard) -> Option<String> {
    let count = |value: Option<&Value>| -> usize {
        value.and_then(Value::as_str).map(text_lines).unwrap_or(0)
    };
    match card.name.as_str() {
        "patch" => {
            if card.is_error {
                return None;
            }
            let hunks = patch_hunks(card);
            if !hunks.is_empty() {
                let added: u64 = hunks.iter().map(|hunk| hunk.new_lines).sum();
                let removed: u64 = hunks.iter().map(|hunk| hunk.old_lines).sum();
                return Some(format!("+{added} −{removed}"));
            }
            let edits = patch_call_edits(card);
            let removed: usize = edits.iter().map(|(old, _)| text_lines(old)).sum();
            let added: usize = edits.iter().map(|(_, new)| text_lines(new)).sum();
            Some(format!("+{added} −{removed}"))
        }
        "write" => {
            let added = count(card.arguments.get("content"));
            match card
                .structured
                .as_ref()
                .and_then(|value| value.get("previous_lines"))
                .and_then(Value::as_u64)
            {
                Some(removed) => Some(format!("+{added} −{removed}")),
                None => Some(format!("+{added}")),
            }
        }
        _ => None,
    }
}

/// `shell`'s body: the output with the stderr section marked. The tool's **text** separates
/// the two streams (`--- stdout ---` / `--- stderr ---`, verified in `radian-coding`'s
/// `shell.rs`); the structured value carries byte counts only, so the marker is the source of
/// truth for the warning style.
pub(super) fn shell_body(_card: &ToolCard, text: &str) -> Vec<BodyLine> {
    let mut in_stderr = false;
    text_body_lines(text, move |line| {
        if line.trim() == "--- stderr ---" {
            in_stderr = true;
        }
        if in_stderr {
            LineRole::Warn
        } else {
            LineRole::Normal
        }
    })
}

/// `read`: the path and the line range the call asked for.
pub(super) fn read_head(card: &ToolCard, _text: &str) -> String {
    let path = card
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| {
            card.structured
                .as_ref()
                .and_then(|value| value.get("path"))
                .and_then(Value::as_str)
        })
        .unwrap_or("<path>");
    let offset = card.arguments.get("offset").and_then(Value::as_u64);
    let limit = card.arguments.get("limit").and_then(Value::as_u64);
    match (offset, limit) {
        (Some(offset), Some(limit)) => format!("{path} {}-{}", offset + 1, offset + limit),
        (Some(offset), None) => format!("{path} {}-", offset + 1),
        (None, Some(limit)) => format!("{path} 1-{limit}"),
        (None, None) => path.to_owned(),
    }
}

/// `read`'s body: the text, shown only when the fold mode expands it (`read` is a small tool:
/// a successful call is one line, ADR 0028).
pub(super) fn read_body(_card: &ToolCard, text: &str) -> Vec<BodyLine> {
    text_body_lines(text, |_| LineRole::Normal)
}

/// `patch`: the path the replacement is in. The `+N −M` counts are the meta's
/// ([`meta_parts`]), not the title's (ADR 0028).
pub(super) fn patch_head(card: &ToolCard, _text: &str) -> String {
    string_field(&card.arguments, "path").unwrap_or_else(|| "<path>".into())
}

/// A `string` field of an argument object.
pub(super) fn string_field(object: &Value, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// `write`: the path the content was written to. The `+N −M` counts are the meta's.
pub(super) fn write_head(card: &ToolCard, _text: &str) -> String {
    string_field(&card.arguments, "path").unwrap_or_else(|| "<path>".into())
}

/// `write`'s body (ADR 0028): the content it wrote, one `Added` diff row
/// per line, numbered from 1. A `write` is a whole-file replacement with no hunk to compare, so
/// every row is an addition, and the entry's text — the content itself — carries no sign: the
/// `+` and the line number are synthesized (drawn, never copied). The language comes from the
/// call's path, like `patch`.
pub(super) fn write_body(card: &ToolCard, text: &str) -> Vec<BodyLine> {
    let lang = call_path(card)
        .as_deref()
        .and_then(crate::highlight::language_for_path);
    let mut out = Vec::new();
    let mut offset = 0usize;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let source = offset..offset + stripped.len();
        offset += line.len();
        out.push(BodyLine::Diff(DiffRow {
            number: Some(index as u64 + 1),
            kind: DiffKind::Added,
            source,
            lang,
            signed: false,
        }));
    }
    // `split_inclusive` yields nothing for an empty write: one empty row, like a code block.
    if out.is_empty() {
        out.push(BodyLine::Diff(DiffRow {
            number: Some(1),
            kind: DiffKind::Added,
            source: 0..0,
            lang,
            signed: false,
        }));
    }
    out
}

/// `grep`: the regex and the path it searched (`.` when the call named none).
pub(super) fn grep_head(card: &ToolCard, _text: &str) -> String {
    let pattern = string_field(&card.arguments, "pattern").unwrap_or_default();
    match string_field(&card.arguments, "path") {
        Some(path) => format!("{pattern} {path}"),
        None => pattern,
    }
}

/// `glob`: the pattern it matched.
pub(super) fn glob_head(card: &ToolCard, _text: &str) -> String {
    string_field(&card.arguments, "pattern").unwrap_or_default()
}

/// `ask_user`: how many questions the call asked (ADR 0024 Q3).
pub(super) fn ask_head(card: &ToolCard, _text: &str) -> String {
    // The call's own `questions` array is the request and is there at call time; the result's
    // `answers` is the reply.
    let questions = card
        .arguments
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| questions.len())
        .or_else(|| {
            card.structured
                .as_ref()
                .and_then(|value| value.get("answers"))
                .and_then(Value::as_array)
                .map(|answers| answers.len())
        });
    match questions {
        Some(1) => "1 question".to_string(),
        Some(n) => format!("{n} questions"),
        None => String::new(),
    }
}

/// The memory tools: the record id, the ids a read asked for, or the search query as the title.
pub(super) fn memory_head(card: &ToolCard, _text: &str) -> String {
    if let Some(id) = string_field(&card.arguments, "id") {
        return id;
    }
    if let Some(ids) = card.arguments.get("ids").and_then(Value::as_array) {
        let ids: Vec<&str> = ids.iter().filter_map(Value::as_str).collect();
        return match ids.len() {
            0 => String::new(),
            1 => ids[0].to_owned(),
            n => format!("{n} records"),
        };
    }
    string_field(&card.arguments, "key")
        .or_else(|| string_field(&card.arguments, "query"))
        .unwrap_or_default()
}

/// The unified-diff body (ADR 0028): one row per line of the entry's
/// text. Used by `patch` (whose entry text **is** the synthesized `@@`/`-old`/`+new` diff).
///
/// A hunk header sets the old and new line counters, and every row under it carries its own
/// file's number: `+` and context rows the **new** file's, `-` rows the **old** file's (the
/// unified rule). A diff whose text carries no `@@` header keeps today's numberless form — the
/// sign column, no gutter (`DiffRow::number` stays `None` → the layout draws no gutter at all).
pub(super) fn diff_body(card: &ToolCard, text: &str) -> Vec<BodyLine> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    // The language comes from the file's own path: `patch` and `write` name it in the call;
    // a `+++ b/…` file header in the text overrides it per file.
    let mut path = call_path(card);
    let mut in_hunk = false;
    let (mut old, mut new) = (0u64, 0u64);
    for line in text.split_inclusive('\n') {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let source = offset..offset + stripped.len();
        offset += line.len();
        if let Some((old_start, new_start)) = parse_hunk_header(stripped) {
            old = old_start;
            new = new_start;
            in_hunk = true;
            out.push(BodyLine::Text {
                source,
                // `@@` is structure: `diff_role` classifies it, so the row cannot drift from
                // the fallback's own structure role.
                role: diff_role(stripped),
            });
            continue;
        }
        if diff_structure(stripped) {
            in_hunk = false;
            if let Some(header_path) = file_header_path(stripped) {
                path = Some(header_path.to_owned());
            }
            out.push(BodyLine::Text {
                source,
                role: diff_role(stripped),
            });
            continue;
        }
        match (in_hunk, diff_kind(stripped)) {
            (true, Some(kind)) => {
                let number = match kind {
                    DiffKind::Removed => {
                        let number = old;
                        old += 1;
                        number
                    }
                    DiffKind::Added => {
                        let number = new;
                        new += 1;
                        number
                    }
                    DiffKind::Context => {
                        let number = new;
                        old += 1;
                        new += 1;
                        number
                    }
                };
                let lang = path
                    .as_deref()
                    .and_then(crate::highlight::language_for_path);
                out.push(BodyLine::Diff(DiffRow {
                    number: Some(number),
                    kind,
                    source,
                    lang,
                    signed: true,
                }));
            }
            // Outside a hunk, or a row with no sign at all: the row's whole line takes its
            // classifier's role, with no gutter (the numberless fallback).
            _ => out.push(BodyLine::Text {
                source,
                role: diff_role(stripped),
            }),
        }
    }
    // `split_inclusive` yields nothing for an empty body: one empty line, like a code block.
    if out.is_empty() {
        out.push(BodyLine::Text {
            source: 0..0,
            role: LineRole::Normal,
        });
    }
    out
}

/// A plain body: the entry text, every line on the card's own role.
pub(super) fn plain_body(_card: &ToolCard, text: &str) -> Vec<BodyLine> {
    text_body_lines(text, |_| LineRole::Normal)
}

/// The text a tool **result** entry displays: the tool's own text, except for `patch` (whose
/// card shows the replacement as a diff - the result text is only "replaced 1 occurrence")
/// and `write` (whose card shows the content it wrote). Synthesizing the display text here
/// keeps the one invariant the selection mapping and the search need: **the body's text is
/// the entry's text**, so what is shown is what is copied and searched.
pub fn result_display_text(card: &ToolCard, tool_text: &str) -> String {
    match card.name.as_str() {
        "patch" => patch_diff_text(card).unwrap_or_else(|| tool_text.to_owned()),
        "write" => string_field(&card.arguments, "content").unwrap_or_else(|| tool_text.to_owned()),
        _ => tool_text.to_owned(),
    }
}

/// A patch's body: one block per replaced range — the hunk header naming the range's start
/// lines in both files, then the removed lines and the added ones, each prefixed by the sign
/// the diff roles paint. There are no context lines: the call carries no surrounding text, and
/// the renderer must not pretend it does (ADR 0028 R4).
///
/// The hunks come from the result's structured `hunks` (`radian-coding`'s `patch`), and each
/// hunk's `-`/`+` text is that hunk's `edit`. A result that carries no `hunks` (an older journal
/// entry), or a hunk whose `edit` is missing or out of range, falls back to the call's edits in
/// call order with no `@@` line; the diff body then draws it without a gutter. A **failed**
/// patch changed nothing, so it has no diff at all: the card shows the tool's own error text.
pub(super) fn patch_diff_text(card: &ToolCard) -> Option<String> {
    if card.is_error {
        return None;
    }
    let edits = patch_call_edits(card);
    if edits.is_empty() {
        return None;
    }
    let hunks = patch_hunks(card);
    let addressed = !hunks.is_empty()
        && hunks.iter().all(|hunk| {
            hunk.edit
                .is_some_and(|edit| (1..=edits.len()).contains(&edit))
        });
    let mut out = String::new();
    if addressed {
        for hunk in &hunks {
            // `addressed` guarantees every hunk names one of the call's edits.
            if let Some(edit) = hunk.edit {
                out.push_str(&format!(
                    "@@ -{},{} +{},{} @@\n",
                    hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
                ));
                let (old, new) = &edits[edit - 1];
                push_diff_lines(&mut out, old, new);
            }
        }
    } else {
        for (old, new) in &edits {
            push_diff_lines(&mut out, old, new);
        }
    }
    Some(out)
}

/// One edit's `-old` / `+new` rows, in that order.
fn push_diff_lines(out: &mut String, old: &str, new: &str) {
    for line in old.lines() {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in new.lines() {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
}

/// A text's line count for the `+N −M` meta: `str::lines`, so a trailing newline ends the last
/// line instead of adding an empty one, and an empty text counts zero.
fn text_lines(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count()
    }
}

/// The call's edits in **call order**, each as its `(old, new)` text: the single `old`/`new`
/// pair, or one entry per item of the `edits` array. Empty when the call carries neither.
pub(super) fn patch_call_edits(card: &ToolCard) -> Vec<(String, String)> {
    if let Some(edits) = card.arguments.get("edits").and_then(Value::as_array) {
        return edits
            .iter()
            .map(|edit| {
                (
                    string_field(edit, "old").unwrap_or_default(),
                    string_field(edit, "new").unwrap_or_default(),
                )
            })
            .collect();
    }
    match (
        string_field(&card.arguments, "old"),
        string_field(&card.arguments, "new"),
    ) {
        (Some(old), Some(new)) => vec![(old, new)],
        _ => Vec::new(),
    }
}

/// One replaced range from the result's `hunks` (ADR 0028): its start line and line count in
/// each file, and the 1-based edit of the call that produced it (`None` for a result written
/// before the `edit` field existed).
pub(super) struct PatchHunk {
    old_start: u64,
    old_lines: u64,
    new_start: u64,
    new_lines: u64,
    edit: Option<usize>,
}

/// The result's `hunks` in file order. Empty when the result carries none.
pub(super) fn patch_hunks(card: &ToolCard) -> Vec<PatchHunk> {
    card.structured
        .as_ref()
        .and_then(|value| value.get("hunks"))
        .and_then(Value::as_array)
        .map(|hunks| {
            hunks
                .iter()
                .filter_map(|hunk| {
                    let field = |name: &str| hunk.get(name).and_then(Value::as_u64);
                    Some(PatchHunk {
                        old_start: field("old_start")?,
                        old_lines: field("old_lines")?,
                        new_start: field("new_start")?,
                        new_lines: field("new_lines")?,
                        edit: field("edit").map(|edit| edit as usize),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A compact one-line JSON rendering, for headers.
pub(super) fn compact_json(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Truncates on a char boundary, appending `…` when something was cut.
pub(super) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
