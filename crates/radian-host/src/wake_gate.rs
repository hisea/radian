//! The sliding-window wake budget (ADR 0037 decision 5: the rate gate).
//!
//! The background-job notifier and the Stop-hook continuations share this one type: at most
//! [`MAX_WAKES_PER_WINDOW`] wakes in any [`WAKE_WINDOW`]. A wake older than the window no
//! longer counts, so the budget recovers on its own with no user message. A user message
//! still clears it at once ([`WakeGate::reset_wakes`]).
//!
//! The clock is `tokio::time::Instant`, so tests drive it with the paused clock
//! (`tokio::time::pause`/`advance`) instead of wall-clock sleeps.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

/// How many wakes fit in one window (ADR 0037 decision 5). From the dogfood journals
/// (`~/.radian/sessions/*.jsonl`, 2026-10-15…20, read only: 166 sessions, 439 settles, 11
/// wake-started turns): wake-started turns arrive minutes apart (gaps min 19 s, median
/// ~6 min; the busiest session holds 6 wakes over ~70 min, at most 3 in any 10 minutes),
/// so a normal worker chain never reaches 10 in 10 minutes. A self-exciting loop (a job
/// that ends at once whose turn only starts the next job, gaps ~1 s) fills the budget in
/// seconds and is degraded within a minute.
pub const MAX_WAKES_PER_WINDOW: u32 = 10;

/// How far back a wake counts against the budget (ADR 0037 decision 5).
pub const WAKE_WINDOW: Duration = Duration::from_secs(10 * 60);

/// At most [`MAX_WAKES_PER_WINDOW`] recorded wakes newer than [`WAKE_WINDOW`].
///
/// The two call sites ([`crate::job_notify::JobNotifier::deliver`] and
/// [`crate::CommandHooks::run_stop_once`]) are each driven by one task at a time, so the
/// check-then-record pair (`wake_allowed` in the gate condition, `note_wake` once the
/// `followup` lands) needs no cross-await atomicity beyond the lock held inside each call.
#[derive(Debug, Default)]
pub struct WakeGate {
    wakes: Mutex<VecDeque<tokio::time::Instant>>,
}

impl WakeGate {
    /// An empty budget.
    pub fn new() -> Self {
        Self {
            wakes: Mutex::new(VecDeque::new()),
        }
    }

    /// Drops every wake at least [`WAKE_WINDOW`] old. Compares forward (`first + window <=
    /// now`) rather than subtracting the window from now, so a fresh clock never underflows.
    fn prune_locked(wakes: &mut VecDeque<tokio::time::Instant>) {
        let now = tokio::time::Instant::now();
        while wakes
            .front()
            .is_some_and(|first| *first + WAKE_WINDOW <= now)
        {
            wakes.pop_front();
        }
    }

    /// Whether another wake fits in the window right now. Record it with [`WakeGate::note_wake`]
    /// only when the wake lands (the inbox `followup` succeeded): a refused `followup`
    /// (a full inbox) spends no budget.
    pub fn wake_allowed(&self) -> bool {
        let mut wakes = self
            .wakes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Self::prune_locked(&mut wakes);
        wakes.len() < MAX_WAKES_PER_WINDOW as usize
    }

    /// Records one wake that just landed.
    pub fn note_wake(&self) {
        let mut wakes = self
            .wakes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Self::prune_locked(&mut wakes);
        wakes.push_back(tokio::time::Instant::now());
    }

    /// A user message clears the budget at once. The window no longer needs a user message
    /// to recover, but one still resets it immediately (ADR 0037 decision 3's reset).
    pub fn reset_wakes(&self) {
        self.wakes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
    }

    /// Wakes inside the window right now (tests).
    pub fn wakes(&self) -> u32 {
        let mut wakes = self
            .wakes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Self::prune_locked(&mut wakes);
        u32::try_from(wakes.len()).unwrap_or(u32::MAX)
    }
}
