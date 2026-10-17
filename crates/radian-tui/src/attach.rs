//! `radian attach <session> [--follow]` (ADR 0043 slice ①, decision D14): a **read-only** view
//! of another session's committed facts.
//!
//! **What it reads.** The journal is the observation bus (D15): no socket, no token, and the
//! observed session does not know anyone is looking. [`JournalObserver`] is the read side — it
//! wraps `radian_session::JournalTail`, whose per-line parser is the **same** `LineScanner` and
//! `TranscriptFold` a resume's `Session::open` folds, and which never takes the writer's lock
//! and never writes. The transcript it hands back is the recovered history of the committed
//! prefix, and the entries are built by the **same** `App::push_message` the `radian --resume`
//! path uses (`cli.rs`), rendered by the same block renderer, so an attach frame and a resume
//! frame agree by construction.
//!
//! **What it cannot show.** Streaming text and tool progress are live, never durable (ADR 0010
//! K2: `ModelTextDelta`, `ModelReasoningDelta`, `ToolProgress`, `ModelRequestStarted`), so a
//! message appears whole when it is committed and the frame never animates. "It is running" is
//! the last `TurnStarted` with no `TurnFinished` (a durable runtime event), shown as one
//! `working…` line at the transcript's tail — never a spinner (D14). There is deliberately no
//! "waiting for approval" line: the runtime journals only the *decision*
//! (`PermissionResolved`), so an open approval is not a durable fact.
//!
//! **What it does with input.** Scrolling and quitting only (scroll keys as the main TUI binds
//! them, plus `q`/Esc/Ctrl-C). Every other key is swallowed: no input event, no command, no
//! write — the view holds no inbox and no command path at all.

use std::path::Path;

use agent_core::Message;
use radian_host::{JournalObserver, SessionObserver};
use radian_session::EventData;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, Entry, EntryKind};
use crate::projection::TurnState;

/// The line a running session shows (ADR 0043 D14). `…` is one character, not three dots.
pub const WORKING_LINE: &str = "working…";

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

/// The read-only view of one session file.
pub struct AttachView {
    /// The entry store and the scroll position. `App` is the TUI's own view state, so the
    /// transcript is built and measured by exactly the code a resume uses; nothing in the
    /// session-facing half of it (inbox, commands, effects) is reachable from here.
    app: App,
    /// The committed facts, read-only over the journal file.
    observer: JournalObserver,
    id: String,
    /// How many of the observer's history messages are already entries.
    pushed: usize,
    /// Whether the `working…` entry is currently the transcript's last entry.
    working: bool,
}

impl AttachView {
    /// Opens `path` read-only and folds its committed prefix. The file must exist; the caller
    /// reports a missing session before entering the terminal.
    pub fn open(path: &Path, id: &str) -> Result<Self, String> {
        let observer = JournalObserver::open(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let mut view = Self {
            app: App::new("", id),
            observer,
            id: id.to_owned(),
            pushed: 0,
            working: false,
        };
        view.rebuild();
        Ok(view)
    }

    /// The session id this view is attached to.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The transcript's committed history, as of the last read.
    pub fn history(&self) -> &[Message] {
        self.observer.history()
    }

    /// Whether a turn is open: the last `TurnStarted` with no `TurnFinished` (ADR 0043 D14).
    /// A durable fact only — the observer folds the two runtime events into the projection.
    pub fn running(&self) -> bool {
        self.observer.projection().turn() == TurnState::Running
    }

    /// Rebuilds every entry from the observer's history: the initial load and the rare case
    /// where the history is **replaced** (a compaction commits a `TranscriptReplaced` segment,
    /// ADR 0019) rather than extended.
    fn rebuild(&mut self) {
        self.app = App::new("", self.id.clone());
        let history = self.observer.history().to_vec();
        for message in &history {
            self.app.push_message(message);
        }
        self.pushed = history.len();
        self.working = false;
        self.append_working();
    }

    /// Appends the `working…` line when a turn is open and it is not there yet.
    fn append_working(&mut self) {
        if !self.working && self.running() {
            self.app
                .session
                .entries
                .push(Entry::new(EntryKind::Notice, WORKING_LINE));
            self.working = true;
        }
    }

    /// Removes the `working…` line. It is the tail whenever it exists: every append goes
    /// through [`Self::append_working`] **after** the messages that would follow it.
    fn drop_working(&mut self) {
        if self.working {
            self.app.session.entries.pop();
            self.working = false;
        }
    }

    /// Reads the bytes another process appended since the last read (`--follow`), folds them
    /// and updates the frame's state. Returns whether anything the frame shows changed.
    ///
    /// The file is being appended by a session that flushes each record as it is written
    /// (ADR 0040), so a half-written line is normal, not an error: the tail keeps it for the
    /// next read.
    pub fn poll(&mut self) -> Result<bool, String> {
        let update = self
            .observer
            .poll()
            .map_err(|error| format!("cannot read {}: {error}", self.observer.path().display()))?;
        // A replacement segment rewrites the effective history; so does a reset (the file was
        // replaced or truncated, ADR 0023 U6). Both rebuild from the new history.
        let replaced = update.events.iter().any(|event| {
            matches!(
                &event.data,
                EventData::TranscriptReplaced { .. }
                    | EventData::TranscriptReplacementCommitted { .. }
            )
        });
        if update.reset || replaced || update.history.len() < self.pushed {
            self.rebuild();
            return Ok(true);
        }
        let was_working = self.working;
        self.drop_working();
        let mut changed = false;
        if update.history.len() > self.pushed {
            for message in &update.history[self.pushed..] {
                self.app.push_message(message);
            }
            self.pushed = update.history.len();
            changed = true;
        }
        self.append_working();
        Ok(changed || self.working != was_working)
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
        let transcript = chunks[0];
        let width = transcript.width as usize;
        let height = transcript.height as usize;
        let theme = self.app.theme.clone();
        let modes = self.app.display_modes();
        // Measure first (the per-entry line counts are cached), then clamp the scroll position
        // against the real total, then cut the visible window — the order the TUI's own frame
        // uses (`render::draw`), so the scroll keys and the footer agree with what is drawn.
        let (total, _) = crate::render::visible_lines_with(
            &mut self.app.session.entries,
            modes,
            width,
            0,
            0,
            &theme,
            crate::spinner::Frame::default(),
        );
        self.app.set_viewport(transcript.height, total);
        let (_, visible) = crate::render::visible_lines_with(
            &mut self.app.session.entries,
            modes,
            width,
            self.app.session.scroll,
            height,
            &theme,
            crate::spinner::Frame::default(),
        );
        frame.render_widget(Paragraph::new(visible), transcript);
        let footer = Line::from(Span::styled(self.footer_text(), self.app.theme.hint));
        frame.render_widget(Paragraph::new(footer), chunks[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// A scratch directory that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("radian-attach-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One envelope line. `critical` is what the reader's unknown-event policy keys on
    /// (ADR 0006 §7).
    fn envelope(seq: u64, event: &str, critical: bool, data: serde_json::Value) -> String {
        format!(
            r#"{{"schema_version":1,"session_id":"attach-sid","seq":{seq},"ts":"2026-10-17T00:00:0{seq}.000000Z","event":"{event}","critical":{critical},"data":{data}}}"#
        )
    }

    /// A `MessageAppended` body for the message at transcript `index`.
    fn appended(index: u64, message: agent_core::Message) -> serde_json::Value {
        serde_json::json!({"index": index, "message": message})
    }

    /// Writes a journal of `lines` and opens the view over it.
    fn view(label: &str, id: &str, lines: &[String]) -> (Scratch, AttachView) {
        let scratch = Scratch::new(label);
        let path = scratch.file(&format!("{id}.jsonl"));
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        let view = AttachView::open(&path, id).expect("the view opens");
        (scratch, view)
    }

    /// The screen a frame draws at `width` x `height`.
    fn screen(view: &mut AttachView, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| view.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A journal with one committed user turn and one committed assistant reply, plus a
    /// **live** event line that must never be rendered: `ModelTextDelta` is not durable
    /// (ADR 0010 K2), and the refresh of this test is the reader's own unknown-event policy —
    /// a non-critical unknown line is skipped, never fed to the projection.
    fn committed_journal() -> Vec<String> {
        vec![
            envelope(
                0,
                "SessionCreated",
                true,
                serde_json::json!({"workspace": "/w", "config_snapshot": {}}),
            ),
            envelope(
                1,
                "TurnStarted",
                true,
                serde_json::json!({"turn_id": "t-1"}),
            ),
            envelope(
                2,
                "MessageAppended",
                true,
                appended(0, agent_core::Message::user("ATTACH-COMMITTED-USER")),
            ),
            // The live delta: never durable, so it can only reach the frame through a reader
            // that renders live events — which is the mutation this test catches.
            envelope(
                3,
                "ModelTextDelta",
                false,
                serde_json::json!({"text": "ATTACH-LIVE-DELTA"}),
            ),
            envelope(
                4,
                "MessageAppended",
                true,
                appended(1, agent_core::Message::assistant("ATTACH-COMMITTED-REPLY")),
            ),
            envelope(
                5,
                "TurnFinished",
                true,
                serde_json::json!({"turn_id": "t-1", "outcome": "completed"}),
            ),
        ]
    }

    /// The committed transcript is what a frame shows, and a live delta is not part of it.
    ///
    /// Mutation: give the view a path from a live event (an `AgentEvent` sink, or an
    /// unknown-event policy that keeps the line) and renders `ATTACH-LIVE-DELTA` → FAILED.
    #[test]
    fn a_frame_shows_the_committed_transcript_and_never_a_live_delta() {
        let (_scratch, mut view) = view("committed", "commit-me", &committed_journal());
        let shown = screen(&mut view, 80, 24);
        assert!(
            shown.contains("ATTACH-COMMITTED-USER"),
            "the committed user message is not on screen:\n{shown}"
        );
        assert!(
            shown.contains("ATTACH-COMMITTED-REPLY"),
            "the committed assistant message is not on screen:\n{shown}"
        );
        assert!(
            !shown.contains("ATTACH-LIVE-DELTA"),
            "a live delta reached the frame:\n{shown}"
        );
        // The view holds no streaming buffer at all: the projection's stream is the live half
        // of ADR 0010, and an attach frame must never read it.
        assert_eq!(
            view.history().len(),
            2,
            "the committed history is the two messages"
        );
    }

    /// `working…` is the last `TurnStarted` with no `TurnFinished` — a durable fact — and it
    /// leaves when the turn finishes.
    ///
    /// Mutation: show the line from anything else (a live event, a timer) → the second half
    /// (the line must be gone after `TurnFinished`) FAILED.
    #[test]
    fn the_working_line_follows_the_open_turn() {
        let mut lines = committed_journal();
        // A second turn that never finished: the journal ends with its `TurnStarted`.
        lines.push(envelope(
            6,
            "TurnStarted",
            true,
            serde_json::json!({"turn_id": "t-2"}),
        ));
        let (_scratch, mut view) = view("working", "run-me", &lines);
        let shown = screen(&mut view, 80, 24);
        assert!(
            shown.contains(WORKING_LINE),
            "an open turn does not show `working…`:\n{shown}"
        );
        assert!(view.running(), "the last TurnStarted has no TurnFinished");

        // The turn finishes: the line goes away (a follow poll, folded in place).
        let path = view.observer.path().to_owned();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!(
            "{}\n",
            envelope(
                7,
                "TurnFinished",
                true,
                serde_json::json!({"turn_id": "t-2", "outcome": "completed"})
            )
        ));
        std::fs::write(&path, text).unwrap();
        assert!(
            view.poll().expect("the new lines fold"),
            "the poll changed the frame"
        );
        assert!(!view.running(), "the turn is finished");
        let shown = screen(&mut view, 80, 24);
        assert!(
            !shown.contains(WORKING_LINE),
            "a finished turn still shows `working…`:\n{shown}"
        );
    }

    /// A message committed after the first read appears on the next poll (the `--follow` half
    /// of the view), and the `working…` line stays at the transcript's tail.
    ///
    /// Mutation: drop the observer's poll (read the file once) → the new message never shows.
    #[test]
    fn a_follow_poll_shows_a_new_committed_message() {
        let (_scratch, mut view) = view("follow", "follow-me", &committed_journal());
        assert!(!screen(&mut view, 80, 24).contains("ATTACH-NEW-MESSAGE"));

        let path = view.observer.path().to_owned();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!(
            "{}\n",
            envelope(
                6,
                "MessageAppended",
                true,
                appended(2, agent_core::Message::assistant("ATTACH-NEW-MESSAGE"))
            )
        ));
        std::fs::write(&path, text).unwrap();

        assert!(view.poll().expect("the new line folds"));
        let shown = screen(&mut view, 80, 24);
        assert!(
            shown.contains("ATTACH-NEW-MESSAGE"),
            "the appended message is not on screen:\n{shown}"
        );
        assert_eq!(view.history().len(), 3, "the folded history grew by one");
    }

    /// The footer carries the attach facts and nothing that belongs to the session's process.
    #[test]
    fn the_footer_names_only_the_attach_facts() {
        let (_scratch, mut view) = view("footer", "footer-me", &committed_journal());
        screen(&mut view, 80, 24);
        let footer = view.footer_text();
        assert!(
            footer.contains("attach · footer-me · read-only"),
            "{footer}"
        );
        assert!(footer.contains("scroll"), "{footer}");
        for absent in ["tokens", "cost", "model", "workspace", "idle", "running"] {
            assert!(
                !footer.contains(absent),
                "the footer must not carry `{absent}`: {footer}"
            );
        }
    }

    /// Only scrolling and quitting do anything; every other key is swallowed.
    #[test]
    fn a_key_that_is_not_a_scroll_or_quit_key_is_ignored() {
        let (_scratch, mut view) = view("keys", "keys-me", &committed_journal());
        screen(&mut view, 80, 8);
        let key = |code: KeyCode, modifiers: KeyModifiers| KeyEvent::new(code, modifiers);
        for ignored in [
            key(KeyCode::Char('x'), KeyModifiers::NONE),
            key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Up, KeyModifiers::NONE),
            key(KeyCode::Tab, KeyModifiers::NONE),
            key(KeyCode::Char('/'), KeyModifiers::NONE),
        ] {
            assert_eq!(
                view.on_key(&ignored),
                AttachKey::Ignored,
                "{ignored:?} is not a scroll or quit key"
            );
        }
        for quit in [
            key(KeyCode::Char('q'), KeyModifiers::NONE),
            key(KeyCode::Esc, KeyModifiers::NONE),
            key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(view.on_key(&quit), AttachKey::Quit, "{quit:?} quits");
        }
    }

    /// The scroll keys move the window and change the footer's position; `ctrl-end` follows
    /// again. The transcript here is longer than the view, so the position is not `all`.
    #[test]
    fn the_scroll_keys_move_the_window_and_bottom_follows_again() {
        let mut lines = committed_journal();
        for index in 2..40u64 {
            lines.push(envelope(
                index + 4,
                "MessageAppended",
                true,
                appended(
                    index,
                    agent_core::Message::assistant(format!("line {index}")),
                ),
            ));
        }
        let (_scratch, mut view) = view("scroll", "scroll-me", &lines);
        screen(&mut view, 80, 10);
        assert!(view.app.session.max_scroll > 0, "the transcript scrolls");
        assert!(view.app.session.follow, "a fresh view follows the tail");

        assert_eq!(
            view.on_key(&KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            AttachKey::Redraw
        );
        screen(&mut view, 80, 10);
        assert!(
            !view.app.session.follow,
            "page up pauses the follow (it is no longer at the bottom)"
        );
        let paused = view.app.session.scroll;

        assert_eq!(
            view.on_key(&KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL)),
            AttachKey::Redraw
        );
        screen(&mut view, 80, 10);
        assert!(view.app.session.follow, "ctrl-end follows again");
        assert!(
            view.app.session.scroll > paused,
            "and it is back at the tail"
        );
        assert!(
            view.footer_text().contains("follow"),
            "{}",
            view.footer_text()
        );
    }

    /// The view's history is the same fold a resume produces for the same file.
    #[test]
    fn the_history_is_the_resume_fold_of_the_same_file() {
        let (scratch, view) = view("fold", "fold-me", &committed_journal());
        let file = scratch.file("fold-me.jsonl");
        let session = radian_session::Session::open(&file).expect("a valid session file");
        assert_eq!(
            view.history(),
            session.history().expect("the fold").as_slice(),
            "the attach view and Session::open must fold the same history"
        );
    }
}
