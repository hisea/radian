//! The thinking channel's display (ADR 0028): the collapsed/hidden
//! modes, the header line (its wall-clock duration or the live spinner frame) and the expanded
//! body, which is the same Markdown layout as an Assistant reply with the code frame behind the
//! renderer's `│ ` rail. `blocks/mod.rs` owns the block model and the line counts.

use super::markdown::{MdGlyphs, code_frame_bottom, code_frame_top, code_lines, md_lines_cached};
use super::*;

/// The reasoning channel's measured wall clock (ADR 0028): the
/// difference between the entry's first-delta instant and its end. `None` for a resumed
/// entry (neither instant was ever stamped) and while the entry is still live (no end yet).
fn reasoning_duration(entry: &Entry) -> Option<String> {
    let started = entry.reasoning_started?;
    let ended = entry.reasoning_ended?;
    Some(crate::timing::format_duration(
        ended.saturating_duration_since(started),
    ))
}

/// The expanded thinking channel's header line: `thinking · <duration>`, `thinking · <spinner
/// frame>` while streaming, and bare `thinking` for a resumed entry with no duration (ADR
/// 0028 revision 5 decision 4). The `◻ ` marker is the marker column's, drawn by the renderer.
pub(super) fn reasoning_header(entry: &Entry, spinner: crate::spinner::Frame) -> String {
    if entry.live {
        return format!("thinking · {}", spinner.glyph);
    }
    match reasoning_duration(entry) {
        Some(duration) => format!("thinking · {duration}"),
        None => "thinking".to_string(),
    }
}

/// The collapsed reasoning placeholder (ADR 0028): `thinking · <duration>
/// · N lines`, `thinking · <spinner frame>` while streaming, and `thinking · N lines` for a
/// resumed entry. The line count is the entry's own text; the `◼ ` marker is the marker
/// column's (ADR 0028 rule 5), so the body column is the same folded and expanded.
pub(super) fn placeholder(entry: &Entry, spinner: crate::spinner::Frame) -> String {
    let lines = entry.text.lines().count().max(1);
    if entry.live {
        return format!("thinking · {}", spinner.glyph);
    }
    match reasoning_duration(entry) {
        Some(duration) => format!("thinking · {duration} · {lines} lines"),
        None => format!("thinking · {lines} lines"),
    }
}

/// The expanded thinking channel's body (ADR 0028): the header line,
/// then the entry's Markdown laid out at `body_width`. It is the **same** Markdown layout as
/// an Assistant reply ([`md_lines_cached`], fences through [`code_lines`]) — the thinking
/// channel only differs in how the renderer colours it (one role, no syntax highlighting) and
/// in the `│ ` rail the renderer puts in the marker column. Fenced code blocks keep their
/// structure (the weak fence lines, `code_lines`) but are **never highlighted**.
pub(super) fn reasoning_body_lines(
    entry: &Entry,
    blocks: &[Block],
    modes: DisplayModes,
    width: usize,
    text: &str,
    spinner: crate::spinner::Frame,
) -> Vec<BodyLine> {
    // The prose rows carry the `│ ` rail, so they lay out two columns narrower; a code block
    // draws its own frame in the marker column (like an Assistant reply), so it uses the full
    // width.
    let body_width = width
        .saturating_sub(crate::render::marker_prefix(EntryKind::Reasoning).width())
        .max(1);
    let mut lines = vec![BodyLine::prose(
        Vec::new(),
        0,
        reasoning_header(entry, spinner),
    )];
    for block in blocks {
        match block {
            Block::Paragraph { start, end, .. } => {
                lines.extend(md_lines_cached(
                    entry,
                    *start,
                    *end,
                    body_width,
                    modes.glyphs,
                    text,
                ));
            }
            Block::Code {
                start,
                end,
                closed,
                lang,
            } => {
                let source = &text[*start..*end];
                let md_glyphs = MdGlyphs::for_glyphs(modes.glyphs);
                // ADR 0035: the same open code frame as an Assistant
                // reply — a top line titled with the language, a rail on every code row, a
                // bottom line when the block is closed. `highlight = None`: the thinking body
                // is never syntax-highlighted (ADR 0028).
                lines.push(code_frame_top(lang, *start, width, &md_glyphs));
                lines.extend(code_lines(source, *start, *closed, width, &md_glyphs, None));
                if *closed && source.lines().count() > 1 {
                    lines.push(code_frame_bottom(*start, &md_glyphs));
                }
            }
        }
    }
    lines
}
