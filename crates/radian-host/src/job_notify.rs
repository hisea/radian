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
use std::path::PathBuf;
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

/// The one live notice line a lost completion notice shows: it names the job and the
/// enqueue error, so the reason survives even when the diagnostics write below fails
/// (disk-full is the likely cause). Like the wake-budget line it says how to continue.
pub fn job_notice_lost_line(job_id: &str, reason: &str) -> String {
    format!(
        "job {job_id} finished but its completion notice was lost ({reason}) — send any message to continue"
    )
}

/// How many times a settle's inbox enqueue is attempted, and the pauses between them
/// (ADR 0037 decision 5's failure rule): a failed journal append (for example ENOSPC)
/// leaves the journal usable — `write_locked` advances `next_seq` only on success — so
/// a later attempt can still land once space is free. A full inbox drains the same way.
/// Bounded so one stuck settle cannot hold the subscriber (and every other settle)
/// longer than about 1.2 s.
pub const NOTICE_ATTEMPTS: u32 = 3;
/// The pauses before the second and third attempts (about 0.2 s and 1 s apart).
pub const NOTICE_RETRY_DELAYS: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(200),
    std::time::Duration::from_secs(1),
];

/// Which inbox lane a settle's completion notice takes. One settle, one lane: a failed
/// `followup` never falls back to `inject` — every enqueue writes the journal first
/// (ADR 0001 S1), so it fails the same way when the disk is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeLane {
    /// Idle and the gate is open: wake a turn.
    Followup,
    /// Busy and the gate is open: context now, a turn of its own when stranded.
    PendingWake,
    /// The gate is full, busy or idle: wait for a turn without ever opening one.
    Degraded,
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
    /// A lost completion notice's line (the same display path as `wake`, one live
    /// notice). Fired once per settle whose enqueue failed every attempt; the TUI
    /// reports `blocked` with the `job notice lost` label until the next turn starts.
    notice_lost: HooksWarning,
    /// Settles whose notice failed every enqueue attempt (ADR 0037 decision 5's failure
    /// rule). Unlike a degrade, a lost notice is gone: no later wake absorbs it, so the
    /// count never clears on a wake — only a user message (`reset_wakes`) starts over.
    /// `exec -p` ends the run on these (exit 3) instead of waiting for a turn.
    lost: AtomicU64,
    /// Overrides the diagnostics directory the lost-notice record goes to (tests): the
    /// production path is the real cache directory, which tests must not touch.
    diagnostics_dir: std::sync::Mutex<Option<PathBuf>>,
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
        notice_lost: HooksWarning,
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
            notice_lost,
            lost: AtomicU64::new(0),
            diagnostics_dir: std::sync::Mutex::new(None),
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
    /// The lost count starts over too: the user saw the stop and continued anyway.
    pub fn reset_wakes(&self) {
        self.gate.reset_wakes();
        self.degraded.store(0, Ordering::SeqCst);
        self.lost.store(0, Ordering::SeqCst);
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

    /// How many settles lost their notice to a failed enqueue (tests).
    pub fn lost(&self) -> u64 {
        self.lost.load(Ordering::SeqCst)
    }

    /// How many lost notices no turn will ever run for. `exec -p` ends the session on
    /// these (exit 3) instead of waiting for a wake that cannot come.
    pub fn lost_pending(&self) -> u64 {
        self.lost.load(Ordering::SeqCst)
    }

    /// Overrides the diagnostics directory the lost-notice record goes to. The
    /// production path is the real cache directory; tests point it at a scratch
    /// directory so they never touch it.
    pub fn set_diagnostics_dir(&self, dir: PathBuf) {
        *self
            .diagnostics_dir
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(dir);
    }

    /// Marks one settle fully handled: the journal write and the inject/followup have
    /// both returned. Called for every settle, notified or not.
    fn note_handled(&self) {
        self.settled_handled.fetch_add(1, Ordering::SeqCst);
        self.settled_signal.notify_one();
    }

    /// Enqueues one completion notice on its lane, retrying a few times with growing
    /// pauses (ADR 0037 decision 5's failure rule). `Ok` means the notice is durable
    /// and visible to the driver; `Err` is the last attempt's error text.
    async fn enqueue_notice(
        &self,
        text: String,
        source: InputSource,
        lane: NoticeLane,
    ) -> Result<(), String> {
        let mut last_error = String::new();
        for attempt in 0..NOTICE_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(NOTICE_RETRY_DELAYS[(attempt - 1) as usize]).await;
            }
            let result = match lane {
                NoticeLane::Followup => self.inbox.followup(text.clone(), source).await.map(|_| ()),
                NoticeLane::PendingWake => self
                    .inbox
                    .inject_pending_wake(text.clone(), source)
                    .await
                    .map(|_| ()),
                NoticeLane::Degraded => self
                    .inbox
                    .inject_degraded(text.clone(), source)
                    .await
                    .map(|_| ()),
            };
            match result {
                Ok(()) => return Ok(()),
                Err(error) => last_error = error.to_string(),
            }
        }
        Err(last_error)
    }

    /// Every enqueue attempt for `job_id`'s notice failed: the session must not wait
    /// silently for a wake that cannot come (ADR 0037 decision 5's failure rule). Counts
    /// the loss, records the reason best-effort in the diagnostics directory (which may
    /// itself fail when the disk is full), and shows the live stop: one notice line
    /// naming the job and the reason, herdr `blocked` with the `job notice lost` label
    /// until the next turn starts, exit 3 in `exec -p`.
    fn note_lost(&self, job_id: &str, reason: &str) {
        let line = job_notice_lost_line(job_id, reason);
        let dir = self
            .diagnostics_dir
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .or_else(crate::diagnostics::diagnostics_dir);
        if let Some(dir) = dir {
            let name = crate::diagnostics::job_notice_log_name(
                self.journal.session_id(),
                crate::diagnostics::now_millis(),
            );
            let _ = crate::diagnostics::append(&dir.join(name), &format!("{line}\n"));
        }
        self.lost.fetch_add(1, Ordering::SeqCst);
        (self.notice_lost)(&line);
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
                        if let Err(reason) = self
                            .enqueue_notice(text, source, NoticeLane::PendingWake)
                            .await
                        {
                            self.note_lost(&id, &reason);
                        }
                    } else {
                        match self
                            .enqueue_notice(text, source, NoticeLane::Degraded)
                            .await
                        {
                            Ok(()) => {
                                let degraded = self.degraded.fetch_add(1, Ordering::SeqCst) + 1;
                                (self.wake)(&wake_budget_line(degraded));
                            }
                            Err(reason) => self.note_lost(&id, &reason),
                        }
                    }
                } else if self.gate.wake_allowed() {
                    // Idle: wake a turn, up to the rate gate. The woken turn absorbs the
                    // waiting injects, so a wake also ends the degrade burst: the count
                    // (and the herdr `wake budget` label, cleared at turn start) does not
                    // wait for a user message.
                    match self
                        .enqueue_notice(text, source, NoticeLane::Followup)
                        .await
                    {
                        Ok(()) => {
                            self.gate.note_wake();
                            self.degraded.store(0, Ordering::SeqCst);
                        }
                        Err(reason) => self.note_lost(&id, &reason),
                    }
                } else {
                    // THE WAKE-GATE DEGRADE POINT (ADR 0037 decision 5): the rate gate is
                    // full, so this settle is injected only and wakes nothing. The durable
                    // record says so (`degraded`), and the TUI/herdr legs hear it through
                    // the wake notice. This `else` branch is the single degrade point:
                    // keep its shape (gate check above, marked `inject` here). `exec -p`
                    // ends the session on these (exit 3) instead of waiting for a turn
                    // that cannot come.
                    match self
                        .enqueue_notice(text, source, NoticeLane::Degraded)
                        .await
                    {
                        Ok(()) => {
                            let degraded = self.degraded.fetch_add(1, Ordering::SeqCst) + 1;
                            (self.wake)(&wake_budget_line(degraded));
                        }
                        Err(reason) => self.note_lost(&id, &reason),
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
