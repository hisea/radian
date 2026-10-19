//! The read-only observer of a session's **committed** facts (ADR 0023 U6, ADR 0043).
//!
//! [`JournalObserver`] wraps a [`JournalTail`] and folds the events it reads into a
//! [`SessionProjection`], so a consumer that did not open the session (the control port's
//! `subscribe`, `radian attach`, the TUI's read-only follow) sees the same facts the owning
//! process would show. It is the single source of committed facts.
//!
//! **What it answers.** "What is it doing" (the projection: turn state, usage, the last tool)
//! and "when was its last write" (the last event time and the last successful `write`/`patch`
//! time). Questions and approvals are **not** here: they are inbox inputs, not committed
//! facts.
//!
//! **Committed means committed.** The tail drops an uncommitted replacement segment (ADR
//! 0019), so the effective history, the projection and both timestamps never show one. A
//! torn tail is not read until its line is complete, and a replaced/truncated file resets the
//! reader (ADR 0023 U6).
//!
//! **Trait choice.** This is a narrower trait, [`SessionObserver`], not
//! [`SessionFacade`](crate::session::SessionFacade). A facade is a *door*: it requires
//! `pending`, `answer`, `compact`, `set_approval`, `skills`, `trust`, `jobs` and the rest.
//! An observer has no inbox, no queue and no command path, so implementing `SessionFacade`
//! would force fake answers. The narrower trait says exactly what the observer can do, and
//! `capabilities()` returns `[Observe]` to match.

use std::collections::HashMap;
use std::path::Path;

use agent_core::Message;
use radian_session::{EventData, JournalTail, ReadError, SessionEvent, TailUpdate, TurnOutcome};

use crate::projection::{LastToolCall, ProjectionRead, SessionProjection, TurnState};
use crate::session::SessionCapability;

/// The only capability an observer declares. A `&'static` slice so `capabilities()` can return
/// a borrow without owning an array per observer.
const OBSERVE_ONLY: [SessionCapability; 1] = [SessionCapability::Observe];

/// A tool whose **successful** settle is a file write (for the last-write timestamp).
fn is_write_tool(name: &str) -> bool {
    matches!(name, "write" | "patch")
}

/// The read-only half of the facade: committed facts only, no commands (ADR 0023 U6).
pub trait SessionObserver: Send + Sync {
    /// What the observer can do. Always `[Observe]`.
    fn capabilities(&self) -> &[SessionCapability];

    /// The committed session facts: turn state, usage, the last tool.
    fn projection(&self) -> ProjectionRead<'_>;

    /// The timestamp of the last committed event. `None` before the first event.
    fn last_event_ts(&self) -> Option<&str>;

    /// The timestamp of the last **successful** `write`/`patch`: the last time the agent wrote
    /// a file. `None` when it has not written one yet.
    fn last_write_ts(&self) -> Option<&str>;
}

/// An observer over one session file. See the module docs for the boundary.
pub struct JournalObserver {
    tail: JournalTail,
    projection: SessionProjection,
    /// The committed transcript, as of the last poll. Not on [`SessionObserver`]: the
    /// observer trait exposes the projection and the two timestamps. The TUI's read-only
    /// follow and the tests read it here.
    history: Vec<Message>,
    last_event_ts: Option<String>,
    last_write_ts: Option<String>,
    /// `call_id -> tool name` for calls that have started and not settled, so a settle can be
    /// attributed to a `write`/`patch` (the settle carries only the id).
    live_calls: HashMap<String, String>,
    /// Position of the next `ToolExecutionStarted` in its batch. A transcript append ends the
    /// previous batch (ADR 0006 §5.5).
    tool_index: usize,
}

impl JournalObserver {
    /// Opens `path` read-only and folds its committed prefix. No writer lock is taken and
    /// nothing is written.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ReadError> {
        let (tail, update) = JournalTail::open_with_update(path)?;
        let mut observer = Self {
            tail,
            projection: SessionProjection::new(),
            history: Vec::new(),
            last_event_ts: None,
            last_write_ts: None,
            live_calls: HashMap::new(),
            tool_index: 0,
        };
        observer.fold(&update);
        Ok(observer)
    }

    /// Reads the events appended since the last poll and folds them. The returned
    /// [`TailUpdate`] is the tail's own result (reset flag, events, effective history).
    pub fn poll(&mut self) -> Result<TailUpdate, ReadError> {
        let update = self.tail.poll()?;
        self.fold(&update);
        Ok(update)
    }

    /// The committed transcript as of the last poll.
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// How many of [`JournalObserver::history`]'s messages are the committed transcript. The
    /// repaired suffix after it (ADR 0002) is one result per call with no committed result
    /// message yet — an open call of a running turn, or one a crash left unanswered.
    pub fn committed_len(&self) -> usize {
        self.tail.committed_len()
    }

    /// Whether a live session process holds the file's writer lock (ADR 0043): a read-only,
    /// non-blocking probe, used to tell a running session's open tool calls from a crash's
    /// synthesised results. Never blocks the writer and never writes.
    pub fn live(&self) -> bool {
        radian_session::session_is_live(self.tail.path())
    }

    /// The path being observed.
    pub fn path(&self) -> &Path {
        self.tail.path()
    }

    /// The end of the committed prefix read so far.
    pub fn offset(&self) -> u64 {
        self.tail.offset()
    }

    /// Folds one poll's result into the projection and the two timestamps.
    ///
    /// Only the **new** events are folded: a poll that brings nothing changes nothing. A reset
    /// (a new inode or a shorter file) discards the old file's facts first; the tail then
    /// re-reads from byte 0, so the events folded in are the whole new file and the facts are
    /// rebuilt from scratch — exactly what a fresh `open` would produce.
    fn fold(&mut self, update: &TailUpdate) {
        if update.reset {
            self.projection.reset();
            self.live_calls.clear();
            self.tool_index = 0;
            self.last_event_ts = None;
            self.last_write_ts = None;
        }
        for event in &update.events {
            self.apply(event);
        }
        self.history = update.history.clone();
    }

    /// One committed event's contribution to the projection and the timestamps.
    fn apply(&mut self, event: &SessionEvent) {
        self.last_event_ts = Some(event.ts.clone());
        match &event.data {
            // A transcript append ends the tool batch before it: the next start is position 0.
            EventData::MessageAppended { .. } => self.tool_index = 0,
            EventData::TurnStarted { .. } => {
                self.projection.set_turn(TurnState::Running);
            }
            EventData::TurnFinished { outcome, .. } => {
                self.projection.set_turn(match outcome {
                    TurnOutcome::Cancelled => TurnState::Cancelled,
                    _ => TurnState::Idle,
                });
            }
            EventData::TurnFailed { .. } => {
                self.projection.set_turn(TurnState::Idle);
            }
            // A recorded fee is authoritative (ADR 0007). An old record has none, and
            // the observer has no route to price it (ADR 0027), so it adds tokens only.
            EventData::ModelUsageRecorded { usage, fee }
            | EventData::CompactionUsageRecorded { usage, fee, .. } => match fee {
                Some(fee) => {
                    self.projection.add_recorded_usage(usage, *fee);
                }
                None => {
                    self.projection.add_usage(usage);
                }
            },
            EventData::ToolExecutionStarted { call_id, name, .. } => {
                self.projection.set_last_tool(LastToolCall {
                    index: self.tool_index,
                    call_id: call_id.clone(),
                    name: name.clone(),
                });
                self.tool_index += 1;
                self.live_calls.insert(call_id.clone(), name.clone());
            }
            EventData::ToolExecutionSettled { call_id, result } => {
                let name = self.live_calls.remove(call_id);
                if !result.is_error
                    && let Some(name) = name
                    && is_write_tool(&name)
                {
                    self.last_write_ts = Some(event.ts.clone());
                }
            }
            _ => {}
        }
    }
}

impl SessionObserver for JournalObserver {
    fn capabilities(&self) -> &[SessionCapability] {
        &OBSERVE_ONLY
    }

    fn projection(&self) -> ProjectionRead<'_> {
        self.projection.read()
    }

    fn last_event_ts(&self) -> Option<&str> {
        self.last_event_ts.as_deref()
    }

    fn last_write_ts(&self) -> Option<&str> {
        self.last_write_ts.as_deref()
    }
}
