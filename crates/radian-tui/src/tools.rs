//! Tool cards (ADR 0028 R4): the data a transcript card shows, the per-tool renderers, and
//! the fold rules.
//!
//! **The data model** ([`ToolCard`]) is filled from the messages themselves, so the live
//! commit path and the resume replay path go through exactly the same code
//! ([`crate::app::App::push_message`]): the assistant message's `ToolCall` gives the name and
//! the argument JSON, the tool message's `ToolResult` gives `structured` and the outcome.
//! Nothing is journaled for timing (neither `AgentEvent::ToolStarted`/`ToolFinished` nor the
//! journal records a timestamp), so `duration_ms` has **no producer on either path** and every
//! renderer must render its absence.
//!
//! **The registry** is compile-time: [`renderer_for`] looks the tool name up in a static
//! table and falls back to [`generic`] (decision 5: extension renderers will hang off
//! the same table later). A renderer is a pure function — the card, the entry's text and the
//! width in, the header line and the body plan out; it never touches the terminal, the theme
//! or the clock.
//!
//! **The body plan** ([`BodyLine`]) keeps the mapping honest: a text line carries its byte
//! range *in the entry's text*, so the selection/copy mapping ([`crate::select`]) and the
//! search see the real source characters; only a synthesized hint (`[N more lines]`) has no
//! source.

use std::ops::Range;

use serde_json::Value;

use crate::blocks::ReasoningMode;

/// One tool call's card data. `structured` and `duration_ms` may legitimately be absent
/// (`None`): a tool that reports none, and the timing no source carries at all.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCard {
    /// The provider's call id: what pairs a tool message's result with its call, in the live
    /// path and in a resume replay alike.
    pub call_id: String,
    /// The model-facing tool name.
    pub name: String,
    /// The call's argument JSON (always present: the provider always sends arguments).
    pub arguments: Value,
    /// The result's machine-readable form, when the tool produced one.
    pub structured: Option<Value>,
    /// How long the call took. **No producer yet** (see the module docs); renderers omit it
    /// when it is `None`.
    pub duration_ms: Option<u64>,
}

impl ToolCard {
    pub fn new(call_id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            arguments,
            structured: None,
            duration_ms: None,
        }
    }

    /// The argument JSON's one-line summary, for the generic header (and the fallback for a
    /// tool whose renderer wants a field that is not there).
    pub fn arguments_summary(&self) -> String {
        let text = compact_json(&self.arguments);
        truncate_chars(&text, 120)
    }
}

/// How tool card bodies fold (ADR 0028 R4, `tui.display.tool_output`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolFold {
    /// Always the preview lines.
    Collapsed,
    /// Never fold.
    Expanded,
    /// Fold everything except a failed call's body (the default).
    #[default]
    Errors,
}

impl ToolFold {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "collapsed" => Some(Self::Collapsed),
            "expanded" => Some(Self::Expanded),
            "errors" => Some(Self::Errors),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Collapsed => "collapsed",
            Self::Expanded => "expanded",
            Self::Errors => "errors",
        }
    }
}

/// The role a body line renders with (ADR 0028 R4 + ADR 0029's diff roles). `Normal` is the
/// card body's own role; `Warn` is a warning channel (stderr); the `Diff*` roles are the
/// unified-diff line kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineRole {
    Normal,
    Warn,
    DiffAdded,
    DiffRemoved,
    DiffContext,
    DiffHunk,
}

/// One display line of a card body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyLine {
    /// A line of the entry's own text. `source` is the byte range **in the entry's text**,
    /// so the selection mapping and the copy reach the real characters.
    Text {
        source: Range<usize>,
        role: LineRole,
    },
    /// A synthesized hint (`[12 more lines]`, `[read: 40 lines, collapsed]`): no source, so
    /// it neither copies nor matches a search.
    Hint(String),
}

/// A rendered card: the header line (the caller lays it out and truncates it) and the body
/// plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub head: String,
    pub body: Vec<BodyLine>,
}

/// The header of a card, **without** width: the layout truncates it like a code line, so the
/// same string is the entry's copyable text and the displayed line. `text` is the tool's
/// result text when it is known (the `git_diff` counts are parsed from it; a call-time head
/// has none and stays terse - the pairing refreshes it when the result arrives).
pub fn head_text(card: &ToolCard, text: &str) -> String {
    renderer_for(&card.name).head(card, text)
}

/// The body plan for a card, given the entry's full text and the fold settings.
pub fn body_plan(
    card: &ToolCard,
    text: &str,
    is_error: bool,
    fold: ToolFold,
    preview_lines: usize,
) -> Vec<BodyLine> {
    let (lines, collapsed_hint) = renderer_for(&card.name).body(card, text);
    // An error's body is never hidden, in any mode (ADR 0028 R4).
    if is_error {
        return lines;
    }
    // A renderer whose body is "not shown by default" (`read`) collapses to its hint in both
    // folded modes; `expanded` shows its lines.
    let text_lines: Vec<BodyLine> = lines
        .iter()
        .filter(|line| matches!(line, BodyLine::Text { .. }))
        .cloned()
        .collect();
    match fold {
        ToolFold::Expanded => lines,
        // `collapsed` (ADR 0028 R4's interpretation): the header only, the body folded to one
        // line count hint.
        ToolFold::Collapsed => match collapsed_hint {
            Some(hint) => vec![BodyLine::Hint(hint)],
            None => vec![BodyLine::Hint(format!("[{} lines]", text_lines.len()))],
        },
        // `errors` (the default): the last `tool_preview_lines` lines.
        ToolFold::Errors => {
            if let Some(hint) = collapsed_hint {
                return vec![BodyLine::Hint(hint)];
            }
            let preview = preview_lines.max(1);
            if text_lines.len() <= preview {
                return lines;
            }
            let hidden = text_lines.len() - preview;
            let mut out = vec![BodyLine::Hint(format!("[{hidden} more lines]"))];
            out.extend(text_lines.into_iter().skip(hidden));
            out
        }
    }
}

/// A tool's body renderer: its lines, and — when the tool prefers to show nothing while
/// collapsed (`read`) — the hint that replaces them.
type BodyFn = fn(&ToolCard, &str) -> (Vec<BodyLine>, Option<String>);

/// What a tool's renderer produces: the header line and the body renderer.
pub struct ToolRender {
    head: fn(&ToolCard, &str) -> String,
    body: BodyFn,
}

impl ToolRender {
    /// The header line for a card (width-independent; the layout truncates it).
    pub fn head(&self, card: &ToolCard, text: &str) -> String {
        (self.head)(card, text)
    }

    /// The body's full lines, plus the collapsed hint when the tool has one.
    pub fn body(&self, card: &ToolCard, text: &str) -> (Vec<BodyLine>, Option<String>) {
        (self.body)(card, text)
    }
}

/// The compile-time registry (ADR 0028 decision 5). The match is on the **exact**
/// model-facing built-in name: an MCP tool is aliased `server__tool` (verified in
/// `agent-extensions`' `alias.rs`), so a server-provided `read` arrives as `fs__read` — its
/// arguments and output are not the built-in's, and it must take [`generic`] like every other
/// unknown tool. Everything not listed falls back to the generic renderer; the fallback is by
/// construction, never a panic.
pub fn renderer_for(name: &str) -> ToolRender {
    match name {
        "shell" => ToolRender {
            head: shell_head,
            body: shell_body,
        },
        "read" => ToolRender {
            head: read_head,
            body: read_body,
        },
        "patch" => ToolRender {
            head: patch_head,
            body: diff_body,
        },
        "write" => ToolRender {
            head: write_head,
            body: plain_body,
        },
        "git_diff" => ToolRender {
            head: git_diff_head,
            body: diff_body,
        },
        "git_status" => ToolRender {
            head: list_head,
            body: plain_body,
        },
        "list" => ToolRender {
            head: list_head,
            body: plain_body,
        },
        "ask_user" => ToolRender {
            head: ask_head,
            body: plain_body,
        },
        // The memory tools (ADR 0021): operation and record id, the tool's own record
        // summary as the body.
        "memory_search" => ToolRender {
            head: memory_head,
            body: plain_body,
        },
        "memory_get" => ToolRender {
            head: memory_head,
            body: plain_body,
        },
        "memory_save" => ToolRender {
            head: memory_head,
            body: plain_body,
        },
        "memory_revise" => ToolRender {
            head: memory_head,
            body: plain_body,
        },
        _ => ToolRender {
            head: generic_head,
            body: generic_body,
        },
    }
}

/// `MCP and other tools`: the name and the arguments' JSON summary.
fn generic_head(card: &ToolCard, _text: &str) -> String {
    format!("{} {}", card.name, card.arguments_summary())
}

/// The generic body: the structured output pretty-printed when the tool reported one, the
/// plain text otherwise. The pretty print is synthesized (the text is a different rendering),
/// so its lines carry no source range — the header and a text-backed body stay selectable,
/// a structured-only pretty print does not (reported).
fn generic_body(_card: &ToolCard, text: &str) -> (Vec<BodyLine>, Option<String>) {
    (text_body_lines(text, |_| LineRole::Normal), None)
}

/// A pure text body: every line of `text`, with the byte range of each. `role` classifies the
/// line (the diff renderers pass a classifier; everyone else `Normal`).
fn text_body_lines(text: &str, mut role: impl FnMut(&str) -> LineRole) -> Vec<BodyLine> {
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

/// The unified-diff line classifier (ADR 0028 R4): file headers and hunks are structure,
/// `+`/`-` are added/removed, everything else is context. File headers are checked first,
/// because `+++`/`---` also start with the add/remove markers.
fn diff_role(line: &str) -> LineRole {
    if line.starts_with("diff ")
        || line.starts_with("index ")
        || line.starts_with("+++")
        || line.starts_with("---")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
        || line.starts_with("similarity ")
        || line.starts_with("rename ")
    {
        return LineRole::DiffHunk;
    }
    if line.starts_with("@@") {
        return LineRole::DiffHunk;
    }
    if line.starts_with('+') {
        return LineRole::DiffAdded;
    }
    if line.starts_with('-') {
        return LineRole::DiffRemoved;
    }
    LineRole::DiffContext
}

/// `shell`: the command and the exit code.
fn shell_head(card: &ToolCard, _text: &str) -> String {
    let command = card
        .structured
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
        .unwrap_or_else(|| card.arguments_summary());
    let command = truncate_chars(&command.replace('\n', " "), 80);
    let exit = exit_code(card);
    match exit {
        Some(code) => format!("shell: {command} (exit {code})"),
        None if card.structured.is_some() => format!("shell: {command} (no exit code)"),
        None => format!("shell: {command}"),
    }
}

/// The exit code from the structured result (`shell`'s own JSON), when it has one.
fn exit_code(card: &ToolCard) -> Option<i64> {
    card.structured
        .as_ref()
        .and_then(|value| value.get("exit_code"))
        .and_then(Value::as_i64)
}

/// `shell`'s body: the output with the stderr section marked. The tool's **text** separates
/// the two streams (`--- stdout ---` / `--- stderr ---`, verified in `radian-coding`'s
/// `shell.rs`); the structured value carries byte counts only, so the marker is the source of
/// truth for the warning style.
fn shell_body(_card: &ToolCard, text: &str) -> (Vec<BodyLine>, Option<String>) {
    let mut in_stderr = false;
    (
        text_body_lines(text, move |line| {
            if line.trim() == "--- stderr ---" {
                in_stderr = true;
            }
            if in_stderr {
                LineRole::Warn
            } else {
                LineRole::Normal
            }
        }),
        None,
    )
}

/// `read`: the path and the line range the call asked for.
fn read_head(card: &ToolCard, _text: &str) -> String {
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
        (Some(offset), Some(limit)) => {
            format!("read: {path} (lines {}..{})", offset + 1, offset + limit)
        }
        (Some(offset), None) => format!("read: {path} (from line {})", offset + 1),
        (None, Some(limit)) => format!("read: {path} (first {limit} lines)"),
        (None, None) => format!("read: {path}"),
    }
}

/// `read`'s body: **collapsed by default** (ADR 0028 R4's table): the text is shown only when
/// the fold mode expands it. The line count comes from the entry text, so the hint is honest.
fn read_body(_card: &ToolCard, text: &str) -> (Vec<BodyLine>, Option<String>) {
    let lines = text.lines().count();
    (
        text_body_lines(text, |_| LineRole::Normal),
        Some(format!("[read: {lines} lines]")),
    )
}

/// `patch`: the path and the line counts of the replacement, computed from the call's
/// `old`/`new` (the result carries no diff of its own - verified in `radian-coding`).
fn patch_head(card: &ToolCard, _text: &str) -> String {
    let path = string_field(&card.arguments, "path").unwrap_or_else(|| "<path>".into());
    let (removed, added) = patch_line_counts(card);
    format!("patch: {path} +{added}/-{removed}")
}

/// The `+N`/`-M` of a patch: the line count of `new` and of `old`. An empty string counts as
/// zero lines, and a trailing newline does not add one (`a\n` is one line).
fn patch_line_counts(card: &ToolCard) -> (usize, usize) {
    let count = |value: Option<&Value>| -> usize {
        value
            .and_then(Value::as_str)
            .map(|text| {
                if text.is_empty() {
                    0
                } else {
                    text.lines().count()
                }
            })
            .unwrap_or(0)
    };
    let removed = count(card.arguments.get("old"));
    let added = count(card.arguments.get("new"));
    (removed, added)
}

/// A `string` field of an argument object.
fn string_field(object: &Value, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// `write`: the path, the byte count and the line count of the content.
fn write_head(card: &ToolCard, _text: &str) -> String {
    let path = string_field(&card.arguments, "path").unwrap_or_else(|| "<path>".into());
    let content = string_field(&card.arguments, "content").unwrap_or_default();
    let lines = if content.is_empty() {
        0
    } else {
        content.lines().count()
    };
    format!("write: {path} ({} bytes, {lines} lines)", content.len())
}

/// `git_diff`: the file count and the added/removed line counts, parsed from the diff text.
fn git_diff_head(_card: &ToolCard, text: &str) -> String {
    if text.trim().is_empty() {
        // No result text yet (the call-time head): the counts are not known, so they are not
        // claimed. The pairing refreshes this when the diff arrives.
        return "git_diff".to_string();
    }
    let (files, added, removed) = diff_counts(text);
    format!("git_diff: {files} files +{added}/-{removed}")
}

/// Counts a unified diff's files and `+`/`-` lines (the `git_diff` result text; `structured`
/// is `None` for that tool).
fn diff_counts(text: &str) -> (usize, usize, usize) {
    let mut files = 0usize;
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in text.lines() {
        if line.starts_with("diff ") {
            files += 1;
        } else if line.starts_with("@@") || line.starts_with("+++") || line.starts_with("---") {
            // Structural, not content.
        } else if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (files, added, removed)
}

/// `git_status` / `list`: the entry count.
fn list_head(card: &ToolCard, _text: &str) -> String {
    match card.name.as_str() {
        "git_status" => {
            let entries = card
                .structured
                .as_ref()
                .and_then(|value| value.get("entries"))
                .and_then(Value::as_u64);
            match entries {
                Some(n) => format!("git_status: {n} entries"),
                None => "git_status".to_string(),
            }
        }
        _ => {
            let entries = card
                .structured
                .as_ref()
                .and_then(|value| value.get("entries"))
                .and_then(Value::as_u64);
            let total = card
                .structured
                .as_ref()
                .and_then(|value| value.get("total_entries"))
                .and_then(Value::as_u64);
            let base = string_field(&card.arguments, "path").unwrap_or_else(|| ".".into());
            match (entries, total) {
                (Some(entries), Some(total)) if total > entries => {
                    format!("list: {base} ({entries} of {total} entries)")
                }
                (Some(entries), _) => format!("list: {base} ({entries} entries)"),
                _ => format!("list: {base}"),
            }
        }
    }
}

/// `ask_user`: how many questions the call asked (ADR 0024 Q3).
fn ask_head(card: &ToolCard, _text: &str) -> String {
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
        Some(1) => "ask_user: 1 question".to_string(),
        Some(n) => format!("ask_user: {n} questions"),
        None => "ask_user".to_string(),
    }
}

/// The memory tools: the operation and the record id (or the search query).
fn memory_head(card: &ToolCard, _text: &str) -> String {
    let operation = card.name.strip_prefix("memory_").unwrap_or(&card.name);
    let id = string_field(&card.arguments, "id")
        .or_else(|| string_field(&card.arguments, "key"))
        .or_else(|| string_field(&card.arguments, "query"));
    match id {
        Some(id) => format!("memory_{operation}: {id}"),
        None => format!("memory_{operation}"),
    }
}

/// The unified-diff body: the entry text's lines with their roles. Used by `patch` (whose
/// entry text **is** the synthesized `-old`/`+new` diff) and by `git_diff` (whose entry text
/// is git's own unified diff).
fn diff_body(_card: &ToolCard, text: &str) -> (Vec<BodyLine>, Option<String>) {
    (text_body_lines(text, diff_role), None)
}

/// A plain body: the entry text, every line on the card's own role.
fn plain_body(_card: &ToolCard, text: &str) -> (Vec<BodyLine>, Option<String>) {
    (text_body_lines(text, |_| LineRole::Normal), None)
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

/// A patch's body: the removed lines then the added ones, each prefixed by the marker the
/// diff roles paint. There are no context lines: the call carries no surrounding text, and
/// the renderer must not pretend it does (ADR 0028 R4).
fn patch_diff_text(card: &ToolCard) -> Option<String> {
    let old = string_field(&card.arguments, "old")?;
    let new = string_field(&card.arguments, "new")?;
    let mut out = String::new();
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
    Some(out)
}

/// A compact one-line JSON rendering, for headers.
fn compact_json(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Truncates on a char boundary, appending `…` when something was cut.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The fold mode and preview line count a frame lays tool bodies out with. Part of the layout
/// cache key (ADR 0028 R1: the fold state is a cache key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolDisplay {
    pub fold: ToolFold,
    pub preview_lines: usize,
}

impl Default for ToolDisplay {
    fn default() -> Self {
        Self {
            fold: ToolFold::Errors,
            preview_lines: 5,
        }
    }
}

/// Everything the layout cache key depends on besides the width and the text length.
///
/// `mermaid` (ADR 0028 R8) is here rather than beside the layout call: a rendered diagram is
/// part of what a laid-out line is, so toggling it must invalidate the same caches the folds
/// do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayModes {
    pub reasoning: ReasoningMode,
    pub tools: ToolDisplay,
    pub mermaid: bool,
}

impl Default for DisplayModes {
    fn default() -> Self {
        // The config default (`tui.display.mermaid`) is on, so a default set of modes draws
        // diagrams too.
        Self {
            reasoning: ReasoningMode::default(),
            tools: ToolDisplay::default(),
            mermaid: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn card(name: &str, arguments: Value) -> ToolCard {
        ToolCard::new("c1", name, arguments)
    }

    fn lines(plan: &[BodyLine]) -> Vec<String> {
        plan.iter()
            .map(|line| match line {
                BodyLine::Text { source, .. } => format!("text {source:?}"),
                BodyLine::Hint(hint) => hint.clone(),
            })
            .collect()
    }

    #[test]
    fn the_registry_falls_back_to_generic_for_an_unknown_tool() {
        let unknown = renderer_for("mcp__server__do_thing");
        let generic = renderer_for("some_other_tool");
        let card = card("mcp__server__do_thing", json!({"a": 1}));
        assert_eq!(unknown.head(&card, ""), generic.head(&card, ""));
        assert!(unknown.head(&card, "").contains("mcp__server__do_thing"));
    }

    /// An MCP tool is aliased `server__tool` (verified in `agent-extensions`' `alias.rs`); a
    /// server-provided `read`/`shell`/`list` is **not** the built-in tool, so it takes the
    /// generic renderer - its arguments and output format are its server's own (ADR 0028 R4's
    /// table: "MCP and other tools"). The reviewer's T3.
    #[test]
    fn an_mcp_alias_never_reaches_a_builtin_renderer() {
        let generic = renderer_for("something_unknown");
        for name in [
            "fs__read",
            "x__shell",
            "mcp__github__list",
            "server.read",
            "srv__patch",
        ] {
            let card = card(name, json!({"path": "a"}));
            let renderer = renderer_for(name);
            assert_eq!(
                renderer.head(&card, ""),
                generic.head(&card, ""),
                "{name} must be generic"
            );
            assert!(renderer.head(&card, "").starts_with(name), "{name}");
        }
        // The exact built-in names keep their renderers.
        assert!(
            renderer_for("read")
                .head(&card("read", json!({"path": "a"})), "")
                .starts_with("read: a")
        );
        assert!(
            renderer_for("shell")
                .head(&card("shell", json!({"command": "ls"})), "")
                .starts_with("shell: ls")
        );
        assert!(
            renderer_for("memory_get")
                .head(&card("memory_get", json!({"id": "m1"})), "")
                .starts_with("memory_get: m1")
        );
    }

    #[test]
    fn the_shell_head_carries_the_command_and_the_exit_code() {
        let mut card = card("shell", json!({"command": "ls -la /tmp"}));
        // No structured value yet (or ever): the exit code is simply not known.
        assert_eq!(shell_head(&card, ""), "shell: ls -la /tmp");
        card.structured = Some(json!({"command": "ls -la /tmp", "exit_code": 2}));
        assert_eq!(shell_head(&card, ""), "shell: ls -la /tmp (exit 2)");
        // A signal death (no code) from the structured value.
        card.structured = Some(json!({"command": "sleep 9", "exit_code": null}));
        assert_eq!(shell_head(&card, ""), "shell: sleep 9 (no exit code)");
        // A long command is cut to one line.
        let mut long = ToolCard::new("c1", "shell", json!({"command": "x".repeat(200)}));
        long.structured = Some(json!({"exit_code": 0}));
        let head = shell_head(&long, "");
        assert!(head.ends_with("(exit 0)"));
        assert!(head.chars().count() < 100, "{head}");
    }

    #[test]
    fn the_read_head_shows_the_path_and_the_line_range() {
        assert_eq!(
            read_head(&card("read", json!({"path": "a.rs"})), ""),
            "read: a.rs"
        );
        assert_eq!(
            read_head(
                &card("read", json!({"path": "a.rs", "offset": 40, "limit": 10})),
                ""
            ),
            "read: a.rs (lines 41..50)"
        );
        assert_eq!(
            read_head(&card("read", json!({"path": "a.rs", "offset": 40})), ""),
            "read: a.rs (from line 41)"
        );
        assert_eq!(
            read_head(&card("read", json!({"path": "a.rs", "limit": 5})), ""),
            "read: a.rs (first 5 lines)"
        );
    }

    #[test]
    fn the_fold_modes_decide_the_body() {
        let text = "one\ntwo\nthree\nfour\nfive\nsix\nseven";
        let card = card("generic_tool", json!({}));
        // The three modes differ for a successful card: everything, the last preview lines
        // behind a hint, or the header with the body folded to its line count.
        assert_eq!(
            body_plan(&card, text, false, ToolFold::Expanded, 3).len(),
            7
        );
        let errors = body_plan(&card, text, false, ToolFold::Errors, 3);
        assert_eq!(errors.len(), 4);
        assert_eq!(lines(&errors)[0], "[4 more lines]");
        assert!(matches!(
            &errors[1],
            BodyLine::Text { source, .. } if source == &(text.find("five").unwrap()..text.find("five").unwrap() + 4)
        ));
        let collapsed = body_plan(&card, text, false, ToolFold::Collapsed, 3);
        assert_eq!(lines(&collapsed), vec!["[7 lines]"]);
        // A failure never folds, in any mode.
        for fold in [ToolFold::Expanded, ToolFold::Errors, ToolFold::Collapsed] {
            assert_eq!(body_plan(&card, text, true, fold, 3).len(), 7, "{fold:?}");
        }
        // A body that fits the preview is left whole under `errors`...
        assert_eq!(
            body_plan(&card, "a\nb", false, ToolFold::Errors, 5).len(),
            2
        );
        // ...and still folds to the hint under `collapsed`.
        assert_eq!(
            body_plan(&card, "a\nb", false, ToolFold::Collapsed, 5).len(),
            1
        );
    }

    #[test]
    fn the_read_body_collapses_to_a_hint() {
        let card = card("read", json!({"path": "a.rs"}));
        let (body, hint) = read_body(&card, "l1\nl2\nl3");
        assert_eq!(body.len(), 3);
        assert_eq!(hint.as_deref(), Some("[read: 3 lines]"));
        let collapsed = body_plan(&card, "l1\nl2\nl3", false, ToolFold::Collapsed, 5);
        assert_eq!(lines(&collapsed), vec!["[read: 3 lines]"]);
        // `errors` shows the same hint (a read body is never previewed).
        let errors = body_plan(&card, "l1\nl2\nl3", false, ToolFold::Errors, 5);
        assert_eq!(lines(&errors), vec!["[read: 3 lines]"]);
        // Expanded shows the lines.
        assert_eq!(
            body_plan(&card, "l1\nl2\nl3", false, ToolFold::Expanded, 5).len(),
            3
        );
    }

    #[test]
    fn the_shell_body_marks_the_stderr_section() {
        let text = "$ run\nexit code: 1\n--- stdout ---\nout\n--- stderr ---\nboom\nmore";
        let card = card("shell", json!({"command": "run"}));
        let (body, _) = shell_body(&card, text);
        let warned: Vec<usize> = body
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                matches!(
                    line,
                    BodyLine::Text {
                        role: LineRole::Warn,
                        ..
                    }
                )
            })
            .map(|(i, _)| i)
            .collect();
        // The marker line itself and the two stderr lines.
        assert_eq!(warned, vec![4, 5, 6]);
    }

    #[test]
    fn the_patch_head_counts_the_replacement_lines() {
        let patch = card(
            "patch",
            json!({"path": "src/日本.rs", "old": "one\ntwo\n", "new": "one\ntwo\nthree\nfour"}),
        );
        assert_eq!(patch_head(&patch, ""), "patch: src/日本.rs +4/-2");
        // An empty side counts zero; a trailing newline does not add a line.
        let empty = card("patch", json!({"path": "a", "old": "", "new": "x\n"}));
        assert_eq!(patch_head(&empty, ""), "patch: a +1/-0");
    }

    #[test]
    fn the_write_head_counts_bytes_and_lines() {
        let write = card(
            "write",
            json!({"path": "a/日本.txt", "content": "l1\nl2\nl3"}),
        );
        assert_eq!(
            write_head(&write, ""),
            "write: a/日本.txt (8 bytes, 3 lines)"
        );
        let empty = card("write", json!({"path": "e", "content": ""}));
        assert_eq!(write_head(&empty, ""), "write: e (0 bytes, 0 lines)");
    }

    #[test]
    fn the_git_diff_head_counts_from_the_text() {
        let text =
            "diff --git a/x b/x\nindex 1..2\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n+another";
        assert_eq!(
            git_diff_head(&card("git_diff", json!({})), text),
            "git_diff: 1 files +2/-1"
        );
        // No text yet (the call-time head): the counts are not claimed.
        assert_eq!(git_diff_head(&card("git_diff", json!({})), ""), "git_diff");
    }

    #[test]
    fn the_diff_role_classifies_every_line_kind() {
        assert_eq!(diff_role("diff --git a/x b/x"), LineRole::DiffHunk);
        assert_eq!(diff_role("index 111..222 100644"), LineRole::DiffHunk);
        assert_eq!(diff_role("--- a/x"), LineRole::DiffHunk);
        assert_eq!(diff_role("+++ b/x"), LineRole::DiffHunk);
        assert_eq!(diff_role("@@ -1,3 +1,4 @@ fn main"), LineRole::DiffHunk);
        assert_eq!(diff_role("+added"), LineRole::DiffAdded);
        assert_eq!(diff_role("-removed"), LineRole::DiffRemoved);
        assert_eq!(diff_role(" context"), LineRole::DiffContext);
    }

    #[test]
    fn the_list_git_status_ask_and_memory_heads() {
        let list = card("list", json!({"path": "src"}));
        let mut list = list;
        list.structured = Some(json!({"path": "src", "entries": 3, "total_entries": 9}));
        assert_eq!(list_head(&list, ""), "list: src (3 of 9 entries)");
        let status = card("git_status", json!({}));
        assert_eq!(list_head(&status, ""), "git_status");
        let mut status = status;
        status.structured = Some(json!({"entries": 2}));
        assert_eq!(list_head(&status, ""), "git_status: 2 entries");
        let ask = card(
            "ask_user",
            json!({"questions": [{"id": "q1"}, {"id": "q2"}]}),
        );
        assert_eq!(ask_head(&ask, ""), "ask_user: 2 questions");
        let ask1 = card("ask_user", json!({"questions": [{"id": "q1"}]}));
        assert_eq!(ask_head(&ask1, ""), "ask_user: 1 question");
        assert_eq!(
            memory_head(&card("memory_get", json!({"id": "mem-7"})), ""),
            "memory_get: mem-7"
        );
        assert_eq!(
            memory_head(&card("memory_search", json!({"query": "auth flow"})), ""),
            "memory_search: auth flow"
        );
        assert_eq!(
            memory_head(&card("memory_revise", json!({"id": "mem-9"})), ""),
            "memory_revise: mem-9"
        );
    }

    #[test]
    fn a_patch_body_is_the_entry_text_with_diff_roles() {
        // The entry text a patch result displays is the synthesized `-old`/`+new` diff.
        let mut patch = card(
            "patch",
            json!({"path": "a", "old": "old one\nold two\n", "new": "new one\n"}),
        );
        patch.structured = Some(json!({"path": "a", "replaced": 1}));
        let text = result_display_text(&patch, "replaced 1 occurrence in a");
        assert_eq!(text, "-old one\n-old two\n+new one\n");
        let (body, _) = diff_body(&patch, &text);
        let roles: Vec<LineRole> = body
            .iter()
            .map(|line| match line {
                BodyLine::Text { role, .. } => *role,
                BodyLine::Hint(_) => panic!("no hint"),
            })
            .collect();
        assert_eq!(
            roles,
            vec![
                LineRole::DiffRemoved,
                LineRole::DiffRemoved,
                LineRole::DiffAdded
            ]
        );
    }

    #[test]
    fn a_write_body_is_the_content() {
        let write = card("write", json!({"path": "a", "content": "l1\nl2"}));
        assert_eq!(result_display_text(&write, "wrote 5 bytes to a"), "l1\nl2");
        // Every other tool keeps its own result text.
        let read = card("read", json!({"path": "a"}));
        assert_eq!(result_display_text(&read, "the file"), "the file");
    }

    #[test]
    fn an_empty_body_is_one_empty_line() {
        let (body, _) = generic_body(&card("t", json!({})), "");
        assert_eq!(body.len(), 1);
        assert_eq!(lines(&body), vec!["text 0..0".to_string()]);
    }

    #[test]
    fn the_fold_modes_round_trip_through_their_spellings() {
        for mode in [ToolFold::Collapsed, ToolFold::Expanded, ToolFold::Errors] {
            assert_eq!(ToolFold::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(ToolFold::default(), ToolFold::Errors);
        assert_eq!(ToolFold::parse("sometimes"), None);
    }
}
