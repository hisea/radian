//! The inspector overlay's keys and snapshot lifecycle: opening on a page (or the last
//! page for `/panel`), tab switches, selection, detail, the jobs page's `k` confirm, and
//! event-driven refreshes. Pure `update` code; the snapshot itself is loaded by the loop.

use super::*;
use crate::command::InspectorPage;
use crate::inspector::Inspector;

impl App {
    /// Opens the inspector on `requested` (`None` = the last page for `/panel`, else jobs).
    /// The loop already gathered the snapshot, so this only pushes the overlay.
    pub(super) fn inspector_opened(
        &mut self,
        requested: Option<InspectorPage>,
        snapshot: crate::inspector::InspectorSnapshot,
    ) -> UpdateResult {
        let page = requested
            .or(self.inspector_last)
            .unwrap_or(InspectorPage::Jobs);
        self.inspector_last = Some(page);
        // One overlay at a time: reopening switches the page instead of stacking.
        self.inspector_close();
        self.push_overlay(Overlay::Inspector(Inspector::new(page, snapshot)));
        UpdateResult::dirty()
    }

    /// Reloads the open inspector's snapshot, keeping its page and clamping the selection.
    /// A late answer with no overlay open changes nothing.
    pub(super) fn inspector_refreshed(
        &mut self,
        snapshot: crate::inspector::InspectorSnapshot,
    ) -> UpdateResult {
        let Some(inspector) = self.inspector_mut() else {
            return UpdateResult::none();
        };
        inspector.refresh(snapshot);
        UpdateResult::dirty()
    }

    /// Handles one key for the inspector overlay.
    pub(super) fn inspector_key(&mut self, key: KeyEvent) -> UpdateResult {
        // `Esc` with a pending `k` confirm cancels the confirm first; otherwise it closes.
        if key.code == KeyCode::Esc {
            if let Some(inspector) = self.inspector_mut()
                && inspector.pending_kill.take().is_some()
            {
                return UpdateResult::dirty();
            }
            self.inspector_close();
            return UpdateResult::dirty();
        }
        // A pending `k` confirm owns `y`/`n`.
        if self
            .inspector()
            .is_some_and(|inspector| inspector.pending_kill.is_some())
        {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let id = self
                        .inspector_mut()
                        .and_then(|inspector| inspector.pending_kill.take());
                    if let Some(id) = id {
                        return UpdateResult::effect(Effect::Jobs(command::JobsRequest::Kill(id)));
                    }
                    return UpdateResult::dirty();
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    if let Some(inspector) = self.inspector_mut() {
                        inspector.pending_kill = None;
                    }
                    return UpdateResult::dirty();
                }
                _ => return UpdateResult::none(),
            }
        }
        match key.code {
            KeyCode::Left => {
                if let Some(inspector) = self.inspector_mut() {
                    inspector.prev_page();
                    self.inspector_last = Some(inspector.page);
                }
                UpdateResult::dirty()
            }
            KeyCode::Right => {
                if let Some(inspector) = self.inspector_mut() {
                    inspector.next_page();
                    self.inspector_last = Some(inspector.page);
                }
                UpdateResult::dirty()
            }
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    if let Some(inspector) = self.inspector_mut() {
                        inspector.prev_page();
                        self.inspector_last = Some(inspector.page);
                    }
                } else if let Some(inspector) = self.inspector_mut() {
                    inspector.next_page();
                    self.inspector_last = Some(inspector.page);
                }
                UpdateResult::dirty()
            }
            KeyCode::BackTab => {
                if let Some(inspector) = self.inspector_mut() {
                    inspector.prev_page();
                    self.inspector_last = Some(inspector.page);
                }
                UpdateResult::dirty()
            }
            KeyCode::Up => {
                if let Some(inspector) = self.inspector_mut() {
                    inspector.move_selection(-1);
                }
                UpdateResult::dirty()
            }
            KeyCode::Down => {
                if let Some(inspector) = self.inspector_mut() {
                    inspector.move_selection(1);
                }
                UpdateResult::dirty()
            }
            KeyCode::PageUp => {
                if let Some(inspector) = self.inspector_mut() {
                    inspector.move_selection(-10);
                }
                UpdateResult::dirty()
            }
            KeyCode::PageDown => {
                if let Some(inspector) = self.inspector_mut() {
                    inspector.move_selection(10);
                }
                UpdateResult::dirty()
            }
            KeyCode::Enter => {
                if let Some(inspector) = self.inspector_mut()
                    && !inspector.is_empty()
                {
                    inspector.detail = !inspector.detail;
                }
                UpdateResult::dirty()
            }
            // The only write action: `k` on the jobs page arms the confirm, and `y`
            // kills through the same path as `/jobs kill`.
            KeyCode::Char('k') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let id = self
                    .inspector()
                    .and_then(|inspector| inspector.selected_job_id());
                match id {
                    Some(id) => {
                        if let Some(inspector) = self.inspector_mut() {
                            inspector.pending_kill = Some(id);
                        }
                        UpdateResult::dirty()
                    }
                    None => UpdateResult::none(),
                }
            }
            _ => UpdateResult::none(),
        }
    }
}
