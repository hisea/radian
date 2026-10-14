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
    // An error is never hidden, in any mode (ADR 0028 R4).
    let expanded = is_error
        || match fold {
            ToolFold::Expanded => true,
            ToolFold::Errors => false,
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
        assert_eq!(unknown.head(&card), generic.head(&card));
        assert!(unknown.head(&card).contains("mcp__server__do_thing"));
        // A built-in name that is a suffix of a namespaced one still matches its renderer.
        assert!(
            renderer_for("server.shell")
                .head(&card)
                .starts_with("shell:")
        );
    }

    #[test]
    fn the_shell_head_carries_the_command_and_the_exit_code() {
        let mut card = card("shell", json!({"command": "ls -la /tmp"}));
        // No structured value yet (or ever): the exit code is simply not known.
        assert_eq!(shell_head(&card), "shell: ls -la /tmp");
        card.structured = Some(json!({"command": "ls -la /tmp", "exit_code": 2}));
        assert_eq!(shell_head(&card), "shell: ls -la /tmp (exit 2)");
        // A signal death (no code) from the structured value.
        card.structured = Some(json!({"command": "sleep 9", "exit_code": null}));
        assert_eq!(shell_head(&card), "shell: sleep 9 (no exit code)");
        // A long command is cut to one line.
        let mut long = ToolCard::new("c1", "shell", json!({"command": "x".repeat(200)}));
        long.structured = Some(json!({"exit_code": 0}));
        let head = shell_head(&long);
        assert!(head.ends_with("(exit 0)"));
        assert!(head.chars().count() < 100, "{head}");
    }

    #[test]
    fn the_read_head_shows_the_path_and_the_line_range() {
        assert_eq!(
            read_head(&card("read", json!({"path": "a.rs"}))),
            "read: a.rs"
        );
        assert_eq!(
            read_head(&card(
                "read",
                json!({"path": "a.rs", "offset": 40, "limit": 10})
            )),
            "read: a.rs (lines 41..50)"
        );
        assert_eq!(
            read_head(&card("read", json!({"path": "a.rs", "offset": 40}))),
            "read: a.rs (from line 41)"
        );
        assert_eq!(
            read_head(&card("read", json!({"path": "a.rs", "limit": 5}))),
            "read: a.rs (first 5 lines)"
        );
    }

    #[test]
    fn the_fold_modes_decide_the_body() {
        let text = "one\ntwo\nthree\nfour\nfive\nsix\nseven";
        let card = card("generic_tool", json!({}));
        let expanded = body_plan(&card, text, false, ToolFold::Expanded, 5);
        assert_eq!(expanded.len(), 7);
        // collapsed: a hint plus the LAST preview lines.
        let collapsed = body_plan(&card, text, false, ToolFold::Collapsed, 3);
        assert_eq!(collapsed.len(), 4);
        assert_eq!(lines(&collapsed)[0], "[4 more lines]");
        assert!(matches!(
            &collapsed[1],
            BodyLine::Text { source, .. } if source == &(text.find("five").unwrap()..text.find("five").unwrap() + 4)
        ));
        // errors: folded unless the call failed; a failure never folds, in any mode.
        assert_eq!(body_plan(&card, text, false, ToolFold::Errors, 3).len(), 4);
        assert_eq!(body_plan(&card, text, true, ToolFold::Errors, 3).len(), 7);
        assert_eq!(
            body_plan(&card, text, false, ToolFold::Collapsed, 3).len(),
            4
        );
        assert_eq!(
            body_plan(&card, text, true, ToolFold::Collapsed, 3).len(),
            7
        );
        // A body that already fits the preview is left whole.
        assert_eq!(
            body_plan(&card, "a\nb", false, ToolFold::Collapsed, 5).len(),
            2
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
            .filter(|(_, line)| matches!(line, BodyLine::Text { warn: true, .. }))
            .map(|(i, _)| i)
            .collect();
        // The marker line itself and the two stderr lines.
        assert_eq!(warned, vec![4, 5, 6]);
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
