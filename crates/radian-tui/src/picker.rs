//! The startup session picker (`radian-tui --pick`): a full-screen list shown **before** the
//! main UI, so a user can choose which session to open (Phase 4).
//!
//! It never creates a session file: it reads the workspace's session index, which is a
//! disposable cache over the JSONL files (ADR 0006 §10). `Enter` hands the chosen id back to
//! the caller, which opens it through the ordinary `--resume` path; `n` asks for a new
//! session; `Esc`/`Ctrl-C` quits without creating anything.
//!
//! Keys:
//! - `Up`/`Down` move the selection; `PageUp`/`PageDown` move a screenful.
//! - type to filter by id or summary substring; `Backspace` edits the filter.
//! - `Tab` or `Ctrl-S` toggles full-text search (the index's `search`); a search result shows
//!   its snippet.
//! - `Enter` resumes the selection, `n` starts a new session (empty filter only), `Esc`/`Ctrl-C`
//!   quits.

use std::io;
use std::time::Duration;

use radian_host::sessions::{self, SearchRow, SessionRow};
use radian_session::Index;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::terminal::TtyGuard;

/// What the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Open this session id (through the resume path).
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

struct State<'a> {
    rows: &'a [SessionRow],
    index: Option<&'a Index>,
    workspace: &'a str,
    query: String,
    search: bool,
    selected: usize,
    hits: Vec<SearchRow>,
}

impl State<'_> {
    /// The rows currently shown. In filter mode a row matches when its id or summary
    /// contains the query (case-insensitive). In search mode the index's full-text hits are
    /// shown; an empty query shows nothing there.
    fn visible(&self) -> Vec<Display> {
        if self.search {
            self.hits
                .iter()
                .map(|hit| Display {
                    id: hit.session_id.clone(),
                    detail: hit.snippet.clone(),
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
            self.hits = match self.index {
                Some(index) if !self.query.is_empty() => {
                    sessions::search(index, self.workspace, &self.query, 20).unwrap_or_default()
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
}

/// Runs the picker on the terminal until the user chooses. The terminal is restored before
/// returning, on every path (the guard's `Drop`).
pub fn run(rows: &[SessionRow], index: Option<&Index>, workspace: &str) -> io::Result<Outcome> {
    let mut state = State {
        rows,
        index,
        workspace,
        query: String::new(),
        search: false,
        selected: 0,
        hits: Vec::new(),
    };
    state.refresh();

    let mut guard = TtyGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let outcome = loop {
        terminal.draw(|frame| draw(frame, &state))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if let Some(outcome) = on_key(&mut state, key) {
            break outcome;
        }
    };
    guard.restore();
    Ok(outcome)
}

/// Handles one key. `Some` means the picker is done.
fn on_key(state: &mut State<'_>, key: KeyEvent) -> Option<Outcome> {
    match key.code {
        KeyCode::Esc => return Some(Outcome::Quit),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return Some(Outcome::Quit);
        }
        KeyCode::Enter => {
            return Some(match state.selected_id() {
                Some(id) => Outcome::Resume(id),
                None if state.rows.is_empty() => Outcome::New,
                // A filter that matched nothing: stay in the picker.
                None => return None,
            });
        }
        KeyCode::Up => {
            state.selected = state.selected.saturating_sub(1);
        }
        KeyCode::Down => {
            state.selected = state.selected.saturating_add(1);
        }
        KeyCode::Tab => {
            state.search = !state.search;
            state.selected = 0;
            state.refresh();
        }
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.search = !state.search;
            state.selected = 0;
            state.refresh();
        }
        KeyCode::Backspace => {
            state.query.pop();
            state.selected = 0;
            state.refresh();
        }
        // `n` starts a new session only while the filter is empty; once the user is typing, it
        // is an ordinary filter character.
        KeyCode::Char('n') if state.query.is_empty() && !state.search => {
            return Some(Outcome::New);
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.query.push(c);
            state.selected = 0;
            state.refresh();
        }
        _ => {}
    }
    None
}

fn draw(frame: &mut ratatui::Frame, state: &State<'_>) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    let title = if state.search {
        " sessions · full-text search "
    } else {
        " sessions "
    };
    frame.render_widget(
        Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    let prompt = if state.search { "search" } else { "filter" };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(format!("{prompt}: ")),
            Span::raw(state.query.clone()),
            Span::styled("▏", Style::default().add_modifier(Modifier::DIM)),
        ])),
        chunks[1],
    );

    let visible = state.visible();
    let height = chunks[2].height as usize;
    // Keep the selection visible.
    let start = state.selected.saturating_sub(height.saturating_sub(1));
    let mut lines = Vec::new();
    for (index, row) in visible.iter().enumerate().skip(start).take(height) {
        let selected = index == state.selected;
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
        let message = if state.rows.is_empty() {
            "no sessions yet — press n for a new one"
        } else if state.search && state.query.is_empty() {
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

    frame.render_widget(
        Paragraph::new(
            "↑/↓ select · type to filter · Tab search · Enter resume · n new · Esc quit",
        )
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

    #[test]
    fn filtering_narrows_by_id_and_summary_and_escape_quits() {
        let rows = [row("alpha", "first"), row("beta", "second")];
        let mut state = State {
            rows: &rows,
            index: None,
            workspace: "/w",
            query: String::new(),
            search: false,
            selected: 0,
            hits: Vec::new(),
        };
        state.refresh();
        assert_eq!(state.visible().len(), 2);
        // Typing "sec" leaves only beta.
        for c in "sec".chars() {
            on_key(&mut state, key(KeyCode::Char(c)));
        }
        assert_eq!(state.visible().len(), 1);
        assert_eq!(state.selected_id().as_deref(), Some("beta"));
        // Esc quits without choosing.
        assert_eq!(on_key(&mut state, key(KeyCode::Esc)), Some(Outcome::Quit));
    }

    #[test]
    fn enter_chooses_and_n_starts_a_new_session() {
        let rows = [row("alpha", "first")];
        let mut state = State {
            rows: &rows,
            index: None,
            workspace: "/w",
            query: String::new(),
            search: false,
            selected: 0,
            hits: Vec::new(),
        };
        state.refresh();
        assert_eq!(
            on_key(&mut state, key(KeyCode::Enter)),
            Some(Outcome::Resume("alpha".into()))
        );
        assert_eq!(
            on_key(&mut state, key(KeyCode::Char('n'))),
            Some(Outcome::New)
        );
        // Once the filter is non-empty, `n` is an ordinary character.
        on_key(&mut state, key(KeyCode::Char('x')));
        assert_eq!(on_key(&mut state, key(KeyCode::Char('n'))), None);
        assert_eq!(state.query, "xn");
    }
}
