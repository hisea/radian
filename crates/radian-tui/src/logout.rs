//! The `/logout` key picker (ADR 0026 D3): when an entry holds several keys, the confirmation
//! becomes a list of them, each named and masked, plus an `all` row. Pure state, like the
//! `/login` wizard; the removal is an [`crate::app::Effect`] the loop runs.
//!
//! Keys are hardcoded (a destructive choice, like the approval): ↑/↓ moves, Enter chooses,
//! `a` chooses all, and Esc keeps everything.

use radian_host::KeyChoice;

/// The picker's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogoutKeysWizard {
    pub provider: String,
    pub keys: Vec<KeyChoice>,
    /// `0..keys.len()` selects one key; `keys.len()` is `all`.
    pub cursor: usize,
}

impl LogoutKeysWizard {
    pub fn new(provider: impl Into<String>, keys: Vec<KeyChoice>) -> Self {
        Self {
            provider: provider.into(),
            keys,
            cursor: 0,
        }
    }

    /// The rows the panel lists, numbered as the manual shows them, with `all` last.
    pub fn rows(&self) -> Vec<String> {
        let mut rows: Vec<String> = self
            .keys
            .iter()
            .enumerate()
            .map(|(index, key)| format!("{} {} ({})", index + 1, key.name, key.masked))
            .collect();
        rows.push("all keys".to_owned());
        rows
    }

    /// The row in effect: `None` for `all`, else the key's display name.
    pub fn selected(&self) -> Option<String> {
        self.keys.get(self.cursor).map(|key| key.name.clone())
    }

    pub fn move_cursor(&mut self, delta: i32) {
        let last = self.keys.len();
        self.cursor = if delta < 0 {
            self.cursor.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            (self.cursor + delta as usize).min(last)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wizard() -> LogoutKeysWizard {
        LogoutKeysWizard::new(
            "opencode-go",
            vec![
                KeyChoice {
                    name: "first".to_owned(),
                    masked: "sk-…irst".to_owned(),
                },
                KeyChoice {
                    name: "team".to_owned(),
                    masked: "sk-…cond".to_owned(),
                },
            ],
        )
    }

    /// The list is the manual's `1 first (sk-…irst)  2 team (sk-…cond)` plus `all`.
    #[test]
    fn the_rows_number_each_key_and_end_with_all() {
        let wizard = wizard();
        assert_eq!(
            wizard.rows(),
            vec![
                "1 first (sk-…irst)".to_owned(),
                "2 team (sk-…cond)".to_owned(),
                "all keys".to_owned(),
            ]
        );
        assert_eq!(wizard.selected().as_deref(), Some("first"));
    }

    #[test]
    fn the_cursor_walks_the_keys_and_all() {
        let mut wizard = wizard();
        wizard.move_cursor(1);
        assert_eq!(wizard.selected().as_deref(), Some("team"));
        wizard.move_cursor(1);
        assert_eq!(wizard.selected(), None, "all");
        wizard.move_cursor(1);
        assert_eq!(wizard.selected(), None, "the cursor stops at all");
        wizard.move_cursor(-5);
        assert_eq!(wizard.selected().as_deref(), Some("first"));
    }
}
