//! The writer's replacement-segment gate (ADR 0018 §A, ADR 0019 §2).
//!
//! The core writes one replacement as an unbroken run —
//! `TranscriptReplaced`, its `len` messages, `TranscriptReplacementCommitted` — and the file
//! is unreadable while that run is in flight: ADR 0019 §2 makes any other event inside the
//! segment corruption. A host thread can append a runtime event (`InboxEnqueued`, a user
//! typing during compaction) at any moment, so the writer holds those appends back until the
//! segment closes, and fails them if a core append inside the segment failed.
//!
//! The gate is deliberately tiny and lock-free across `await`s: a [`std::sync::Mutex`] guards
//! the state, and a [`tokio::sync::Notify`] wakes waiters. A core append runs in
//! `spawn_blocking`, so the notification must work from a blocking thread — `Notify` does.

use std::sync::Mutex;

use tokio::sync::Notify;

#[derive(Default)]
struct State {
    /// The revision of the open replacement segment, or `None`.
    open: Option<u64>,
    /// A core append inside the segment failed; the session is unrecoverable in process, so
    /// no further runtime event may be written.
    poisoned: bool,
    /// How many host appends are waiting.
    waiting: usize,
}

/// Shared replacement-segment state, one per `SessionJournal`.
pub(crate) struct SegmentGate {
    state: Mutex<State>,
    closed: Notify,
    #[cfg(feature = "testing")]
    waiter: Notify,
}

impl SegmentGate {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            closed: Notify::new(),
            #[cfg(feature = "testing")]
            waiter: Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A `TranscriptReplaced` was written: the segment is open.
    pub(crate) fn begin(&self, revision: u64) {
        self.lock().open = Some(revision);
    }

    /// A matching commit or abort was written: the segment closed.
    pub(crate) fn close(&self, revision: u64) {
        let mut state = self.lock();
        if state.open == Some(revision) {
            state.open = None;
            drop(state);
            self.closed.notify_waiters();
        }
    }

    /// A core append inside the open segment failed. The session needs recovery, and a host
    /// append waiting on the segment must fail rather than write into the broken run.
    pub(crate) fn poison(&self) {
        self.lock().poisoned = true;
        self.closed.notify_waiters();
    }

    pub(crate) fn is_open(&self) -> bool {
        self.lock().open.is_some()
    }

    /// Waits until no segment is open. `Err(())` means the gate was poisoned by a failed core
    /// append; the caller maps it to an error and writes nothing.
    ///
    /// Race-free: the `Notified` future is enabled **before** the state is read, so a
    /// `notify_waiters` between the read and the `await` still wakes this waiter.
    pub(crate) async fn wait_closed(&self) -> Result<(), ()> {
        loop {
            let notified = self.closed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.lock();
                if state.poisoned {
                    return Err(());
                }
                if state.open.is_none() {
                    return Ok(());
                }
                state.waiting += 1;
            }
            #[cfg(feature = "testing")]
            self.waiter.notify_waiters();
            notified.await;
            {
                let mut state = self.lock();
                state.waiting = state.waiting.saturating_sub(1);
            }
        }
    }

    /// Test seam: completes once a host append has registered itself as waiting.
    #[cfg(feature = "testing")]
    pub(crate) async fn wait_for_waiter(&self) {
        loop {
            let notified = self.waiter.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.lock().waiting > 0 {
                return;
            }
            notified.await;
        }
    }
}
