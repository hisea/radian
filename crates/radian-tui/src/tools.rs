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

/// One display line of a card body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyLine {
    /// A line of the entry's own text. `source` is the byte range **in the entry's text**,
    /// so the selection mapping and the copy reach the real characters.
    Text { source: Range<usize>, warn: bool },
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
/// same string is the entry's copyable text and the displayed line.
pub fn head_text(card: &ToolCard) -> String {
    renderer_for(&card.name).head(card)
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
    let expanded = match fold {
        ToolFold::Expanded => true,
        // An error is never hidden (ADR 0028 R4).
        ToolFold::Errors => is_error,
        ToolFold::Collapsed => false,
    };
    if expanded {
        return lines;
    }
    let preview = preview_lines.max(1);
    if let Some(hint) = collapsed_hint {
        // A renderer that shows nothing when collapsed (read): one hint line.
        return vec![BodyLine::Hint(hint)];
    }
    let text_lines: Vec<BodyLine> = lines
        .iter()
        .filter(|line| matches!(line, BodyLine::Text { .. }))
        .cloned()
        .collect();
    if text_lines.len() <= preview {
        return lines;
    }
    let hidden = text_lines.len() - preview;
    let skip = hidden;
    let mut out = vec![BodyLine::Hint(format!("[{hidden} more lines]"))];
    out.extend(text_lines.into_iter().skip(skip));
    out
}

/// A tool's body renderer: its lines, and — when the tool prefers to show nothing while
/// collapsed (`read`) — the hint that replaces them.
type BodyFn = fn(&ToolCard, &str) -> (Vec<BodyLine>, Option<String>);

/// What a tool's renderer produces: the header line and the body renderer.
pub struct ToolRender {
    head: fn(&ToolCard) -> String,
    body: BodyFn,
}

impl ToolRender {
    /// The header line for a card (width-independent; the layout truncates it).
    pub fn head(&self, card: &ToolCard) -> String {
        (self.head)(card)
    }

    /// The body's full lines, plus the collapsed hint when the tool has one.
    pub fn body(&self, card: &ToolCard, text: &str) -> (Vec<BodyLine>, Option<String>) {
        (self.body)(card, text)
    }
}

/// The compile-time registry (ADR 0028 decision 5). The name is the model-facing tool
/// name; everything not listed — MCP tools included — takes [`generic`]. A name listed here
/// is never a panic: the fallback is the generic renderer, by construction.
pub fn renderer_for(name: &str) -> ToolRender {
    match base_name(name) {
        "shell" => ToolRender {
            head: shell_head,
            body: shell_body,
        },
        "read" => ToolRender {
            head: read_head,
            body: read_body,
        },
        _ => ToolRender {
            head: generic_head,
            body: generic_body,
        },
    }
}

/// The renderer's lookup key: an MCP tool may be namespaced (`server.tool` or
/// `mcp__server__tool`); the built-in names are matched on the last segment.
fn base_name(name: &str) -> &str {
    name.rsplit(['.', '_']).next().unwrap_or(name)
}

/// `MCP and other tools`: the name and the arguments' JSON summary.
fn generic_head(card: &ToolCard) -> String {
    format!("{} {}", card.name, card.arguments_summary())
}

/// The generic body: the structured output pretty-printed when the tool reported one, the
/// plain text otherwise. The pretty print is synthesized (the text is a different rendering),
/// so its lines carry no source range — the header and a text-backed body stay selectable,
/// a structured-only pretty print does not (reported).
fn generic_body(_card: &ToolCard, text: &str) -> (Vec<BodyLine>, Option<String>) {
    (text_body_lines(text, None), None)
}

/// A pure text body: every line of `text`, with the byte range of each.
fn text_body_lines(text: &str, warn_between: Option<(&str, &str)>) -> Vec<BodyLine> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    let mut warn = false;
    for line in text.split_inclusive('\n') {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let range = offset..offset + stripped.len();
        if let Some((start, end)) = warn_between {
            let trimmed = stripped.trim();
            if trimmed.starts_with(start) {
                warn = true;
            }
            if trimmed.starts_with(end) {
                warn = false;
            }
        }
        out.push(BodyLine::Text {
            source: range,
            warn,
        });
        offset += line.len();
    }
    // `split_inclusive` yields nothing for an empty text: one empty line, like a code block.
    if out.is_empty() {
        out.push(BodyLine::Text {
            source: 0..0,
            warn: false,
        });
    }
    out
}

/// `shell`: the command and the exit code.
fn shell_head(card: &ToolCard) -> String {
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
    (
        text_body_lines(text, Some(("--- stderr ---", "\u{0}never"))),
        None,
    )
}

/// `read`: the path and the line range the call asked for.
fn read_head(card: &ToolCard) -> String {
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
        text_body_lines(text, None),
        Some(format!("[read: {lines} lines]")),
    )
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DisplayModes {
    pub reasoning: ReasoningMode,
    pub tools: ToolDisplay,
}
