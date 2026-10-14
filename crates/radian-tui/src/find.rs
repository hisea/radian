//! In-transcript search (Phase 5): the pure matching and jump-position state behind `Ctrl-F`.
//!
//! Matching is a case-insensitive substring over the **displayed** text: the same wrapped lines
//! the renderer shows, so a user message's folded `@` attachment (`[file: path, N lines]`) is
//! searched, not the file content it hides. A match is one wrapped display line.
//!
//! Search walks **upward**: opening the box records the current viewport top as the anchor, and
//! the first query picks the nearest match above it (like a terminal's reverse scrollback
//! search). `prev` walks to earlier matches, `next` back toward later ones, and neither wraps.

use crate::app::Entry;
use crate::render::display_lines;
use crate::tools::{DisplayModes, ToolFold};

/// One matching wrapped display line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchLine {
    /// The entry's index in `App::entries`.
    pub entry: usize,
    /// The display line within that entry (0-based).
    pub line: usize,
}

/// The open find box's state. `matches` and `starts` are computed once per query change, never
/// per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindState {
    pub query: String,
    /// The matching display lines, in transcript order.
    pub matches: Vec<MatchLine>,
    /// The absolute transcript line each entry starts at, at `width`. Indexed by entry.
    pub starts: Vec<usize>,
    /// The current match (an index into `matches`), or `None` before a query resolves.
    pub cursor: Option<usize>,
    /// The viewport top when the box was opened; the first query searches above it.
    pub anchor: usize,
    /// The width `matches` and `starts` were computed at.
    pub width: usize,
}

impl FindState {
    /// Opens a find box anchored at the current viewport top.
    pub fn open(anchor: usize) -> Self {
        Self {
            query: String::new(),
            matches: Vec::new(),
            starts: Vec::new(),
            cursor: None,
            anchor,
            width: 0,
        }
    }

    /// Recomputes the matches and the current match after the query or the width changed.
    pub fn recompute(&mut self, entries: &[Entry], modes: DisplayModes, width: usize) {
        let (matches, starts) = compute(entries, modes, width, &self.query);
        self.matches = matches;
        self.starts = starts;
        self.width = width;
        // The nearest match above the anchor, or the first match when none is above (a search
        // opened at the top of the transcript).
        self.cursor = self.nearest_above();
    }

    fn nearest_above(&self) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        self.matches
            .iter()
            .rposition(|m| self.starts[m.entry] + m.line < self.anchor)
            .or(Some(0))
    }

    /// The absolute transcript line of the current match.
    pub fn current_line(&self) -> Option<usize> {
        let cursor = self.cursor?;
        let m = self.matches.get(cursor)?;
        Some(self.starts[m.entry] + m.line)
    }

    /// The earlier match (keyboard: `Enter`, `Ctrl-F` again). Does not wrap.
    pub fn prev(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.cursor = Some(match self.cursor {
            Some(0) | None => 0,
            Some(i) => i - 1,
        });
    }

    /// The later match (keyboard: `Shift-Enter`, `Ctrl-G`). Does not wrap.
    pub fn next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() - 1;
        self.cursor = Some(match self.cursor {
            Some(i) if i < last => i + 1,
            Some(i) => i,
            None => 0,
        });
    }

    /// The status-bar text: `match i/N`, `no match`, or `None` while the query is empty.
    pub fn status(&self) -> Option<String> {
        if self.query.is_empty() {
            return None;
        }
        match self.cursor {
            Some(i) if !self.matches.is_empty() => {
                Some(format!("match {}/{}", i + 1, self.matches.len()))
            }
            _ => Some("no match".to_string()),
        }
    }
}

/// Every wrapped display line of `entries` at `width` that contains `query`
/// (case-insensitive), plus the absolute start line of each entry. One pass, one wrap per
/// entry; an empty query matches nothing (but the starts are still filled for the viewport).
pub fn compute(
    entries: &[Entry],
    modes: DisplayModes,
    width: usize,
    query: &str,
) -> (Vec<MatchLine>, Vec<usize>) {
    let needle = query.to_lowercase();
    let mut matches = Vec::new();
    let mut starts = Vec::with_capacity(entries.len());
    let mut line = 0usize;
    for (entry, item) in entries.iter().enumerate() {
        starts.push(line);
        // Folded tool bodies must not vanish from the search (this slice): the match
        // geometry is the **expanded** form. Slice 4's auto-expand will line the jump up.
        let mut modes = modes;
        modes.tools.fold = ToolFold::Expanded;
        let lines = display_lines(item, modes, width);
        if !needle.is_empty() {
            for (index, text) in lines.iter().enumerate() {
                if text.to_lowercase().contains(&needle) {
                    matches.push(MatchLine { entry, line: index });
                }
            }
        }
        line += lines.len();
    }
    (matches, starts)
}

/// The viewport top that centers `line` in a window `height` lines tall, clamped to the
/// transcript's scroll range.
pub fn centered_scroll(line: usize, height: usize, max_scroll: usize) -> usize {
    line.saturating_sub(height / 2).min(max_scroll)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::ReasoningMode;

    /// Both folds expanded, so a test's geometry does not depend on the fold defaults.
    const EXPANDED: DisplayModes = DisplayModes {
        reasoning: ReasoningMode::Expanded,
        tools: crate::tools::ToolDisplay {
            fold: ToolFold::Expanded,
            preview_lines: 5,
        },
        mermaid: true,
    };
    use crate::app::{Entry, EntryKind};

    fn entry(kind: EntryKind, text: &str) -> Entry {
        Entry::new(kind, text)
    }

    #[test]
    fn matching_is_case_insensitive_and_per_line() {
        let entries = vec![
            entry(EntryKind::User, "Hello World"),
            entry(EntryKind::Assistant, "nothing here"),
            entry(EntryKind::Notice, "say hello\nand HELLO again"),
        ];
        let (matches, starts) = compute(&entries, EXPANDED, 80, "hello");
        // The user entry's block has a padding row above and below (ADR 0028 R1): its text
        // line is line 1 of its three, and every later entry's start shifts by it.
        assert_eq!(
            matches,
            vec![
                MatchLine { entry: 0, line: 1 },
                MatchLine { entry: 2, line: 0 },
                MatchLine { entry: 2, line: 1 },
            ]
        );
        assert_eq!(starts, vec![0, 3, 4]);
        // The third entry's two lines start at 4 and 5.
        assert_eq!(starts[2], 4);
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        let entries = vec![entry(EntryKind::User, "hello")];
        let (matches, starts) = compute(&entries, EXPANDED, 80, "");
        assert!(matches.is_empty());
        assert_eq!(starts, vec![0]);
    }

    #[test]
    fn a_folded_attachment_is_matched_by_its_display_text() {
        let text = "<file path=\"big.rs\">\nSECRET-INSIDE\n</file>\nplease review";
        let entries = vec![entry(EntryKind::User, text)];
        // The folded display text has `[file: big.rs, 2 lines]`, not the hidden content.
        assert!(compute(&entries, EXPANDED, 80, "review").0.len() == 1);
        assert!(
            compute(&entries, EXPANDED, 80, "SECRET-INSIDE")
                .0
                .is_empty(),
            "the folded file content must not be searched"
        );
        assert_eq!(compute(&entries, EXPANDED, 80, "big.rs").0.len(), 1);
    }

    #[test]
    fn navigation_does_not_wrap_and_anchors_above() {
        let entries = vec![
            entry(EntryKind::User, "hit one"),
            entry(EntryKind::Assistant, "filler"),
            entry(EntryKind::User, "hit two"),
        ];
        let mut state = FindState::open(100); // anchor below everything
        state.query = "hit".into();
        state.recompute(&entries, EXPANDED, 80);
        assert_eq!(state.matches.len(), 2);
        // Nearest above the anchor is the last match.
        assert_eq!(state.cursor, Some(1));
        assert_eq!(state.status().as_deref(), Some("match 2/2"));
        state.prev();
        assert_eq!(state.cursor, Some(0));
        assert_eq!(state.status().as_deref(), Some("match 1/2"));
        state.prev();
        assert_eq!(state.cursor, Some(0), "no wrap past the first");
        state.next();
        state.next();
        assert_eq!(state.cursor, Some(1), "no wrap past the last");
    }

    #[test]
    fn centering_clamps_to_the_scroll_range() {
        assert_eq!(centered_scroll(0, 10, 100), 0);
        assert_eq!(centered_scroll(50, 10, 100), 45);
        assert_eq!(centered_scroll(1000, 10, 100), 100);
    }
}
