//! Tool blocks (ADR 0028): the data a transcript block shows, the
//! per-tool renderers, and the fold rules.
//!
//! **The data model** ([`ToolCard`]) is filled from the messages themselves, so the live
//! commit path and the resume replay path go through exactly the same code
//! ([`crate::app::App::push_message`]): the assistant message's `ToolCall` gives the name and
//! the argument JSON, the tool message's `ToolResult` gives `structured` and the outcome.
//! Nothing is journaled for timing (neither `AgentEvent::ToolStarted`/`ToolFinished` nor the
//! journal records a timestamp), so `duration_ms` has **no producer on either path** — a
//! renderer shows a duration only from a tool's own `structured` output (`shell` writes
//! `duration_ms`) or from [`ToolCard::duration_ms`] when a caller sets it.
//!
//! **The registry** is compile-time: [`renderer_for`] looks the tool name up in a static
//! table and falls back to [`generic`] (decision 5: extension renderers will hang off
//! the same table later). A renderer is a pure function — the card, the entry's text and the
//! width in, the title line and the body plan out; it never touches the terminal, the theme
//! or the clock.
//!
//! **The title and the meta.** A renderer's head function returns the **title** only — the
//! call's own summary (`$ <command>`, `path 100-160`); the block's head line is
//! `• <tool name> <title> · <meta> <status>` and the name, the meta ([`meta_parts`]) and the
//! status glyph are the block layer's (ADR 0028).
//!
//! **The body plan** ([`BodyLine`]) keeps the mapping honest: a text line carries its byte
//! range *in the entry's text*, so the selection/copy mapping ([`crate::select`]) and the
//! search see the real source characters; a synthesized cell (a marker, a diff gutter, the
//! meta) has no source and never copies.

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
    /// when it is `None`. A shell result's structured output carries its own `duration_ms`,
    /// which the meta falls back to.
    pub duration_ms: Option<u64>,
    /// The paired result's body line count, once it has arrived (`None` while the call runs):
    /// the collapsed fold's `· N lines` (ADR 0028). The head lives on the
    /// **call** entry while the body lives on the result's, so the call entry can only know the
    /// count through this field, which the pairing fills ([`crate::app::App::push_message`]).
    pub result_lines: Option<usize>,
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
/// block body's own role; `Warn` is a warning channel (stderr); the `Diff*` roles are the
/// unified-diff line kinds; `Frame` is the structural level (ADR 0029).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineRole {
    Normal,
    Warn,
    DiffAdded,
    DiffRemoved,
    DiffContext,
    DiffHunk,
    /// A structural row (ADR 0029's `frame`): the diff's `@@` headers and file
    /// headers, which are information about the diff, not content in it.
    Frame,
    /// The `ask_user` block's prompt and title (ADR 0024 R1): the emphasis role.
    Accent,
    /// A weak line (an option description, a note).
    Muted,
    /// A structural line (the box borders, a labeled divider).
    Dim,
    /// A heading: the accent, bold.
    Heading,
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
    /// A unified-diff row (ADR 0028): a synthesized line-number gutter,
    /// the sign, and the row's own code. One row, three roles — see [`DiffRole`].
    Diff(DiffRow),
}

impl BodyLine {
    /// Whether the line draws content (rather than a synthesized hint): the fold's line count
    /// and the block's "how many lines are folded away" both ask this.
    pub fn is_content(&self) -> bool {
        matches!(self, Self::Text { .. } | Self::Diff(_))
    }
}

/// Which side of a unified diff a row is on (ADR 0028).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Added,
    Removed,
    Context,
}

impl DiffKind {
    /// The sign the row carries in the entry's text, which the row draws in its sign column.
    pub fn sign(self) -> char {
        match self {
            Self::Added => '+',
            Self::Removed => '-',
            Self::Context => ' ',
        }
    }

    /// The role the row's **code** takes when it is not highlighted: a `+`/`-` row's code is
    /// the file's own text (the sign carries the diff colour), a context row's is the context
    /// colour (ADR 0028).
    pub fn code_role(self) -> DiffRole {
        match self {
            Self::Added | Self::Removed => DiffRole::Code,
            Self::Context => DiffRole::Context,
        }
    }

    /// The role the row's sign takes.
    pub fn sign_role(self) -> DiffRole {
        match self {
            Self::Added => DiffRole::Added,
            Self::Removed => DiffRole::Removed,
            Self::Context => DiffRole::Context,
        }
    }
}

/// One diff row's data. The number is synthesized (`number` is not part of the entry's text),
/// while `source` **is**: the copy of a row is the row's own text, the sign and the code,
/// without the gutter — the rule the entry-text model has always had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    /// The 1-based line to draw right-aligned in the 4-cell gutter (`diff_line_number`).
    /// `None` keeps the sign column only: the numberless form a diff without start lines gets
    /// (an older journal entry, an MCP tool's text).
    pub number: Option<u64>,
    pub kind: DiffKind,
    /// The row's bytes in the entry's display text: the sign, then the code.
    pub source: Range<usize>,
    /// The language the code highlights with — the file's suffix through
    /// [`crate::highlight::language_for_path`] — or `None` when the path names no language we
    /// highlight (the code then draws unhighlighted).
    pub lang: Option<&'static str>,
}

/// The role one **piece** of a diff row renders with. A diff row carries three at once — the
/// synthesized line number, the sign and the code — while [`LineRole`] is a whole line's role,
/// so the diff's parts are per-piece roles like the `ask_user` block's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRole {
    /// The 4-cell line-number gutter: `diff_line_number`, one level weaker than the code. It is
    /// synthesized: drawn, never copied and never searchable.
    Number,
    /// The `+` sign (`diff_added`).
    Added,
    /// The `-` sign (`diff_removed`).
    Removed,
    /// A context row's sign or code (`diff_context`).
    Context,
    /// A `+`/`-` row's own code, unhighlighted: the text colour, so the sign stays the only
    /// coloured part of the row.
    Code,
    /// A highlighted piece of the code: the same scope→role table a fenced code block uses
    /// (ADR 0028 R2).
    Syntax(crate::highlight::SyntaxRole),
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

/// A tool body's plan: the rows to draw, and how many content rows the fold hides (the
/// `· N lines` the head's meta gains under `collapsed`, ADR 0028).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BodyPlan {
    pub lines: Vec<BodyLine>,
    pub hidden: usize,
}

/// The body plan for a block, given the entry's full text and the fold settings.
///
/// The three folds (ADR 0028 R4's interpretation, revision 2 decision 5): `expanded` shows
/// every row, `errors` (the default) a failed call in full and the last `tool_preview_lines`
/// rows of a successful one, `collapsed` nothing at all (the head's meta then carries
/// `· N lines`). A **small** tool ([`ToolRender::compact`]) has no folded body either way: its
/// successful call is one head line, and only an error or `expanded` gives it rows.
pub fn body_plan(
    card: &ToolCard,
    text: &str,
    is_error: bool,
    fold: ToolFold,
    preview_lines: usize,
) -> BodyPlan {
    let renderer = renderer_for(&card.name);
    let lines = renderer.body(card, text);
    let content = lines.iter().filter(|line| line.is_content()).count();
    // An error's body is never hidden, in any mode (ADR 0028 R4).
    if is_error || fold == ToolFold::Expanded {
        return BodyPlan { lines, hidden: 0 };
    }
    // A small tool's successful call is one head line (ADR 0028).
    if renderer.compact || fold == ToolFold::Collapsed {
        return BodyPlan {
            lines: Vec::new(),
            hidden: content,
        };
    }
    // `errors` (the default): the last `tool_preview_lines` rows.
    let preview = preview_lines.max(1);
    if content <= preview {
        return BodyPlan { lines, hidden: 0 };
    }
    let hidden = content - preview;
    let shown = lines
        .into_iter()
        .filter(|line| line.is_content())
        .skip(hidden)
        .collect();
    BodyPlan {
        lines: shown,
        hidden,
    }
}

/// A tool's body renderer: the lines its output lays out to.
type BodyFn = fn(&ToolCard, &str) -> Vec<BodyLine>;

/// What a tool's renderer produces: the title line and the body renderer.
pub struct ToolRender {
    head: fn(&ToolCard, &str) -> String,
    body: BodyFn,
    /// A **small** tool (ADR 0028, kept by revision 5's decision 1):
    /// `read`, `grep`, `glob`, `list` and any other whose body is not worth showing by
    /// default. A successful call draws its head line only; an error or `Ctrl-O` gives it
    /// rows.
    pub compact: bool,
}

impl ToolRender {
    /// The title line for a card (width-independent; the layout truncates it).
    pub fn head(&self, card: &ToolCard, text: &str) -> String {
        (self.head)(card, text)
    }

    /// The body's full lines.
    pub fn body(&self, card: &ToolCard, text: &str) -> Vec<BodyLine> {
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
            compact: false,
        },
        "read" => ToolRender {
            head: read_head,
            body: read_body,
            compact: true,
        },
        "patch" => ToolRender {
            head: patch_head,
            body: diff_body,
            compact: false,
        },
        "write" => ToolRender {
            head: write_head,
            body: plain_body,
            compact: false,
        },
        "git_diff" => ToolRender {
            head: git_diff_head,
            body: diff_body,
            compact: false,
        },
        "git_status" => ToolRender {
            head: list_head,
            body: plain_body,
            compact: false,
        },
        "list" => ToolRender {
            head: list_head,
            body: list_body,
            compact: true,
        },
        // `grep` / `glob`: the search tools, whose successful call is one line naming what was
        // searched (ADR 0028's small tools).
        "grep" => ToolRender {
            head: grep_head,
            body: plain_body,
            compact: true,
        },
        "glob" => ToolRender {
            head: glob_head,
            body: plain_body,
            compact: true,
        },
        "ask_user" => ToolRender {
            head: ask_head,
            body: plain_body,
            compact: false,
        },
        // The memory tools (ADR 0021): operation and record id, the tool's own record
        // summary as the body.
        "memory_search" => ToolRender {
            head: memory_head,
            body: plain_body,
            compact: false,
        },
        "memory_get" => ToolRender {
            head: memory_head,
            body: plain_body,
            compact: false,
        },
        "memory_save" => ToolRender {
            head: memory_head,
            body: plain_body,
            compact: false,
        },
        "memory_revise" => ToolRender {
            head: memory_head,
            body: plain_body,
            compact: false,
        },
        _ => ToolRender {
            head: generic_head,
            body: generic_body,
            compact: false,
        },
    }
}

/// `MCP and other tools`: the name and the arguments' JSON summary.
fn generic_head(card: &ToolCard, _text: &str) -> String {
    card.arguments_summary()
}

/// The generic body: the structured output pretty-printed when the tool reported one, the
/// plain text otherwise. The pretty print is synthesized (the text is a different rendering),
/// so its lines carry no source range — the title and a text-backed body stay selectable,
/// a structured-only pretty print does not (reported).
fn generic_body(_card: &ToolCard, text: &str) -> Vec<BodyLine> {
    text_body_lines(text, |_| LineRole::Normal)
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

/// The unified-diff line classifier for a row the renderer draws **without** line numbers (the
/// fallback: an older journal entry or an MCP tool's text): structure is weak, `+`/`-` carry the
/// diff roles, everything else is context. File headers are checked first, because `+++`/`---`
/// also start with the add/remove markers.
fn diff_role(line: &str) -> LineRole {
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
fn diff_structure(line: &str) -> bool {
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
fn diff_kind(line: &str) -> Option<DiffKind> {
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
fn parse_hunk_header(line: &str) -> Option<(u64, u64)> {
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
fn file_header_path(line: &str) -> Option<&str> {
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
fn call_path(card: &ToolCard) -> Option<String> {
    string_field(&card.arguments, "path").or_else(|| {
        card.structured
            .as_ref()
            .and_then(|value| string_field(value, "path"))
    })
}

/// `shell`'s title: the command verbatim as `$ <command>` (its own newlines kept — a
/// multi-line command wraps, continuation rows from the body column).
fn shell_head(card: &ToolCard, _text: &str) -> String {
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
/// synthesized diff and `list`'s table included), which `the_line_count_mirrors_every_renderer`
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
fn duration_text(ms: u64) -> String {
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

/// The `+N −M` a `patch` or `write` head shows: `patch` counts the call's `old`/`new` lines,
/// `write` the content it wrote (and the file's previous line count when the result carries
/// one, `git_diff`-style diffs being slice ④'s).
fn diff_meta(card: &ToolCard) -> Option<String> {
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
    match card.name.as_str() {
        "patch" => {
            let removed = count(card.arguments.get("old"));
            let added = count(card.arguments.get("new"));
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
fn shell_body(_card: &ToolCard, text: &str) -> Vec<BodyLine> {
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
        (Some(offset), Some(limit)) => format!("{path} {}-{}", offset + 1, offset + limit),
        (Some(offset), None) => format!("{path} {}-", offset + 1),
        (None, Some(limit)) => format!("{path} 1-{limit}"),
        (None, None) => path.to_owned(),
    }
}

/// `read`'s body: the text, shown only when the fold mode expands it (`read` is a small tool:
/// a successful call is one line, ADR 0028).
fn read_body(_card: &ToolCard, text: &str) -> Vec<BodyLine> {
    text_body_lines(text, |_| LineRole::Normal)
}

/// `patch`: the path the replacement is in. The `+N −M` counts are the meta's
/// ([`meta_parts`]), not the title's (ADR 0028).
fn patch_head(card: &ToolCard, _text: &str) -> String {
    string_field(&card.arguments, "path").unwrap_or_else(|| "<path>".into())
}

/// A `string` field of an argument object.
fn string_field(object: &Value, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// `write`: the path the content was written to. The `+N −M` counts are the meta's.
fn write_head(card: &ToolCard, _text: &str) -> String {
    string_field(&card.arguments, "path").unwrap_or_else(|| "<path>".into())
}

/// `git_diff`: the file count and the added/removed line counts, parsed from the diff text
/// (`git_diff` reports no structured counts, so they stay in the title).
fn git_diff_head(_card: &ToolCard, text: &str) -> String {
    if text.trim().is_empty() {
        // No result text yet (the call-time head): the counts are not known, so they are not
        // claimed. The pairing refreshes this when the diff arrives.
        return String::new();
    }
    let (files, added, removed) = diff_counts(text);
    format!("{files} files +{added} −{removed}")
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
            let entries = entry_counts(card).0;
            match entries {
                Some(n) => format!("{n} entries"),
                None => String::new(),
            }
        }
        _ => {
            let (entries, total) = entry_counts(card);
            let base = string_field(&card.arguments, "path").unwrap_or_else(|| ".".into());
            match (entries, total) {
                (Some(entries), Some(total)) if total > entries => {
                    format!("{base} ({entries} of {total} entries)")
                }
                (Some(entries), _) => format!("{base} ({entries} entries)"),
                _ => base,
            }
        }
    }
}

/// `(entries, total_entries)` from the result's structured output.
fn entry_counts(card: &ToolCard) -> (Option<u64>, Option<u64>) {
    let field = |key: &str| {
        card.structured
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(Value::as_u64)
    };
    (field("entries"), field("total_entries"))
}

/// `grep`: the regex and the path it searched (`.` when the call named none).
fn grep_head(card: &ToolCard, _text: &str) -> String {
    let pattern = string_field(&card.arguments, "pattern").unwrap_or_default();
    match string_field(&card.arguments, "path") {
        Some(path) => format!("{pattern} {path}"),
        None => pattern,
    }
}

/// `glob`: the pattern it matched.
fn glob_head(card: &ToolCard, _text: &str) -> String {
    string_field(&card.arguments, "pattern").unwrap_or_default()
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
        Some(1) => "1 question".to_string(),
        Some(n) => format!("{n} questions"),
        None => String::new(),
    }
}

/// The memory tools: the record id (or the search query) as the title.
fn memory_head(card: &ToolCard, _text: &str) -> String {
    string_field(&card.arguments, "id")
        .or_else(|| string_field(&card.arguments, "key"))
        .or_else(|| string_field(&card.arguments, "query"))
        .unwrap_or_default()
}

/// The unified-diff body (ADR 0028): one row per line of the entry's
/// text. Used by `patch` (whose entry text **is** the synthesized `@@`/`-old`/`+new` diff) and
/// by `git_diff` (whose entry text is git's own unified diff).
///
/// A hunk header sets the old and new line counters, and every row under it carries its own
/// file's number: `+` and context rows the **new** file's, `-` rows the **old** file's (the
/// unified rule). A diff whose text carries no `@@` header keeps today's numberless form — the
/// sign column, no gutter (`DiffRow::number` stays `None` → the layout draws no gutter at all).
fn diff_body(card: &ToolCard, text: &str) -> Vec<BodyLine> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    // The language comes from the file's own path: `patch` and `write` name it in the call,
    // `git_diff`'s text names it per file (`+++ b/…`).
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
fn plain_body(_card: &ToolCard, text: &str) -> Vec<BodyLine> {
    text_body_lines(text, |_| LineRole::Normal)
}

/// `list`'s body: the aligned table [`result_display_text`] built for the entry, one line per
/// entry. (The table **is** the entry's text, so a copy yields the table as shown; the raw
/// `kind\tsize\tname` text stays in the journal.)
fn list_body(_card: &ToolCard, text: &str) -> Vec<BodyLine> {
    text_body_lines(text, |_| LineRole::Normal)
}

/// The `list` result as an aligned table: a short kind marker, the size right-aligned in a
/// human-readable unit, the name left-aligned, and the directories first. The tool's own text
/// is `kind\tsize\tname` per line (`radian-coding`'s `list.rs`); a line that does not have
/// that shape is passed through unchanged, so an unexpected output still shows something.
fn list_table(tool_text: &str) -> String {
    struct Row {
        kind: &'static str,
        size: u64,
        name: String,
    }
    let marker = |kind: &str| match kind {
        "dir" => "d",
        "file" => "f",
        "symlink" => "l",
        _ => "-",
    };
    let mut rows: Vec<Row> = Vec::new();
    let mut passthrough: Vec<String> = Vec::new();
    for line in tool_text.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(kind), Some(size), Some(name)) = (fields.next(), fields.next(), fields.next())
        else {
            if !line.trim().is_empty() {
                passthrough.push(line.to_owned());
            }
            continue;
        };
        let Ok(size) = size.trim().parse::<u64>() else {
            passthrough.push(line.to_owned());
            continue;
        };
        let kind: &'static str = match kind.trim() {
            "dir" => "dir",
            "file" => "file",
            "symlink" => "symlink",
            _ => "other",
        };
        rows.push(Row {
            kind,
            size,
            name: name.to_owned(),
        });
    }
    // Directories first, then by name (the tool sorts by name; the table re-sorts by kind).
    rows.sort_by(|a, b| {
        let rank = |kind: &str| match kind {
            "dir" => 0,
            "file" => 1,
            "symlink" => 2,
            _ => 3,
        };
        rank(a.kind)
            .cmp(&rank(b.kind))
            .then_with(|| a.name.cmp(&b.name))
    });
    let size_width = rows
        .iter()
        .map(|row| human_size(row.size).chars().count())
        .max()
        .unwrap_or(1);
    let mut out = String::new();
    for row in &rows {
        out.push_str(&format!(
            "{} {:>size_width$}  {}\n",
            marker(row.kind),
            human_size(row.size),
            row.name
        ));
    }
    for line in passthrough {
        out.push_str(&line);
        out.push('\n');
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// A byte count as a short human-readable size: `0 B`, `512 B`, `1.7K`, `2.4M`.
fn human_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1024 * 1024 * 1024, "G"), (1024 * 1024, "M"), (1024, "K")];
    for (scale, suffix) in UNITS {
        if bytes >= scale {
            let value = bytes as f64 / scale as f64;
            // One decimal below 10, none above (1.7K, 24K).
            return if value < 10.0 {
                format!("{value:.1}{suffix}")
            } else {
                format!("{value:.0}{suffix}")
            };
        }
    }
    format!("{bytes} B")
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
        // `list` shows an aligned table instead of its tab-separated raw text (owner hand
        // test: the tabs rendered as nothing, so `dir512crates` ran together).
        "list" => list_table(tool_text),
        _ => tool_text.to_owned(),
    }
}

/// A patch's body: one block per replaced range — the hunk header naming the range's start
/// lines in both files, then the removed lines and the added ones, each prefixed by the sign
/// the diff roles paint. There are no context lines: the call carries no surrounding text, and
/// the renderer must not pretend it does (ADR 0028 R4).
///
/// The headers are built from the result's structured `hunks` (`radian-coding`'s `patch`); a
/// result that carries none (an older journal entry) keeps the numberless form, and the diff
/// body then draws it without a gutter.
fn patch_diff_text(card: &ToolCard) -> Option<String> {
    let old = string_field(&card.arguments, "old")?;
    let new = string_field(&card.arguments, "new")?;
    let hunks = patch_hunks(card);
    let mut out = String::new();
    for index in 0..hunks.len().max(1) {
        if let Some((old_start, old_lines, new_start, new_lines)) = hunks.get(index) {
            out.push_str(&format!(
                "@@ -{old_start},{old_lines} +{new_start},{new_lines} @@\n"
            ));
        }
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
    Some(out)
}

/// The result's `hunks`: `(old_start, old_lines, new_start, new_lines)` per replaced range, in
/// file order (ADR 0028). Empty when the result carries none.
fn patch_hunks(card: &ToolCard) -> Vec<(u64, u64, u64, u64)> {
    card.structured
        .as_ref()
        .and_then(|value| value.get("hunks"))
        .and_then(Value::as_array)
        .map(|hunks| {
            hunks
                .iter()
                .filter_map(|hunk| {
                    let field = |name: &str| hunk.get(name).and_then(Value::as_u64);
                    Some((
                        field("old_start")?,
                        field("old_lines")?,
                        field("new_start")?,
                        field("new_lines")?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
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
    /// Code-block syntax highlighting (ADR 0028 R2, `tui.display.highlight`). In the layout
    /// cache key for the same reason as `mermaid`: highlighted lines are what is drawn.
    pub highlight: bool,
    /// The footer's glyph set (ADR 0027 C2): the tool card's frame characters follow it
    /// (ADR 0028, `ascii` draws `+ - |`), and so do the Markdown elements that have
    /// an ascii fallback (ADR 0035) and the `ask_user` block's box and marks
    /// (ADR 0024 R1). In the layout cache key: the drawn glyphs differ.
    pub glyphs: crate::footer::Glyphs,
}

impl Default for DisplayModes {
    fn default() -> Self {
        // The config default (`tui.display.mermaid`) is on, so a default set of modes draws
        // diagrams too.
        Self {
            reasoning: ReasoningMode::default(),
            tools: ToolDisplay::default(),
            mermaid: true,
            highlight: true,
            glyphs: crate::footer::Glyphs::default(),
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
                BodyLine::Diff(row) => {
                    format!("diff {:?} {:?} {:?}", row.number, row.kind, row.source)
                }
            })
            .collect()
    }

    #[test]
    fn the_registry_falls_back_to_generic_for_an_unknown_tool() {
        let unknown = renderer_for("mcp__server__do_thing");
        let generic = renderer_for("some_other_tool");
        let card = card("mcp__server__do_thing", json!({"a": 1}));
        assert_eq!(unknown.head(&card, ""), generic.head(&card, ""));
        assert!(call_text(&card, "").starts_with("mcp__server__do_thing"));
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
            assert!(call_text(&card, "").starts_with(name), "{name}");
        }
        // The exact built-in names keep their renderers.
        assert_eq!(
            renderer_for("read").head(&card("read", json!({"path": "a"})), ""),
            "a"
        );
        assert_eq!(
            renderer_for("shell").head(&card("shell", json!({"command": "ls"})), ""),
            "$ ls"
        );
        assert_eq!(
            renderer_for("memory_get").head(&card("memory_get", json!({"id": "m1"})), ""),
            "m1"
        );
    }

    /// The head functions return the **title** only: the tool name is the block layer's
    /// (ADR 0028), and `call_text` joins the two.
    #[test]
    fn the_shell_head_is_the_dollar_command_with_its_own_newlines() {
        let one = card("shell", json!({"command": "ls -la /tmp"}));
        assert_eq!(shell_head(&one, ""), "$ ls -la /tmp");
        // A multi-line command keeps every line: the head row wraps, one row per line.
        let multi = card("shell", json!({"command": "a\nb"}));
        assert_eq!(shell_head(&multi, ""), "$ a\nb");
        // A structured result (the exit code, a signal death) does not change the title.
        let mut with_exit = card("shell", json!({"command": "sleep 9"}));
        with_exit.structured = Some(json!({"command": "sleep 9", "exit_code": 2}));
        assert_eq!(shell_head(&with_exit, ""), "$ sleep 9");
        // The call entry's text carries the name and the whole command, so it copies.
        assert_eq!(call_text(&with_exit, ""), "shell $ sleep 9");
        // A long command is not cut in the text: the layout truncates the row (R9).
        let long = ToolCard::new("c1", "shell", json!({"command": "x".repeat(200)}));
        assert!(call_text(&long, "").contains(&"x".repeat(200)));
    }

    #[test]
    fn the_read_head_shows_the_path_and_the_line_range() {
        assert_eq!(
            read_head(&card("read", json!({"path": "a.rs"})), ""),
            "a.rs"
        );
        assert_eq!(
            read_head(
                &card("read", json!({"path": "a.rs", "offset": 40, "limit": 10})),
                ""
            ),
            "a.rs 41-50"
        );
        assert_eq!(
            read_head(&card("read", json!({"path": "a.rs", "offset": 40})), ""),
            "a.rs 41-"
        );
        assert_eq!(
            read_head(&card("read", json!({"path": "a.rs", "limit": 5})), ""),
            "a.rs 1-5"
        );
    }

    #[test]
    fn the_fold_modes_decide_the_body() {
        let text = "one\ntwo\nthree\nfour\nfive\nsix\nseven";
        let card = card("generic_tool", json!({}));
        // `expanded`: everything.
        let expanded = body_plan(&card, text, false, ToolFold::Expanded, 3);
        assert_eq!(expanded.lines.len(), 7);
        assert_eq!(expanded.hidden, 0);
        // `errors` (the default): the last `preview_lines` rows, the rest counted as hidden.
        let errors = body_plan(&card, text, false, ToolFold::Errors, 3);
        assert_eq!(errors.lines.len(), 3);
        assert_eq!(errors.hidden, 4);
        assert_eq!(
            lines(&errors.lines)[0],
            format!(
                "text {:?}",
                text.find("five").unwrap()..text.find("five").unwrap() + 4
            )
        );
        // `collapsed`: no rows at all, the whole body counted (the head's `· N lines`).
        let collapsed = body_plan(&card, text, false, ToolFold::Collapsed, 3);
        assert!(collapsed.lines.is_empty());
        assert_eq!(collapsed.hidden, 7);
        // A failure never folds, in any mode.
        for fold in [ToolFold::Expanded, ToolFold::Errors, ToolFold::Collapsed] {
            let plan = body_plan(&card, text, true, fold, 3);
            assert_eq!(plan.lines.len(), 7, "{fold:?}");
            assert_eq!(plan.hidden, 0, "{fold:?}");
        }
        // A body that fits the preview is left whole under `errors`...
        let fits = body_plan(&card, "a\nb", false, ToolFold::Errors, 5);
        assert_eq!(fits.lines.len(), 2);
        assert_eq!(fits.hidden, 0);
        // ...and still hides everything under `collapsed`.
        let hidden = body_plan(&card, "a\nb", false, ToolFold::Collapsed, 5);
        assert!(hidden.lines.is_empty());
        assert_eq!(hidden.hidden, 2);
    }

    #[test]
    fn the_read_body_is_a_one_liner_until_it_is_expanded() {
        let card = card("read", json!({"path": "a.rs"}));
        let body = read_body(&card, "l1\nl2\nl3");
        assert_eq!(body.len(), 3);
        // `read` is a small tool (ADR 0028): a successful call draws no
        // rows, in either folded mode, and the emitted body is the line count.
        for fold in [ToolFold::Errors, ToolFold::Collapsed] {
            let plan = body_plan(&card, "l1\nl2\nl3", false, fold, 5);
            assert!(plan.lines.is_empty(), "{fold:?}");
            assert_eq!(plan.hidden, 3, "{fold:?}");
        }
        // Expanded shows the lines; an error always does.
        assert_eq!(
            body_plan(&card, "l1\nl2\nl3", false, ToolFold::Expanded, 5)
                .lines
                .len(),
            3
        );
        assert_eq!(
            body_plan(&card, "l1\nl2\nl3", true, ToolFold::Errors, 5)
                .lines
                .len(),
            3
        );
    }

    /// The line count the collapsed head's `· N lines` uses
    /// ([`body_line_count`]) is the plan's own content count, for every built-in renderer.
    #[test]
    fn the_line_count_mirrors_every_renderer() {
        let cases: Vec<(&str, Value, &str)> = vec![
            ("shell", json!({"command": "ls"}), "out\nmore\n"),
            ("read", json!({"path": "a.rs"}), "l1\nl2\nl3"),
            (
                "patch",
                json!({"path": "a.rs", "old": "a\n", "new": "b\n"}),
                "@@ -1,1 +1,1 @@\n-a\n+b\n",
            ),
            ("write", json!({"path": "a", "content": "x\n"}), "x\n"),
            ("list", json!({"path": "."}), "dir\t0\tsrc\n"),
            ("git_status", json!({}), "M a.rs\n"),
            ("grep", json!({"pattern": "x"}), "a.rs:1:x\n"),
            ("glob", json!({"pattern": "*"}), "a.rs\n"),
            ("mcp__s__t", json!({}), "one\ntwo"),
        ];
        for (name, arguments, text) in cases {
            let card = card(name, arguments);
            let plan = body_plan(&card, text, false, ToolFold::Expanded, 5);
            assert_eq!(
                plan.lines.len(),
                body_line_count(text),
                "{name}: the count must need no layout"
            );
        }
        // An empty result is one empty row, in the plan and in the count.
        let empty = card("shell", json!({"command": "true"}));
        assert_eq!(
            body_plan(&empty, "", false, ToolFold::Expanded, 5)
                .lines
                .len(),
            1
        );
        assert_eq!(body_line_count(""), 1);
    }

    #[test]
    fn the_shell_body_marks_the_stderr_section() {
        let text = "$ run\nexit code: 1\n--- stdout ---\nout\n--- stderr ---\nboom\nmore";
        let card = card("shell", json!({"command": "run"}));
        let body = shell_body(&card, text);
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

    /// `patch` / `write`: the title is the path, the `+N −M` counts are the meta's
    /// (ADR 0028).
    #[test]
    fn the_patch_and_write_meta_counts_the_replacement_lines() {
        let patch = card(
            "patch",
            json!({"path": "src/日本.rs", "old": "one\ntwo\n", "new": "one\ntwo\nthree\nfour"}),
        );
        assert_eq!(patch_head(&patch, ""), "src/日本.rs");
        assert_eq!(meta_parts(&patch, false), vec!["+4 −2"]);
        // An empty side counts zero; a trailing newline does not add a line.
        let empty = card("patch", json!({"path": "a", "old": "", "new": "x\n"}));
        assert_eq!(meta_parts(&empty, false), vec!["+1 −0"]);
        // `write` counts the content it wrote; a `previous_lines` field (when a result carries
        // one) adds the removed side.
        let write = card(
            "write",
            json!({"path": "a/日本.txt", "content": "l1\nl2\nl3"}),
        );
        assert_eq!(write_head(&write, ""), "a/日本.txt");
        assert_eq!(meta_parts(&write, false), vec!["+3"]);
        let mut overwrite = write.clone();
        overwrite.structured = Some(json!({"path": "a", "bytes_written": 8, "previous_lines": 5}));
        assert_eq!(write_head(&overwrite, ""), "a/日本.txt");
        assert_eq!(meta_parts(&overwrite, false), vec!["+3 −5"]);
    }

    /// The meta's fields (ADR 0028): only what exists, in order —
    /// `cancelled`, the shell outcome, the counts, the duration.
    #[test]
    fn the_meta_writes_only_the_data_that_exists() {
        let shell = card("shell", json!({"command": "make test"}));
        assert!(meta_parts(&shell, false).is_empty());
        let mut done = shell.clone();
        done.structured = Some(json!({"command": "make test", "exit_code": 0, "timeout_secs": 30}));
        done.duration_ms = Some(50);
        assert_eq!(meta_parts(&done, false), vec!["exit 0", "0.05s"]);
        // `timeout 600s` only when the call timed out.
        let mut timed_out = shell.clone();
        timed_out.structured = Some(json!({
            "command": "sleep", "exit_code": null, "timed_out": true, "timeout_secs": 600,
        }));
        assert_eq!(meta_parts(&timed_out, false), vec!["timeout 600s"]);
        // A cancelled call says so, and `entry.error_code` reaches it through the caller.
        assert_eq!(meta_parts(&shell, true), vec!["cancelled"]);
        let mut flagged = shell.clone();
        flagged.structured = Some(json!({"command": "x", "cancelled": true}));
        assert_eq!(meta_parts(&flagged, false), vec!["cancelled"]);
        // A shell result's own `duration_ms` is a duration the card has no producer for.
        let mut measured = card("shell", json!({"command": "true"}));
        measured.structured =
            Some(json!({"command": "true", "exit_code": 0, "duration_ms": 12400}));
        assert_eq!(meta_parts(&measured, false), vec!["exit 0", "12.4s"]);
    }

    #[test]
    fn the_git_diff_head_counts_from_the_text() {
        let text =
            "diff --git a/x b/x\nindex 1..2\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n+another";
        assert_eq!(
            git_diff_head(&card("git_diff", json!({})), text),
            "1 files +2 −1"
        );
        // No text yet (the call-time head): the counts are not claimed.
        assert_eq!(git_diff_head(&card("git_diff", json!({})), ""), "");
        assert_eq!(call_text(&card("git_diff", json!({})), ""), "git_diff");
    }

    /// The classifier for the numberless fallback: `@@` and file headers are structure (the
    /// `frame` role, ADR 0028), the signs carry the diff roles.
    #[test]
    fn the_diff_role_classifies_every_line_kind() {
        assert_eq!(diff_role("diff --git a/x b/x"), LineRole::Frame);
        assert_eq!(diff_role("index 111..222 100644"), LineRole::Frame);
        assert_eq!(diff_role("--- a/x"), LineRole::Frame);
        assert_eq!(diff_role("+++ b/x"), LineRole::Frame);
        assert_eq!(diff_role("@@ -1,3 +1,4 @@ fn main"), LineRole::Frame);
        assert_eq!(diff_role("+added"), LineRole::DiffAdded);
        assert_eq!(diff_role("-removed"), LineRole::DiffRemoved);
        assert_eq!(diff_role(" context"), LineRole::DiffContext);
    }

    /// The kind a row's sign puts it on, and the structures that have no sign at all.
    #[test]
    fn the_diff_kind_reads_the_sign() {
        assert_eq!(diff_kind("+added"), Some(DiffKind::Added));
        assert_eq!(diff_kind("-removed"), Some(DiffKind::Removed));
        assert_eq!(diff_kind(" context"), Some(DiffKind::Context));
        assert_eq!(diff_kind(""), Some(DiffKind::Context));
        // `+++`/`---` are file headers, not content, and `\ No newline` is a marker.
        assert_eq!(diff_kind("+++ b/x"), None);
        assert_eq!(diff_kind("--- a/x"), None);
        assert_eq!(diff_kind("@@ -1 +1 @@"), None);
        assert_eq!(diff_kind("\\ No newline at end of file"), None);
    }

    #[test]
    fn the_list_git_status_ask_and_memory_heads() {
        let list = card("list", json!({"path": "src"}));
        let mut list = list;
        list.structured = Some(json!({"path": "src", "entries": 3, "total_entries": 9}));
        assert_eq!(list_head(&list, ""), "src (3 of 9 entries)");
        let status = card("git_status", json!({}));
        assert_eq!(list_head(&status, ""), "");
        let mut status = status;
        status.structured = Some(json!({"entries": 2}));
        assert_eq!(list_head(&status, ""), "2 entries");
        let ask = card(
            "ask_user",
            json!({"questions": [{"id": "q1"}, {"id": "q2"}]}),
        );
        assert_eq!(ask_head(&ask, ""), "2 questions");
        let ask1 = card("ask_user", json!({"questions": [{"id": "q1"}]}));
        assert_eq!(ask_head(&ask1, ""), "1 question");
        assert_eq!(
            memory_head(&card("memory_get", json!({"id": "mem-7"})), ""),
            "mem-7"
        );
        assert_eq!(
            memory_head(&card("memory_search", json!({"query": "auth flow"})), ""),
            "auth flow"
        );
        assert_eq!(
            memory_head(&card("memory_revise", json!({"id": "mem-9"})), ""),
            "mem-9"
        );
        // The search tools are small tools with a title naming what was searched.
        assert_eq!(
            grep_head(&card("grep", json!({"pattern": "fn main"})), ""),
            "fn main"
        );
        assert_eq!(
            grep_head(
                &card("grep", json!({"pattern": "main", "path": "src/lib.rs"})),
                ""
            ),
            "main src/lib.rs"
        );
        assert_eq!(
            glob_head(&card("glob", json!({"pattern": "src/**/*.rs"})), ""),
            "src/**/*.rs"
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
        let body = diff_body(&patch, &text);
        let roles: Vec<LineRole> = body
            .iter()
            .map(|line| match line {
                BodyLine::Text { role, .. } => *role,
                // No `hunks` in the result: the body keeps today's numberless form.
                BodyLine::Diff(row) => {
                    panic!("a diff without start lines must not number: {row:?}")
                }
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

    /// The plan's diff rows as `(line number, kind)`, so a test can assert the numbering row by
    /// row.
    fn diff_rows(plan: &[BodyLine]) -> Vec<(Option<u64>, DiffKind)> {
        plan.iter()
            .filter_map(|line| match line {
                BodyLine::Diff(row) => Some((row.number, row.kind)),
                _ => None,
            })
            .collect()
    }

    /// The plan's row at `index` and its `LineRole`, for the rows that are not diff rows.
    fn text_role(plan: &[BodyLine]) -> Vec<LineRole> {
        plan.iter()
            .filter_map(|line| match line {
                BodyLine::Text { role, .. } => Some(*role),
                _ => None,
            })
            .collect()
    }

    /// A `patch` result that carries `hunks`: one `@@` block per replaced range, every row
    /// numbered from its own header, and the **second** range's rows already carry the first
    /// range's line delta (a two-line replacement that became one line moves them up).
    #[test]
    fn a_patch_body_numbers_each_replaced_range() {
        let mut patch = card(
            "patch",
            json!({"path": "src/日本.rs", "old": "a\nb\n", "new": "A\n", "replace_all": true}),
        );
        patch.structured = Some(json!({
            "path": "src/日本.rs",
            "replaced": 2,
            "matched": "exact",
            "hunks": [
                {"old_start": 2, "old_lines": 2, "new_start": 2, "new_lines": 1},
                {"old_start": 5, "old_lines": 2, "new_start": 4, "new_lines": 1},
            ],
        }));
        let text = result_display_text(&patch, "replaced 2 occurrences in src/日本.rs");
        assert_eq!(
            text,
            "@@ -2,2 +2,1 @@\n-a\n-b\n+A\n@@ -5,2 +4,1 @@\n-a\n-b\n+A\n"
        );
        let body = diff_body(&patch, &text);
        assert_eq!(
            diff_rows(&body),
            vec![
                (Some(2), DiffKind::Removed),
                (Some(3), DiffKind::Removed),
                (Some(2), DiffKind::Added),
                (Some(5), DiffKind::Removed),
                (Some(6), DiffKind::Removed),
                (Some(4), DiffKind::Added),
            ]
        );
        // The two `@@` headers are structure, not content.
        assert_eq!(text_role(&body), vec![LineRole::Frame, LineRole::Frame]);
        // Every row knows the file's language from the call's path.
        assert!(
            body.iter().all(
                |line| matches!(line, BodyLine::Diff(row) if row.lang == Some("rs"))
                    || matches!(line, BodyLine::Text { .. })
            ),
            "{body:?}"
        );
    }

    /// `git_diff`: the numbers come from the text's own `@@` headers, file headers and `@@` are
    /// structure, and a second hunk's counters start from its own header.
    #[test]
    fn a_git_diff_body_numbers_from_its_own_hunk_headers() {
        let text = "diff --git a/src/main.rs b/src/main.rs\n\
                    index 1..2 100644\n\
                    --- a/src/main.rs\n\
                    +++ b/src/main.rs\n\
                    @@ -10,3 +10,4 @@ fn main() {\n\
                    \x20let a = 1;\n\
                    -let b = 2;\n\
                    +let b = 3;\n\
                    +let c = 4;\n\
                    \x20context\n\
                    @@ -30,2 +31,2 @@\n\
                    -old tail\n\
                    +new tail\n\
                    \x20end\n";
        let body = diff_body(&card("git_diff", json!({})), text);
        assert_eq!(
            diff_rows(&body),
            vec![
                (Some(10), DiffKind::Context),
                (Some(11), DiffKind::Removed),
                (Some(11), DiffKind::Added),
                (Some(12), DiffKind::Added),
                (Some(13), DiffKind::Context),
                (Some(30), DiffKind::Removed),
                (Some(31), DiffKind::Added),
                (Some(32), DiffKind::Context),
            ]
        );
        // `diff --git`, `index`, `---`, `+++` and the two `@@` headers: six structure rows.
        assert_eq!(
            text_role(&body),
            vec![
                LineRole::Frame,
                LineRole::Frame,
                LineRole::Frame,
                LineRole::Frame,
                LineRole::Frame,
                LineRole::Frame,
            ]
        );
        // `git_diff` names the file per file header, so the rows highlight as Rust.
        assert!(
            body.iter()
                .all(|line| !matches!(line, BodyLine::Diff(row) if row.lang != Some("rs"))),
            "{body:?}"
        );
    }

    /// A diff whose text carries no `@@` header (an MCP tool's text, an older journal entry)
    /// keeps today's numberless form: the sign column, no gutter, the diff roles on the row.
    #[test]
    fn a_diff_without_hunk_headers_keeps_the_sign_column() {
        let body = diff_body(&card("git_diff", json!({})), "-gone\n+added\n context\n");
        assert!(diff_rows(&body).is_empty(), "{body:?}");
        assert_eq!(
            text_role(&body),
            vec![
                LineRole::DiffRemoved,
                LineRole::DiffAdded,
                LineRole::DiffContext
            ]
        );
        // A `+`/`+`-in-a-header pair without a hunk is still structure.
        let body = diff_body(&card("git_diff", json!({})), "--- a/x\n+++ b/x\n");
        assert!(body.iter().all(|line| matches!(
            line,
            BodyLine::Text {
                role: LineRole::Frame,
                ..
            }
        )));
    }

    /// The hunk header parser: both start lines, with the counts optional (a one-line range
    /// omits them, as git writes it).
    #[test]
    fn a_hunk_header_gives_both_start_lines() {
        assert_eq!(parse_hunk_header("@@ -2,2 +2,1 @@"), Some((2, 2)));
        assert_eq!(parse_hunk_header("@@ -1 +1 @@ fn main"), Some((1, 1)));
        assert_eq!(
            parse_hunk_header("@@ -10,3 +40,7 @@ fn next"),
            Some((10, 40))
        );
        assert_eq!(parse_hunk_header("@@ nope @@"), None);
        assert_eq!(parse_hunk_header("-not a header"), None);
        assert_eq!(parse_hunk_header("+not a header"), None);
    }

    /// The language a diff row highlights with is the file's suffix: from the call for `patch`,
    /// from the file header for `git_diff`, and nothing when the suffix names no language.
    #[test]
    fn a_diff_row_takes_the_language_from_the_files_suffix() {
        let mut patch = card(
            "patch",
            json!({"path": "notes.txt", "old": "a\n", "new": "b\n"}),
        );
        patch.structured = Some(json!({
            "hunks": [{"old_start": 1, "old_lines": 1, "new_start": 1, "new_lines": 1}],
        }));
        let text = result_display_text(&patch, "replaced 1 occurrence");
        assert_eq!(text, "@@ -1,1 +1,1 @@\n-a\n+b\n");
        let body = diff_body(&patch, &text);
        assert!(
            body.iter().all(
                |line| matches!(line, BodyLine::Diff(row) if row.lang.is_none())
                    || matches!(line, BodyLine::Text { .. })
            ),
            "`txt` is not a language we highlight: {body:?}"
        );
        // `git_diff` reads the path out of the file header instead.
        let text = "diff --git a/x.md b/x.md\n+++ b/x.md\n@@ -1,1 +1,1 @@\n-# t\n+# T\n";
        let body = diff_body(&card("git_diff", json!({})), text);
        assert!(
            body.iter().all(
                |line| matches!(line, BodyLine::Diff(row) if row.lang == Some("md"))
                    || matches!(line, BodyLine::Text { .. })
            ),
            "{body:?}"
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
    fn a_list_result_becomes_an_aligned_table_with_directories_first() {
        let text = "file\t512\tb.md\ndir\t4096\tsrc\nfile\t1693\tCargo.toml\nsymlink\t0\tlatest\n";
        let table = list_table(text);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 4, "{table}");
        // Directories first, then files by name, then the symlink.
        assert!(
            lines[0].starts_with("d ") && lines[0].ends_with("src"),
            "{table}"
        );
        assert!(lines[1].ends_with("Cargo.toml"), "{table}");
        assert!(lines[2].ends_with("b.md"), "{table}");
        assert!(
            lines[3].starts_with("l ") && lines[3].ends_with("latest"),
            "{table}"
        );
        // The size column is right-aligned and human-readable.
        assert!(lines[0].contains("4.0K"), "{table}");
        assert!(lines[1].contains("1.7K"), "{table}");
        assert!(lines[2].contains("512 B"), "{table}");
        assert!(lines[3].contains("0 B"), "{table}");
        // The size field is right-aligned, so the NAME column is what lines up.
        let name_columns: Vec<usize> = lines
            .iter()
            .map(|line| {
                line.find(|c: char| c.is_alphabetic() && !c.is_ascii_digit())
                    .unwrap_or(0)
            })
            .collect();
        assert!(
            name_columns.windows(2).all(|pair| pair[0] == pair[1]),
            "the names align: {table} {name_columns:?}"
        );
        // No tab survives.
        assert!(!table.contains('\t'), "{table}");
    }

    #[test]
    fn a_list_line_without_the_tool_shape_passes_through() {
        let table = list_table("dir\t4096\tsrc\nnot a list line\n");
        assert!(table.contains("not a list line"), "{table}");
        assert!(table.starts_with("d "), "{table}");
    }

    #[test]
    fn a_tool_body_expands_its_tabs_for_display_and_copies_the_source() {
        use crate::blocks;
        use crate::tools::ToolCard;
        let card = ToolCard::new("c1", "generic_tool", json!({}));
        let text = "a\tb\n";
        let lines = blocks::body_text_lines_for_test(&card, text, 40);
        assert_eq!(lines[0], "a    b", "the tab is four spaces wide");
        // The mapping still points at the source: the four cells are one tab, so a selection
        // over the line copies the original tab (ADR 0028 R9's rule for code, applied to a
        // tool body).
        let laid = blocks::layout_for_test(&card, text, 40);
        // Every cell of a text line is a real source character (`Some`), and the tab's four
        // cells all point at the tab's own offset, so the line's range copies the tab.
        assert!(
            laid[0].chars.iter().all(|cell| cell.is_some()),
            "a text line's cells are all source offsets"
        );
        let start = laid[0]
            .chars
            .first()
            .copied()
            .flatten()
            .expect("a source cell");
        assert_eq!(
            &text[start..laid[0].end],
            "a\tb",
            "the copy keeps the source tab"
        );
    }

    #[test]
    fn an_empty_body_is_one_empty_line() {
        let body = generic_body(&card("t", json!({})), "");
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
