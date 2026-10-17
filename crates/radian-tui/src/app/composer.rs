//! `App`'s side of the input box: putting the text back after a failed `@` expansion, the
//! workspace root `Tab` completes against, and applying a completion (the single-candidate
//! insert and the longest-common-prefix extension, with the range it replaces).

use super::*;

impl App {
    /// Puts `text` back into the input box. An expansion failure must not lose the message
    /// the user typed.
    pub fn restore_input(&mut self, text: String) {
        self.input.set_text(&text);
    }

    /// Records the workspace root, so `Tab` can complete `@` file references against it.
    pub fn set_workspace(&mut self, workspace: impl Into<PathBuf>) {
        self.session.workspace = Some(workspace.into());
    }

    /// Applies one `@` completion (Phase 4):
    ///
    /// * one candidate: insert it; a directory keeps its trailing `/`, a file gets a trailing
    ///   space so the next word can be typed;
    /// * several: extend the word to their longest common prefix; a second `Tab` already at
    ///   that prefix shows the candidate list (the renderer draws `self.completion`).
    ///
    /// Reading the directory happens in the loop; this is only the state change.
    pub(super) fn apply_completion(&mut self, completion: Completion) {
        if completion.candidates.len() == 1 {
            let candidate = &completion.candidates[0];
            let insert = if candidate.ends_with('/') {
                candidate.clone()
            } else {
                format!("{candidate} ")
            };
            self.replace_range(completion.range, &insert);
            self.completion_clear();
            return;
        }
        let chars: Vec<char> = self.input.text().chars().collect();
        let typed: String = chars[completion.range.clone()].iter().collect();
        if completion.common_prefix.chars().count() > typed.chars().count() {
            self.replace_range(completion.range, &completion.common_prefix);
            self.completion_clear();
            return;
        }
        // The menu is up: keep the raw candidates so the highlighted row can be accepted.
        // `completion_set` clears the previous menu (including this field), so it is set after.
        let raw = completion.candidates.clone();
        self.completion_set(
            completion
                .candidates
                .iter()
                .map(|candidate| crate::command::Candidate {
                    insert: candidate.clone(),
                    description: String::new(),
                    current: false,
                })
                .collect(),
        );
        self.mention_completion = Some((completion.range, raw));
    }

    /// Replaces the char range `range` with `replacement`, leaving the cursor right after the
    /// replacement and the rest of the text unchanged.
    pub(super) fn replace_range(&mut self, range: std::ops::Range<usize>, replacement: &str) {
        let chars: Vec<char> = self.input.text().chars().collect();
        let mut text: Vec<char> = Vec::with_capacity(chars.len() + replacement.len());
        text.extend_from_slice(&chars[..range.start]);
        text.extend(replacement.chars());
        let after = chars.len() - range.end;
        text.extend_from_slice(&chars[range.end..]);
        self.input.set_text(&text.into_iter().collect::<String>());
        for _ in 0..after {
            self.input.left();
        }
    }
}
