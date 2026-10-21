//! The `/btw` side question's `update` code (backlog `tui-btw-side-question`):
//! asking, reopening, streaming into the overlay, and finishing. Pure: the
//! loop runs the request and feeds the deltas back as [`Input`]s.

use super::*;
use crate::btw::{BtwOverlay, BtwState};

impl App {
    /// Runs `/btw [question]`: `None` reopens the last side answer, `Some`
    /// starts a side question unless one is already running.
    pub(super) fn run_btw(&mut self, question: Option<String>) -> UpdateResult {
        match question {
            None => self.reopen_btw(),
            Some(question) => {
                if self.session.btw_in_flight {
                    self.push_notice("a side question is already running (Esc cancels it)");
                    return UpdateResult::dirty();
                }
                self.session.btw_in_flight = true;
                // One overlay at a time: a new question replaces the old answer.
                self.btw_close();
                self.push_overlay(Overlay::Btw(BtwOverlay::asking(question.clone())));
                UpdateResult::effect(Effect::SideQuestion { question })
            }
        }
    }

    /// Reopens the last finished side answer (`/btw` alone). Nothing to reopen
    /// is a notice, never an error.
    fn reopen_btw(&mut self) -> UpdateResult {
        match self.session.btw_last.clone() {
            Some(answer) => {
                self.btw_close();
                self.push_overlay(Overlay::Btw(BtwOverlay::reopen(answer)));
                UpdateResult::dirty()
            }
            None => {
                self.push_notice("no previous side answer yet (ask with /btw <question>)");
                UpdateResult::dirty()
            }
        }
    }

    /// Handles one key for the `/btw` overlay: Esc cancels a running request (and
    /// closes the overlay) or closes a finished one; arrows/PgUp/PgDn scroll.
    pub(super) fn btw_key(&mut self, key: KeyEvent) -> UpdateResult {
        match key.code {
            KeyCode::Esc => {
                let running = self.session.btw_in_flight;
                self.session.btw_in_flight = false;
                self.btw_close();
                if running {
                    UpdateResult::effect(Effect::CancelSideQuestion)
                } else {
                    UpdateResult::dirty()
                }
            }
            KeyCode::Up => self.btw_scroll(-1),
            KeyCode::Down => self.btw_scroll(1),
            KeyCode::PageUp => self.btw_scroll(-10),
            KeyCode::PageDown => self.btw_scroll(10),
            _ => UpdateResult::none(),
        }
    }

    fn btw_scroll(&mut self, delta: i32) -> UpdateResult {
        if let Some(overlay) = self.btw_mut() {
            overlay.scroll_by(delta);
            UpdateResult::dirty()
        } else {
            UpdateResult::none()
        }
    }

    /// Appends streamed side text to the open overlay. A delta for a request
    /// that is no longer running is dropped.
    pub(super) fn side_delta(&mut self, delta: String) -> UpdateResult {
        if !self.session.btw_in_flight {
            return UpdateResult::none();
        }
        if let Some(overlay) = self.btw_mut() {
            overlay.push_text(&delta);
            UpdateResult::dirty()
        } else {
            UpdateResult::none()
        }
    }

    /// Finishes the in-flight side question: a `Done` stores the answer (for
    /// `/btw` alone to reopen) and shows the usage line; a `Failed` shows the
    /// error in the overlay only, never as a turn failure; a `Cancelled` after
    /// the overlay closed is ignored.
    pub(super) fn side_done(&mut self, outcome: radian_host::btw::SideOutcome) -> UpdateResult {
        use radian_host::btw::SideOutcome;
        if !self.session.btw_in_flight {
            return UpdateResult::none();
        }
        self.session.btw_in_flight = false;
        match outcome {
            SideOutcome::Done {
                text,
                tool_calls_dropped,
                usage,
                fee_micros,
            } => {
                let usage_line = crate::btw::usage_line(usage.as_ref(), fee_micros);
                let answer = crate::btw::BtwAnswer {
                    question: self
                        .btw()
                        .map(|overlay| overlay.question.clone())
                        .unwrap_or_default(),
                    text,
                    usage_line,
                    tools_note: tool_calls_dropped,
                };
                self.session.btw_last = Some(answer.clone());
                if let Some(overlay) = self.btw_mut() {
                    overlay.state = BtwState::Done;
                    overlay.answer = Some(answer);
                }
                UpdateResult::dirty()
            }
            SideOutcome::Failed(error) => {
                if let Some(overlay) = self.btw_mut() {
                    overlay.state = BtwState::Failed(error);
                }
                UpdateResult::dirty()
            }
            SideOutcome::Cancelled { .. } => {
                // Esc already closed the overlay when it cancelled; a late
                // cancel outcome changes nothing.
                self.btw_close();
                UpdateResult::dirty()
            }
        }
    }
}
