//! The folded transcript entry: what a session's committed facts become on the way to a
//! renderer or a remote client (ADR 0023 U1).
//!
//! **One type, two folds.** A journal's committed messages and an ACP attempt's
//! `result.jsonl` both fold into a `Vec<Entry>` of this type ([`AttachFold`], [`AcpFold`]). The
//! type is pure data: no terminal, no theme, no ratatui, and no session handle. `radian-tui`
//! draws it (its own `Entry` is this entry plus render state), and the hub sends it.
//!
//! **The JSON shape is stable** (the hub's `/transcript` and `/conversation` routes send it).
//! Every entry is one snake_case object with a `kind` tag:
//!
//! ```json
//! {"kind": "assistant", "text": "Done.",
//!  "is_error": false, "result": null, "error_code": null, "card": "none",
//!  "tool": null, "compaction": null, "interrupted": false, "images": []}
//! ```
//!
//! The fields are always present, in that order. A tool entry fills `tool` and `card`; a
//! compaction summary fills `compaction`; a user or tool-result entry may carry `images`. One
//! field is deliberately **not** serialized: an image's base64 payload (`ImagePart::data`). The
//! placeholder line that stands in for it is in `text`, so a client that lists entries never
//! receives the bytes the user attached.

use std::sync::Arc;

use serde::Serialize;

/// Where a fold's entry list changed since a consumer last read it, so the consumer (the TUI's
/// render entries, or the hub's copy) can mirror only what moved. See [`EntryList`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    /// Every entry must be rebuilt: the fold reset from scratch (a replacement segment, a
    /// replaced or truncated file).
    pub reset: bool,
    /// The first index whose entry is new or different. Every entry before it is unchanged.
    pub from: usize,
}

/// A fold's entry list, with the change mark a mirroring consumer reads.
///
/// The mark is a **lowest changed index**, not an edit list: a consumer truncates its own copy
/// at that index and re-reads the rest. Every mutation records the index it first touched, so
/// "truncate then extend" reproduces the list exactly.
#[derive(Debug, Clone, Default)]
pub struct EntryList {
    entries: Vec<Entry>,
    change: Option<Change>,
}

impl EntryList {
    pub fn new() -> Self {
        Self::default()
    }

    /// The entries, oldest first.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// One entry by index, for a fold's own bookkeeping.
    pub fn get(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }

    /// The mutable entry store. The folds use it for the in-place rewrites (a tool card's head
    /// as its result arrives); the mark must then be set with [`EntryList::mark`].
    pub fn get_mut(&mut self, index: usize) -> Option<&mut Entry> {
        self.entries.get_mut(index)
    }

    /// Every entry, mutably, for a fold's own scan (the tool-result pairing). The caller marks
    /// what it changed.
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, Entry> {
        self.entries.iter_mut()
    }

    /// Sets the lowest changed index, or the reset flag.
    pub fn mark(&mut self, from: usize) {
        self.change = Some(match self.change {
            None => Change { reset: false, from },
            Some(change) => Change {
                reset: change.reset,
                from: change.from.min(from),
            },
        });
    }

    /// The whole list must be rebuilt.
    pub fn mark_reset(&mut self) {
        self.change = Some(Change {
            reset: true,
            from: 0,
        });
    }

    /// Appends one entry and returns its index.
    pub fn push(&mut self, entry: Entry) -> usize {
        let index = self.entries.len();
        self.entries.push(entry);
        self.mark(index);
        index
    }

    /// Inserts one entry at `at` (clamped to the end) and returns its index.
    pub fn insert(&mut self, at: usize, entry: Entry) -> usize {
        let at = at.min(self.entries.len());
        self.entries.insert(at, entry);
        self.mark(at);
        at
    }

    /// Replaces one entry in place.
    pub fn replace(&mut self, index: usize, entry: Entry) {
        if let Some(slot) = self.entries.get_mut(index) {
            *slot = entry;
            self.mark(index);
        }
    }

    /// Removes the last entry.
    pub fn pop(&mut self) {
        self.entries.pop();
        self.mark(self.entries.len());
    }

    /// Drops the oldest `count` entries (a bounded window moving on).
    pub fn drain_front(&mut self, count: usize) {
        let count = count.min(self.entries.len());
        if count == 0 {
            return;
        }
        self.entries.drain(0..count);
        self.mark(0);
    }

    /// Empties the list. A consumer must rebuild from scratch.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.mark(0);
    }

    /// Takes the change mark, clearing it. `None` means nothing changed since the last call.
    pub fn take_change(&mut self) -> Option<Change> {
        self.change.take()
    }
}

/// What kind of transcript line an entry is. The renderer maps this to a style; the JSON shape
/// tags an entry with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind {
    User,
    Assistant,
    /// The model's separate reasoning channel (ADR 0014); never sent to a provider.
    Reasoning,
    ToolCall,
    ToolResult,
    /// A host notice: an open tool call's state, the `working…` line, an ACP status or plan
    /// line, permission answers.
    Notice,
    /// The startup screen's wordmark (ADR 0027). It is the first entry in a session.
    Banner,
    /// The startup information's collapsed/expanded lines.
    Startup,
    /// The project trust line under the wordmark (ADR 0031 T3).
    TrustHint,
    /// A startup safety line (an unavailable sandbox, an unsandboxed MCP, an unknown window, a
    /// memory/config warning): it is **never** folded away.
    Warning,
    /// The one-line `Tip: …` under the startup block.
    Tip,
    /// A compaction summary (ADR 0028 R5): one line when folded, the summary and the user's
    /// verbatim words when expanded.
    Compaction,
    /// The `/context` grid view: the estimated categories as a grid with a legend.
    Context,
    /// The attempt's outcome line the conversation page draws for an ACP worker's `Result`
    /// record: `Result <outcome> <text>`, in the done or the failed colour (ADR 0048 §13).
    Outcome {
        ok: bool,
    },
    /// A user's `!`/`!!` command and its output (ADR 0033). The renderer colors `!` (goes to
    /// the model's context) and `!!` (does not) differently; the first line is `$ cmd`.
    Bash {
        exclude_from_context: bool,
    },
}

/// Where a tool entry sits in a call/result pair (ADR 0028's merge). The data stays two
/// entries; this records the pairing, which the `ask_user` record reads to decide whether to
/// draw its block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Card {
    /// Not a tool entry.
    #[default]
    None,
    /// The call that heads a block its result closes.
    Head,
    /// The result that closes the block its call opened.
    Tail,
    /// A tool entry with no pair: a call still running, or a torn transcript.
    Solo,
}

/// One image a transcript entry carries, as the fold sees it: the wire payload and the display
/// facts. The payload is not serialized (see the module docs); the placeholder line is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImagePart {
    /// Standard base64, the part's own payload. Shared (`Arc`) so pushing a message does not
    /// copy the text twice. Never sent to a client.
    #[serde(skip_serializing)]
    pub data: Arc<str>,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    /// `[image: <source>, W×H, N KB]`, the line drawn directly above the image.
    pub placeholder: String,
}

/// A compaction summary's display data (ADR 0028 R5). The `summary` and the verbatim user
/// words come from the message's `origin` (ADR 0018) through Core's own readers; the revision
/// and the token counts are **live only** — the journaled message does not carry them, so a
/// fold of a finished journal shows `── context compacted ──` without inventing numbers.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CompactionInfo {
    pub summary: String,
    pub user_messages: Vec<String>,
    pub revision: Option<u64>,
    pub tokens_before: Option<u64>,
    pub tokens_after: Option<u64>,
    /// ADR 0018: live only — `true` when the summary path failed and the deterministic
    /// fallback committed instead.
    pub deterministic: bool,
    /// The messages the deterministic fallback dropped; live only, like the numbers above.
    pub dropped: Option<u64>,
}

impl CompactionInfo {
    /// The text the entry shows when expanded: the model's summary, then the user's words
    /// (ADR 0018), each as its own paragraph. This **is** the entry's text, so it is what the
    /// selection copies and the search matches.
    pub fn display_text(&self) -> String {
        let mut out = self.summary.trim().to_owned();
        for message in &self.user_messages {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(message.trim());
        }
        out
    }

    /// The folded one-liner, omitting whatever is not known: `── context compacted (rev 3,
    /// 12000 → 4000) ──`, `── context compacted (rev 3) ──` or `── context compacted ──`. A
    /// deterministic compaction (ADR 0018) says "without a summary".
    pub fn line(&self) -> String {
        let label = if self.deterministic {
            "context compacted without a summary"
        } else {
            "context compacted"
        };
        let mut parts: Vec<String> = Vec::new();
        if let Some(revision) = self.revision {
            parts.push(format!("rev {revision}"));
        }
        if let (Some(before), Some(after)) = (self.tokens_before, self.tokens_after) {
            parts.push(format!("{before} → {after}"));
        }
        if let Some(dropped) = self.dropped {
            parts.push(format!("{dropped} dropped"));
        }
        if parts.is_empty() {
            format!("── {label} ──")
        } else {
            format!("── {label} ({}) ──", parts.join(", "))
        }
    }
}

/// One folded transcript entry.
///
/// The fields the folds never produce (`Banner`, `Startup`, `Tip`, a live reasoning clock, the
/// renderer's caches) live on `radian-tui`'s own entry, which wraps this one. Everything a
/// reader of a transcript needs is here.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct Entry {
    /// The entry kind, flattened into the object as `"kind": "<snake_case name>"`.
    #[serde(flatten)]
    pub kind: EntryKind,
    pub text: String,
    /// The entry is an error (a failed tool call, an interrupted reply).
    pub is_error: bool,
    /// A tool call's outcome, once its result has arrived: `None` while it is running,
    /// `Some(is_error)` after. The card's background follows it (ADR 0028 R4's states).
    pub result: Option<bool>,
    /// A tool result's error code (`interrupted` for a cancelled call, ADR 0001).
    pub error_code: Option<String>,
    /// Where this tool entry sits in a merged card (ADR 0028).
    pub card: Card,
    /// The tool card's data (ADR 0028 R4). `None` for every non-tool entry.
    pub tool: Option<Box<crate::transcript::ToolCard>>,
    /// A compaction summary's data (ADR 0028 R5). `None` for every other entry.
    pub compaction: Option<Box<CompactionInfo>>,
    /// The committed assistant message was an interrupted prefix (ADR 0003).
    pub interrupted: bool,
    /// The images the entry's message carried (ADR 0028 R10), in order. A user message's
    /// placeholder line is already in `text`.
    pub images: Vec<ImagePart>,
}

impl Entry {
    pub fn new(kind: EntryKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            is_error: false,
            result: None,
            error_code: None,
            card: Card::None,
            tool: None,
            compaction: None,
            interrupted: false,
            images: Vec::new(),
        }
    }
}
