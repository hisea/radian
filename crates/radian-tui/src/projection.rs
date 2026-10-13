//! The minimal session projection (ADR 0023 U2; PR ④ grows it).
//!
//! The event sink writes streaming deltas **here** instead of sending them into the UI channel:
//! a delta is a display update, not a fact (ADR 0010 K2), so it may be merged or dropped. The
//! projection is the thing that must not lose a *wake-up*, and the rule is:
//!
//! - the producer appends to the buffer, bumps the revision, then sets `dirty` with `Release`
//!   and reports "send a notification" **only when the flag was `false`** — so at most one
//!   `Dirty` per session is in flight;
//! - the UI clears the flag with `Acquire` **before** reading the buffer, so any update that
//!   happens after that read sets the flag again and produces another notification.
//!
//! `Relaxed` is never used for the flag. The buffer itself is behind a `Mutex`, so its contents
//! are ordered by the lock; the flag's ordering is what makes the *decision* to read it
//! correct, and the `Acquire` read publishes the buffer writes to the UI thread.
//!
//! `revision` is a plain counter: it is only ever read after [`SessionProjection::take_dirty`]
//! returned `true`, which is the acquire that orders it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Which stream a chunk belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Assistant,
    Reasoning,
}

/// The accumulated streaming text, in the order the streams first appeared. The order matters:
/// the renderer shows the reasoning block above the answer when the provider streamed it first,
/// exactly as the deltas arrived.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamSnapshot {
    /// The streams that have appeared, first appearance first (at most two entries).
    pub order: Vec<StreamKind>,
    pub assistant: String,
    pub reasoning: String,
}

#[derive(Debug, Default)]
struct Stream {
    order: Vec<StreamKind>,
    assistant: String,
    reasoning: String,
}

/// One session's projection. This PR holds the streaming buffer and the dirty flag; PR ④ moves
/// the rest of the session facts here.
#[derive(Debug, Default)]
pub struct SessionProjection {
    stream: Mutex<Stream>,
    revision: AtomicU64,
    dirty: AtomicBool,
}

impl SessionProjection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one delta. Returns `true` when the caller must send a `Dirty` notification: the
    /// flag was `false`, so this is the first update since the UI last looked.
    pub fn push_delta(&self, kind: StreamKind, text: &str) -> bool {
        {
            let mut stream = self.stream.lock().expect("the stream lock");
            if !stream.order.contains(&kind) {
                stream.order.push(kind);
            }
            match kind {
                StreamKind::Assistant => stream.assistant.push_str(text),
                StreamKind::Reasoning => stream.reasoning.push_str(text),
            }
        }
        self.revision.fetch_add(1, Ordering::Relaxed);
        // `Release`: the buffer write and the revision bump above are visible to a reader that
        // acquires this flag.
        !self.dirty.swap(true, Ordering::Release)
    }

    /// Drops the streaming buffer: a committed message replaced the live view. This does not
    /// wake the UI — the commit itself is a fact on the channel, and an empty snapshot is a
    /// no-op.
    pub fn clear_stream(&self) {
        let mut stream = self.stream.lock().expect("the stream lock");
        stream.order.clear();
        stream.assistant.clear();
        stream.reasoning.clear();
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    /// Clears the dirty flag and reports whether it was set. The UI calls this **before**
    /// reading the projection; a `false` means another `Dirty` is already in flight (or the
    /// update was already consumed), so the read can be skipped.
    pub fn take_dirty(&self) -> bool {
        // `Acquire`: the buffer reads that follow see everything the producer wrote before it
        // set the flag.
        self.dirty.swap(false, Ordering::Acquire)
    }

    /// The accumulated stream. Read after [`Self::take_dirty`].
    pub fn snapshot(&self) -> StreamSnapshot {
        let stream = self.stream.lock().expect("the stream lock");
        StreamSnapshot {
            order: stream.order.clone(),
            assistant: stream.assistant.clone(),
            reasoning: stream.reasoning.clone(),
        }
    }

    /// The number of changes since construction. Only meaningful after
    /// [`Self::take_dirty`] returned `true`.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wake-up contract, part one: a delta wakes the UI once, and a second delta before the
    /// UI looks does **not** enqueue a second notification.
    #[test]
    fn a_delta_wakes_the_ui_once_until_it_reads() {
        let projection = SessionProjection::new();
        assert!(
            projection.push_delta(StreamKind::Assistant, "a"),
            "the first delta wakes the UI"
        );
        assert!(
            !projection.push_delta(StreamKind::Assistant, "b"),
            "a second delta before the UI looks must not enqueue another notification"
        );
        assert_eq!(projection.snapshot().assistant, "ab");
        assert_eq!(projection.revision(), 2);

        assert!(projection.take_dirty(), "the flag was set");
        assert!(!projection.take_dirty(), "clearing it once is enough");
    }

    /// The wake-up contract, part two: an update **after** the UI read always produces another
    /// notification, so no wake-up is lost.
    #[test]
    fn an_update_after_the_read_wakes_the_ui_again() {
        let projection = SessionProjection::new();
        projection.push_delta(StreamKind::Assistant, "a");
        assert!(projection.take_dirty());
        let seen = projection.snapshot();
        assert_eq!(seen.assistant, "a");

        assert!(
            projection.push_delta(StreamKind::Assistant, "b"),
            "an update after the read must wake the UI again"
        );
        assert_eq!(projection.snapshot().assistant, "ab");
        assert!(projection.take_dirty());
    }

    #[test]
    fn the_stream_order_is_first_appearance() {
        let projection = SessionProjection::new();
        projection.push_delta(StreamKind::Reasoning, "think");
        projection.push_delta(StreamKind::Assistant, "answer");
        projection.push_delta(StreamKind::Reasoning, " more");
        let snapshot = projection.snapshot();
        assert_eq!(
            snapshot.order,
            vec![StreamKind::Reasoning, StreamKind::Assistant]
        );
        assert_eq!(snapshot.reasoning, "think more");
        assert_eq!(snapshot.assistant, "answer");
    }

    #[test]
    fn clear_stream_drops_the_live_view() {
        let projection = SessionProjection::new();
        projection.push_delta(StreamKind::Assistant, "a");
        projection.take_dirty();
        projection.clear_stream();
        let snapshot = projection.snapshot();
        assert_eq!(snapshot, StreamSnapshot::default());
        // Clearing does not wake the UI: the committed message is the event that matters.
        assert!(!projection.take_dirty());
    }
}
