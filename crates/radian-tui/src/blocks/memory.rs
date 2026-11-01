//! The memory card (ADR 0021, ADR 0035): a successful model `write` or `patch` into a memory
//! scope draws one open frame instead of the ordinary tool card. The title, the type and the
//! path come from [`crate::memory_card::MemoryRef`], resolved once when the result settled.

use unicode_width::UnicodeWidthChar;

use super::ask::{AskGlyphs, ask_bottom, ask_line, ask_top};
use super::*;

/// Whether the entry draws the memory card: a `write`/`patch` (not a `read`) whose card
/// resolved to a memory file.
pub(super) fn is_memory_card_entry(entry: &Entry) -> bool {
    entry
        .tool
        .as_deref()
        .and_then(|card| card.memory.as_ref())
        .is_some_and(|memory| !memory.read)
}

/// The card as an open frame: `╭─ memory ───…`, one content row, `╰──` (ADR 0035's card
/// shape). The title keeps the text role; the separators, the type and the path are weak.
pub(super) fn memory_card_lines(
    memory: &crate::memory_card::MemoryRef,
    width: usize,
    glyphs: crate::footer::Glyphs,
) -> Vec<BodyLine> {
    let g = AskGlyphs::for_glyphs(glyphs);
    let mut out = vec![ask_line(ask_top(&g, width, "memory"))];
    out.extend(body_rows(&g, width, &memory.line()));
    out.push(ask_line(ask_bottom(&g)));
    out
}

/// The card's content row, wrapped to the content width. Each character keeps its role: the
/// title's text role, and the weak level from the first separator on.
fn body_rows(g: &AskGlyphs, width: usize, line: &str) -> Vec<BodyLine> {
    let marker = format!("{} ", g.vertical);
    let inner = width
        .saturating_sub(unicode_width::UnicodeWidthStr::width(marker.as_str()))
        .max(1);
    let split = line.find(" · ").unwrap_or(line.len());
    let mut rows: Vec<Vec<(char, MdStyle)>> = vec![Vec::new()];
    let mut used = 0usize;
    for (index, ch) in line.char_indices() {
        let style = if index < split {
            MdStyle::Plain
        } else {
            MdStyle::Muted
        };
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > inner && used > 0 {
            rows.push(Vec::new());
            used = 0;
        }
        rows.last_mut().expect("one row exists").push((ch, style));
        used += w;
    }
    rows.into_iter()
        .map(|cells| {
            let mut pieces = vec![(marker.clone(), MdStyle::Frame)];
            pieces.extend(cells.into_iter().map(|(ch, style)| (ch.to_string(), style)));
            ask_line(pieces)
        })
        .collect()
}
