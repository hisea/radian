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
//! **An open tool call.** The fold synthesises an error result for every call with no committed
//! result message (ADR 0002): on a **live** session that is exactly a call the runtime has
//! requested but not started (`queued`) or started but not settled (`running`), so showing the
//! crash wording there would be wrong. The view probes whether a live writer holds the file's
//! lock ([`radian_session::session_is_live`]): while it does, the synthesised results of open
//! calls are shown as `queued`/`running` lines and the real result replaces them when the settle
//! commits during `--follow`; only a session that is **not** live gets the recovery wording.
//!
//! **What it does with input.** Scrolling and quitting only (scroll keys as the main TUI binds
//! them, plus `q`/Esc/Ctrl-C). Every other key is swallowed: no input event, no command, no
//! write — the view holds no inbox and no command path at all.

use std::collections::HashMap;
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

/// The `queued`/`running` state of an open call, from the fold's synthesised result (ADR 0002).
/// The synthesised `not_started_after_crash` code means no `ToolExecutionStarted` was recorded
/// (`queued`); any other code — `outcome_unknown_after_crash`, or a real settled result whose
/// `Message::Tool` is not committed yet — means the call had started (`running`). A real
/// result in the suffix settles within the next commit, so it is transient.
fn open_call_state(result: &agent_core::ToolResult) -> &'static str {
    if result.error_code.as_deref() == Some("not_started_after_crash") {
        "queued"
    } else {
        "running"
    }
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
    /// How many **committed** messages of the observer's history are already entries. The
    /// repaired suffix after them is not counted here: it is rebuilt from the open calls.
    pushed: usize,
    /// Every declared tool call seen so far, `call_id -> (name, arguments)`, for an open
    /// call's line. Maintained as committed messages are pushed.
    calls: HashMap<String, (String, serde_json::Value)>,
    /// The `queued`/`running` lines currently at the transcript's tail (a live session's open
    /// calls), in call order: kept so a no-op read can be told from a change and the block
    /// rebuilt.
    pending: Vec<String>,
    /// Whether a live session process held the file's writer lock at the last read.
    live: bool,
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
            calls: HashMap::new(),
            pending: Vec::new(),
            live: false,
            working: false,
        };
        view.rebuild();
        Ok(view)
    }

    /// The session id this view is attached to.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The file being read. Read-only, like everything else here.
    pub fn path(&self) -> &Path {
        self.observer.path()
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
    /// ADR 0019), shrinks, or the session's live state flips (a live session's open calls are
    /// shown differently from a crash's).
    fn rebuild(&mut self) {
        self.app = App::new("", self.id.clone());
        self.calls.clear();
        self.pending.clear();
        self.working = false;
        self.live = self.observer.live();
        let history = self.observer.history().to_vec();
        let committed = self.observer.committed_len().min(history.len());
        self.push_committed(&history[..committed]);
        self.pushed = committed;
        // The repaired suffix (ADR 0002): one result per call with no committed result yet.
        let repairs = &history[committed..];
        if self.live {
            let desired = self.pending_lines(repairs);
            for line in &desired {
                self.app
                    .session
                    .entries
                    .push(Entry::new(EntryKind::Notice, line.clone()));
            }
            self.pending = desired;
        } else {
            // Not live: the fold's recovery wording is the truth (the turn really did not
            // finish). A static file does not change, so these are pushed once here.
            for message in repairs {
                self.app.push_message(message);
            }
        }
        self.append_working();
    }

    /// Pushes committed messages as entries, remembering every declared call for an open call's
    /// line. A `Message::Tool` result pairs with its own call (ADR 0028 R4) inside
    /// [`App::push_message`], so a settled call's real result replaces its `queued`/`running`
    /// line as soon as the commit is folded.
    fn push_committed(&mut self, messages: &[Message]) {
        for message in messages {
            if let Message::Assistant { tool_calls, .. } = message {
                for call in tool_calls {
                    self.calls
                        .insert(call.id.clone(), (call.name.clone(), call.arguments.clone()));
                }
            }
            self.app.push_message(message);
        }
    }

    /// The `queued`/`running` line of each open call in the repaired suffix, in call order.
    fn pending_lines(&self, repairs: &[Message]) -> Vec<String> {
        repairs
            .iter()
            .filter_map(|message| {
                let Message::Tool { result, .. } = message else {
                    return None;
                };
                let state = open_call_state(result);
                let label = self
                    .calls
                    .get(&result.call_id)
                    .map(|(name, arguments)| {
                        let card = crate::tools::ToolCard::new(
                            result.call_id.clone(),
                            name.clone(),
                            arguments.clone(),
                        );
                        crate::tools::call_text(&card, "")
                    })
                    .unwrap_or_else(|| result.call_id.clone());
                Some(format!("{state}: {label}"))
            })
            .collect()
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
        // The live state decides how an open call is shown, so a flip rebuilds too.
        let live = self.observer.live();
        if update.reset || replaced || update.history.len() < self.pushed || live != self.live {
            self.rebuild();
            return Ok(true);
        }
        if !live {
            // No writer holds the file: the open calls the last rebuild pushed (the fold's
            // recovery wording) stay, and a static journal does not change between polls. The
            // `working…` line still follows the durable turn state, so a `TurnStarted` or
            // `TurnFinished` must move it. A committed change here (a session that ran and
            // exited between two polls) rebuilds once from the new fold.
            if update.history_changed {
                self.rebuild();
                return Ok(true);
            }
            let was_working = self.working;
            self.drop_working();
            self.append_working();
            return Ok(self.working != was_working);
        }
        let committed = self.observer.committed_len().min(update.history.len());
        let was_working = self.working;
        let prev_pending = std::mem::take(&mut self.pending);
        self.drop_working();
        // The previous pending block sits right below the `working…` line.
        for _ in 0..prev_pending.len() {
            self.app.session.entries.pop();
        }
        let mut changed = false;
        if committed > self.pushed {
            self.push_committed(&update.history[self.pushed..committed]);
            self.pushed = committed;
            changed = true;
        }
        let desired = self.pending_lines(&update.history[committed..]);
        if desired != prev_pending {
            changed = true;
        }
        for line in &desired {
            self.app
                .session
                .entries
                .push(Entry::new(EntryKind::Notice, line.clone()));
        }
        self.pending = desired;
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
