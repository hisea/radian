//! An ACP worker's `result.jsonl` folded into transcript entries (ADR 0048 §13).
//!
//! The wrapper writes every protocol line to the attempt's `result.jsonl` as
//! `{"type":"acp","message":<protocol object>}` and every permission answer as
//! `{"type":"acp","answer":{...}}` (ADR 0048 decision 7). This module turns that stream into
//! the folded [`Entry`] list, so an ACP attempt is drawn with the transcript's Markdown,
//! thinking block, tool cards and diffs instead of one row per token — and the hub can send the
//! same entries (ADR 0023 U1).
//!
//! The fold is append-only after the first entry: a growing stream appends to the current text
//! run or updates a tool card in place. It keeps at most [`MAX_ENTRIES`] entries, each text
//! capped at [`MAX_ENTRY_BYTES`]; when the window is full the oldest [`DROP`] entries are
//! dropped, so the memory cost is fixed and a worker that prints without bound cannot exhaust
//! the reader. Unanswered permission request ids are capped at `MAX_PERMISSIONS` for the same
//! reason. `usage_update`, `session_info_update`, `available_commands_update`,
//! `current_mode_update` and an unknown `sessionUpdate` are not transcript rows; at most one
//! weak status line carries the latest context use and title. A line that is neither ACP nor
//! `Result` keeps the row view's text as one weak notice, so a mixed file stays readable.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::entry::{Card, Entry, EntryKind, EntryList};
use super::parts::{first_summary, line_breaks_to_spaces};
use super::tool::{ToolCard, body_line_count, result_display_text};

/// The most transcript entries the fold keeps.
pub const MAX_ENTRIES: usize = 512;

/// The most bytes one entry's text keeps. A stream longer than this is cut at a character
/// boundary; the rest is dropped.
pub const MAX_ENTRY_BYTES: usize = 8 * 1024;

/// How many oldest entries are dropped when the window is full. Dropping in a batch keeps the
/// per-line cost amortised instead of shifting the whole vector on every chunk.
pub const DROP: usize = 128;

/// The most unanswered permission requests the fold remembers. An id leaves the map when its
/// answer arrives, so the cap only bites on a stream of requests that are never answered; the
/// oldest is evicted and its answer falls back to the newest tool, as any unknown id does.
/// Keeping the map bounded is what makes the fold's memory fixed (ADR 0048 §13).
const MAX_PERMISSIONS: usize = 64;

/// One unanswered `session/request_permission`: the `toolCallId` it asks about and when it
/// arrived, so the oldest entry is the one evicted at [`MAX_PERMISSIONS`].
struct PermissionRequest {
    tool: String,
    seq: u64,
}

/// One tool call as the protocol reports it, merged across `tool_call` and `tool_call_update`.
struct ToolState {
    id: String,
    title: String,
    kind: String,
    status: String,
    /// The newest `content` array, whole.
    content: Option<Value>,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
    /// The call entry's index.
    call: Option<usize>,
    /// The result entry's index, once there is output to draw.
    result: Option<usize>,
    /// The permission answer's index, once one was attached.
    permission: Option<usize>,
}

/// The one weak status line `usage_update` and `session_info_update` share.
struct StatusLine {
    entry: Option<usize>,
    used: Option<u64>,
    size: Option<u64>,
    title: Option<String>,
}

/// The current run of text chunks. The entry is already in the transcript; the next chunk of the
/// same kind appends to it.
struct Run {
    kind: RunKind,
    entry: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    Message,
    Thought,
    User,
}

/// The folded transcript of one ACP attempt.
pub struct AcpFold {
    entries: EntryList,
    run: Option<Run>,
    tools: HashMap<String, ToolState>,
    plan: Option<usize>,
    status: Option<StatusLine>,
    /// Unanswered `session/request_permission` request ids → the tool they ask about.
    permissions: HashMap<String, PermissionRequest>,
    /// Arrival order of the entries in `permissions`; the oldest is evicted at the cap.
    permission_seq: u64,
    /// The newest tool a permission answer can be attached to when its request id is unknown.
    last_tool: Option<String>,
}

impl Default for AcpFold {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpFold {
    pub fn new() -> Self {
        Self {
            entries: EntryList::new(),
            run: None,
            tools: HashMap::new(),
            plan: None,
            status: None,
            permissions: HashMap::new(),
            permission_seq: 0,
            last_tool: None,
        }
    }

    /// Whether the attempt has produced no entry yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The folded entries, oldest first.
    pub fn entries(&self) -> &[Entry] {
        self.entries.entries()
    }

    /// The change mark for a consumer that mirrors [`AcpFold::entries`].
    pub fn take_change(&mut self) -> Option<super::entry::Change> {
        self.entries.take_change()
    }

    /// How many unanswered permission requests the fold remembers (its bound is
    /// `MAX_PERMISSIONS`).
    pub fn outstanding_permissions(&self) -> usize {
        self.permissions.len()
    }

    /// Folds one complete line of the attempt's `result.jsonl`.
    pub fn push_line(&mut self, raw: &str) {
        let Ok(Value::Object(object)) = serde_json::from_str::<Value>(raw) else {
            self.push_row(raw);
            return;
        };
        match object.get("type").and_then(Value::as_str) {
            Some("acp") => self.acp_line(&object),
            Some("Result") => self.outcome(&object),
            _ => self.push_row(raw),
        }
    }

    /// A line that is neither ACP nor `Result`: the row view's text as one weak notice, so a
    /// mixed file keeps every line in order.
    fn push_row(&mut self, raw: &str) {
        self.run = None;
        self.push_entry(Entry::new(EntryKind::Notice, row_text(raw)));
    }

    /// The `Result` line: the outcome in the done or the failed colour, the text folded to one
    /// line. An empty text adds nothing.
    fn outcome(&mut self, object: &Map<String, Value>) {
        self.run = None;
        let outcome = object.get("outcome").and_then(Value::as_str).unwrap_or("-");
        let text = object
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut body = format!("Result {outcome}");
        if !text.is_empty() {
            body.push(' ');
            body.push_str(&line_breaks_to_spaces(text));
        }
        let ok = outcome == "completed";
        self.push_entry(Entry::new(EntryKind::Outcome { ok }, cap_text(body)));
    }

    /// One `{"type":"acp", ...}` line: a protocol message or a permission answer.
    fn acp_line(&mut self, object: &Map<String, Value>) {
        if let Some(answer) = object.get("answer").and_then(Value::as_object) {
            self.permission(answer);
            return;
        }
        let Some(message) = object.get("message").and_then(Value::as_object) else {
            self.push_row(&Value::Object(object.clone()).to_string());
            return;
        };
        match message.get("method").and_then(Value::as_str) {
            Some("session/update") => {
                if let Some(update) = message
                    .get("params")
                    .and_then(|params| params.get("update"))
                {
                    self.session_update(update);
                }
            }
            Some("session/request_permission") => {
                let tool = message
                    .get("params")
                    .and_then(|params| params.get("toolCall"))
                    .and_then(|tool| tool.get("toolCallId"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let (Some(id), Some(tool)) = (message.get("id").and_then(request_key), &tool) {
                    self.remember_permission(id, tool.clone());
                }
                if tool.is_some() {
                    self.last_tool = tool;
                }
            }
            // Every other protocol line is bookkeeping: it is not a transcript row.
            _ => {}
        }
    }

    fn session_update(&mut self, update: &Value) {
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "agent_message_chunk" => {
                self.push_chunk(RunKind::Message, &chunk_text(update.get("content")));
                return;
            }
            "agent_thought_chunk" => {
                self.push_chunk(RunKind::Thought, &chunk_text(update.get("content")));
                return;
            }
            "user_message_chunk" => {
                self.push_chunk(RunKind::User, &chunk_text(update.get("content")));
                return;
            }
            _ => {
                // Any other update ends the contiguous text run.
                self.run = None;
            }
        }
        match kind {
            "tool_call" | "tool_call_update" => self.tool_update(update),
            "plan" => self.plan_update(update),
            "usage_update" => {
                let used = update.get("used").and_then(Value::as_u64);
                let size = update.get("size").and_then(Value::as_u64);
                self.status_update(used, size, None);
            }
            "session_info_update" => {
                let title = update
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.status_update(None, None, title);
            }
            // `available_commands_update`, `current_mode_update` and any unknown kind are not
            // transcript rows (ADR 0048 §13).
            _ => {}
        }
    }

    /// Appends one text chunk to the current run, or opens a new entry for a new run. Chunks of
    /// different kinds never merge, so a thought and a message stay two blocks.
    fn push_chunk(&mut self, kind: RunKind, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(run) = &self.run
            && run.kind == kind
            && let Some(entry) = self.entries.get_mut(run.entry)
        {
            append_capped(&mut entry.text, text);
            self.entries.mark(run.entry);
            return;
        }
        self.run = None;
        let entry_kind = match kind {
            RunKind::Message => EntryKind::Assistant,
            RunKind::Thought => EntryKind::Reasoning,
            RunKind::User => EntryKind::User,
        };
        let mut entry = Entry::new(entry_kind, text);
        cap_entry(&mut entry);
        if let Some(index) = self.push_entry(entry) {
            self.run = Some(Run { kind, entry: index });
        }
    }

    fn tool_update(&mut self, update: &Value) {
        let id = update
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_owned();
        let state = self.tools.entry(id.clone()).or_insert_with(|| ToolState {
            id: id.clone(),
            title: String::new(),
            kind: String::new(),
            status: String::new(),
            content: None,
            raw_input: None,
            raw_output: None,
            call: None,
            result: None,
            permission: None,
        });
        if let Some(title) = update.get("title").and_then(Value::as_str) {
            state.title = title.to_owned();
        }
        if let Some(kind) = update.get("kind").and_then(Value::as_str) {
            state.kind = kind.to_owned();
        }
        if let Some(status) = update.get("status").and_then(Value::as_str) {
            state.status = status.to_owned();
        }
        if let Some(content) = update.get("content") {
            state.content = Some(match content {
                Value::Array(_) => content.clone(),
                other => Value::Array(vec![other.clone()]),
            });
        }
        if let Some(raw) = update.get("rawInput") {
            state.raw_input = Some(raw.clone());
        }
        if let Some(raw) = update.get("rawOutput") {
            state.raw_output = Some(raw.clone());
        }
        self.last_tool = Some(id.clone());
        self.refresh_tool(&id);
    }

    /// Rebuilds one tool's card from the merged state and keeps its call and result entries in
    /// the transcript. An update for an unknown id creates the entry.
    fn refresh_tool(&mut self, id: &str) {
        let Some(state) = self.tools.get(id) else {
            return;
        };
        let title = if !state.title.is_empty() {
            state.title.clone()
        } else if !state.kind.is_empty() {
            state.kind.clone()
        } else {
            state.id.clone()
        };
        let title = cap_text(title);
        let status = status_text(&state.status);
        let diffs = content_diffs(state.content.as_ref());
        let output = tool_output(state);
        let mut card = card_for(state, &diffs, status.clone());
        let display = result_text(&card, &diffs, output.as_deref());
        if let Some((_, split)) = &display {
            card.trailing_from = *split;
        }
        let result_state = match status.as_deref() {
            Some("done") => Some(false),
            Some("failed") => Some(true),
            _ => None,
        };

        // The call entry: reuse the tracked index when it still points at this card, else find
        // it, else append one. An out-of-order `tool_call_update` takes the append path.
        let mut call = self
            .tools
            .get(id)
            .and_then(|state| state.call)
            .filter(|index| {
                self.entries.get(*index).is_some_and(|entry| {
                    entry.kind == EntryKind::ToolCall
                        && entry.tool.as_ref().is_some_and(|card| card.call_id == id)
                })
            });
        if call.is_none() {
            call = self.entries.entries().iter().position(|entry| {
                entry.kind == EntryKind::ToolCall
                    && entry.tool.as_ref().is_some_and(|card| card.call_id == id)
            });
        }
        let call = match call {
            Some(index) => index,
            None => {
                let mut entry = Entry::new(EntryKind::ToolCall, title.clone());
                entry.tool = Some(Box::new(card.clone()));
                entry.card = Card::Solo;
                entry.result = result_state;
                entry.is_error = status.as_deref() == Some("failed");
                let Some(index) = self.push_entry(entry) else {
                    return;
                };
                if let Some(state) = self.tools.get_mut(id) {
                    state.call = Some(index);
                }
                index
            }
        };
        let result = self
            .tools
            .get(id)
            .and_then(|state| state.result)
            .filter(|index| {
                self.entries
                    .get(*index)
                    .is_some_and(|entry| entry.kind == EntryKind::ToolResult)
            });
        {
            let mut entry = Entry::new(EntryKind::ToolCall, title);
            entry.tool = Some(Box::new(card.clone()));
            entry.card = if result.is_some() {
                Card::Head
            } else {
                Card::Solo
            };
            entry.result = result_state;
            entry.is_error = status.as_deref() == Some("failed");
            if let Some(index) = result {
                entry.tool.as_mut().unwrap().result_lines = self
                    .entries
                    .get(index)
                    .map(|result| body_line_count(&result.text));
            }
            self.entries.replace(call, entry);
        }

        let Some((display, _)) = display else {
            return;
        };
        if let Some(index) = result {
            let mut entry = Entry::new(EntryKind::ToolResult, display);
            entry.tool = Some(Box::new(card));
            entry.is_error = status.as_deref() == Some("failed");
            entry.card = Card::Tail;
            self.entries.replace(index, entry);
            return;
        }
        let lines = body_line_count(&display);
        let mut entry = Entry::new(EntryKind::ToolResult, display);
        entry.tool = Some(Box::new(card));
        entry.is_error = status.as_deref() == Some("failed");
        entry.card = Card::Tail;
        let Some(index) = self.insert_entry(call + 1, entry) else {
            return;
        };
        if let Some(state) = self.tools.get_mut(id) {
            state.result = Some(index);
        }
        if let Some(call) = self.tools.get(id).and_then(|state| state.call) {
            self.mark_result(call, lines);
        }
    }

    /// The call entry heads the merged card and knows how many body rows its result has (the
    /// collapsed head's `· N lines`).
    fn mark_result(&mut self, call: usize, lines: usize) {
        if let Some(entry) = self.entries.get_mut(call)
            && entry.kind == EntryKind::ToolCall
        {
            entry.card = Card::Head;
            if let Some(card) = entry.tool.as_mut() {
                card.result_lines = Some(lines);
            }
            self.entries.mark(call);
        }
    }

    fn plan_update(&mut self, update: &Value) {
        self.run = None;
        let mut text = String::from("plan");
        if let Some(entries) = update.get("entries").and_then(Value::as_array) {
            for item in entries {
                let status = item.get("status").and_then(Value::as_str).unwrap_or("-");
                let content = item
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                text.push('\n');
                text.push_str(status);
                text.push_str("  ");
                text.push_str(content);
            }
        }
        let text = cap_text(text);
        if let Some(index) = self.plan {
            self.entries
                .replace(index, Entry::new(EntryKind::Notice, text));
            return;
        }
        if let Some(index) = self.push_entry(Entry::new(EntryKind::Notice, text)) {
            self.plan = Some(index);
        }
    }

    /// At most one weak line shows the latest context use and title; the next update replaces
    /// it instead of appending another.
    fn status_update(&mut self, used: Option<u64>, size: Option<u64>, title: Option<String>) {
        let status = self.status.get_or_insert(StatusLine {
            entry: None,
            used: None,
            size: None,
            title: None,
        });
        if used.is_some() {
            status.used = used;
        }
        if size.is_some() {
            status.size = size;
        }
        if let Some(title) = title {
            status.title = Some(title);
        }
        let mut parts = Vec::new();
        match (status.used, status.size) {
            (Some(used), Some(size)) => parts.push(format!("context {used}/{size}")),
            (Some(used), None) => parts.push(format!("context {used}")),
            (None, Some(size)) => parts.push(format!("context ?/{size}")),
            (None, None) => {}
        }
        if let Some(title) = status
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            parts.push(title.to_owned());
        }
        let entry = status.entry;
        if parts.is_empty() {
            return;
        }
        let text = cap_text(parts.join(" · "));
        if let Some(index) = entry {
            self.entries
                .replace(index, Entry::new(EntryKind::Notice, text));
            return;
        }
        if let Some(index) = self.push_entry(Entry::new(EntryKind::Notice, text))
            && let Some(status) = self.status.as_mut()
        {
            status.entry = Some(index);
        }
    }

    /// Remembers one unanswered request. At [`MAX_PERMISSIONS`] the oldest entry is evicted, so
    /// a stream of requests that are never answered cannot grow the map without bound.
    fn remember_permission(&mut self, id: String, tool: String) {
        self.permission_seq += 1;
        let seq = self.permission_seq;
        if self.permissions.len() >= MAX_PERMISSIONS && !self.permissions.contains_key(&id) {
            let oldest = self
                .permissions
                .iter()
                .min_by_key(|(_, request)| request.seq)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.permissions.remove(&oldest);
            }
        }
        self.permissions.insert(id, PermissionRequest { tool, seq });
    }

    /// The answer to a permission request: one weak line after the tool it answers.
    fn permission(&mut self, answer: &Map<String, Value>) {
        self.run = None;
        let kind = answer.get("kind").and_then(Value::as_str).unwrap_or("-");
        let text = format!("permission {kind}");
        let request = answer.get("requestId").and_then(request_key);
        let tool = request
            .and_then(|id| self.permissions.remove(&id))
            .map(|request| request.tool)
            .or_else(|| self.last_tool.clone());
        let Some(tool) = tool else {
            self.push_entry(Entry::new(EntryKind::Notice, text));
            return;
        };
        let Some((call, result, permission)) = self
            .tools
            .get(&tool)
            .map(|state| (state.call, state.result, state.permission))
        else {
            self.push_entry(Entry::new(EntryKind::Notice, text));
            return;
        };
        if let Some(index) = permission {
            self.entries
                .replace(index, Entry::new(EntryKind::Notice, text));
            return;
        }
        let after = match (call, result) {
            (Some(call), Some(result)) => Some(call.max(result) + 1),
            (Some(call), None) => Some(call + 1),
            _ => None,
        };
        let Some(after) = after else {
            self.push_entry(Entry::new(EntryKind::Notice, text));
            return;
        };
        if let Some(index) = self.insert_entry(after, Entry::new(EntryKind::Notice, text))
            && let Some(state) = self.tools.get_mut(&tool)
        {
            state.permission = Some(index);
        }
    }

    /// Appends one entry. Returns its index after the memory window was trimmed, or `None` when
    /// the entry itself fell out of the window.
    fn push_entry(&mut self, mut entry: Entry) -> Option<usize> {
        cap_entry(&mut entry);
        let mut index = self.entries.push(entry);
        let dropped = self.trim();
        if index < dropped {
            return None;
        }
        index -= dropped;
        Some(index)
    }

    /// Inserts one entry at `at`, shifting every tracked index after it. Returns the inserted
    /// index after trimming, or `None` when it fell out of the window.
    fn insert_entry(&mut self, at: usize, entry: Entry) -> Option<usize> {
        let at = self.entries.insert(at, entry);
        self.shift_from(at, 1);
        let dropped = self.trim();
        if at < dropped {
            None
        } else {
            Some(at - dropped)
        }
    }

    /// Drops the oldest [`DROP`] entries once the window is full. Returns how many were dropped.
    fn trim(&mut self) -> usize {
        let len = self.entries.len();
        if len <= MAX_ENTRIES {
            return 0;
        }
        let drop = DROP.min(len);
        self.entries.drain_front(drop);
        self.forget_before(drop);
        drop
    }

    fn shift_from(&mut self, at: usize, delta: usize) {
        if let Some(run) = &mut self.run
            && run.entry >= at
        {
            run.entry += delta;
        }
        if let Some(index) = &mut self.plan
            && *index >= at
        {
            *index += delta;
        }
        if let Some(status) = &mut self.status
            && let Some(index) = &mut status.entry
            && *index >= at
        {
            *index += delta;
        }
        for tool in self.tools.values_mut() {
            shift_index(&mut tool.call, at, delta);
            shift_index(&mut tool.result, at, delta);
            shift_index(&mut tool.permission, at, delta);
        }
    }

    /// Forgets every index before `drop`, and every tool whose call entry was dropped. A later
    /// update for such a tool starts a fresh card, exactly as an out-of-order update does.
    fn forget_before(&mut self, drop: usize) {
        if let Some(run) = &mut self.run {
            if run.entry < drop {
                self.run = None;
            } else {
                run.entry -= drop;
            }
        }
        if self.plan.is_some_and(|index| index < drop) {
            self.plan = None;
        } else if let Some(index) = &mut self.plan {
            *index -= drop;
        }
        if let Some(status) = &mut self.status {
            if status.entry.is_some_and(|index| index < drop) {
                status.entry = None;
            } else if let Some(index) = &mut status.entry {
                *index -= drop;
            }
        }
        let mut removed = Vec::new();
        for (id, tool) in &mut self.tools {
            if tool.call.is_some_and(|index| index < drop) {
                removed.push(id.clone());
                continue;
            }
            drop_index(&mut tool.call, drop);
            drop_index(&mut tool.result, drop);
            drop_index(&mut tool.permission, drop);
        }
        for id in removed {
            self.tools.remove(&id);
            if self.last_tool.as_deref() == Some(id.as_str()) {
                self.last_tool = None;
            }
        }
    }
}

/// A JSON-RPC request id as a map key. The protocol allows a number or a string id and the
/// wrapper logs the agent's own `id` value unchanged, so both become keys; the prefix keeps the
/// number `7` and the string `"7"` apart, as the protocol treats them as two requests.
fn request_key(id: &Value) -> Option<String> {
    match id {
        Value::Number(number) => Some(format!("n:{number}")),
        Value::String(text) => Some(format!("s:{text}")),
        _ => None,
    }
}

/// A tool content block of type `diff`, reduced to what the patch card needs.
struct DiffContent {
    path: String,
    old: String,
    new: String,
}

fn card_for(state: &ToolState, diffs: &[DiffContent], status: Option<String>) -> ToolCard {
    let mut card = match diffs {
        [] => ToolCard::new(
            state.id.clone(),
            if state.kind.is_empty() {
                "acp"
            } else {
                state.kind.as_str()
            },
            state.raw_input.clone().unwrap_or(Value::Null),
        ),
        // One file keeps the single `old`/`new` pair, so its body is exactly the one the fold
        // drew before and its `+N −M` counts come from the same arguments.
        [diff] => ToolCard::new(
            state.id.clone(),
            "patch",
            json!({"path": diff.path, "old": diff.old, "new": diff.new}),
        ),
        // Several files: the call's edits sum the counts, and the body is built from the
        // blocks themselves so each file keeps its own header (`diffs_text`).
        many => ToolCard::new(
            state.id.clone(),
            "patch",
            json!({
                "path": many[0].path,
                "edits": many
                    .iter()
                    .map(|diff| json!({"old": diff.old, "new": diff.new}))
                    .collect::<Vec<_>>(),
            }),
        ),
    };
    card.status = status;
    card
}

/// The status the head shows. ACP's `pending` is the card's queued state, `in_progress` its
/// running one (ADR 0048 §13).
fn status_text(status: &str) -> Option<String> {
    match status {
        "" => None,
        "pending" => Some("queued".to_owned()),
        "in_progress" => Some("running".to_owned()),
        "completed" => Some("done".to_owned()),
        "failed" => Some("failed".to_owned()),
        other => Some(other.to_owned()),
    }
}

/// Every `diff` content block of a tool's newest `content` array, in order. Each block draws:
/// a call that edited two files draws both (ADR 0048 §13).
fn content_diffs(content: Option<&Value>) -> Vec<DiffContent> {
    let Some(items) = content.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            if item.get("type").and_then(Value::as_str) != Some("diff") {
                return None;
            }
            Some(DiffContent {
                path: item
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                old: item
                    .get("oldText")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                new: item
                    .get("newText")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect()
}

/// The result entry's text: the `diff` blocks as the patch body, then the `content` text
/// after them (ADR 0048 §13). The second value is the byte offset where that plain text
/// starts, when a diff comes first and the text survives the entry cap; the body renderer
/// uses it to draw the tail as ordinary rows instead of diff rows.
fn result_text(
    card: &ToolCard,
    diffs: &[DiffContent],
    output: Option<&str>,
) -> Option<(String, Option<usize>)> {
    if diffs.is_empty() {
        // No diff: the text is the body, through the built-in renderer for the card's kind - a
        // `write` still shows the content it wrote, a `patch` its own diff (ADR 0028).
        return output.map(|text| (cap_text(result_display_text(card, text)), None));
    }
    let diff_text = if diffs.len() == 1 {
        result_display_text(card, "")
    } else {
        diffs_text(diffs)
    };
    let split = diff_text.len();
    let raw = match output {
        // A blank row separates the diff from the text, so the text is not read as the last
        // file's rows.
        Some(text) => format!("{diff_text}\n{text}"),
        None => diff_text,
    };
    let text = cap_text(raw);
    let split = (split < text.len()).then_some(split);
    Some((text, split))
}

/// Several `diff` blocks draw in order, each under its own `---`/`+++` file header, so the
/// rows on either side of a file boundary are not read as one file's change and each file
/// highlights by its own path (ADR 0048 §13).
fn diffs_text(diffs: &[DiffContent]) -> String {
    let mut out = String::new();
    for diff in diffs {
        out.push_str("--- a/");
        out.push_str(&diff.path);
        out.push('\n');
        out.push_str("+++ b/");
        out.push_str(&diff.path);
        out.push('\n');
        let card = ToolCard::new(
            "",
            "patch",
            json!({"path": diff.path, "old": diff.old, "new": diff.new}),
        );
        out.push_str(&result_display_text(&card, ""));
    }
    out
}

/// The result text a tool's `content` blocks carry: `content` text joined in order, else the
/// `rawOutput` when it is a string (or a compact object). `diff` blocks are the card's body and
/// `terminal` blocks have only an id, so neither contributes text (ADR 0048 §13).
fn tool_output(state: &ToolState) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(content) = state.content.as_ref().and_then(Value::as_array) {
        for item in content {
            if item.get("type").and_then(Value::as_str) != Some("content") {
                continue;
            }
            let text = chunk_text(item.get("content"));
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }
    if parts.is_empty()
        && let Some(raw) = &state.raw_output
        && !raw.is_null()
    {
        parts.push(match raw {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        });
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// A chunk's text: the content block's `text`, an array of blocks joined, or a bare string.
fn chunk_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| chunk_text(Some(item)))
            .collect::<String>(),
        Some(Value::Object(object)) => object
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

/// The row view's text for a non-ACP line: the type and the first summary, or the object
/// compactly. Kept identical to `super::row_line`'s text so a mixed file reads as before.
fn row_text(raw: &str) -> String {
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(raw) else {
        return raw.to_owned();
    };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_owned();
    let body = match first_summary(&object) {
        Some(summary) => line_breaks_to_spaces(summary),
        None => Value::Object(object).to_string(),
    };
    format!("{kind} {body}")
}

fn cap_text(text: String) -> String {
    if text.len() <= MAX_ENTRY_BYTES {
        return text;
    }
    let mut end = MAX_ENTRY_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn cap_entry(entry: &mut Entry) {
    if entry.text.len() > MAX_ENTRY_BYTES {
        entry.text = cap_text(std::mem::take(&mut entry.text));
    }
}

fn append_capped(text: &mut String, chunk: &str) {
    if text.len() >= MAX_ENTRY_BYTES {
        return;
    }
    let room = MAX_ENTRY_BYTES - text.len();
    let mut end = room.min(chunk.len());
    while !chunk.is_char_boundary(end) {
        end -= 1;
    }
    text.push_str(&chunk[..end]);
}

fn shift_index(index: &mut Option<usize>, at: usize, delta: usize) {
    if let Some(value) = index
        && *value >= at
    {
        *value += delta;
    }
}

fn drop_index(index: &mut Option<usize>, drop: usize) {
    match *index {
        Some(value) if value < drop => *index = None,
        Some(value) => *index = Some(value - drop),
        None => {}
    }
}
