//! A tool call's card data and the text the fold writes into an entry (ADR 0028).
//!
//! [`ToolCard`] is filled from the messages themselves, so a journal fold and a live session's
//! commit path hold the same data. [`call_text`] is the head row's text — the tool name and the
//! renderer's title — and it is the **entry's text**, so what is drawn is what is copied and
//! searched. [`result_display_text`] is a result entry's text: a `patch` shows its diff and a
//! `write` the content it wrote, everything else its own result text.
//!
//! The head functions and the two text functions live here because the folds need the text, not
//! only a renderer that could draw it. `radian-tui` uses them for its renderer registry, so the
//! fold and the screen cannot disagree about a tool's title.

use serde::Serialize;
use serde_json::Value;

use super::memory::MemoryRef;

/// One tool call's card data. `structured` and `duration_ms` may legitimately be absent
/// (`None`): a tool that reports none, and the timing no source carries at all.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolCard {
    /// The provider's call id: what pairs a tool message's result with its call.
    pub call_id: String,
    /// The model-facing tool name.
    pub name: String,
    /// The call's argument JSON (always present: the provider always sends arguments).
    pub arguments: Value,
    /// The result's machine-readable form, when the tool produced one.
    pub structured: Option<Value>,
    /// How long the call took. **No producer yet**: renderers omit it when it is `None`. A
    /// shell result's structured output carries its own `duration_ms`.
    pub duration_ms: Option<u64>,
    /// The paired result's body line count, once it has arrived (`None` while the call runs):
    /// the collapsed fold's `· N lines` (ADR 0028). The head lives on the **call** entry while
    /// the body lives on the result's, so the call entry can only know the count through this
    /// field, which the pairing fills.
    pub result_lines: Option<usize>,
    /// Whether the paired result is an error. A failed `patch` changed nothing, so its card
    /// shows the tool's own error text instead of the call's edits, and no `+N −M`.
    pub is_error: bool,
    /// The ACP tool call's status, as the conversation page draws it: `queued`, `running`,
    /// `done` or `failed` (ADR 0048 §13). It rides on the head's meta instead of the status
    /// glyph, so a `terminal` content block (which has no result to pair) still shows its
    /// state. `None` for every radian-owned tool call, whose status is the glyph alone.
    pub status: Option<String>,
    /// The byte offset in the result entry's display text where a plain `content` text starts
    /// after a `diff` body (ADR 0048 §13). `None` when the body is all one kind.
    pub trailing_from: Option<usize>,
    /// The memory file this call settled on (ADR 0035): present only after a successful
    /// `read`, `write` or `patch` whose path is a record under the memory root. Host-side
    /// display data; never serialized (the hub's clients draw the entry text instead).
    #[serde(skip_serializing)]
    pub memory: Option<MemoryRef>,
}

impl ToolCard {
    pub fn new(call_id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            arguments,
            structured: None,
            duration_ms: None,
            result_lines: None,
            is_error: false,
            status: None,
            trailing_from: None,
            memory: None,
        }
    }

    /// The argument JSON's one-line summary, for the generic header (and the fallback for a
    /// tool whose renderer wants a field that is not there).
    pub fn arguments_summary(&self) -> String {
        let text = compact_json(&self.arguments);
        truncate_chars(&text, 120)
    }
}

/// The header of a card, **without** width: the layout truncates it like a code line, so the
/// same string is the entry's copyable text and the displayed line. `text` is the tool's result
/// text when it is known (a call-time head has none and stays terse — the pairing refreshes it
/// when the result arrives).
pub fn head_text(card: &ToolCard, text: &str) -> String {
    head_for(&card.name)(card, text)
}

/// The head function the registry uses for `name`. One match serves both the fold's text and
/// the renderer registry, so a tool's title is computed in exactly one place.
pub fn head_for(name: &str) -> fn(&ToolCard, &str) -> String {
    match name {
        "shell" => shell_head,
        "read" => read_head,
        "patch" => patch_head,
        "write" => write_head,
        "grep" => grep_head,
        "glob" => glob_head,
        "ask_user" => ask_head,
        _ => generic_head,
    }
}

/// The head text of a tool **call** entry (ADR 0028): the tool name and the renderer's title,
/// `shell $ make test` — the text the head row draws and copies.
pub fn call_text(card: &ToolCard, text: &str) -> String {
    // ADR 0035: a memory read's one line is the memory, not the read tool's own head; the
    // write/patch card's text is its body row, so search and copy still see the title, the type
    // and the path.
    if let Some(memory) = &card.memory {
        return if memory.read {
            format!("memory used: {}", memory.title)
        } else {
            memory.line()
        };
    }
    let title = head_text(card, text);
    if title.is_empty() {
        return card.name.clone();
    }
    format!("{} {}", card.name, title)
}

/// The rows a renderer's body lays out to, from the display text alone: the collapsed head's
/// `· N lines` (ADR 0028) and the pairing's [`ToolCard::result_lines`].
///
/// Every built-in renderer emits exactly one row per line of the display text (`patch`'s
/// synthesized diff included), so the fold counts without building the rows.
pub fn body_line_count(text: &str) -> usize {
    if text.is_empty() {
        1
    } else {
        text.lines().count()
    }
}

/// The text a tool **result** entry displays: the tool's own text, except for `patch` (whose
/// card shows the replacement as a diff — the result text is only "replaced 1 occurrence") and
/// `write` (whose card shows the content it wrote). Synthesizing the display text here keeps the
/// one invariant the selection mapping and the search need: **the body's text is the entry's
/// text**.
pub fn result_display_text(card: &ToolCard, tool_text: &str) -> String {
    match card.name.as_str() {
        "patch" => patch_diff_text(card).unwrap_or_else(|| tool_text.to_owned()),
        "write" => string_field(&card.arguments, "content").unwrap_or_else(|| tool_text.to_owned()),
        _ => tool_text.to_owned(),
    }
}

/// `MCP and other tools`: the name and the arguments' JSON summary.
pub fn generic_head(card: &ToolCard, _text: &str) -> String {
    card.arguments_summary()
}

/// The command a shell card runs, verbatim (its own newlines kept).
fn shell_command(card: &ToolCard) -> String {
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

/// `shell`'s title: the command verbatim as `$ <command>` (its own newlines kept — a
/// multi-line command wraps, continuation rows from the body column).
pub fn shell_head(card: &ToolCard, _text: &str) -> String {
    let command = shell_command(card);
    if command.is_empty() {
        return card.arguments_summary();
    }
    format!("$ {command}")
}

/// `read`: the path and the line range the call asked for.
pub fn read_head(card: &ToolCard, _text: &str) -> String {
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

/// `patch`: the path the replacement is in. The `+N −M` counts are the meta's, not the
/// title's (ADR 0028).
pub fn patch_head(card: &ToolCard, _text: &str) -> String {
    string_field(&card.arguments, "path").unwrap_or_else(|| "<path>".into())
}

/// A `string` field of an argument object.
pub fn string_field(object: &Value, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// `write`: the path the content was written to. The `+N −M` counts are the meta's.
pub fn write_head(card: &ToolCard, _text: &str) -> String {
    string_field(&card.arguments, "path").unwrap_or_else(|| "<path>".into())
}

/// `grep`: the regex and the path it searched (`.` when the call named none).
pub fn grep_head(card: &ToolCard, _text: &str) -> String {
    let pattern = string_field(&card.arguments, "pattern").unwrap_or_default();
    match string_field(&card.arguments, "path") {
        Some(path) => format!("{pattern} {path}"),
        None => pattern,
    }
}

/// `glob`: the pattern it matched.
pub fn glob_head(card: &ToolCard, _text: &str) -> String {
    string_field(&card.arguments, "pattern").unwrap_or_default()
}

/// `ask_user`: how many questions the call asked (ADR 0024 Q3).
pub fn ask_head(card: &ToolCard, _text: &str) -> String {
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

/// A patch's body: one block per replaced range — the hunk header naming the range's start
/// lines in both files, then the removed lines and the added ones, each prefixed by the sign
/// the diff roles paint. There are no context lines: the call carries no surrounding text, and
/// the renderer must not pretend it does (ADR 0028 R4).
///
/// The hunks come from the result's structured `hunks`, and each hunk's `-`/`+` text is that
/// hunk's `edit`. A result that carries no `hunks` (an older journal entry), or a hunk whose
/// `edit` is missing or out of range, falls back to the call's edits in call order with no `@@`
/// line. A **failed** patch changed nothing, so it has no diff at all.
pub fn patch_diff_text(card: &ToolCard) -> Option<String> {
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
pub fn text_lines(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count()
    }
}

/// The call's edits in **call order**, each as its `(old, new)` text: the single `old`/`new`
/// pair, or one entry per item of the `edits` array. Empty when the call carries neither.
pub fn patch_call_edits(card: &ToolCard) -> Vec<(String, String)> {
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
pub struct PatchHunk {
    pub old_start: u64,
    pub old_lines: u64,
    pub new_start: u64,
    pub new_lines: u64,
    pub edit: Option<usize>,
}

/// The result's `hunks` in file order. Empty when the result carries none.
pub fn patch_hunks(card: &ToolCard) -> Vec<PatchHunk> {
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
pub fn compact_json(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Truncates on a char boundary, appending `…` when something was cut.
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
