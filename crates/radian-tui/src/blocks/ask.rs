//! The `ask_user` record (ADR 0028, ADR 0024 R1): the open
//! frame - `╭─ ask …` top, `│ ` body rows, `╰──` tail - drawn from the call's arguments and the
//! result's structured answers. `blocks/mod.rs` owns the block model and the line counts.

use super::*;
use unicode_width::UnicodeWidthStr;

/// Whether the entry is part of an `ask_user` pair (either the call or its result): its block
/// has its own shape, so the count goes through the layout.
pub(super) fn is_ask_entry(entry: &Entry) -> bool {
    entry
        .tool
        .as_deref()
        .is_some_and(|card| card.name == "ask_user")
}
/// The frame and mark glyphs of the `ask_user` record (ADR 0028 /
/// ADR 0035): an **open** frame - `╭─ <label> ───…` for the top line and
/// the three-cell `╰──` tail, unicode by default and pure ASCII under `glyphs = ascii`. The one
/// marker source for the transcript record **and** the panel (ADR 0024
/// implementation note): single, multi and `Other` share the circle pair (ADR 0028
/// decision 2 - `○`/`◉`, never bold).
pub(crate) struct AskGlyphs {
    pub(crate) top_left: char,
    pub(crate) bottom_left: char,
    pub(crate) horizontal: char,
    pub(crate) vertical: char,
    pub(crate) single_on: &'static str,
    pub(crate) single_off: &'static str,
    pub(crate) multi_on: &'static str,
    pub(crate) multi_off: &'static str,
    pub(crate) desc: &'static str,
}

impl AskGlyphs {
    pub(crate) fn for_glyphs(glyphs: crate::footer::Glyphs) -> Self {
        match glyphs {
            crate::footer::Glyphs::Ascii => Self {
                top_left: '+',
                bottom_left: '+',
                horizontal: '-',
                vertical: '|',
                single_on: "(*)",
                single_off: "( )",
                multi_on: "(*)",
                multi_off: "( )",
                desc: "->",
            },
            crate::footer::Glyphs::Unicode | crate::footer::Glyphs::Nerd => Self {
                top_left: '╭',
                bottom_left: '╰',
                horizontal: '─',
                vertical: '│',
                single_on: "◉",
                single_off: "○",
                multi_on: "◉",
                multi_off: "○",
                desc: "↳",
            },
        }
    }
}

/// A synthesized `ask_user` frame line: no source characters (it neither copies nor matches),
/// styled by `md` pieces. `code` is set so the renderer emits an empty lead and the frame's
/// `╭`/`│`/`╰` sits in the marker column at column 1 (ADR 0035); the
/// block draws its own marker column, so the entry's `->` marker is not used.
fn ask_line(pieces: Vec<(String, MdStyle)>) -> BodyLine {
    let text: String = pieces.iter().map(|(text, _)| text.as_str()).collect();
    let md: Vec<MdPiece> = pieces
        .into_iter()
        .map(|(text, style)| MdPiece {
            text,
            style,
            offset: None,
        })
        .collect();
    BodyLine {
        chars: Vec::new(),
        end: 0,
        text,
        code: true,
        frame: Frame::None,
        role: tools::LineRole::Normal,
        hint: true,
        diagram: None,
        reason: false,
        md: Some(md),
        syntax: None,
    }
}

/// A pending `ask_user` call (ADR 0024 rule 1): one weak line, now the open frame's
/// top line (`╭─ ask · N question(s) · answering below ───…`). The questions live in the panel
/// below until they are answered, so the same ask is never drawn twice. The count comes from
/// the call's own arguments.
pub(super) fn ask_pending_line(
    card: &tools::ToolCard,
    width: usize,
    glyphs: crate::footer::Glyphs,
) -> Vec<BodyLine> {
    let count = card
        .arguments
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let label = format!(
        "ask · {count} question{} · answering below",
        if count == 1 { "" } else { "s" }
    );
    let g = AskGlyphs::for_glyphs(glyphs);
    vec![ask_line(ask_top(&g, width, &label))]
}

/// The `ask_user` record as an open frame (ADR 0028): the request from
/// the call's `arguments`, the answers from the result's `structured`. Three states:
/// unanswered (the call, no structured result), answered, and declined/cancelled (an error
/// result). The content is the ADR 0024 R1 structure (prompt, options, `↳`
/// description, answer/note, status); only the frame is the open one.
pub(super) fn ask_block(
    card: &tools::ToolCard,
    text: &str,
    is_error: bool,
    width: usize,
    glyphs: crate::footer::Glyphs,
) -> Vec<BodyLine> {
    let g = AskGlyphs::for_glyphs(glyphs);
    let questions: Vec<&serde_json::Value> = card
        .arguments
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .map(|questions| questions.iter().collect())
        .unwrap_or_default();
    let answers = card
        .structured
        .as_ref()
        .and_then(|value| value.get("answers"))
        .and_then(serde_json::Value::as_array);
    let mut out = Vec::new();
    // ADR 0028 / ADR 0035: the open frame's top
    // line carries the bare label `ask`; the per-question `[id] · kind · options:N` divider is
    // gone (the prompt, options, `↳` description and answer are the only content).
    out.push(ask_line(ask_top(&g, width, "ask")));
    for question in &questions {
        let id = question
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let kind = question
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("single");
        let options = question
            .get("options")
            .and_then(serde_json::Value::as_array);
        let prompt = question
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        // The prompt is the frame's first body line, at the body column (no extra indent).
        out.extend(ask_body(
            &g,
            width,
            (String::new(), MdStyle::Dim),
            (prompt.to_owned(), MdStyle::Accent),
        ));
        let reply = answers.and_then(|answers| {
            answers
                .iter()
                .find(|reply| reply.get("id").and_then(serde_json::Value::as_str) == Some(id))
        });
        let selected: Vec<&str> = reply
            .and_then(|reply| reply.get("selected").and_then(serde_json::Value::as_array))
            .map(|selected| {
                selected
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect()
            })
            .unwrap_or_default();
        if kind != "text" {
            for option in options.map(Vec::as_slice).unwrap_or(&[]) {
                let option_id = option
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let chosen = selected.contains(&option_id);
                let mark = match (kind, chosen) {
                    ("multi", true) => g.multi_on,
                    ("multi", false) => g.multi_off,
                    (_, true) => g.single_on,
                    (_, false) => g.single_off,
                };
                let mut label = option
                    .get("label")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if option
                    .get("recommended")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    label.push_str(" (Recommended)");
                }
                // The circle marker sits at the body column (column 3); the label follows.
                out.extend(ask_body(
                    &g,
                    width,
                    (format!("{mark} "), MdStyle::Dim),
                    (label, MdStyle::Plain),
                ));
                if let Some(description) = option
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                {
                    // The `↳` description is indented two cells under the option's label.
                    out.extend(ask_body(
                        &g,
                        width,
                        (format!("  {} ", g.desc), MdStyle::Dim),
                        (description.to_owned(), MdStyle::Muted),
                    ));
                }
            }
        }
        if let Some(reply) = reply {
            for (key, style) in [("other", MdStyle::Muted), ("note", MdStyle::Muted)] {
                if let Some(value) = reply.get(key).and_then(serde_json::Value::as_str) {
                    out.extend(ask_body(
                        &g,
                        width,
                        (format!("  {key}: "), MdStyle::Dim),
                        (value.to_owned(), style),
                    ));
                }
            }
        }
    }
    // Declined/cancelled (an error result) is one status line; the text is the tool failure.
    if is_error {
        let status = text.lines().next().unwrap_or("(no answer)").to_owned();
        out.extend(ask_body(
            &g,
            width,
            (String::new(), MdStyle::Dim),
            (status, MdStyle::Muted),
        ));
    }
    out.push(ask_line(ask_bottom(&g)));
    out
}

/// The open frame's top line (ADR 0028 / ADR 0035):
/// `╭─ <label> ───…`, with the rule running to the body width and **no** right corner. The whole
/// line is the `frame` role (ADR 0029's structural level).
fn ask_top(g: &AskGlyphs, width: usize, label: &str) -> Vec<(String, MdStyle)> {
    let prefix = format!("{}{} ", g.top_left, g.horizontal);
    let mut pieces = vec![
        (prefix.clone(), MdStyle::Frame),
        (label.to_owned(), MdStyle::Frame),
        (" ".to_string(), MdStyle::Frame),
    ];
    let used = UnicodeWidthStr::width(prefix.as_str()) + UnicodeWidthStr::width(label) + 1;
    let fill = width.saturating_sub(used);
    pieces.push((g.horizontal.to_string().repeat(fill), MdStyle::Frame));
    pieces
}

/// Wraps an `ask_user` prompt to `width` display cells (CJK counts 2).
fn ask_wrap(text: &str, width: usize) -> Vec<String> {
    crate::render::wrap_char_offsets(text, width.max(1))
        .into_iter()
        .map(|line| {
            line.chars
                .into_iter()
                .filter_map(|i| text.get(i..).and_then(|r| r.chars().next()))
                .collect()
        })
        .collect()
}

/// One content row of the open frame (ADR 0028 / ADR 0035
/// decision 1): the marker column `│ ` in columns 1-2, `prefix` (the marker or indent, at
/// column 3), and the value wrapped to whatever the prefix leaves of the body width (CJK by
/// display width). A continuation row repeats the prefix's cells so the value's column holds.
/// There is no right border and no trailing padding - the owner's picture leaves the body rows
/// open (a banded theme pads them on screen, which is slice ①'s concern, not this one).
fn ask_body(
    g: &AskGlyphs,
    width: usize,
    prefix: (String, MdStyle),
    value: (String, MdStyle),
) -> Vec<BodyLine> {
    let marker = format!("{} ", g.vertical);
    let inner = width.saturating_sub(UnicodeWidthStr::width(marker.as_str()));
    let prefix_width = UnicodeWidthStr::width(prefix.0.as_str());
    let avail = inner.saturating_sub(prefix_width).max(1);
    ask_wrap(&value.0, avail)
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            let lead = if index == 0 {
                prefix.0.clone()
            } else {
                " ".repeat(prefix_width)
            };
            ask_line(vec![
                (marker.clone(), MdStyle::Frame),
                (lead, prefix.1),
                (chunk, value.1),
            ])
        })
        .collect()
}

/// The open frame's tail (ADR 0028 / ADR 0035):
/// exactly three cells, `╰──` (ascii `+--`), in the frame role. It does not run to the top
/// line's width.
fn ask_bottom(g: &AskGlyphs) -> Vec<(String, MdStyle)> {
    vec![(
        format!("{}{}{}", g.bottom_left, g.horizontal, g.horizontal),
        MdStyle::Frame,
    )]
}
