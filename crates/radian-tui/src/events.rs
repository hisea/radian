//! The UI loop's vocabulary: one channel, four kinds of source (ADR 0023 U2).
//!
//! The channel carries three promises: **facts and control** are never dropped (an unbounded
//! `mpsc`, so a producer never blocks, ADR 0010 K3-1), **dirty notifications** are coalescible
//! (at most one per session in flight), and **terminal input** is ordered. **Process and host
//! state** (signals, the driver's end) is one event each. Nothing is polled: every source sends
//! here.
//!
//! This module is a library module rather than part of `main.rs` so the sink's rule — one
//! `Dirty` per wake-up, never one per delta — has a unit test.

use std::sync::Arc;
use std::sync::mpsc::Sender;

use agent_core::{AgentEvent, EventSink, RunStatus};
use radian_permissions::{ApprovalOutcome, ApprovalRequest};
use ratatui::crossterm::event::KeyEvent;

use crate::app::Input as AppInput;
use crate::projection::{LastToolCall, SessionProjection, StreamKind, TurnState};

/// One event on the UI's single channel (ADR 0023 U2).
pub enum UiEvent {
    /// A committed core event. Deltas never come this way: they accumulate in the projection
    /// and wake the UI with [`UiEvent::Dirty`].
    Agent(AgentEvent),
    /// A runtime event (ADR 0017's turn failure).
    Runtime(radian_session::RuntimeEvent),
    /// A tool call waiting for the human.
    Approval(ApprovalPrompt),
    /// The result of an effect, as the input it becomes (ADR 0023 U3).
    Command(AppInput),
    /// The projection changed. At most one is in flight per session.
    Dirty,
    /// A terminal event.
    Input(InputEvent),
    /// The terminal went away (the pty master closed, the ssh session dropped).
    TerminalGone,
    /// A signal asked the app to stop.
    Signal(i32),
    /// The driver task ended.
    DriverStopped,
}

/// A terminal event, forwarded by the input thread.
pub enum InputEvent {
    Key(KeyEvent),
    Paste(String),
    Resize { rows: u16, cols: u16 },
}

/// A tool call waiting for the human: the request the UI shows, and the channel the permission
/// layer awaits. The **loop** owns the channel (ADR 0023 U3: `update` is pure), keyed by
/// `request_id` so an answer or an abandon can find it.
pub struct ApprovalPrompt {
    pub request: ApprovalRequest,
    pub reply: tokio::sync::oneshot::Sender<ApprovalOutcome>,
}

/// Forwards core events to the UI thread (ADR 0023 U2). Deltas are **not** forwarded: they
/// accumulate in the projection, and the UI is woken with one `Dirty` when it has not looked
/// yet. `emit` stays short and non-blocking (ADR 0010 K3-1).
pub struct ProjectionSink {
    tx: Sender<UiEvent>,
    projection: Arc<SessionProjection>,
}

impl ProjectionSink {
    pub fn new(tx: Sender<UiEvent>, projection: Arc<SessionProjection>) -> Self {
        Self { tx, projection }
    }

    /// Appends a delta and wakes the UI when it has not been woken since it last looked.
    fn stream(&self, kind: StreamKind, text: &str) {
        if self.projection.push_delta(kind, text) {
            let _ = self.tx.send(UiEvent::Dirty);
        }
    }

    /// Writes the session facts one event carries into the projection (ADR 0023 U5). Returns
    /// whether the UI must be woken. `App` never writes these; it handles the transcript, the
    /// note and the effects only.
    fn fact(&self, event: &AgentEvent) -> bool {
        match event {
            AgentEvent::TurnStarted => self.projection.set_turn(TurnState::Running),
            // ADR 0018: the request carries its projection and budget `B`, for the status bar.
            AgentEvent::ModelRequestStarted { budget, .. } => {
                self.projection.set_request_started(budget.clone())
            }
            // Usage is the provider's own report; each present field is added to the session
            // total, and a field that was never reported stays `None` (never a zero). A
            // response without usage carries no fact.
            AgentEvent::ModelResponseReceived {
                usage: Some(usage), ..
            } => self.projection.add_usage(usage),
            AgentEvent::ToolStarted {
                index,
                call_id,
                name,
            } => self.projection.set_last_tool(LastToolCall {
                index: *index,
                call_id: call_id.clone(),
                name: name.clone(),
            }),
            AgentEvent::RunFinished { status } => self.projection.set_turn(match status {
                RunStatus::Cancelled(_) => TurnState::Cancelled,
                _ => TurnState::Idle,
            }),
            _ => false,
        }
    }
}

impl EventSink for ProjectionSink {
    fn emit(&self, event: AgentEvent) {
        match &event {
            AgentEvent::ModelTextDelta { text } => {
                return self.stream(StreamKind::Assistant, text);
            }
            AgentEvent::ModelReasoningDelta { text } => {
                return self.stream(StreamKind::Reasoning, text);
            }
            // The committed message replaces the live view, so the stream buffer is done.
            AgentEvent::TranscriptAppended { .. } => self.projection.clear_stream(),
            _ => {}
        }
        // The fact is written to the projection, and the event still goes on the channel:
        // facts are never dropped (ADR 0023 U2), and `App` handles the transcript, the note
        // and the effects of the same event.
        if self.fact(&event) {
            let _ = self.tx.send(UiEvent::Dirty);
        }
        let _ = self.tx.send(UiEvent::Agent(event));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::StreamSnapshot;

    fn sink() -> (
        ProjectionSink,
        Arc<SessionProjection>,
        std::sync::mpsc::Receiver<UiEvent>,
    ) {
        let projection = Arc::new(SessionProjection::new());
        let (tx, rx) = std::sync::mpsc::channel();
        (
            ProjectionSink::new(tx, Arc::clone(&projection)),
            projection,
            rx,
        )
    }

    /// The sink's rule: one `Dirty` per wake-up, never one per delta.
    #[test]
    fn the_sink_sends_one_dirty_until_the_ui_looks() {
        let (sink, projection, rx) = sink();
        sink.emit(AgentEvent::ModelTextDelta { text: "a".into() });
        sink.emit(AgentEvent::ModelTextDelta { text: "b".into() });
        assert_eq!(
            rx.try_iter().count(),
            1,
            "two deltas before the UI looks are one notification"
        );
        assert_eq!(projection.snapshot().assistant, "ab");

        // The UI looks (clears the flag), so the next delta must wake it again.
        assert!(projection.take_dirty());
        sink.emit(AgentEvent::ModelTextDelta { text: "c".into() });
        assert_eq!(
            rx.try_iter().count(),
            1,
            "the update after the read wakes it"
        );
        assert_eq!(projection.snapshot().assistant, "abc");
    }

    /// Deltas are not facts; a commit is, and it ends the stream.
    #[test]
    fn deltas_are_not_facts_and_a_commit_clears_the_stream() {
        let (sink, projection, rx) = sink();
        sink.emit(AgentEvent::ModelTextDelta { text: "a".into() });
        sink.emit(AgentEvent::ModelReasoningDelta { text: "r".into() });
        assert_eq!(
            rx.try_iter().count(),
            1,
            "the deltas only wake the UI, they are not events"
        );
        assert_eq!(projection.snapshot().reasoning, "r");

        sink.emit(AgentEvent::TranscriptAppended {
            index: 0,
            message: agent_core::Message::assistant("a"),
        });
        let events: Vec<UiEvent> = rx.try_iter().collect();
        assert_eq!(events.len(), 1, "the commit is a fact");
        assert!(matches!(
            events[0],
            UiEvent::Agent(AgentEvent::TranscriptAppended { .. })
        ));
        assert_eq!(
            projection.snapshot(),
            StreamSnapshot::default(),
            "the live view is over"
        );
    }
}
