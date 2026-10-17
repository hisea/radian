//! The UI loop's vocabulary: one channel, four kinds of source (ADR 0023 U2).
//!
//! The channel carries three promises: **facts and control** are never dropped (an unbounded
//! `mpsc`, so a producer never blocks, ADR 0010 K3-1), **dirty notifications** are coalescible
//! (at most one per session in flight), and **terminal input** is ordered. **Process and host
//! state** (signals, the driver's end) is one event each. Nothing is polled: every source sends
//! here.
//!
//! This module is a library module rather than part of `cli.rs` so the sink's rule — one
//! `Dirty` per wake-up, never one per delta — has a unit test.

use std::sync::Arc;
use std::sync::mpsc::Sender;

use agent_core::{AgentEvent, EventSink, RunStatus};
use ratatui::crossterm::event::{
    Event, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};

use crate::app::{Input as AppInput, MouseScrollDirection};
use crate::projection::{LastToolCall, SessionProjection, StreamKind, TurnState};

/// One event on the UI's single channel (ADR 0023 U2).
pub enum UiEvent {
    /// A committed core event. Deltas never come this way: they accumulate in the projection
    /// and wake the UI with [`UiEvent::Dirty`].
    Agent(AgentEvent),
    /// A runtime event (ADR 0017's turn failure).
    Runtime(radian_session::RuntimeEvent),
    /// The pending queue changed (ADR 0023 U7): the UI re-reads its head.
    Pending,
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
    DriverStopped(radian_host::DriverEnd),
}

/// A terminal event, forwarded by the input thread.
#[derive(Debug)]
pub enum InputEvent {
    Key(KeyEvent),
    Paste(String),
    Resize {
        rows: u16,
        cols: u16,
    },
    /// A mouse-wheel notch. Only `ScrollUp`/`ScrollDown` map here; every other mouse event
    /// (click, drag, move, focus) is dropped by [`input_event`].
    MouseScroll(MouseScrollDirection),
    /// The left button went down at `(row, col)` (screen coordinates, `0`-based): the press
    /// that may start a transcript selection.
    MouseDown {
        row: u16,
        col: u16,
    },
    /// The left button moved while held: extends the selection.
    MouseDrag {
        row: u16,
        col: u16,
    },
    /// The left button came up: ends the selection and copies it.
    MouseUp {
        row: u16,
        col: u16,
    },
}

/// Maps a crossterm terminal event to the UI's input vocabulary. `None` means the event is
/// dropped: it never reaches the channel and never causes a frame. Terminal input is ordered,
/// so this is the one place the raw event names are translated; it is pure, so the mapping (in
/// particular "a click is not input") has a unit test.
///
/// Mouse mapping: the wheel scrolls, and the **left** button's `Down`/`Drag`/`Up` become the
/// transcript-selection gesture (the terminal must have `?1002h` on, which it has whenever
/// mouse reporting is on). Motion *without* a button (`Moved`) is not reported under
/// `?1002h` and maps to `None` either way, so an idle pointer never wakes the loop.
/// Everything else — other buttons, scroll-press/release, focus — is dropped. A **Shift**ed
/// left button is dropped too: the terminals that do report it use Shift as the "bypass
/// mouse reporting" key, and Radian keeps the terminal's native selection as the fallback.
///
/// The resize is special-cased by the caller, which coalesces a burst into the last one.
pub fn input_event(event: Event) -> Option<InputEvent> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Release => None,
        Event::Key(key) => Some(InputEvent::Key(key)),
        Event::Paste(text) => Some(InputEvent::Paste(text)),
        // crossterm's order is `(cols, rows)`; the UI's own `InputEvent` is `{ rows, cols }`.
        Event::Resize(cols, rows) => Some(InputEvent::Resize { rows, cols }),
        Event::Mouse(mouse) => {
            let shifted = mouse.modifiers.contains(KeyModifiers::SHIFT);
            let at = || (mouse.row, mouse.column);
            match mouse.kind {
                MouseEventKind::ScrollUp if !shifted => {
                    Some(InputEvent::MouseScroll(MouseScrollDirection::Up))
                }
                MouseEventKind::ScrollDown if !shifted => {
                    Some(InputEvent::MouseScroll(MouseScrollDirection::Down))
                }
                MouseEventKind::Down(MouseButton::Left) if !shifted => {
                    let (row, col) = at();
                    Some(InputEvent::MouseDown { row, col })
                }
                MouseEventKind::Drag(MouseButton::Left) if !shifted => {
                    let (row, col) = at();
                    Some(InputEvent::MouseDrag { row, col })
                }
                MouseEventKind::Up(MouseButton::Left) if !shifted => {
                    let (row, col) = at();
                    Some(InputEvent::MouseUp { row, col })
                }
                _ => None,
            }
        }
        _ => None,
    }
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
            // ADR 0018: the summarizer's call is billed too, so its usage joins the
            // same totals (and the same per-call pricing) as a conversation response. It is a
            // separate event because it is not this turn's generation: the timer above must
            // not treat the summary as the turn's response.
            AgentEvent::CompactionUsage { usage, .. } => self.projection.add_usage(usage),
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
            // ADR 0017: a retry discards the failed attempt's streamed text (it was
            // live-only, ADR 0010), so the buffer is cleared before the next attempt starts.
            AgentEvent::ModelRetrying { .. } => self.projection.clear_stream(),
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
    use ratatui::crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent};

    fn mouse(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// The wheel and the left button map; every other mouse event is dropped before the
    /// channel, so a pointer move can never cause a frame (ADR 0023 U2, plan B).
    #[test]
    fn only_a_wheel_notch_and_the_left_button_are_mouse_input() {
        assert!(matches!(
            input_event(mouse(MouseEventKind::ScrollUp)),
            Some(InputEvent::MouseScroll(MouseScrollDirection::Up))
        ));
        assert!(matches!(
            input_event(mouse(MouseEventKind::ScrollDown)),
            Some(InputEvent::MouseScroll(MouseScrollDirection::Down))
        ));
        // The selection gesture: left button down, drag, up — with the cell carried along.
        let down = input_event(mouse(MouseEventKind::Down(MouseButton::Left)));
        assert!(
            matches!(down, Some(InputEvent::MouseDown { row: 4, col: 3 })),
            "{down:?}"
        );
        let drag = input_event(mouse(MouseEventKind::Drag(MouseButton::Left)));
        assert!(
            matches!(drag, Some(InputEvent::MouseDrag { row: 4, col: 3 })),
            "{drag:?}"
        );
        let up = input_event(mouse(MouseEventKind::Up(MouseButton::Left)));
        assert!(
            matches!(up, Some(InputEvent::MouseUp { row: 4, col: 3 })),
            "{up:?}"
        );
        for dropped in [
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Middle),
            MouseEventKind::Drag(MouseButton::Right),
            MouseEventKind::Moved,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            assert!(input_event(mouse(dropped)).is_none(), "{dropped:?}");
        }
        // Shift is the terminals' "bypass mouse reporting" key: a shifted left button or
        // wheel is dropped, so the terminal's native selection stays the fallback (README).
        for shifted in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
        ] {
            let event = MouseEvent {
                kind: shifted,
                column: 3,
                row: 4,
                modifiers: KeyModifiers::SHIFT,
            };
            assert!(input_event(Event::Mouse(event)).is_none(), "{shifted:?}");
        }
        // A key release is dropped too; a key press is not.
        assert!(
            input_event(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )))
            .is_none()
        );
        assert!(matches!(
            input_event(Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE
            ))),
            Some(InputEvent::Key(_))
        ));
    }

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

    /// ADR 0023 U5: a fact event is written to the projection, and the UI is woken with a
    /// `Dirty` (not with the event alone, which the renderer does not read for facts).
    #[test]
    fn a_fact_event_writes_the_projection_and_wakes_the_ui() {
        let (sink, projection, rx) = sink();
        sink.emit(AgentEvent::ModelResponseReceived {
            stop_reason: agent_core::StopReason::EndTurn,
            tool_calls: 0,
            usage: Some(agent_core::ModelUsage {
                input: Some(5),
                output: Some(2),
                ..agent_core::ModelUsage::default()
            }),
            limits: None,
        });
        {
            let read = projection.read();
            assert_eq!(read.usage_input(), Some(5));
            assert_eq!(read.usage_output(), Some(2));
        }
        let events: Vec<UiEvent> = rx.try_iter().collect();
        assert!(
            events.iter().any(|event| matches!(event, UiEvent::Dirty)),
            "the fact update must wake the UI"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                UiEvent::Agent(AgentEvent::ModelResponseReceived { .. })
            )),
            "and the fact itself still reaches the channel"
        );
    }

    /// F2 (ADR 0018): the summarizer's call joins the session totals — both the
    /// token counters and the fee — even though it is not a conversation response. Its own
    /// event is what carries the fact, so the turn timer never mistakes a summary for this
    /// turn's generation.
    #[test]
    fn a_compaction_usage_joins_the_session_totals() {
        let (sink, projection, _rx) = sink();
        projection.set_price(Some(radian_host::ModelPrice {
            input: 3_000_000, // $3 / Mtok
            output: 15_000_000,
            cache_read: None,
            cache_write: None,
            tiers: Vec::new(),
        }));
        sink.emit(AgentEvent::CompactionUsage {
            revision: 1,
            model: "current".into(),
            usage: agent_core::ModelUsage {
                input: Some(50_000),
                output: Some(1_000),
                input_includes_cache: Some(false),
                ..agent_core::ModelUsage::default()
            },
        });
        {
            let read = projection.read();
            assert_eq!(
                read.usage_input(),
                Some(50_000),
                "the summary's input counts"
            );
            assert_eq!(read.usage_output(), Some(1_000));
            // 50000 x $3 + 1000 x $15 per Mtok = $0.15 + $0.015.
            assert_eq!(read.cost_micros(), Some(165_000));
        }
        // ... and a later conversation response adds to the same totals.
        sink.emit(AgentEvent::ModelResponseReceived {
            stop_reason: agent_core::StopReason::EndTurn,
            tool_calls: 0,
            usage: Some(agent_core::ModelUsage {
                input: Some(10_000),
                input_includes_cache: Some(false),
                ..agent_core::ModelUsage::default()
            }),
            limits: None,
        });
        {
            let read = projection.read();
            assert_eq!(read.usage_input(), Some(60_000));
            assert_eq!(read.cost_micros(), Some(195_000), "$0.165 + $0.03");
        }
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

    /// ADR 0017: a retry discards the failed attempt's live view (its deltas are
    /// live-only) and the event still reaches the UI (it is not a fact).
    #[test]
    fn a_retrying_event_clears_the_stream_and_reaches_the_ui() {
        let (sink, projection, rx) = sink();
        sink.emit(AgentEvent::ModelTextDelta {
            text: "half".into(),
        });
        assert_eq!(projection.snapshot().assistant, "half");

        sink.emit(AgentEvent::ModelRetrying {
            attempt: 1,
            max: 3,
            delay: std::time::Duration::from_secs(4),
            reason: "the stream was dropped".into(),
        });
        let events: Vec<UiEvent> = rx.try_iter().collect();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, UiEvent::Agent(AgentEvent::ModelRetrying { .. }))),
            "the retry reaches the UI"
        );
        assert_eq!(
            projection.snapshot(),
            StreamSnapshot::default(),
            "the failed attempt's live view is gone"
        );
    }
}
