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
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

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
    /// overlay sets them (ADR 0013 decision 3).
    secrets: Vec<String>,
    /// Whether the picker has the keyboard. An in-session overlay opens **unfocused** so the
    /// composer keeps the keys until the user navigates into it (ADR 0023 U4).
    focused: bool,
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
        }
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
            None if self.rows.is_empty() => Some(Outcome::New),
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
            KeyCode::Char('n') if self.query.is_empty() && !self.search => {
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
pub fn draw(frame: &mut ratatui::Frame, area: Rect, picker: &Picker) {
    // The in-session overlay draws over the transcript; clear first so no transcript cell
    // shows through the picker's rows.
    frame.render_widget(ratatui::widgets::Clear, area);
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    let title = if picker.search {
        " sessions · full-text search "
    } else {
        " sessions "
    };
    frame.render_widget(
        Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    let prompt = if picker.search { "search" } else { "filter" };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(format!("{prompt}: ")),
            Span::raw(picker.query.clone()),
            Span::styled("▏", Style::default().add_modifier(Modifier::DIM)),
        ])),
        chunks[1],
    );

    let visible = picker.visible();
    let height = chunks[2].height as usize;
    // Keep the selection visible.
    let start = picker.selected.saturating_sub(height.saturating_sub(1));
    let mut lines = Vec::new();
    for (index, row) in visible.iter().enumerate().skip(start).take(height) {
        let selected = index == picker.selected;
        let style = if selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!(" {} ", row.id),
            style.add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            format!("   {}", row.detail),
            style,
        )));
    }
    if visible.is_empty() {
        let message = if picker.rows.is_empty() {
            "no sessions yet — press n for a new one"
        } else if picker.search && picker.query.is_empty() {
            "type a query to search the message text"
        } else {
            "no match"
        };
        lines.push(Line::from(Span::styled(
            message,
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        chunks[2],
    );

    // The hint names the selected id, so a user (and a test) can read the `--resume` line.
    let hint = picker
        .resume_hint()
        .unwrap_or_else(|| "no selection".to_string());
    frame.render_widget(
        Paragraph::new(format!(
            "↑/↓ select · type to filter · Tab search · Enter resume · n new · Esc close   {hint}"
        ))
        .style(Style::default().add_modifier(Modifier::DIM)),
        chunks[3],
    );
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
