//! Background-job completion notifications and the durable start/settle records (ADR 0037
//! slices 3-4).
//!
//! The host owns the [`Jobs`] registry (it passes it to `CodingTools`), so it is the one that
//! sees a job start and settle. For each event it writes the durable record
//! ([`RuntimeEvent::BackgroundJobStarted`] / [`RuntimeEvent::BackgroundJobSettled`]); on a
//! settle the model should hear about it sends **one System message**: while a turn is in
//! flight it uses the pending-wake `next_step` lane (context for the next step, opening the
//! next turn itself when stranded past the turn end — ADR 0011), and while the
//! driver is idle the `followup` lane (which wakes a turn). At most `MAX_WAKES_PER_WINDOW` wakes fit
//! in any `WAKE_WINDOW` (the [`WakeGate`] rate gate, ADR 0037 decision 5); past the gate a
//! settle degrades to `inject` only, so a job that starts another job cannot wake the session
//! forever, while a normal worker chain (wake gaps of minutes) never fills the budget.
//!
//! A settle is **not** notified when the model already has it: a `job` wait (`op: "wait"`)
//! that returned the settle, a `job` kill (`op: "kill"`), or a session-exit cleanup (the
//! registry marks those `notify: false`). A timeout **is** notified: the model set the timeout
//! but did not ask to kill the job, so it may be waiting on it.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use agent_core::{InputSource, Message};
use radian_coding::{JobEvent, Jobs};
use radian_runtime::{Inbox, RuntimeEvents};
use radian_session::{EventData, RuntimeEvent, Session, SessionJournal};
use tokio::sync::Notify;

use super::wake_gate::WakeGate;
use crate::hooks::HooksWarning;

/// The most output bytes a completion message inlines (ADR 0037 decision 5; omp's
/// `ASYNC_INLINE_RESULT_MAX_CHARS` idea). 4 KiB is the tail of a failing build between two
/// tool calls without letting a chatty job flood the context; the rest is read with
/// `job`, the model's only channel to a job's output (ADR 0039 decision 1).
pub const NOTIFY_INLINE_BYTES: usize = 4 << 10;

/// The one TUI notice line a degraded wake shows: `N` is the notices degraded since the
/// last wake. The Stop-hook degrade reuses it (ADR 0045 §2).
pub fn wake_budget_line(degraded: u32) -> String {
    format!(
        "wake budget used up: {degraded} job notices are waiting — send any message to continue"
    )
}

/// The per-session job notifier. One per session; the host holds it for `reset_wakes`.
pub struct JobNotifier {
    /// Weak: the subscriber task holds the notifier and waits for the registry's channel to
    /// close, and the channel's sender lives in the registry. A strong reference here was a
    /// cycle - the registry never dropped, `Closed` never came, and every ended session kept
    /// its task, journal (with its lock) and inbox until the process exited (found 2026-10-17:
    /// a removed task's id could not be spawned again in the same process).
    jobs: Weak<Jobs>,
    inbox: Arc<Inbox>,
    journal: Arc<SessionJournal>,
    /// Shared with the driver: `true` while a turn is in flight.
    turn_running: Arc<AtomicBool>,
    /// The live runtime-event sink (ADR 0027 slice ③ item 5): the TUI's footer
    /// `jobs` item reads the start/settle pair from here, not by polling the journal file.
    /// The record is journaled first, then forwarded, like the driver's own boundaries.
    events: Arc<dyn RuntimeEvents>,
    /// The sliding-window wake budget (ADR 0037 decision 5): at most a window's worth of
    /// wakes, expiring on their own with no user message needed.
    gate: WakeGate,
    /// Notices degraded to inject-only since the last wake. A woken turn absorbs the
    /// waiting injects (the driver drains `next_step` at turn start), so a wake ends
    /// the burst; a user message resets it at once.
    degraded: AtomicU32,
    /// The wake-budget notice line (the TUI shows it as a transcript notice, the CLI
    /// prints it). Fired on every degrade; the TUI shows one line per burst.
    wake: HooksWarning,
    /// How many settles this notifier has fully handled (journaled, then injected or
    /// followed up). `exec -p` waits until this catches up with the registry's settle
    /// total, so a settle recorded just before the check is never missed.
    settled_handled: AtomicU64,
    /// Bumped after every fully handled settle, so a waiter sees each delivery.
    settled_signal: Arc<Notify>,
}

impl JobNotifier {
    pub fn new(
        jobs: Arc<Jobs>,
        inbox: Arc<Inbox>,
        journal: Arc<SessionJournal>,
        turn_running: Arc<AtomicBool>,
        events: Arc<dyn RuntimeEvents>,
        wake: HooksWarning,
    ) -> Arc<Self> {
        Arc::new(Self {
            jobs: Arc::downgrade(&jobs),
            inbox,
            journal,
            turn_running,
            events,
            gate: WakeGate::new(),
            degraded: AtomicU32::new(0),
            wake,
            settled_handled: AtomicU64::new(0),
            settled_signal: Arc::new(Notify::new()),
        })
    }

    /// Spawns the subscriber on `runtime`. It ends when the registry is dropped (a session
    /// end), which closes the channel.
    ///
    /// The subscription is taken **before** the task is spawned (ADR 0039 decision 6): an event
    /// sent between `spawn` returning and the task first running would otherwise be lost, and
    /// the first job of a session is exactly such an event.
    pub fn spawn(self: &Arc<Self>, runtime: &tokio::runtime::Handle) {
        // The registry is already gone (the session ended before the spawn): nothing to watch.
        let Some(jobs) = self.jobs.upgrade() else {
            return;
        };
        let mut rx = jobs.subscribe();
        // Drop the strong reference before the task starts, or the task would keep the
        // registry (and so its own channel) alive.
        drop(jobs);
        let notifier = Arc::clone(self);
        runtime.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => notifier.deliver(event).await,
                    // A subscriber that fell behind loses old notices; the settle record and
                    // the state are still in the journal and the registry.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// A user message resets the wake budget (ADR 0037 decision 3's reset survives the
    /// rate gate: the window recovers on its own, and a user message clears it at once).
    pub fn reset_wakes(&self) {
        self.gate.reset_wakes();
        self.degraded.store(0, Ordering::SeqCst);
    }

    /// Wakes inside the window right now (tests).
    pub fn wakes(&self) -> u32 {
        self.gate.wakes()
    }

    /// How many notices were degraded since the last wake (tests). A woken turn absorbs
    /// the waiting injects, so a wake resets this to zero; a user message does too.
    pub fn degraded(&self) -> u32 {
        self.degraded.load(Ordering::SeqCst)
    }

    /// How many settles have been fully handled (journaled, then injected or followed
    /// up). Only moves forward.
    pub fn settled_handled(&self) -> u64 {
        self.settled_handled.load(Ordering::SeqCst)
    }

    /// Resolves after every fully handled settle. A waiter re-checks its exit condition
    /// on each one.
    pub fn settled_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.settled_signal)
    }

    /// How many idle settles the wake gate degraded to inject-only. Each one is a notice
    /// no turn will ever start for. `exec -p` ends the session on these (exit 3) instead
    /// of waiting.
    pub fn degraded_pending(&self) -> u64 {
        u64::from(self.degraded.load(Ordering::SeqCst))
    }

    /// Marks one settle fully handled: the journal write and the inject/followup have
    /// both returned. Called for every settle, notified or not.
    fn note_handled(&self) {
        self.settled_handled.fetch_add(1, Ordering::SeqCst);
        self.settled_signal.notify_one();
    }

    /// Handles one lifecycle event: write the durable record, forward it on the live
    /// runtime-event sink (the TUI's footer `jobs` item), then (for a notifiable settle)
    /// deliver the System message on the busy/idle lane.
    pub async fn deliver(&self, event: JobEvent) {
        match event {
            JobEvent::Started {
                id,
                command,
                output,
            } => {
                let record = RuntimeEvent::BackgroundJobStarted {
                    job_id: id,
                    command,
                    output,
                };
                let _ = self.journal.append_runtime_event(record.clone()).await;
                self.events.on_runtime_event(&record);
            }
            JobEvent::Settled {
                id,
                state,
                duration_ms,
                notify,
            } => {
                let record = RuntimeEvent::BackgroundJobSettled {
                    job_id: id.clone(),
                    state: state.label(),
                    duration_ms,
                };
                let _ = self.journal.append_runtime_event(record.clone()).await;
                self.events.on_runtime_event(&record);
                if !notify {
                    self.note_handled();
                    return;
                }
                let text = self.completion_message(&id, &state, duration_ms);
                let source = InputSource::System;
                if self.turn_running.load(Ordering::SeqCst) {
                    // Busy: a pending-wake notice (ADR 0011). Absorbed by the
                    // running turn it is context for the next step, exactly like an
                    // `inject`; stranded past the turn end it opens the next turn itself,
                    // and that opening counts one wake (see `note_promoted_wake`). Past the
                    // gate it degrades to a plain `inject` with no pending wake.
                    if self.gate.wake_allowed() {
                        let _ = self.inbox.inject_pending_wake(text, source).await;
                    } else if self.inbox.inject_degraded(text, source).await.is_ok() {
                        let degraded = self.degraded.fetch_add(1, Ordering::SeqCst) + 1;
                        (self.wake)(&wake_budget_line(degraded));
                    }
                } else if self.gate.wake_allowed() {
                    // Idle: wake a turn, up to the rate gate. The woken turn absorbs the
                    // waiting injects, so a wake also ends the degrade burst: the count
                    // (and the herdr `wake budget` label, cleared at turn start) does not
                    // wait for a user message.
                    if self.inbox.followup(text, source).await.is_ok() {
                        self.gate.note_wake();
                        self.degraded.store(0, Ordering::SeqCst);
                    }
                } else {
                    // THE WAKE-GATE DEGRADE POINT (ADR 0037 decision 5): the rate gate is
                    // full, so this settle is injected only and wakes nothing. The durable
                    // record says so (`degraded`), and the TUI/herdr legs hear it through
                    // the wake notice. This `else` branch is the single degrade point:
                    // keep its shape (gate check above, marked `inject` here). `exec -p`
                    // ends the session on these (exit 3) instead of waiting for a turn
                    // that cannot come.
                    if self.inbox.inject_degraded(text, source).await.is_ok() {
                        let degraded = self.degraded.fetch_add(1, Ordering::SeqCst) + 1;
                        (self.wake)(&wake_budget_line(degraded));
                    }
                }
                self.note_handled();
            }
        }
    }

    /// A pending-wake notice opened the next turn (ADR 0011, ADR 0037 decision
    /// 5): the inbox fires it once per entry, only for a notice that actually opened a
    /// turn. Counts one wake and ends the degrade burst, like an idle `followup` wake;
    /// a notice absorbed by its running turn never fires, so it is never counted.
    /// Wired in `open_session` through [`Inbox::set_pending_wake_callback`](radian_runtime::Inbox::set_pending_wake_callback).
    pub fn note_promoted_wake(&self) {
        self.gate.note_wake();
        self.degraded.store(0, Ordering::SeqCst);
    }

    /// The one System message a settle sends: id, state, duration and the tail of
    /// the output (up to [`NOTIFY_INLINE_BYTES`]).
    fn completion_message(
        &self,
        id: &str,
        state: &radian_coding::JobState,
        duration_ms: u64,
    ) -> String {
        let mut text = format!(
            "[background job {id} finished: {} after {}]",
            state.label(),
            human_duration(duration_ms)
        );
        if let Some((_, output, tail)) = self
            .jobs
            .upgrade()
            .and_then(|jobs| jobs.tail(id, NOTIFY_INLINE_BYTES))
        {
            text.push_str(&format!("\noutput: {output}"));
            if !tail.is_empty() {
                text.push_str("\nthe last part of its output:\n");
                text.push_str(&tail);
                if !tail.ends_with('\n') {
                    text.push('\n');
                }
            }
            text.push_str("Read its output with job.");
        }
        text
    }
}

/// A job that started but never settled in `session`'s journal: a crash orphan (ADR 0037
/// decision 6). Returned in start order as `(job_id, command)`.
pub fn orphan_jobs(session: &Session) -> Vec<(String, String)> {
    let mut started: HashMap<String, String> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut settled: HashSet<String> = HashSet::new();
    for event in session.events() {
        match &event.data {
            EventData::BackgroundJobStarted {
                job_id, command, ..
            } => {
                if started.insert(job_id.clone(), command.clone()).is_none() {
                    order.push(job_id.clone());
                }
            }
            EventData::BackgroundJobSettled { job_id, .. } => {
                settled.insert(job_id.clone());
            }
            _ => {}
        }
    }
    order
        .into_iter()
        .filter(|id| !settled.contains(id))
        .map(|id| {
            let command = started.remove(&id).unwrap_or_default();
            (id, command)
        })
        .collect()
}

/// The System message that reports one orphan: it is not re-run, and the model must start it
/// again if it still needs it.
pub fn orphan_message(job_id: &str, command: &str) -> Message {
    Message::user(format!(
        "[background job {job_id} ({command}) was running when the session stopped; its state \
         is unknown and it was not restarted. Start it again if you still need it.]"
    ))
}

fn human_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let secs = ms / 1000;
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_duration_reads_in_seconds() {
        assert_eq!(human_duration(350), "350ms");
        assert_eq!(human_duration(1200), "1.2s");
        assert_eq!(human_duration(125_000), "2m5s");
    }
}
