//! The session picker (ADR 0023 U4): a reusable **overlay** over the transcript, and the
//! `--pick` startup screen. It never creates a session file: it reads the workspace's session
//! index, which is a disposable cache over the JSONL files (ADR 0006 §10).
//!
//! `Enter` resumes (at startup) or reports the id (in-session); `n` asks for a new session;
//! `Esc`/`Ctrl-C` quits or closes.
//!
//! Keys:
//! - `Up`/`Down` move the selection; `PageUp`/`PageDown` move a screenful.
//! - type to filter by id or summary substring; `Backspace` edits the filter.
//! - `Tab` or `Ctrl-S` toggles full-text search (the index's `search`); a search result shows
//!   its snippet.
//! - `Enter` resumes the selection, `n` starts a new session (empty filter only), `Esc`/`Ctrl-C`
//!   quits.

use radian_host::sessions::{self, SearchRow, SessionRow};
use radian_session::Index;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme::Theme;

/// What the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Open this session id (through the resume path). In-session this becomes a `--resume`
    /// notice rather than an in-place switch (ADR 0022/0023).
    Resume(String),
    /// Start a new session.
    New,
    /// Quit without creating anything.
    Quit,
}

/// One visible row: a session's id and the detail line under it.
struct Display {
    id: String,
    detail: String,
}

/// The picker's state, owned so it can live in the layer stack (an in-session overlay) and in
/// the pre-session `--pick` screen alike.
pub struct Picker {
    rows: Vec<SessionRow>,
    index: Option<Index>,
    workspace: String,
    query: String,
    search: bool,
    selected: usize,
    hits: Vec<SearchRow>,
    /// Values redacted from a search snippet before it is shown (the API key and header
    /// values). Empty at startup, where the process has not read them yet; the in-session
    /// overlay sets them (ADR 0013 T3).
    secrets: Vec<String>,
    /// Whether the picker has the keyboard. An in-session overlay opens **unfocused** so the
    /// composer keeps the keys until the user navigates into it (ADR 0023 U4).
    focused: bool,
    /// Whether this picker may start a new session: `--pick` may, `--resume` without a value may
    /// not (it resumes or quits), so `n` and the empty-list "new" path are not offered.
    allow_new: bool,
}

impl Picker {
    pub fn new(rows: Vec<SessionRow>, index: Option<Index>, workspace: impl Into<String>) -> Self {
        Self {
            rows,
            index,
            workspace: workspace.into(),
            query: String::new(),
            search: false,
            selected: 0,
            hits: Vec::new(),
            secrets: Vec::new(),
            focused: false,
            allow_new: true,
        }
    }

    /// Resume only: no `n`, and an empty list cannot become a new session.
    pub fn without_new(mut self) -> Self {
        self.allow_new = false;
        self
    }

    /// Sets the values to redact from a search snippet (the in-session overlay passes the
    /// host's secret list).
    pub fn with_secrets(mut self, secrets: Vec<String>) -> Self {
        self.secrets = secrets;
        self
    }

    /// Opens the picker in full-text search mode with `query` (the `/sessions <query>` form).
    pub fn with_search(mut self, query: String) -> Self {
        self.search = true;
        self.query = query;
        self.refresh();
        self
    }

    /// Focuses the picker and refreshes the visible rows.
    pub fn focus(&mut self) {
        self.focused = true;
        self.refresh();
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// The rows currently shown. In filter mode a row matches when its id or summary
    /// contains the query (case-insensitive). In search mode the index's full-text hits are
    /// shown; an empty query shows nothing there.
    fn visible(&self) -> Vec<Display> {
        if self.search {
            self.hits
                .iter()
                .map(|hit| Display {
                    id: hit.session_id.clone(),
                    detail: crate::terminal::redact(&hit.snippet, &self.secrets),
                })
                .collect()
        } else {
            let needle = self.query.to_lowercase();
            self.rows
                .iter()
                .filter(|row| {
                    needle.is_empty()
                        || row.session_id.to_lowercase().contains(&needle)
                        || row.summary.to_lowercase().contains(&needle)
                })
                .map(|row| Display {
                    id: row.session_id.clone(),
                    detail: format!(
                        "{}  ·  {} messages  ·  {}",
                        row.last_ts.as_deref().unwrap_or("-"),
                        row.message_count,
                        row.summary
                    ),
                })
                .collect()
        }
    }

    fn refresh(&mut self) {
        if self.search {
            self.hits = match &self.index {
                Some(index) if !self.query.is_empty() => {
                    sessions::search(index, &self.workspace, &self.query, 20).unwrap_or_default()
                }
                _ => Vec::new(),
            };
        }
        let len = self.visible().len();
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    fn selected_id(&self) -> Option<String> {
        self.visible().get(self.selected).map(|row| row.id.clone())
    }

    /// The `--resume <id>` hint for the selected row, or `None` with nothing selected. Screens
    /// and tests read this; it names the id to reopen.
    pub fn resume_hint(&self) -> Option<String> {
        self.selected_id().map(|id| format!("--resume {id}"))
    }

    /// The `Enter` action: resume the selection, or start a new session when the list is empty.
    pub fn on_enter(&self) -> Option<Outcome> {
        match self.selected_id() {
            Some(id) => Some(Outcome::Resume(id)),
            None if self.rows.is_empty() && self.allow_new => Some(Outcome::New),
            // A filter that matched nothing: stay in the picker.
            None => None,
        }
    }

    /// Handles one key. `Some` means the picker is done.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Outcome> {
        match key.code {
            KeyCode::Esc => return Some(Outcome::Quit),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some(Outcome::Quit);
            }
            KeyCode::Enter => return self.on_enter(),
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                self.selected = self.selected.saturating_add(1);
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.selected = self.selected.saturating_add(10);
            }
            KeyCode::Tab => {
                self.search = !self.search;
                self.selected = 0;
                self.refresh();
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search = !self.search;
                self.selected = 0;
                self.refresh();
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.refresh();
            }
            // `n` starts a new session only while the filter is empty; once the user is typing,
            // it is an ordinary filter character.
            KeyCode::Char('n') if self.allow_new && self.query.is_empty() && !self.search => {
                return Some(Outcome::New);
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.push(c);
                self.selected = 0;
                self.refresh();
            }
            _ => {}
        }
        None
    }
}

/// Draws the picker into `area`. The startup screen passes the whole frame; the in-session
/// overlay passes the transcript area, so the input box and status bar stay visible.
///
/// ADR 0035/2: the shared [`crate::panel::Panel`] draws the frame, the filter row
/// and the key hints in the bottom border; the rows go through [`crate::select_list`] (the ❯
/// cursor, two columns above 40 columns, a window and a scrollbar when they overflow). The
/// selected session's `--resume <id>` rides on the top border, so it stays copyable.
pub fn draw(
    frame: &mut ratatui::Frame,
    area: Rect,
    picker: &Picker,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) {
    // The in-session overlay draws over the transcript; clear first so no transcript cell
    // shows through the picker's rows.
    frame.render_widget(ratatui::widgets::Clear, area);
    let ascii = glyphs == crate::footer::Glyphs::Ascii;
    let width = area.width as usize;
    let inner = width.saturating_sub(4).max(1);
    let title = if picker.search {
        "sessions · full-text search"
    } else {
        "sessions"
    };
    let mut panel = crate::panel::Panel::new(title);
    if let Some(hint) = picker.resume_hint() {
        panel = panel.right_title(hint);
    }
    let prompt = if picker.search { "search" } else { "filter" };
    panel = panel.row(Line::from(vec![
        Span::styled(format!("{prompt}: "), theme.text),
        Span::raw(picker.query.clone()),
        Span::styled("▏", theme.hint),
    ]));
    panel = panel.divider();
    let rows = picker.visible();
    let items: Vec<crate::select_list::Item> = rows
        .iter()
        .map(|row| crate::select_list::Item::new(row.id.clone()).description(row.detail.clone()))
        .collect();
    // The list's budget: the area minus the panel's two borders, the filter row and the
    // divider.
    let budget = (area.height as usize).saturating_sub(4).max(1);
    let list = crate::select_list::view(theme, &items, "", picker.selected, budget, inner, ascii);
    for line in list.lines {
        panel = panel.row(line);
    }
    if list.empty {
        panel = panel.row(Line::from(Span::styled(empty_message(picker), theme.hint)));
    }
    panel = panel.hint("↑/↓", "select").hint("⏎", "resume");
    if picker.allow_new {
        panel = panel.hint("n", "new");
    }
    panel = panel.hint("tab", "search").hint("esc", "close");
    frame.render_widget(Paragraph::new(panel.lines(width, theme, glyphs)), area);
}

/// The message shown when the list has no rows (the empty index, an empty search, no match).
fn empty_message(picker: &Picker) -> &'static str {
    if picker.rows.is_empty() {
        if picker.allow_new {
            "no sessions yet — press n for a new one"
        } else {
            "no sessions for this workspace"
        }
    } else if picker.search && picker.query.is_empty() {
        "type a query to search the message text"
    } else {
        "no match"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, summary: &str) -> SessionRow {
        SessionRow {
            session_id: id.into(),
            created_ts: None,
            last_ts: Some("2026-10-13T00:00:00.000000Z".into()),
            message_count: 2,
            summary: summary.into(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn picker(rows: Vec<SessionRow>) -> Picker {
        let mut picker = Picker::new(rows, None, "/w");
        picker.focus();
        picker
    }

    /// `--resume` without a value: resume only, so an empty list cannot become a new session
    /// and `n` is an ordinary filter character.
    #[test]
    fn a_resume_only_picker_offers_no_new_session() {
        let mut state = Picker::new(Vec::new(), None, "/w").without_new();
        assert_eq!(state.on_enter(), None, "no new session from an empty list");
        state.on_key(key(KeyCode::Char('n')));
        assert_eq!(state.query, "n", "`n` filters instead");
        assert_eq!(
            state.on_key(key(KeyCode::Esc)),
            Some(Outcome::Quit),
            "Esc still leaves"
        );

        // The ordinary picker keeps both.
        let state = picker(vec![row("alpha", "first")]);
        assert_eq!(state.on_enter(), Some(Outcome::Resume("alpha".into())));
        let mut empty = picker(Vec::new());
        assert_eq!(empty.on_enter(), Some(Outcome::New));
        assert_eq!(
            empty.on_key(key(KeyCode::Char('n'))),
            Some(Outcome::New),
            "`n` starts a new session"
        );
    }

    #[test]
    fn filtering_narrows_by_id_and_summary_and_escape_quits() {
        let mut state = picker(vec![row("alpha", "first"), row("beta", "second")]);
        assert_eq!(state.visible().len(), 2);
        // Typing "sec" leaves only beta.
        for c in "sec".chars() {
            state.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(state.visible().len(), 1);
        assert_eq!(state.resume_hint().as_deref(), Some("--resume beta"));
        // Esc quits without choosing.
        assert_eq!(state.on_key(key(KeyCode::Esc)), Some(Outcome::Quit));
    }

    #[test]
    fn enter_chooses_and_n_starts_a_new_session() {
        let mut state = picker(vec![row("alpha", "first")]);
        assert_eq!(
            state.on_key(key(KeyCode::Enter)),
            Some(Outcome::Resume("alpha".into()))
        );
        assert_eq!(state.on_key(key(KeyCode::Char('n'))), Some(Outcome::New));
        // Once the filter is non-empty, `n` is an ordinary character.
        state.on_key(key(KeyCode::Char('x')));
        assert_eq!(state.on_key(key(KeyCode::Char('n'))), None);
        assert_eq!(state.query, "xn");
    }

    #[test]
    fn an_unfocused_overlay_does_not_have_the_keys() {
        let state = Picker::new(vec![row("alpha", "first")], None, "/w");
        assert!(!state.is_focused());
    }
}
