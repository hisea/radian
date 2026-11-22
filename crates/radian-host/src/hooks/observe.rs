//! The observe-only events, the session end, and the recent-runs ring.

use super::*;

impl CommandHooks {
    /// Starts every matching observe-only entry's hooks, one run per `command` + `args`
    /// (the project/user dedupe of ADR 0045 §1 item 3). Nothing blocks and no output is
    /// read: a run's exit code and timeout only reach the recent-runs ring.
    pub(super) fn observe_entries(
        &self,
        event: &str,
        entries: &[CompiledObserveEntry],
        value: &str,
        stdin: &serde_json::Value,
    ) {
        let mut seen = std::collections::HashSet::new();
        for entry in entries {
            if !entry.matcher.matches(value) {
                continue;
            }
            for hook in &entry.hooks {
                if !seen.insert(dedup_key(hook)) {
                    continue;
                }
                self.spawn_observe(hook, stdin.clone(), event, event == "StopFailure");
            }
        }
    }

    /// Starts one observe-only run and returns at once (ADR 0045 §6). The run is registered
    /// so [`CommandHooks::wait_observe`] can wait on it and kill it, and so the session end
    /// kills it. Its result is discarded: these events observe, never decide.
    pub(super) fn spawn_observe(
        &self,
        hook: &HookCommand,
        stdin_json: serde_json::Value,
        event: &str,
        waits_on_exit: bool,
    ) {
        if self.async_ended.load(Ordering::SeqCst) {
            return;
        }
        let handle = Arc::new(AsyncRunHandle::default());
        {
            let mut running = self
                .observe_running
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            // Re-checked under the lock: an end that set the flag while this call waited
            // must not be overtaken by a run it would never drain.
            if self.async_ended.load(Ordering::SeqCst) {
                return;
            }
            running.push(ObserveRun {
                handle: Arc::clone(&handle),
                waits_on_exit,
            });
        }
        let this = self.clone();
        let hook = hook.clone();
        let event = event.to_owned();
        self.runtime.spawn(async move {
            // The configured timeout, or 60 s: `async` is inert here and never moves it.
            let _ = this
                .execute_with_timeout(
                    &hook,
                    &stdin_json,
                    &event,
                    None,
                    Some(&handle),
                    hook.observe_timeout(),
                )
                .await;
            handle.exited.store(true, Ordering::SeqCst);
            this.finish_observe(&handle);
        });
    }

    /// Waits at most `timeout` for the `StopFailure` runs to finish, then kills the ones still
    /// running (ADR 0045 §6). `exec -p` calls it before its session end, so a slow
    /// `StopFailure` hook costs at most 5 s and leaves no process group behind; the run's
    /// exit code and output are unchanged. A `Notification` run is never waited for (the
    /// session end kills it).
    pub async fn wait_observe(&self, timeout: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.observe_signal.notified();
            tokio::pin!(notified);
            let waiting = self
                .observe_running
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .iter()
                .any(|run| run.waits_on_exit);
            if !waiting {
                return;
            }
            tokio::select! {
                () = &mut notified => {}
                () = tokio::time::sleep_until(deadline) => {
                    self.kill_waited_observe();
                    return;
                }
            }
        }
    }

    /// Kills the `StopFailure` runs still in flight at the 5 s `exec -p` deadline. Each run
    /// delivers nothing and its recent-runs entry says `cancelled`.
    pub(super) fn kill_waited_observe(&self) {
        let mut running = self
            .observe_running
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut keep = Vec::new();
        for run in running.drain(..) {
            if !run.waits_on_exit {
                keep.push(run);
                continue;
            }
            run.handle.cancelled.store(true, Ordering::SeqCst);
            let pid = run.handle.pid.load(Ordering::SeqCst);
            if pid != 0 {
                kill_group(pid);
            }
        }
        *running = keep;
        drop(running);
        self.observe_signal.notify_waiters();
    }

    /// Kills every observe-only run still in flight (the session end). Each run delivers
    /// nothing and its recent-runs entry says `cancelled`.
    pub(super) fn kill_observe(&self) {
        let mut running = self
            .observe_running
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for run in running.drain(..) {
            run.handle.cancelled.store(true, Ordering::SeqCst);
            let pid = run.handle.pid.load(Ordering::SeqCst);
            if pid != 0 {
                kill_group(pid);
            }
        }
        drop(running);
        self.observe_signal.notify_waiters();
    }

    /// Removes one finished observe run and wakes a waiter.
    pub(super) fn finish_observe(&self, handle: &Arc<AsyncRunHandle>) {
        self.observe_running
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .retain(|run| !Arc::ptr_eq(&run.handle, handle));
        self.observe_signal.notify_waiters();
    }

    /// Ends the session's async hooks (ADR 0045 §5): every still-running run's process group
    /// is killed and its result is never delivered, and no new async hook starts afterwards.
    /// Idempotent. The host calls this when the session's driver is joined
    /// ([`crate::LocalSession::join`]): the TUI exiting, `/new`, and the end of an `exec`
    /// run all land there, and `exec -p` never waits for a hook.
    ///
    /// The delivery gate is taken **before** the flag is set: a delivery either finishes its
    /// enqueue first (it belongs to this session and is not retracted) or, from the flag on,
    /// finds the session ended and enqueues nothing. So when this returns, no async result
    /// can still reach the inbox.
    pub async fn end_session(&self) {
        let _delivery = self.deliver_gate.lock().await;
        self.async_ended.store(true, Ordering::SeqCst);
        let mut running = self
            .async_running
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for run in running.drain(..) {
            // A run whose process already exited is delivering its result; the session is
            // over, so `deliver_async` drops it (the `async_ended` flag) without a kill.
            if run.handle.exited.load(Ordering::SeqCst) {
                continue;
            }
            run.handle.cancelled.store(true, Ordering::SeqCst);
            let pid = run.handle.pid.load(Ordering::SeqCst);
            if pid != 0 {
                kill_group(pid);
            }
        }
        drop(running);
        self.kill_observe();
        self.async_signal.notify_waiters();
    }

    /// Background work that makes the intent-stop validator stand down: the job registry
    /// plus every running async hook (ADR 0045 §3/§5).
    pub(super) fn background_running(&self) -> usize {
        self.jobs_running() + self.async_hooks_running()
    }

    /// Records one finished run: one `tracing` info line, then the in-memory ring (the
    /// oldest entry drops when it is full). Milliseconds are rounded up, so a fast hook
    /// never reads `0ms`.
    pub(super) fn record_run(
        &self,
        event: &str,
        tool: Option<&str>,
        command: &str,
        end: HookRunEnd,
        started: Instant,
    ) {
        let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let record = HookRunRecord {
            event: event.to_owned(),
            tool: tool.map(str::to_owned),
            command: first_line(command),
            end,
            millis: micros.div_ceil(1000).max(1),
        };
        tracing::info!(target: "radian_host::hooks", "{}", record.line());
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while runs.len() >= HOOK_RUN_HISTORY {
            runs.pop_front();
        }
        runs.push_back(record);
    }
}
