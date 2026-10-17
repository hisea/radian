//! The transcript's viewport and the pointer: the viewport the renderer measures, the scroll
//! math (`scroll_by`, the wheel, the follow pin), the left-button selection gesture with its
//! double/triple-click detection and drag auto-scroll, and the geometry constants they share.

use super::*;

/// One mouse-wheel notch (ADR 0023 U2, plan B). The input thread maps crossterm's
/// `MouseEventKind::ScrollUp`/`ScrollDown` to this; no other mouse event reaches the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseScrollDirection {
    Up,
    Down,
}

/// How many transcript lines one wheel notch scrolls. A constant, not a magic number: the
/// wheel is a coarse gesture (a notch is not a line), and this is the one place to tune it.
pub const MOUSE_WHEEL_LINES: usize = 3;

/// How often the drag's auto-scroll ticks while the pointer is held at the transcript's top
/// or bottom edge. Only consulted while a drag is live (`recv_timeout` in the loop), so an
/// idle app still blocks and never redraws (ADR 0023 U2).
pub const AUTO_SCROLL_TICK: std::time::Duration = std::time::Duration::from_millis(50);

/// How quickly a second press counts as a double-click. Wall-clock (not frames): the loop
/// is idle between clicks, so there is nothing else to measure with.
pub const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

/// The last press, for double/triple-click detection: where and when, and how many presses
/// in a row landed on the same cell inside the window (1, 2 or 3; capped at 3).
#[derive(Debug, Clone, Copy)]
pub(super) struct LastClick {
    at: std::time::Instant,
    cell: (u16, u16),
    count: usize,
}

/// The live left-button gesture: the press cell, and where the pointer last was (the
/// auto-scroll edges are read off `last`). Screen rows/columns, `0`-based. `moved` says a
/// drag event actually moved the pointer, so a release on the press cell is a click (which
/// clears) and not a one-char selection. `anchor` is the press's located end, with whether
/// the pointer was on a char there (the end-side extension at release needs to know).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SelectDrag {
    press: (u16, u16),
    last: (u16, u16),
    moved: bool,
    /// The selection's fixed end, located once at press time (an anchor, not a cell, so
    /// scrolling during the drag cannot move it).
    anchor: Anchor,
    anchor_on_char: bool,
    /// How many presses began this gesture (1 click, 2 double, 3 triple): the release counts
    /// as a click — and clears — only for a plain 1.
    count: usize,
}

impl App {
    /// Records the transcript viewport. Called by the renderer with the measured transcript
    /// size, so scroll math works on wrapped lines without the app knowing how to wrap.
    pub fn set_viewport(&mut self, view_height: u16, total_lines: usize) {
        self.session.view_height = view_height;
        self.session.total_lines = total_lines;
        self.session.max_scroll = total_lines.saturating_sub(usize::from(view_height));
        if self.session.follow {
            self.session.scroll = self.session.max_scroll;
        } else {
            self.session.scroll = self.session.scroll.min(self.session.max_scroll);
        }
    }

    pub fn is_following(&self) -> bool {
        self.session.follow
    }

    pub(super) fn set_scroll(&mut self, next: usize) {
        self.session.scroll = next.min(self.session.max_scroll);
        self.session.follow = self.session.scroll >= self.session.max_scroll;
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let step = delta.unsigned_abs() as usize;
        let next = if delta < 0 {
            self.session.scroll.saturating_sub(step)
        } else {
            self.session.scroll.saturating_add(step)
        };
        self.set_scroll(next);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.session.scroll = self.session.max_scroll;
        self.session.follow = true;
    }

    /// The transcript's first line (`ctrl-home`), pausing the follow unless the whole
    /// transcript already fits. The read-only attach view (`crate::attach`) is the other
    /// caller of this scroll state; it holds an `App` for its entries and scroll only.
    pub fn scroll_to_top(&mut self) {
        self.set_scroll(0);
    }

    /// One mouse-wheel notch (ADR 0023 U2, plan B). It acts on the topmost **scrollable**
    /// layer: a modal consumes it (the approval prompt does not scroll), a picker overlay moves
    /// its selection, the `@` completion list consumes it (it has no scroll state of its own,
    /// and the transcript behind it must not move), and otherwise the transcript scrolls. The
    /// transcript path goes through the same [`Action::ScrollLineUp`]/[`Action::ScrollLineDown`]
    /// code as the keys, `MOUSE_WHEEL_LINES` lines per notch.
    pub fn mouse_scroll(&mut self, direction: MouseScrollDirection) -> UpdateResult {
        if self.has_modal() {
            return UpdateResult::none();
        }
        if self.picker().is_some() {
            let key = match direction {
                MouseScrollDirection::Up => KeyCode::Up,
                MouseScrollDirection::Down => KeyCode::Down,
            };
            // A list selection is discrete: one notch moves it one row (the transcript below
            // is the one that scrolls `MOUSE_WHEEL_LINES`).
            return self.picker_key(KeyEvent::new(key, KeyModifiers::NONE));
        }
        if self.completion().is_some() {
            return UpdateResult::none();
        }
        let action = match direction {
            MouseScrollDirection::Up => Action::ScrollLineUp,
            MouseScrollDirection::Down => Action::ScrollLineDown,
        };
        let mut dirty = false;
        for _ in 0..MOUSE_WHEEL_LINES {
            dirty |= self.update_action(action).dirty;
        }
        UpdateResult {
            dirty,
            effects: Vec::new(),
        }
    }

    // ---------- transcript selection (left-button drag) ----------

    /// Whether a left-button drag is live (the loop auto-scrolls only while it is).
    pub fn is_selecting(&self) -> bool {
        self.session.selecting.is_some()
    }

    /// The left button went down. Any press clears the previous selection (a later release
    /// without movement is the click that clears, per the contract); only a press **inside
    /// the transcript** starts a new one — never on the input box, the status bar, a modal
    /// prompt or the picker overlay, and never with `tui.mouse = false`. A press that starts
    /// nothing and clears nothing is not dirty: an idle screen is not redrawn (ADR 0023 U2).
    pub fn mouse_down(&mut self, row: u16, col: u16) -> UpdateResult {
        if !self.mouse_enabled {
            return UpdateResult::none();
        }
        let count = self.bump_click_count(row, col);
        let had = self.session.selection.take();
        let inside = !self.has_modal()
            && self.picker().is_none()
            && (row as usize) < usize::from(self.session.view_height)
            && self.session.transcript_width > 0;
        let cleared = had.is_some();
        if !inside {
            return cleared
                .then(UpdateResult::dirty)
                .unwrap_or(UpdateResult::none());
        }
        // The fixed end is located once, here: a scroll during the drag moves the view, not
        // the anchor. An empty transcript simply starts nothing.
        let press_line = self.session.scroll + row as usize;
        let Some(hit) = select::locate_hit(
            &self.session.entries,
            self.display_modes(),
            self.session.transcript_width,
            press_line,
            col as usize,
        ) else {
            return cleared
                .then(UpdateResult::dirty)
                .unwrap_or(UpdateResult::none());
        };
        // A multi-click replaces the just-cleared selection with its bigger range and
        // anchors the gesture at its start, so a following drag extends from the word (or
        // the entry).
        let (anchor, anchor_on_char) = match count {
            2 => match select::run_at(
                &self.session.entries,
                self.display_modes(),
                self.session.transcript_width,
                press_line,
                col as usize,
            ) {
                Some((start, end)) => {
                    self.session.selection = Some(Selection {
                        anchor: start,
                        head: end,
                    });
                    (start, true)
                }
                None => (hit.anchor, hit.on_char),
            },
            3 => {
                let len = select::entry_display_len(&self.session.entries, hit.anchor.entry);
                if len > 0 {
                    self.session.selection = Some(Selection {
                        anchor: Anchor {
                            entry: hit.anchor.entry,
                            offset: 0,
                        },
                        head: Anchor {
                            entry: hit.anchor.entry,
                            offset: len,
                        },
                    });
                    (
                        Anchor {
                            entry: hit.anchor.entry,
                            offset: 0,
                        },
                        true,
                    )
                } else {
                    (hit.anchor, hit.on_char)
                }
            }
            _ => (hit.anchor, hit.on_char),
        };
        self.session.selecting = Some(SelectDrag {
            press: (row, col),
            last: (row, col),
            moved: false,
            anchor,
            anchor_on_char,
            count,
        });
        cleared
            .then(UpdateResult::dirty)
            .unwrap_or(UpdateResult::none())
    }

    /// The press counter for double/triple-click: one more when the same cell is pressed
    /// again inside [`DOUBLE_CLICK_WINDOW`] (capped at 3), otherwise back to 1.
    fn bump_click_count(&mut self, row: u16, col: u16) -> usize {
        let next = match self.session.last_click {
            Some(click)
                if click.cell == (row, col) && click.at.elapsed() <= DOUBLE_CLICK_WINDOW =>
            {
                (click.count + 1).min(3)
            }
            _ => 1,
        };
        self.session.last_click = Some(LastClick {
            at: std::time::Instant::now(),
            cell: (row, col),
            count: next,
        });
        next
    }

    /// The left button moved while held: extend the selection to the pointer's cell.
    pub fn mouse_drag(&mut self, row: u16, col: u16) -> UpdateResult {
        if !self.mouse_enabled || self.session.selecting.is_none() {
            return UpdateResult::none();
        }
        if let Some(drag) = self.session.selecting.as_mut() {
            if (row, col) != drag.press {
                drag.moved = true;
            }
            drag.last = (row, col);
        }
        self.extend_selection_to(row, col);
        UpdateResult::dirty()
    }

    /// The left button came up: the release cell re-extends the selection one last time —
    /// through the char under it ([`Self::extend_selection_to`]). A click — the pointer
    /// never left the press cell — leaves nothing selected and copies nothing.
    pub fn mouse_up(&mut self, row: u16, col: u16) -> UpdateResult {
        let Some(drag) = self.session.selecting else {
            return UpdateResult::none();
        };
        if !self.mouse_enabled {
            return UpdateResult::none();
        }
        self.session.selecting = None;
        if drag.count == 1 && !drag.moved && (row, col) == drag.press {
            // The click: the press already cleared; nothing to copy. A double/triple click's
            // release copies the word or entry it just selected.
            return UpdateResult::none();
        }
        self.extend_selection_to(row, col);
        let Some(selection) = self.session.selection else {
            return UpdateResult::none();
        };
        if selection.is_empty() {
            self.session.selection = None;
            return UpdateResult::none();
        }
        let text = select::logical_text(&self.session.entries, &selection);
        if text.is_empty() {
            self.session.selection = None;
            return UpdateResult::dirty();
        }
        // Refuse the absurd honestly, with the reason: the transcript selection's own line.
        if text.len() > MAX_CLIPBOARD_BYTES {
            self.session.selection = None;
            self.push_notice(format!(
                "selection too large to copy: {} bytes (the limit is {})",
                text.len(),
                MAX_CLIPBOARD_BYTES
            ));
            return UpdateResult::dirty();
        }
        // `off` means the selection still draws, but releasing it copies nothing.
        if self.clipboard == ClipboardMode::Off {
            return UpdateResult::none();
        }
        UpdateResult::effect(Effect::Copy { text })
    }

    /// One auto-scroll tick of a live drag: the pointer held at (or beyond) the transcript's
    /// top or bottom edge moves the view one line toward it, and the selection's head is
    /// re-located under the pointer. Returns whether anything changed. The loop calls this
    /// only while a drag is live, on its `recv_timeout` tick.
    pub fn mouse_autoscroll(&mut self) -> bool {
        let Some(drag) = self.session.selecting else {
            return false;
        };
        let view = usize::from(self.session.view_height.max(1));
        let direction: i32 = if drag.last.0 == 0 {
            -1
        } else if drag.last.0 as usize >= view {
            1
        } else {
            return false;
        };
        let before = self.session.scroll;
        self.scroll_by(direction);
        if self.session.scroll == before {
            return false;
        }
        self.extend_selection_to(drag.last.0, drag.last.1);
        true
    }

    /// Re-locates the selection under a pointer cell: the earlier of {press anchor, pointer}
    /// is the inclusive start, the later the end — extended **through** the char under it
    /// when that char is what the pointer is on (native-selection semantics: dragging across
    /// "abc" copies "abc" in either direction). A gap (past a line's end, or on a folded
    /// boundary) is a plain boundary and extends nothing. The stored selection is the
    /// normalized `start..end`; rendering and copying read exactly this range.
    fn extend_selection_to(&mut self, row: u16, col: u16) {
        let Some(drag) = self.session.selecting else {
            return;
        };
        let head_line = self.session.scroll + row as usize;
        let Some(hit) = select::locate_hit(
            &self.session.entries,
            self.display_modes(),
            self.session.transcript_width,
            head_line,
            col as usize,
        ) else {
            return;
        };
        let (start, mut end) =
            if (hit.anchor.entry, hit.anchor.offset) < (drag.anchor.entry, drag.anchor.offset) {
                (hit.anchor, drag.anchor)
            } else {
                (drag.anchor, hit.anchor)
            };
        let end_on_char = if end == hit.anchor {
            hit.on_char
        } else {
            drag.anchor_on_char
        };
        if end_on_char {
            end.offset += select::char_len_at(&self.session.entries, end).unwrap_or(1);
        }
        self.session.selection = Some(Selection {
            anchor: start,
            head: end,
        });
    }
}
