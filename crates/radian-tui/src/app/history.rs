//! The input box's history: whether the box is editable at all, browsing it with `Up`/`Down`
//! (the stash that restores the line the user was typing), and the prefill a resumed session
//! gets from its journaled user messages.

use super::*;

impl App {
    /// True while the input box is editable: no approval or command prompt is up, so the
    /// cursor belongs in the input box and keys type into it.
    pub fn input_editable(&self) -> bool {
        !self.has_modal()
    }

    /// `Up` on the first input line: the previous history entry. Starts browsing by stashing
    /// the current line, so `Down` past the newest entry restores it. False with no history.
    pub fn history_up(&mut self) -> bool {
        if self.session.history.is_empty() {
            return false;
        }
        let next = match self.session.history_cursor {
            None => {
                self.session.history_stash = self.input.text();
                self.session.history.len() - 1
            }
            Some(0) => return false,
            Some(i) => i - 1,
        };
        self.session.history_cursor = Some(next);
        self.input.set_text(&self.session.history[next]);
        true
    }

    /// `Down` on the last input line: the next history entry, or the stashed draft at the end.
    pub fn history_down(&mut self) -> bool {
        let Some(i) = self.session.history_cursor else {
            return false;
        };
        if i + 1 < self.session.history.len() {
            self.session.history_cursor = Some(i + 1);
            self.input.set_text(&self.session.history[i + 1]);
        } else {
            self.session.history_cursor = None;
            let stash = std::mem::take(&mut self.session.history_stash);
            self.input.set_text(&stash);
        }
        true
    }

    /// Prefills the history from a resumed session's user messages (ADR 0006 §5.5: the
    /// source rides on the message). Only what the user sent, in order.
    pub fn prefill_history(&mut self, messages: &[Message]) {
        for message in messages {
            if let Message::User { content, meta } = message
                && matches!(meta.source, None | Some(radian_core::InputSource::User))
            {
                self.session.history.push(text_of(content));
            }
        }
    }
}
