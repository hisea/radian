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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use agent_core::{InputSource, Message, OpaqueJson};
    use radian_runtime::{Inbox, RuntimeEvents};
    use radian_session::{InboxLane, RuntimeEvent};

    /// A loop-tagged message the way the TUI builds it: the `origin` the journal audits
    /// (ADR 0005), with `continuation = "loop"` and the iteration number.
    fn loop_message(text: &str, iteration: u64) -> Message {
        let origin = OpaqueJson::parse(
            serde_json::json!({ "continuation": "loop", "iteration": iteration }).to_string(),
        )
        .expect("the loop origin we build is valid JSON");
        let mut message = Message::user(text);
        match &mut message {
            Message::User { meta, .. }
            | Message::Assistant { meta, .. }
            | Message::Tool { meta, .. } => meta.origin = Some(origin),
        }
        message
    }

    /// The loop iteration a message carries, or `None` when it is not a loop re-send.
    fn loop_iteration(message: &Message) -> Option<u64> {
        let origin = match message {
            Message::User { meta, .. }
            | Message::Assistant { meta, .. }
            | Message::Tool { meta, .. } => meta.origin.as_ref()?,
        };
        let value: serde_json::Value = serde_json::from_str(origin.get()).ok()?;
        if value.get("continuation")?.as_str()? != "loop" {
            return None;
        }
        value.get("iteration")?.as_u64()
    }

    /// The `/loop` continuation re-sends through `Inbox::followup` (source `User`, the
    /// loop's own caps as the bound) and never touches the wake gate: twenty loop
    /// iterations keep `source = User` with increasing `iteration` meta, and the budget
    /// stays at zero (backlog `tui-loop-continuation.md` acceptance).
    #[tokio::test]
    async fn loop_iterations_use_user_followups_outside_the_wake_budget() {
        let gate = WakeGate::new();
        let inbox = Inbox::new(64);
        // The inbox keeps the last installed sink: one shared probe for the whole test.
        let seen: Arc<std::sync::Mutex<Vec<(InboxLane, InputSource, Message)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let probe = Arc::clone(&seen);
        inbox.set_runtime_events(Arc::new(move |event: &RuntimeEvent| {
            if let RuntimeEvent::InboxEnqueued {
                lane,
                source,
                message,
                ..
            } = event
            {
                probe
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push((*lane, *source, message.clone()));
            }
        }));
        for iteration in 1..=20u64 {
            inbox
                .followup(loop_message("go", iteration), InputSource::User)
                .await
                .expect("the inbox has room");
            assert!(gate.wake_allowed(), "the loop never fills the budget");
        }
        assert_eq!(gate.wakes(), 0, "twenty loop iterations record no wake");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 20);
        for (index, (lane, source, message)) in seen.iter().enumerate() {
            let iteration = index as u64 + 1;
            assert_eq!(*lane, InboxLane::NextTurn);
            assert_eq!(*source, InputSource::User);
            assert_eq!(
                loop_iteration(message),
                Some(iteration),
                "iteration {iteration} is tagged"
            );
        }
    }
}
