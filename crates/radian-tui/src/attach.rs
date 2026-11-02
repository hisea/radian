//! `radian attach <session> [--follow]` (ADR 0043): a **read-only** view of another session's
//! committed facts.
//!
//! **What it reads.** The journal is the observation bus: no socket, no token, and the observed
//! session does not know anyone is looking. The fold itself is the host's
//! ([`radian_host::transcript::AttachFold`], ADR 0023 U1) — the same code the hub folds a
//! journal with — and it wraps `radian_session::JournalTail`, whose per-line parser is the
//! **same** `LineScanner` and `TranscriptFold` a resume's `Session::open` folds, and which never
//! takes the writer's lock and never writes. The transcript it hands back is the recovered
//! history of the committed prefix. This module **mirrors** the fold's entries into the TUI's
//! render entries and draws them; the rendering is the only thing here that is not the fold's.
//!
//! **What it cannot show.** Streaming text and tool progress are live, never durable (ADR 0010
//! K2: `ModelTextDelta`, `ModelReasoningDelta`, `ToolProgress`, `ModelRequestStarted`), so a
//! message appears whole when it is committed and the frame never animates. "It is running" is
//! the last `TurnStarted` with no `TurnFinished` (a durable runtime event), shown as one
//! `working…` line at the transcript's tail — never a spinner (D14). There is deliberately no
//! "waiting for approval" line: the runtime journals only the *decision*
//! (`PermissionResolved`), so an open approval is not a durable fact.
//!
//! **An open tool call.** The fold synthesises an error result for every call with no committed
//! result message (ADR 0002) and shows it as a `queued`/`running` line while a live writer holds
//! the file's lock, or with the recovery wording otherwise. See the fold's own docs.
//!
//! **What it does with input.** Scrolling and quitting only (scroll keys as the main TUI binds
//! them, plus `q`/Esc/Ctrl-C). Every other key is swallowed: no input event, no command, no
//! write — the view holds no inbox and no command path at all.

use std::path::Path;

use radian_core::Message;
use radian_host::transcript::AttachFold;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, Entry};
use crate::theme::Theme;

/// The line a running session shows (ADR 0043 decision 7). `…` is one character, not three
/// dots. The fold writes it (a remote reader sees the same line).
pub use radian_host::transcript::WORKING_LINE;

/// What one key did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachKey {
    /// The view is done: leave the terminal.
    Quit,
    /// The frame changed (a scroll key).
    Redraw,
    /// Not a key this view acts on. Swallowed.
    Ignored,
}

/// The read-only view of one session file: the host's fold, mirrored into the TUI's render
/// entries and drawn.
pub struct AttachView {
    /// The committed facts, folded by the host (ADR 0023 U1).
    fold: AttachFold,
    /// The entry store and the scroll position. `App` is the TUI's own view state, so the
    /// transcript is measured and drawn by exactly the code a resume uses; nothing in the
    /// session-facing half of it (inbox, commands, effects) is reachable from here.
    app: App,
    id: String,
}

impl AttachView {
    /// Opens `path` read-only and folds its committed prefix. The file must exist; the caller
    /// reports a missing session before entering the terminal.
    pub fn open(path: &Path, id: &str) -> Result<Self, String> {
        // ADR 0035: a memory card in the observed transcript resolves against this process's
        // memory root, the same one the recording session used. The fold resolves it, so the
        // hub's entries carry the same card.
        let fold = AttachFold::open(path, radian_host::memory_dir())?;
        let mut app = App::new("", id);
        app.set_memory_root(radian_host::memory_dir());
        let mut view = Self {
            fold,
            app,
            id: id.to_owned(),
        };
        view.sync();
        Ok(view)
    }

    /// The session id this view is attached to.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The file being read. Read-only, like everything else here.
    pub fn path(&self) -> &Path {
        self.fold.path()
    }

    /// The transcript's committed history, as of the last read.
    pub fn history(&self) -> &[Message] {
        self.fold.history()
    }

    /// The entries the frame draws: the fold's entries, each with the render state the TUI
    /// adds (the decoded images).
    pub fn entries(&self) -> &[Entry] {
        &self.app.session.entries
    }

    /// Whether a turn is open: the last `TurnStarted` with no `TurnFinished` (ADR 0043 decision 7).
    /// A durable fact only — the observer folds the two runtime events into the projection.
    pub fn running(&self) -> bool {
        self.fold.running()
    }

    /// Mirrors the fold's entry list into the render entries: only what the fold marked as
    /// changed, so a follow that appends one message converts one entry.
    fn sync(&mut self) {
        let Some(change) = self.fold.take_change() else {
            return;
        };
        let folded = self.fold.entries();
        let from = if change.reset { 0 } else { change.from };
        let from = from.min(folded.len()).min(self.app.session.entries.len());
        // A rebuild from scratch (a replacement segment, a reset, a live flip) starts the view
        // over, exactly as the fresh `App` a self-contained rebuild used to build did: the
        // scroll goes back to the top and the end is followed again (ADR 0023 U6's replacement
        // rule, as the attach view applied it).
        if change.reset {
            self.app.session.scroll = 0;
            self.app.session.follow = true;
        }
        self.app.session.entries.truncate(from);
        let app = &mut self.app;
        for entry in &folded[from..] {
            let converted = Entry::from_fold(entry, &mut app.images);
            app.session.entries.push(converted);
        }
    }

    /// Reads the bytes another process appended since the last read (`--follow`), folds them
    /// and updates the frame's state. Returns whether anything the frame shows changed.
    ///
    /// The file is being appended by a session that flushes each record as it is written
    /// (ADR 0040), so a half-written line is normal, not an error: the tail keeps it for the
    /// next read.
    pub fn poll(&mut self) -> Result<bool, String> {
        let changed = self.fold.poll()?;
        if changed {
            self.sync();
        }
        Ok(changed)
    }

    /// One key press. Anything that is not a scroll key or a quit key is [`AttachKey::Ignored`].
    pub fn on_key(&mut self, key: &KeyEvent) -> AttachKey {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('q') if !ctrl => AttachKey::Quit,
            KeyCode::Esc => AttachKey::Quit,
            KeyCode::Char('c') if ctrl => AttachKey::Quit,
            KeyCode::PageUp => self.scroll(-(self.page() as i32)),
            KeyCode::PageDown => self.scroll(self.page() as i32),
            KeyCode::Up if shift => self.scroll(-1),
            KeyCode::Down if shift => self.scroll(1),
            KeyCode::Home if ctrl => {
                self.app.scroll_to_top();
                AttachKey::Redraw
            }
            KeyCode::End if ctrl => {
                self.app.scroll_to_bottom();
                AttachKey::Redraw
            }
            _ => AttachKey::Ignored,
        }
    }

    /// One line at a time, up (`delta < 0`) or down.
    fn scroll(&mut self, delta: i32) -> AttachKey {
        self.app.scroll_by(delta);
        AttachKey::Redraw
    }

    /// The theme the transcript draws with. The caller that owns a theme (`radian arc`) sets
    /// it; a bare attach view keeps the default.
    pub fn set_theme(&mut self, theme: Theme) {
        self.app.theme = theme;
    }

    /// Scrolls the transcript one line at a time, up (`delta < 0`) or down.
    pub fn scroll_lines(&mut self, delta: i32) {
        self.app.scroll_by(delta);
    }

    /// Pins the transcript to its end (follow again).
    pub fn follow_end(&mut self) {
        self.app.scroll_to_bottom();
    }

    /// The visible rows a page key moves: what the last frame measured.
    fn page(&self) -> usize {
        usize::from(self.app.session.view_height.max(1))
    }

    /// The footer: the two facts this process owns — that this is an attach view of `id`, and
    /// where the transcript is scrolled. Never the model, usage or tier: those belong to the
    /// session's own process, not to a read-only reader.
    pub fn footer_text(&self) -> String {
        let position = if self.app.session.max_scroll == 0 {
            "all".to_owned()
        } else {
            format!(
                "{}/{}",
                self.app.session.scroll, self.app.session.max_scroll
            )
        };
        format!(
            " attach · {} · read-only │ scroll {position} {}",
            self.id,
            if self.app.session.follow {
                "follow"
            } else {
                "paused"
            }
        )
    }

    /// Draws one frame: the transcript, then the footer.
    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        self.draw(frame, chunks[0]);
        let footer = Line::from(Span::styled(self.footer_text(), self.app.theme.hint));
        frame.render_widget(Paragraph::new(footer), chunks[1]);
    }

    /// Draws the transcript into `area` (no footer): the arc TUI's conversation tab puts the
    /// same blocks inside its own frame, so an attached session and a task's journal agree.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let theme = self.app.theme.clone();
        draw_entries(&mut self.app, frame, area, &theme);
    }
}

/// Draws an [`App`]'s transcript entries into `area` with the attach view's layout, theme and
/// scroll state. The attach view and the arc conversation tab's folded ACP stream both call
/// this, so both draw the same blocks by construction (ADR 0048 §13).
pub(crate) fn draw_entries(app: &mut App, frame: &mut Frame, area: Rect, theme: &Theme) {
    let width = area.width as usize;
    let height = area.height as usize;
    // The image run follows the area's height (ADR 0028 R10), exactly as in the TUI's own
    // frame; the width is recorded through the same call.
    app.set_transcript_size(width, area.height);
    let modes = app.display_modes();
    // Measure first (the per-entry line counts are cached), then clamp the scroll position
    // against the real total, then cut the visible window — the order the TUI's own frame
    // uses (`render::draw`), so the scroll keys and the footer agree with what is drawn.
    let (total, _) = crate::render::visible_lines_with(
        &mut app.session.entries,
        modes,
        width,
        height,
        0,
        0,
        theme,
        crate::spinner::Frame::default(),
    );
    app.set_viewport(area.height, total);
    let (_, visible) = crate::render::visible_lines_with(
        &mut app.session.entries,
        modes,
        width,
        height,
        app.session.scroll,
        height,
        theme,
        crate::spinner::Frame::default(),
    );
    frame.render_widget(Paragraph::new(visible), area);
}
