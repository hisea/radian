//! Transcript selection: where a mouse cell is in the text, and what a selection copies.
//!
//! A selection is **two anchors**, each `(entry index, byte offset into the entry's display
//! text)` — never a screen position. New transcript output, scrolling and resizes therefore
//! leave the selection where it was; only the renderer re-derives the cells to highlight.
//!
//! Everything here is pure and derived from the *same* wrap the renderer uses
//! ([`crate::render::wrap_char_offsets`]), so what is highlighted is what is copied:
//!
//! * [`locate`] maps a transcript cell (line, column) to an anchor;
//! * [`logical_text`] extracts the selection's logical text — wrapping inserts no newline,
//!   the text's own newlines are kept, entries are joined by newlines, and the `> `-style
//!   prefixes and the status bar are never included;
//! * [`highlight_rows`] maps a selection back onto the visible rows' display columns, for
//!   [`crate::render`] to style with the theme's `selection` role.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::Entry;
use crate::render::{display_lines, display_text, marker_prefix, wrap_char_offsets};

/// One end of a selection: entry `entry`'s display text at byte offset `offset` (always on a
/// char boundary: it comes from `char_indices` or `text.len()`).
///
/// Offsets are into the **display text** ([`display_text`]): a user entry's `@` attachments
/// are folded, and the folded line is both what is shown and what is copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub entry: usize,
    pub offset: usize,
}

/// A transcript selection: where the press landed and where the head (the latest drag cell)
/// is. `anchor` is the fixed end, `head` the moving one; [`Selection::ordered`] sorts them,
/// because a drag upward selects backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Anchor,
    pub head: Anchor,
}

impl Selection {
    /// The anchors as `(start, end)`: the earlier position first, so a copy is `text[s..e]`
    /// and a highlight is one contiguous range.
    pub fn ordered(&self) -> (Anchor, Anchor) {
        if (self.anchor.entry, self.anchor.offset) <= (self.head.entry, self.head.offset) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Whether the two ends coincide (a press that has not been dragged yet).
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// Maps a click at transcript `line` (a wrapped line, `0` = the transcript's first) and
/// display column `col` to an anchor. A line or column past the content clamps to the end of
/// the last entry, so a drag below the text keeps extending to the end. `None` only when
/// there is no entry at all.
pub fn locate(entries: &[Entry], width: usize, line: usize, col: usize) -> Option<Anchor> {
    let mut remaining = line;
    for (index, entry) in entries.iter().enumerate() {
        let count = display_lines(entry, width).len();
        if remaining >= count {
            remaining -= count;
            continue;
        }
        return Some(locate_in_entry(entry, width, remaining, col, index));
    }
    // Past the last entry: the end of the last one (or `None` with no transcript).
    let last = entries.len().checked_sub(1)?;
    let text_len = display_text(&entries[last]).len();
    Some(Anchor {
        entry: last,
        offset: text_len,
    })
}

/// Maps a cell inside one entry to an anchor. `local_line` is `0`-based within the entry's
/// wrapped lines (clamped), `col` a transcript display column (the `> ` prefix's cells are
/// part of the line, so a click on the marker lands on the line's first char).
fn locate_in_entry(entry: &Entry, width: usize, local_line: usize, col: usize, index: usize) -> Anchor {
    let text = display_text(entry);
    let prefix = marker_prefix(entry.kind).width();
    let body_width = width.saturating_sub(prefix).max(1);
    let lines = wrap_char_offsets(&text, body_width);
    let line = &lines[local_line.min(lines.len() - 1)];
    let body_col = col.saturating_sub(prefix);
    // Walk the line's chars accumulating display cells; the click lands on the char whose
    // cell range covers `body_col` (the second cell of a wide char selects that char).
    let mut used = 0usize;
    for &offset in &line.chars {
        let ch = text[offset..].chars().next().expect("a char boundary");
        let w = ch.width().unwrap_or(0);
        if body_col < used + w.max(1) {
            return Anchor {
                entry: index,
                offset,
            };
        }
        used += w;
    }
    // Past the line's last cell: the line's end gap (before the paragraph's own newline).
    Anchor {
        entry: index,
        offset: line.end,
    }
}

/// The logical text of a selection: the entries' display text between the anchors, wrapped
/// lines rejoined (a soft wrap inserts nothing), the text's own newlines kept, entries
/// joined by one newline. An empty or `is_empty` selection copies nothing.
pub fn logical_text(entries: &[Entry], selection: &Selection) -> String {
    if selection.is_empty() {
        return String::new();
    }
    let (start, end) = selection.ordered();
    if start.entry == end.entry {
        let Some(entry) = entries.get(start.entry) else {
            return String::new();
        };
        let text = display_text(entry);
        let (a, b) = (start.offset.min(text.len()), end.offset.min(text.len()));
        return if a < b {
            text[a..b].to_owned()
        } else {
            String::new()
        };
    }
    let mut out = String::new();
    for (index, entry) in entries
        .iter()
        .enumerate()
        // The range is inclusive of `end.entry`.
        .take((end.entry + 1).min(entries.len()))
        .skip(start.entry)
    {
        if !out.is_empty() {
            out.push('\n');
        }
        let text = display_text(entry);
        if index == start.entry {
            out.push_str(&text[start.offset.min(text.len())..]);
        } else if index == end.entry {
            out.push_str(&text[..end.offset.min(text.len())]);
        } else {
            out.push_str(&text);
        }
    }
    out
}

/// For each visible transcript row (`scroll` = the first visible wrapped line, `height` =
/// how many rows to report), the selected display-column ranges on that row, empty when none.
/// Columns are whole-row coordinates (the prefix included), as `(start, end)` with `end`
/// exclusive; the ranges on one row are merged, because the selection is contiguous. The
/// windowing mirrors [`crate::render::visible_lines`], so the rows line up with the frame.
pub fn highlight_rows(
    entries: &[Entry],
    width: usize,
    scroll: usize,
    height: usize,
    selection: &Selection,
) -> Vec<Vec<(usize, usize)>> {
    let (start, end) = selection.ordered();
    let mut out = Vec::with_capacity(height);
    let mut line = 0usize;
    for (index, entry) in entries.iter().enumerate() {
        if out.len() >= height {
            break;
        }
        let count = display_lines(entry, width).len();
        let next = line + count;
        if next > scroll {
            let first = scroll.saturating_sub(line);
            for local in first..count {
                if out.len() >= height {
                    break;
                }
                out.push(if index < start.entry || index > end.entry {
                    Vec::new()
                } else {
                    row_ranges(entry, width, local, index, start, end)
                });
            }
        }
        line = next;
    }
    out
}

/// The selected display-column ranges of one entry's `local`-th wrapped line. A char is
/// selected when its offset is in `[start.offset, end.offset)`; zero-width chars (which sit
/// between two cells of the same run) join their neighbour's range.
fn row_ranges(
    entry: &Entry,
    width: usize,
    local: usize,
    index: usize,
    start: Anchor,
    end: Anchor,
) -> Vec<(usize, usize)> {
    let text = display_text(entry);
    let prefix = marker_prefix(entry.kind).width();
    let body_width = width.saturating_sub(prefix).max(1);
    let lines = wrap_char_offsets(&text, body_width);
    let Some(line) = lines.get(local) else {
        return Vec::new();
    };
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut used = prefix;
    for &offset in &line.chars {
        let ch = text[offset..].chars().next().expect("a char boundary");
        let w = ch.width().unwrap_or(0);
        let selected = index >= start.entry
            && index <= end.entry
            && (index > start.entry || offset >= start.offset)
            && (index < end.entry || offset < end.offset);
        if selected {
            let cell_end = used + w;
            match ranges.last_mut() {
                // Adjacent or zero-width (a combining mark joins its base's cells).
                Some(last) if last.1 >= used => last.1 = last.1.max(cell_end),
                _ => ranges.push((used, cell_end)),
            }
        }
        used += w;
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Entry, EntryKind};
    use agent_core::{ContentPart, Message, MessageMeta};

    /// An assistant entry (prefix `"  "`: body columns start at 2).
    fn entry(text: &str) -> Entry {
        Entry::new(EntryKind::Assistant, text)
    }

    #[test]
    fn a_cell_maps_across_a_soft_wrap_without_inserting_a_break() {
        let entries = [entry("hello world")];
        // Body width 12: one line. The assistant prefix is two cells, so column 4 is the
        // body's column 2, the third char.
        let width = 14;
        let start = locate(&entries, width, 0, 4).expect("inside");
        assert_eq!(start.offset, 2);
        // Width 7 → body 5: "hello" / "world". Line 1 col 0 is 'w' (byte 6): the soft wrap
        // inserted no char.
        let width = 7;
        let w = locate(&entries, width, 1, 0).expect("second line");
        assert_eq!(w.offset, 6);
        // Clicking past the end of line 0 lands in the gap before the space run.
        let eol = locate(&entries, width, 0, 20).expect("clamped");
        assert_eq!(eol.offset, 5, "the gap after 'hello'");
    }

    #[test]
    fn a_wide_char_selects_from_either_of_its_two_cells() {
        let entries = [entry("日本語")];
        let width = 10; // body 8
        let first_cell = locate(&entries, width, 0, 2).expect("body col 0");
        assert_eq!(first_cell.offset, 0);
        let second_cell = locate(&entries, width, 0, 3).expect("body col 1: 日's right half");
        assert_eq!(second_cell.offset, 0, "both cells select 日");
        let next = locate(&entries, width, 0, 4).expect("body col 2: 本");
        assert_eq!(next.offset, 3, "本 starts at byte 3");
    }

    #[test]
    fn an_empty_line_maps_to_its_paragraph_boundary() {
        let entries = [entry("a\n\nb")];
        let width = 10;
        // Lines: "a", "", "b". The empty line's gap is the second '\n' (byte 2).
        let empty = locate(&entries, width, 1, 4).expect("the empty line");
        assert_eq!(empty.offset, 2);
        let b = locate(&entries, width, 2, 0).expect("'b'");
        assert_eq!(b.offset, 3);
    }

    #[test]
    fn clicking_the_prefix_or_past_the_text_clamps_safely() {
        let entries = [Entry::new(EntryKind::User, "user text")];
        let width = 20;
        // The user prefix "> " is two cells: column 0 and 1 both land on the first char.
        assert_eq!(locate(&entries, width, 0, 0).expect("prefix").offset, 0);
        assert_eq!(locate(&entries, width, 0, 1).expect("prefix").offset, 0);
        assert_eq!(locate(&entries, width, 0, 2).expect("body").offset, 0);
        // Far below and right of everything: the end of the last entry.
        assert_eq!(
            locate(&entries, width, 50, 200).expect("clamped").offset,
            "user text".len()
        );
        assert_eq!(locate(&[], width, 0, 0), None);
    }

    #[test]
    fn logical_text_keeps_original_newlines_and_drops_none() {
        let width = 13;
        // A soft wrap inserts nothing: width 13 → body 11 wraps "first line second line"
        // after "first line", and selecting everything copies the one-line text unchanged.
        let entries = [entry("first line second line")];
        let sel = Selection {
            anchor: locate(&entries, width, 0, 2).expect("line 0 start"),
            head: locate(&entries, width, 1, 200).expect("line 1 end"),
        };
        assert_eq!(logical_text(&entries, &sel), "first line second line");
        // The text's own newline survives: the same width gives "first line" / "second line"
        // for the two-paragraph text.
        let entries = [entry("first line\nsecond line")];
        let sel = Selection {
            anchor: locate(&entries, width, 0, 2).expect("line 0"),
            head: locate(&entries, width, 1, 200).expect("line 1"),
        };
        assert_eq!(logical_text(&entries, &sel), "first line\nsecond line");
        // Halfway across a soft wrap: width 12 → body 10 gives "first line" / "second" /
        // "line"; from "second" (line 1) to the end (line 2) the wrap in between contributes
        // nothing.
        let entries = [entry("first line second line")];
        let sel = Selection {
            anchor: locate(&entries, 12, 1, 2).expect("line 1"),
            head: locate(&entries, 12, 2, 200).expect("line 2"),
        };
        assert_eq!(logical_text(&entries, &sel), "second line");
    }

    #[test]
    fn entries_are_joined_by_a_newline_and_prefixes_are_never_copied() {
        let entries = [
            Entry::new(EntryKind::User, "asked"),
            entry("answered"),
        ];
        let sel = Selection {
            anchor: locate(&entries, 20, 0, 2).expect("user start"),
            head: locate(&entries, 20, 1, 200).expect("assistant end"),
        };
        assert_eq!(logical_text(&entries, &sel), "asked\nanswered");
        // A one-entry slice skips the other entirely: the user prefix is two cells, so
        // columns 2..5 (body 0..3) are "ask".
        let sel = Selection {
            anchor: locate(&entries, 20, 0, 2).expect("user"),
            head: locate(&entries, 20, 0, 5).expect("user"),
        };
        assert_eq!(logical_text(&entries, &sel), "ask");
        assert_eq!(
            logical_text(&entries, &Selection { anchor: sel.anchor, head: sel.anchor }),
            ""
        );
    }

    #[test]
    fn a_folded_attachment_is_copied_as_displayed() {
        // The entry is built exactly as `App::push_message` builds one: the attachment part
        // shows as its `[file: …]` fold, never the content.
        let mut app = crate::app::App::new("m", "s");
        let message = Message::User {
            content: vec![
                ContentPart::text("look at this"),
                ContentPart::attachment("l1\nl2\nl3", "notes.txt"),
            ],
            meta: MessageMeta::default(),
        };
        app.push_message(&message);
        let entries = &app.entries;
        // One user entry: "look at this\n\n[file: notes.txt, 3 lines]" — three display lines
        // (the text, the join's empty line, the fold). Selecting all of it copies the fold.
        let sel = Selection {
            anchor: locate(entries, 40, 0, 2).expect("start"),
            head: locate(entries, 40, 2, 200).expect("end"),
        };
        let copied = logical_text(entries, &sel);
        assert!(copied.contains("look at this"), "{copied:?}");
        assert!(copied.contains("[file: notes.txt, 3 lines]"), "{copied:?}");
        assert!(!copied.contains("l2"), "the hidden content: {copied:?}");
    }

    #[test]
    fn highlight_rows_reports_the_selected_cells_including_the_prefix() {
        let entries = [entry("hello world")];
        let width = 20;
        // Columns 2..8: the assistant prefix is two cells, so the body columns 0..6 —
        // "hello w" — report as screen columns 2..8.
        let sel = Selection {
            anchor: locate(&entries, width, 0, 2).expect("col 2"),
            head: locate(&entries, width, 0, 8).expect("col 8"),
        };
        let rows = highlight_rows(&entries, width, 0, 1, &sel);
        assert_eq!(rows, vec![vec![(2, 8)]]);
        // Backwards selection normalizes to the same cells.
        let rev = Selection { anchor: sel.head, head: sel.anchor };
        assert_eq!(highlight_rows(&entries, width, 0, 1, &rev), rows);
    }

    #[test]
    fn highlight_rows_covers_whole_lines_of_a_multi_line_selection() {
        let entries = [entry("aaaa bbbb cccc dddd")];
        // Body width 10: "aaaa bbbb" / "cccc dddd".
        let width = 12;
        // From 'b' (line 0 col 7 → byte 5) through the second 'c' (line 1 col 4 → byte 12):
        // selected are "bbbb c".
        let sel = Selection {
            anchor: locate(&entries, width, 0, 7).expect("'b'"),
            head: locate(&entries, width, 1, 4).expect("'c'"),
        };
        let rows = highlight_rows(&entries, width, 0, 2, &sel);
        assert_eq!(rows, vec![vec![(7, 11)], vec![(2, 4)]]);
        // A window scrolled past the selection reports no rows at all.
        assert!(highlight_rows(&entries, width, 2, 3, &sel).is_empty());
        // And the row above it is simply not highlighted.
        assert_eq!(highlight_rows(&entries, width, 1, 1, &sel), vec![vec![(2, 4)]]);
    }

    #[test]
    fn a_selection_survives_a_reflow_of_the_text_around_it() {
        // The anchors are (entry, offset): narrowing the width moves the cells, not the copy.
        let entries = [entry("aaaa bbbb cccc")];
        // Wide: one line, columns 2..10 → bytes 0..8 ("aaaa bbb").
        let wide = Selection {
            anchor: locate(&entries, 30, 0, 2).expect("wide"),
            head: locate(&entries, 30, 0, 10).expect("wide"),
        };
        // Narrow: body width 6 → "aaaa b" / "bbb c" / "cccc"; the same bytes are line 0 from
        // column 2 through line 1's column 4.
        let narrow = Selection {
            anchor: locate(&entries, 8, 0, 2).expect("narrow, same text"),
            head: locate(&entries, 8, 1, 4).expect("narrow, same text"),
        };
        assert_eq!(logical_text(&entries, &narrow), logical_text(&entries, &wide));
        assert_eq!(logical_text(&entries, &wide), "aaaa bbb");
    }
}
