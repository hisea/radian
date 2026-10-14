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
use crate::blocks::{self};
use crate::render::{display_text, marker_prefix};
use crate::tools::DisplayModes;

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

/// Where a press or drag landed: the anchor, and whether the pointer is **on a char's cell**
/// (rather than in the gap past a line's last cell, where nothing is under it). The moving
/// end of a selection extends through the char under the pointer — native-selection
/// semantics: dragging across "abc" copies "abc", not "ab".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub anchor: Anchor,
    pub on_char: bool,
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
pub fn locate(
    entries: &[Entry],
    modes: DisplayModes,
    width: usize,
    line: usize,
    col: usize,
) -> Option<Anchor> {
    locate_hit(entries, modes, width, line, col).map(|hit| hit.anchor)
}

/// [`locate`], plus whether the pointer sits on a char's cell ([`Hit`]).
pub fn locate_hit(
    entries: &[Entry],
    modes: DisplayModes,
    width: usize,
    line: usize,
    col: usize,
) -> Option<Hit> {
    let mut remaining = line;
    for (index, entry) in entries.iter().enumerate() {
        // The container's gap line before this entry (ADR 0028 rule 1) belongs to
        // the **previous** entry's end: a selection dragged across it copies the newline
        // between the two entries, exactly as a trailing blank line of the previous text did.
        let gap = crate::blocks::block_gap(entries.get(index.wrapping_sub(1)), entry);
        if remaining < gap {
            let previous = index.checked_sub(1)?;
            let text_len = display_text(&entries[previous]).len();
            return Some(Hit {
                anchor: Anchor {
                    entry: previous,
                    offset: text_len,
                },
                on_char: false,
            });
        }
        remaining -= gap;
        let count = body_lines(entry, modes, width).len();
        if remaining >= count {
            remaining -= count;
            continue;
        }
        return Some(locate_in_entry(entry, modes, width, remaining, col, index));
    }
    // Past the last entry: the end of the last one (or `None` with no transcript) — a gap.
    let last = entries.len().checked_sub(1)?;
    let text_len = display_text(&entries[last]).len();
    Some(Hit {
        anchor: Anchor {
            entry: last,
            offset: text_len,
        },
        on_char: false,
    })
}

/// Maps a cell inside one entry to a hit. `local_line` is `0`-based within the entry's
/// wrapped lines (clamped), `col` a transcript display column (the `> ` prefix's cells are
/// part of the line, so a click on the marker lands on the line's first char — selecting
/// through it, as a drag to the margin does).
fn locate_in_entry(
    entry: &Entry,
    modes: DisplayModes,
    width: usize,
    local_line: usize,
    col: usize,
    index: usize,
) -> Hit {
    let lines = body_lines(entry, modes, width);
    let line = &lines[local_line.min(lines.len() - 1)];
    let prefix = marker_prefix(entry.kind).width();
    let body_col = col.saturating_sub(prefix);
    // The drawn char of each cell, and its source offset (`None` for a synthesized cell: a
    // bullet, a heading's `##`, a link's `(url)`, an indent). A click on a synthesized cell
    // anchors at the next source offset in the line (or the line's end gap), so it never
    // selects source text that is not there - and never indexes into a multi-byte char.
    let cells: Vec<(char, Option<usize>)> = line
        .text
        .chars()
        .zip(&line.chars)
        .map(|(ch, source)| (ch, *source))
        .collect();
    let mut next = vec![line.end; cells.len()];
    let mut following = line.end;
    for i in (0..cells.len()).rev() {
        if let Some(offset) = cells[i].1 {
            following = offset;
        }
        next[i] = following;
    }
    // Walk the line's chars accumulating display cells; the click lands on the char whose
    // cell range covers `body_col` (the second cell of a wide char selects that char).
    let mut used = 0usize;
    for (i, (ch, source)) in cells.iter().enumerate() {
        let w = ch.width().unwrap_or(0);
        if body_col < used + w.max(1) {
            return Hit {
                anchor: Anchor {
                    entry: index,
                    offset: source.unwrap_or(next[i]),
                },
                on_char: source.is_some(),
            };
        }
        used += w;
    }
    // Past the line's last cell: the line's end gap (before the paragraph's own newline).
    Hit {
        anchor: Anchor {
            entry: index,
            offset: line.end,
        },
        on_char: false,
    }
}

/// The entry's laid-out body lines — the one geometry the selection mapping uses (the same
/// [`crate::blocks::BodyLine`]s the renderer renders).
fn body_lines(entry: &Entry, modes: DisplayModes, width: usize) -> Vec<crate::blocks::BodyLine> {
    let blocks = blocks::parse_blocks_for(entry, modes.reasoning);
    blocks::layout(entry, &blocks, modes, width)
}

/// The char starting at `offset`, or `None` when `offset` is not a char boundary. Offsets
/// always are one (they come from the layout mapping), but a mapping bug must never panic the
/// UI: this skips the cell instead.
fn char_at(text: &str, offset: usize) -> Option<char> {
    debug_assert!(
        text.is_char_boundary(offset),
        "offset {offset} is not a char boundary"
    );
    text.get(offset..).and_then(|rest| rest.chars().next())
}

/// The largest char-boundary substring of `text` inside `[start, end)`; an offset that is not
/// a boundary is snapped back to one instead of panicking.
fn slice(text: &str, start: usize, end: usize) -> &str {
    let mut a = start.min(text.len());
    let mut b = end.min(text.len());
    debug_assert!(
        text.is_char_boundary(a) && text.is_char_boundary(b),
        "selection offsets {a}..{b} are not char boundaries"
    );
    while a > 0 && !text.is_char_boundary(a) {
        a -= 1;
    }
    while b > 0 && !text.is_char_boundary(b) {
        b -= 1;
    }
    if a < b { &text[a..b] } else { "" }
}

/// The byte length of the char at `anchor`'s offset in its entry's display text; `None` at
/// or past the text's end.
pub fn char_len_at(entries: &[Entry], anchor: Anchor) -> Option<usize> {
    let text = display_text(entries.get(anchor.entry)?);
    char_at(&text, anchor.offset).map(char::len_utf8)
}

/// An entry's display-text length in bytes (what a triple-click selects: offsets `0..len`).
pub fn entry_display_len(entries: &[Entry], index: usize) -> usize {
    entries
        .get(index)
        .map(|entry| display_text(entry).len())
        .unwrap_or(0)
}

/// The run of similar chars around a click: a word (non-whitespace) when the pointer is on
/// one, the whitespace run when it is on a space. Both ends are `(start, end after the last
/// char)` — a ready normalized selection range. A click on the prefix or past a line's end
/// expands from the char the gap maps to, exactly like a drag from there would.
pub fn run_at(
    entries: &[Entry],
    modes: DisplayModes,
    width: usize,
    line: usize,
    col: usize,
) -> Option<(Anchor, Anchor)> {
    let hit = locate_hit(entries, modes, width, line, col)?;
    let anchor = hit.anchor;
    let text = display_text(entries.get(anchor.entry)?);
    if anchor.offset >= text.len() {
        return None;
    }
    let ch = char_at(&text, anchor.offset)?;
    let in_word = |c: char| c != ' ' && c != '\n';
    let want = in_word(ch);
    // Walk out from the clicked char while the run keeps its kind: the word it sits in, or
    // the whitespace run (which may cross the entry's own newlines).
    let mut start = anchor.offset;
    while start > 0 {
        let Some(prev) = text[..start].chars().next_back() else {
            break;
        };
        if in_word(prev) != want {
            break;
        }
        start -= prev.len_utf8();
    }
    let mut end = anchor.offset + ch.len_utf8();
    while let Some(next) = text[end..].chars().next() {
        if in_word(next) != want {
            break;
        }
        end += next.len_utf8();
    }
    Some((
        Anchor {
            entry: anchor.entry,
            offset: start,
        },
        Anchor {
            entry: anchor.entry,
            offset: end,
        },
    ))
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
        return slice(&text, start.offset, end.offset).to_owned();
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
            out.push_str(slice(&text, start.offset, text.len()));
        } else if index == end.entry {
            out.push_str(slice(&text, 0, end.offset));
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
    modes: DisplayModes,
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
        // The container's gap line before this entry: it is never highlighted (it carries no
        // text), but it occupies a row.
        let gap = crate::blocks::block_gap(entries.get(index.wrapping_sub(1)), entry);
        if line + gap > scroll {
            for _ in scroll.saturating_sub(line)..gap {
                if out.len() >= height {
                    break;
                }
                out.push(Vec::new());
            }
        }
        line += gap;
        let count = body_lines(entry, modes, width).len();
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
                    row_ranges(entry, modes, width, local, index, start, end)
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
#[allow(clippy::too_many_arguments)]
fn row_ranges(
    entry: &Entry,
    modes: DisplayModes,
    width: usize,
    local: usize,
    index: usize,
    start: Anchor,
    end: Anchor,
) -> Vec<(usize, usize)> {
    let prefix = marker_prefix(entry.kind).width();
    let lines = body_lines(entry, modes, width);
    let Some(line) = lines.get(local) else {
        return Vec::new();
    };
    // Each cell's drawn char and its source offset (`None` for a synthesized cell). A cell is
    // selected when the source text **around** it is selected: for a source cell that is its
    // own offset; for a synthesized cell it is the source boundary before and after it, so a
    // bullet inside a selection highlights but one that starts the selection does not.
    let cells: Vec<(char, Option<usize>)> = line
        .text
        .chars()
        .zip(&line.chars)
        .map(|(ch, source)| (ch, *source))
        .collect();
    let n = cells.len();
    let mut before = vec![line.end; n];
    let mut after = vec![line.end; n];
    let mut last_end: Option<usize> = None;
    let mut first_offset: Option<usize> = None;
    for i in 0..n {
        match cells[i].1 {
            Some(offset) => {
                before[i] = offset;
                last_end = Some(offset + cells[i].0.len_utf8());
                first_offset.get_or_insert(offset);
            }
            None => before[i] = last_end.or(first_offset).unwrap_or(line.end),
        }
    }
    let mut following = line.end;
    for i in (0..n).rev() {
        if let Some(offset) = cells[i].1 {
            following = offset;
        }
        after[i] = following;
    }
    let in_range = |x: usize| {
        index >= start.entry
            && index <= end.entry
            && (index > start.entry || x >= start.offset)
            && (index < end.entry || x < end.offset)
    };
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut used = prefix;
    for i in 0..n {
        let w = cells[i].0.width().unwrap_or(0);
        if in_range(before[i]) && in_range(after[i]) {
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
    use crate::blocks::ReasoningMode;
    use crate::tools::{ToolDisplay, ToolFold};

    /// Both folds expanded, so a test's geometry does not depend on the fold defaults.
    const EXPANDED: DisplayModes = DisplayModes {
        reasoning: ReasoningMode::Expanded,
        tools: ToolDisplay {
            fold: ToolFold::Expanded,
            preview_lines: 5,
        },
        mermaid: true,
        highlight: true,
    };
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
        let start = locate(&entries, EXPANDED, width, 0, 4).expect("inside");
        assert_eq!(start.offset, 2);
        // Width 7 → body 5: "hello" / "world". Line 1 col 0 is 'w' (byte 6): the soft wrap
        // inserted no char.
        let width = 7;
        let w = locate(&entries, EXPANDED, width, 1, 0).expect("second line");
        assert_eq!(w.offset, 6);
        // Clicking past the end of line 0 lands in the gap before the space run.
        let eol = locate(&entries, EXPANDED, width, 0, 20).expect("clamped");
        assert_eq!(eol.offset, 5, "the gap after 'hello'");
    }

    #[test]
    fn a_wide_char_selects_from_either_of_its_two_cells() {
        let entries = [entry("日本語")];
        let width = 10; // body 8
        let first_cell = locate(&entries, EXPANDED, width, 0, 2).expect("body col 0");
        assert_eq!(first_cell.offset, 0);
        let second_cell =
            locate(&entries, EXPANDED, width, 0, 3).expect("body col 1: 日's right half");
        assert_eq!(second_cell.offset, 0, "both cells select 日");
        let next = locate(&entries, EXPANDED, width, 0, 4).expect("body col 2: 本");
        assert_eq!(next.offset, 3, "本 starts at byte 3");
    }

    #[test]
    fn an_empty_line_maps_to_its_paragraph_boundary() {
        let entries = [entry("a\n\nb")];
        let width = 10;
        // Lines: "a", "", "b". Markdown lays the blank line at the paragraph block's end
        // (ADR 0028 R2: the block ranges tile the text, so the trailing newline's gap is the
        // block end), which is byte 3 here.
        let empty = locate(&entries, EXPANDED, width, 1, 4).expect("the empty line");
        assert_eq!(empty.offset, 3);
        let b = locate(&entries, EXPANDED, width, 2, 0).expect("'b'");
        assert_eq!(b.offset, 3);
    }

    #[test]
    fn clicking_the_prefix_or_past_the_text_clamps_safely() {
        let entries = [Entry::new(EntryKind::User, "user text")];
        let width = 20;
        // The user prefix "> " is two cells: column 0 and 1 both land on the first char.
        assert_eq!(
            locate(&entries, EXPANDED, width, 0, 0)
                .expect("prefix")
                .offset,
            0
        );
        assert_eq!(
            locate(&entries, EXPANDED, width, 0, 1)
                .expect("prefix")
                .offset,
            0
        );
        assert_eq!(
            locate(&entries, EXPANDED, width, 0, 2)
                .expect("body")
                .offset,
            0
        );
        // Far below and right of everything: the end of the last entry.
        assert_eq!(
            locate(&entries, EXPANDED, width, 50, 200)
                .expect("clamped")
                .offset,
            "user text".len()
        );
        assert_eq!(locate(&[], EXPANDED, width, 0, 0), None);
    }

    #[test]
    fn logical_text_keeps_original_newlines_and_drops_none() {
        let width = 13;
        // A soft wrap inserts nothing: width 13 → body 11 wraps "first line second line"
        // after "first line", and selecting everything copies the one-line text unchanged.
        let entries = [entry("first line second line")];
        let sel = Selection {
            anchor: locate(&entries, EXPANDED, width, 0, 2).expect("line 0 start"),
            head: locate(&entries, EXPANDED, width, 1, 200).expect("line 1 end"),
        };
        assert_eq!(logical_text(&entries, &sel), "first line second line");
        // The text's own newline survives: the same width gives "first line" / "second line"
        // for the two-paragraph text.
        let entries = [entry("first line\nsecond line")];
        let sel = Selection {
            anchor: locate(&entries, EXPANDED, width, 0, 2).expect("line 0"),
            head: locate(&entries, EXPANDED, width, 1, 200).expect("line 1"),
        };
        assert_eq!(logical_text(&entries, &sel), "first line\nsecond line");
        // Halfway across a soft wrap: width 12 → body 10 gives "first line" / "second" /
        // "line"; from "second" (line 1) to the end (line 2) the wrap in between contributes
        // nothing.
        let entries = [entry("first line second line")];
        let sel = Selection {
            anchor: locate(&entries, EXPANDED, 12, 1, 2).expect("line 1"),
            head: locate(&entries, EXPANDED, 12, 2, 200).expect("line 2"),
        };
        assert_eq!(logical_text(&entries, &sel), "second line");
    }

    #[test]
    fn entries_are_joined_by_a_newline_and_prefixes_are_never_copied() {
        let entries = [Entry::new(EntryKind::User, "asked"), entry("answered")];
        // A user block is pad / text / pad, so the text line is 1 and the assistant's line
        // is 3.
        let sel = Selection {
            anchor: locate(&entries, EXPANDED, 20, 1, 2).expect("user start"),
            head: locate(&entries, EXPANDED, 20, 3, 200).expect("assistant end"),
        };
        assert_eq!(logical_text(&entries, &sel), "asked\nanswered");
        // A one-entry slice skips the other entirely: the user prefix is two cells, so
        // columns 2..5 (body 0..3) are "ask".
        let sel = Selection {
            anchor: locate(&entries, EXPANDED, 20, 1, 2).expect("user"),
            head: locate(&entries, EXPANDED, 20, 1, 5).expect("user"),
        };
        assert_eq!(logical_text(&entries, &sel), "ask");
        assert_eq!(
            logical_text(
                &entries,
                &Selection {
                    anchor: sel.anchor,
                    head: sel.anchor
                }
            ),
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
        // One user entry: "look at this\n\n[file: notes.txt, 3 lines]" — the fold plus its
        // padding rows. Line 1 is the text, line 3 the fold; selecting both copies them.
        let sel = Selection {
            anchor: locate(entries, EXPANDED, 40, 1, 2).expect("start"),
            head: locate(entries, EXPANDED, 40, 3, 200).expect("end"),
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
            anchor: locate(&entries, EXPANDED, width, 0, 2).expect("col 2"),
            head: locate(&entries, EXPANDED, width, 0, 8).expect("col 8"),
        };
        let rows = highlight_rows(&entries, EXPANDED, width, 0, 1, &sel);
        assert_eq!(rows, vec![vec![(2, 8)]]);
        // Backwards selection normalizes to the same cells.
        let rev = Selection {
            anchor: sel.head,
            head: sel.anchor,
        };
        assert_eq!(highlight_rows(&entries, EXPANDED, width, 0, 1, &rev), rows);
    }

    #[test]
    fn highlight_rows_covers_whole_lines_of_a_multi_line_selection() {
        let entries = [entry("aaaa bbbb cccc dddd")];
        // Body width 10: "aaaa bbbb" / "cccc dddd".
        let width = 12;
        // From 'b' (line 0 col 7 → byte 5) through the second 'c' (line 1 col 4 → byte 12):
        // selected are "bbbb c".
        let sel = Selection {
            anchor: locate(&entries, EXPANDED, width, 0, 7).expect("'b'"),
            head: locate(&entries, EXPANDED, width, 1, 4).expect("'c'"),
        };
        let rows = highlight_rows(&entries, EXPANDED, width, 0, 2, &sel);
        assert_eq!(rows, vec![vec![(7, 12)], vec![(2, 4)]]);
        // A window scrolled past the selection reports no rows at all.
        assert!(highlight_rows(&entries, EXPANDED, width, 2, 3, &sel).is_empty());
        // And the row above it is simply not highlighted.
        assert_eq!(
            highlight_rows(&entries, EXPANDED, width, 1, 1, &sel),
            vec![vec![(2, 4)]]
        );
    }

    #[test]
    fn a_selection_survives_a_reflow_of_the_text_around_it() {
        // The anchors are (entry, offset): narrowing the width moves the cells, not the copy.
        let entries = [entry("aaaa bbbb cccc")];
        // Wide: one line, columns 2..10 → bytes 0..8 ("aaaa bbb").
        let wide = Selection {
            anchor: locate(&entries, EXPANDED, 30, 0, 2).expect("wide"),
            head: locate(&entries, EXPANDED, 30, 0, 10).expect("wide"),
        };
        // Narrow: body width 6 → "aaaa b" / "bbb c" / "cccc"; the same bytes are line 0 from
        // column 2 through line 1's column 4.
        let narrow = Selection {
            anchor: locate(&entries, EXPANDED, 8, 0, 2).expect("narrow, same text"),
            head: locate(&entries, EXPANDED, 8, 1, 4).expect("narrow, same text"),
        };
        assert_eq!(
            logical_text(&entries, &narrow),
            logical_text(&entries, &wide)
        );
        assert_eq!(logical_text(&entries, &wide), "aaaa bbb");
    }
}
