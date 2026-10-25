//! In-transcript search (Phase 5): the find box on its overlay, the match recomputation on a
//! resize or an edit, the incremental typing and backspace, the fold override that expands a
//! hit, and the jump that centers the view on the current match.

use super::*;

impl App {
    /// The open find box, if any.
    pub fn find_state(&self) -> Option<&FindState> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Find(find)) => Some(find),
            _ => None,
        })
    }

    fn find_mut(&mut self) -> Option<&mut FindState> {
        self.layers.iter_mut().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Find(find)) => Some(find),
            _ => None,
        })
    }

    /// Opens the find box anchored at the current viewport top (`Ctrl-F`). Refused while a
    /// modal prompt is up.
    pub fn find_open(&mut self) {
        if self.has_modal() || self.find_state().is_some() {
            return;
        }
        let anchored = self.session.scroll;
        self.push_overlay(Overlay::Find(FindState::open(anchored)));
    }

    /// Closes the find box; the viewport stays where it is (`Esc`).
    pub fn find_close(&mut self) {
        self.remove_overlay(|overlay| matches!(overlay, Overlay::Find(_)));
    }

    /// The earlier match (`Enter`, or `Ctrl-F` again).
    pub fn find_prev(&mut self) {
        if let Some(find) = self.find_mut() {
            find.prev();
        }
        self.find_jump();
    }

    /// The later match (`Shift-Enter`, `Ctrl-G`).
    pub fn find_next(&mut self) {
        if let Some(find) = self.find_mut() {
            find.next();
        }
        self.find_jump();
    }

    pub(super) fn find_recompute(&mut self) {
        let width = self.session.transcript_width;
        // The image runs depend on the transcript area's height (ADR 0028 R10), so the match
        // geometry does too.
        let height = usize::from(self.session.view_height);
        // Split the borrow: the entries to match (the session view) and the layer that holds
        // the query (the app chrome) are two different fields.
        let modes = self.display_modes();
        let entries = &self.session.entries;
        if let Some(find) = self.layers.iter_mut().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Find(find)) => Some(find),
            _ => None,
        }) {
            find.recompute(entries.as_slice(), modes, width, height);
        }
        self.find_jump();
    }

    pub(super) fn find_push(&mut self, c: char) {
        if let Some(find) = self.find_mut() {
            find.query.push(c);
        }
        self.find_recompute();
    }

    pub(super) fn find_backspace(&mut self) {
        if let Some(find) = self.find_mut() {
            find.query.pop();
        }
        self.find_recompute();
    }

    /// Moves a selection's anchors after an entry was inserted at `from` (the tool result that
    /// belongs right after its call, rule 4): the anchors name entries by index, so an
    /// insertion must carry them along or a selection would point at the wrong entry.
    pub(super) fn shift_anchors(&mut self, from: usize) {
        if let Some(selection) = &mut self.session.selection {
            for anchor in [&mut selection.anchor, &mut selection.head] {
                if anchor.entry >= from {
                    anchor.entry += 1;
                }
            }
        }
    }

    /// Drops every per-entry fold override (ADR 0028 R6): a global fold toggle moots them.
    pub(super) fn clear_fold_overrides(&mut self) {
        for entry in self.session.entries.iter_mut() {
            entry.fold_override = None;
        }
    }

    /// Centers the current match in the viewport and stops following the bottom. A search
    /// that matches nothing leaves the viewport alone.
    ///
    /// ADR 0028 R6: when the match is in a **folded** entry, exactly that entry opens (a
    /// per-entry override), the others stay folded, and the status bar says so. The override
    /// stays until the next global fold toggle: collapsing the hit the moment the box closes
    /// would take the text the user just searched for away again.
    fn find_jump(&mut self) {
        let Some((entry, line)) = self
            .find_state()
            .and_then(FindState::current)
            .map(|m| (m.entry, m.line))
        else {
            return;
        };
        if entry >= self.session.entries.len() {
            return;
        }
        let modes = self.display_modes();
        // Does this entry's display hide content? (Its count can coincide with the expanded
        // one and still hide the text - a one-line thinking channel does exactly that.)
        let folded = crate::blocks::is_folded(&self.session.entries[entry], modes);
        if folded {
            // Exactly one override at a time.
            for other in self.session.entries.iter_mut() {
                other.fold_override = None;
            }
            self.session.entries[entry].fold_override = Some(true);
            self.session.note = Some("expanded 1 folded entry for the match".to_string());
        }
        // The display line of the match: the expanded line index only maps onto the display
        // when the entries above it are laid out folded, so walk the display counts (the
        // entry itself is measured with its override, so its own lines line up).
        let modes = self.display_modes();
        let width = self.session.transcript_width;
        let height = usize::from(self.session.view_height);
        let mut display_line = 0usize;
        for item in self.session.entries[..entry].iter_mut() {
            display_line += crate::blocks::line_count_cached(item, modes, width, height);
        }
        let line = display_line + line;
        let height = usize::from(self.session.view_height.max(1));
        self.session.scroll = crate::find::centered_scroll(line, height, self.session.max_scroll);
        self.session.follow = false;
    }
}
