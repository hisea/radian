//! `radian attach`'s fold: a session's **committed** facts → transcript entries (ADR 0043
//! decision 7, ADR 0023 U1).
//!
//! **What it reads.** The journal is the observation bus: no socket, no token, and the observed
//! session does not know anyone is looking. [`AttachFold`] wraps a [`JournalObserver`], whose
//! per-line parser is the **same** `LineScanner` and `TranscriptFold` a resume's `Session::open`
//! folds, and which never takes the writer's lock and never writes. The entries it produces are
//! the committed history of the prefix, folded by the same message→entry mapping a fold of the
//! journal uses anywhere else (the hub reads them too).
//!
//! **What it cannot show.** Streaming text and tool progress are live, never durable (ADR 0010
//! K2: `ModelTextDelta`, `ModelReasoningDelta`, `ToolProgress`, `ModelRequestStarted`), so a
//! message appears whole when it is committed and the frame never animates. "It is running" is
//! the last `TurnStarted` with no `TurnFinished` (a durable runtime event), shown as one
//! `working…` line at the transcript's tail — never a spinner. There is deliberately no
//! "waiting for approval" line: the runtime journals only the *decision*
//! (`PermissionResolved`), so an open approval is not a durable fact.
//!
//! **An open tool call.** The fold synthesises an error result for every call with no committed
//! result message (ADR 0002): on a **live** session that is exactly a call the runtime has
//! requested but not started (`queued`) or started but not settled (`running`), so showing the
//! crash wording there would be wrong. The fold probes whether a live writer holds the file's
//! lock ([`radian_session::session_is_live`]): while it does, the synthesised results of open
//! calls are shown as `queued`/`running` lines and the real result replaces them when the settle
//! commits during a follow; only a session that is **not** live gets the recovery wording.
//!
//! **What the renderer adds.** Scrolling, keys, themes and the drawing are the TUI's
//! (`radian-tui`'s `attach` module): it mirrors [`AttachFold::entries`] into its render entries
//! and draws those.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use radian_core::{Message, ToolResult};
use radian_session::EventData;
use serde_json::Value;

use crate::observer::{JournalObserver, SessionObserver};

use super::entry::{Card, CompactionInfo, Entry, EntryKind, EntryList};
use super::memory::MemoryRecords;
use super::parts::{bash_entry_from_context, display_parts, image_parts, text_of};
use super::tool::{ToolCard, body_line_count, call_text, result_display_text};

/// The line a running session shows (ADR 0043 decision 7). `…` is one character, not three dots.
pub const WORKING_LINE: &str = "working…";

/// The `queued`/`running` state of an open call, from the fold's synthesised result (ADR 0002).
/// The synthesised `not_started_after_crash` code means no `ToolExecutionStarted` was recorded
/// (`queued`); any other code — `outcome_unknown_after_crash`, or a real settled result whose
/// `Message::Tool` is not committed yet — means the call had started (`running`). A real result
/// in the suffix settles within the next commit, so it is transient.
pub fn open_call_state(result: &ToolResult) -> &'static str {
    if result.error_code.as_deref() == Some("not_started_after_crash") {
        "queued"
    } else {
        "running"
    }
}

/// One session file's committed facts, folded into entries. Read-only: it never writes and
/// never takes the writer's lock.
pub struct AttachFold {
    /// The committed facts, read-only over the journal file.
    observer: JournalObserver,
    /// The folded entries, with the change mark a renderer mirrors.
    entries: EntryList,
    /// How many **committed** messages of the observer's history are already entries. The
    /// repaired suffix after them is not counted here: it is rebuilt from the open calls.
    pushed: usize,
    /// Every declared tool call seen so far, `call_id -> (name, arguments)`, for an open
    /// call's line. Maintained as committed messages are pushed.
    calls: HashMap<String, (String, Value)>,
    /// The record types the transcript's settled calls established, in message order (ADR 0035).
    memory: MemoryRecords,
    /// The memory root a settled call resolves against. `None` when this process has none.
    memory_root: Option<PathBuf>,
    /// The `queued`/`running` lines currently at the transcript's tail (a live session's open
    /// calls), in call order: kept so a no-op read can be told from a change and the block
    /// rebuilt.
    pending: Vec<String>,
    /// Whether a live session process held the file's writer lock at the last read.
    live: bool,
    /// Whether the `working…` entry is currently the transcript's last entry.
    working: bool,
}

impl AttachFold {
    /// Opens `path` read-only and folds its committed prefix. The file must exist; the caller
    /// reports a missing session before entering a terminal.
    pub fn open(path: &Path, memory_root: Option<PathBuf>) -> Result<Self, String> {
        let observer = JournalObserver::open(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let mut fold = Self {
            observer,
            entries: EntryList::new(),
            pushed: 0,
            calls: HashMap::new(),
            memory: MemoryRecords::default(),
            memory_root,
            pending: Vec::new(),
            live: false,
            working: false,
        };
        fold.rebuild();
        Ok(fold)
    }

    /// The file being read. Read-only, like everything else here.
    pub fn path(&self) -> &Path {
        self.observer.path()
    }

    /// The transcript's committed history, as of the last read.
    pub fn history(&self) -> &[Message] {
        self.observer.history()
    }

    /// The folded entries, as of the last read.
    pub fn entries(&self) -> &[Entry] {
        self.entries.entries()
    }

    /// The change mark for a consumer that mirrors [`AttachFold::entries`].
    pub fn take_change(&mut self) -> Option<super::entry::Change> {
        self.entries.take_change()
    }
    /// Whether a turn is open: the last `TurnStarted` with no `TurnFinished` (ADR 0043 decision
    /// 7). A durable fact only — the observer folds the two runtime events into the projection.
    pub fn running(&self) -> bool {
        self.observer.projection().turn() == crate::projection::TurnState::Running
    }

    /// Sets the memory root a later settle resolves against. The view passes the same root the
    /// recording session used (ADR 0035).
    pub fn set_memory_root(&mut self, root: Option<PathBuf>) {
        self.memory_root = root;
    }

    /// Rebuilds every entry from the observer's history: the initial load and the rare case
    /// where the history is **replaced** (a compaction commits a `TranscriptReplaced` segment,
    /// ADR 0019), shrinks, or the session's live state flips (a live session's open calls are
    /// shown differently from a crash's).
    fn rebuild(&mut self) {
        self.entries.clear();
        // A rebuild is a fresh start for a consumer too: it must re-read every entry and may
        // reset its own scroll state (the frame the view drew no longer applies).
        self.entries.mark_reset();
        self.memory = MemoryRecords::default();
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
                self.entries
                    .push(Entry::new(EntryKind::Notice, line.clone()));
            }
            self.pending = desired;
        } else {
            // Not live: the fold's recovery wording is the truth (the turn really did not
            // finish). A static file does not change, so these are pushed once here.
            for message in repairs {
                self.push_message(message);
            }
        }
        self.append_working();
    }

    /// Pushes committed messages as entries, remembering every declared call for an open call's
    /// line. A `Message::Tool` result pairs with its own call (ADR 0028 R4) inside
    /// [`AttachFold::push_message`], so a settled call's real result replaces its
    /// `queued`/`running` line as soon as the commit is folded.
    fn push_committed(&mut self, messages: &[Message]) {
        for message in messages {
            if let Message::Assistant { tool_calls, .. } = message {
                for call in tool_calls {
                    self.calls
                        .insert(call.id.clone(), (call.name.clone(), call.arguments.clone()));
                }
            }
            self.push_message(message);
        }
    }

    /// One committed message as transcript entries. This is the journal's message→entry
    /// mapping: a user message (an attachment placeholder, a compaction summary, a `!` command
    /// block), an assistant message (reasoning, text, one card per call), a tool result (paired
    /// with its call by id).
    pub fn push_message(&mut self, message: &Message) {
        // ADR 0033: a `!` command's `Ran \`cmd\`` context message is a Bash block, not user
        // prose.
        if let Message::User { .. } = message
            && let Some(entry) = bash_entry_from_context(&message.text())
        {
            self.entries.push(entry);
            return;
        }
        match message {
            Message::User { content, .. } => {
                // A compaction summary (ADR 0028 R5) is a `User` message with a
                // `compaction_summary` origin (ADR 0018); it shows as one line, not as a wall
                // of user text. Core's readers supply the summary and the verbatim user
                // words - nothing is parsed out of the body here.
                if radian_core::compaction::is_compaction_summary(message) {
                    let info = CompactionInfo {
                        summary: radian_core::compaction::compaction_summary_text(message)
                            .unwrap_or_default(),
                        user_messages: radian_core::compaction::compaction_user_messages(message)
                            .into_iter()
                            .map(|message| message.text)
                            .collect(),
                        ..CompactionInfo::default()
                    };
                    let mut entry = Entry::new(EntryKind::Compaction, info.display_text());
                    entry.compaction = Some(Box::new(info));
                    self.entries.push(entry);
                    return;
                }
                // ADR 0018: an attachment is its own part, so the display renders it as
                // `[file: P, N lines]` rather than joining its body into the message.
                let mut entry = Entry::new(EntryKind::User, display_parts(content));
                // ADR 0028 R10: the images draw under their placeholder lines.
                entry.images = image_parts(content);
                self.entries.push(entry);
            }
            Message::Assistant {
                content,
                tool_calls,
                reasoning,
                interrupted,
                ..
            } => {
                let reasoning = text_of(reasoning);
                if !reasoning.is_empty() {
                    self.entries
                        .push(Entry::new(EntryKind::Reasoning, reasoning));
                }
                let mut entry = Entry::new(EntryKind::Assistant, text_of(content));
                entry.interrupted = *interrupted;
                self.entries.push(entry);
                // One card per call (ADR 0028 R4): each carries its own name and arguments,
                // and its text is the same header the renderer will show.
                for call in tool_calls {
                    let card =
                        ToolCard::new(call.id.clone(), call.name.clone(), call.arguments.clone());
                    let mut entry = Entry::new(EntryKind::ToolCall, call_text(&card, ""));
                    entry.tool = Some(Box::new(card));
                    // A call with no result yet is a whole card of its own (ADR 0028); the
                    // pairing below turns it into the head of the merged card.
                    entry.card = Card::Solo;
                    self.entries.push(entry);
                }
            }
            Message::Tool { result, .. } => {
                // The result pairs with its call **by id** (the same key the live and the
                // resume paths carry), so a batch of calls pairs correctly whatever order their
                // results arrive in.
                //
                // The pairing is a **single** scan from the back: the call is the most recent
                // matching card, and the index it found is reused for the insertion below.
                let mut paired: Option<(String, Value, Option<super::memory::MemoryRef>)> = None;
                let mut call_index: Option<usize> = None;
                // The result's own text, read once: the memory card resolves from it, and the
                // body's display text and the head both use it.
                let tool_text = result.text();
                let len = self.entries.len();
                for (rev, entry) in self.entries.iter_mut().rev().enumerate() {
                    if entry.kind == EntryKind::ToolCall
                        && entry
                            .tool
                            .as_ref()
                            .is_some_and(|card| card.call_id == result.call_id)
                    {
                        entry.result = Some(result.is_error);
                        if let Some(card) = entry.tool.as_mut() {
                            card.structured = result.structured.clone();
                            card.is_error = result.is_error;
                            // ADR 0035: a settled call into a memory scope carries the
                            // record's type and path, so the renderer draws the memory card
                            // (or the read line) instead of the ordinary tool card. The ref
                            // comes from the recorded call and result alone, never from the
                            // file, so a resume redraws it exactly.
                            card.memory = if result.is_error {
                                None
                            } else {
                                self.memory.settle(
                                    self.memory_root.as_deref(),
                                    &card.name,
                                    &card.arguments,
                                    &tool_text,
                                )
                            };
                            // The head's title can only be final now, so the entry's text -
                            // what renders and copies - is regenerated from the same function
                            // the layout uses. The result's own row count is part of the head's
                            // collapsed meta (`· N lines`).
                            let display = result_display_text(card, &tool_text);
                            card.result_lines = Some(body_line_count(&display));
                            // ADR 0024: the call entry draws the whole ask block (its result
                            // entry is blank), so it keeps the result's own text for the
                            // declined/cancelled status line inside the box.
                            entry.text = if card.name == "ask_user" {
                                tool_text.clone()
                            } else {
                                call_text(card, &tool_text)
                            };
                            paired = Some((
                                card.name.clone(),
                                card.arguments.clone(),
                                card.memory.clone(),
                            ));
                        }
                        let index = len - 1 - rev;
                        call_index = Some(index);
                        self.entries.mark(index);
                        break;
                    }
                }
                let (name, arguments, memory) = paired.unwrap_or_default();
                let mut card = ToolCard::new(result.call_id.clone(), name, arguments);
                card.structured = result.structured.clone();
                card.is_error = result.is_error;
                // The result entry carries the same ref as its call: it closes a merged card
                // and draws nothing (the call entry draws the box), and a torn result whose
                // call is gone has neither name nor arguments, so it carries none.
                card.memory = memory;
                // The body's text **is** the entry's text (the mapping/search invariant): a
                // `patch` shows the synthesized diff, a `write` the content written, every
                // other tool its own result text.
                let display = result_display_text(&card, &tool_text);
                let mut entry = Entry::new(EntryKind::ToolResult, display);
                entry.is_error = result.is_error;
                entry.error_code = result.error_code.clone();
                entry.tool = Some(Box::new(card));
                // ADR 0028 R10: a `read` image (or any tool's image part) draws under a
                // placeholder row inside the block's body.
                entry.images = image_parts(&result.content);
                entry.card = Card::Solo;
                // Rule 4: the result sits right after **its own** call, so a batch reads
                // call/result, call/result instead of every call then every result. The
                // entries are the UI's own order; the journal keeps the message order, so a
                // resume replay runs this same code and lands on the same order. A result
                // whose call is gone (a torn transcript) is appended.
                match call_index {
                    Some(index) => {
                        // The call heads the merged card and this result closes it (ADR 0028):
                        // the drawing merges them, the data stays two entries.
                        if let Some(call) = self.entries.get_mut(index) {
                            call.card = Card::Head;
                            self.entries.mark(index);
                        }
                        entry.card = Card::Tail;
                        self.entries.insert(index + 1, entry);
                    }
                    None => {
                        self.entries.push(entry);
                    }
                }
            }
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
                        let card =
                            ToolCard::new(result.call_id.clone(), name.clone(), arguments.clone());
                        call_text(&card, "")
                    })
                    .unwrap_or_else(|| result.call_id.clone());
                Some(format!("{state}: {label}"))
            })
            .collect()
    }

    /// Appends the `working…` line when a turn is open and it is not there yet.
    fn append_working(&mut self) {
        if !self.working && self.running() {
            self.entries
                .push(Entry::new(EntryKind::Notice, WORKING_LINE));
            self.working = true;
        }
    }

    /// Removes the `working…` line. It is the tail whenever it exists: every append goes
    /// through [`AttachFold::append_working`] **after** the messages that would follow it.
    fn drop_working(&mut self) {
        if self.working {
            self.entries.pop();
            self.working = false;
        }
    }

    /// Reads the bytes another process appended since the last read (a follow), folds them and
    /// updates the entries. Returns whether anything the fold shows changed.
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
            self.entries.pop();
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
            self.entries
                .push(Entry::new(EntryKind::Notice, line.clone()));
        }
        self.pending = desired;
        self.append_working();
        Ok(changed || self.working != was_working)
    }
}
