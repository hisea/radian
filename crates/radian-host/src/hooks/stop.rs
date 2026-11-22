//! The `Stop` cycle: the hook chain, the intent-stop validator and the continuation.

use super::*;

impl CommandHooks {
    /// Runs one `Stop` cycle synchronously: every configured Stop hook in order — the
    /// project layer first, then the user's (ADR 0045 slice B), each command string once —
    /// plus the built-in intent-stop validator (backlog `turn-end-continuation.md` §B),
    /// which runs even with no `Stop` hook configured. Every exit 2 collects its stderr;
    /// after the loop a single continuation opens with the reasons joined in one message,
    /// counted as one wake. Past the continuation cap the verdict degrades to a marked
    /// `inject` (no wake) with the wake-budget notice, so it waits for the next
    /// user-woken turn instead of being dropped silently. Only [`RunStatus::Completed`]
    /// runs the hooks: a cancelled, failed, or otherwise non-completed turn runs nothing
    /// (an exit-2 hook after a user interrupt would otherwise open a turn the user just
    /// asked to end).
    /// [`Hooks::on_turn_end`] runs this cycle with the provenance it captured at that
    /// turn's boundary (`run_stop_once_with`); tests drive this entry directly so there
    /// is no timing involved.
    pub async fn run_stop_once(&self, outcome: &RunOutcome) {
        // The value says what opened *this* turn (nothing, the built-in validator, or a
        // hook); it is consumed once, by this turn's own `Stop` (ADR 0045 §2).
        let continuation = self.take_turn_continuation();
        self.run_stop_once_with(outcome, continuation).await;
    }

    /// Reads and clears the running turn's continuation provenance.
    pub(super) fn take_turn_continuation(&self) -> Continuation {
        Continuation::from_u8(
            self.turn_continuation
                .swap(Continuation::None.as_u8(), Ordering::SeqCst),
        )
    }

    /// Counts the turn that just ended: a hook-opened continuation with no tool call adds
    /// one to the idle streak; a continued turn that called a tool resets it; any other
    /// turn (not continued, or opened by the validator alone) ends the hook streak too.
    pub(super) fn count_idle_continuation(&self, outcome: &RunOutcome, continuation: Continuation) {
        if continuation == Continuation::Hook && outcome.model_calls == 1 {
            self.hook_idle.fetch_add(1, Ordering::SeqCst);
        } else {
            self.hook_idle.store(0, Ordering::SeqCst);
        }
    }

    /// [`CommandHooks::run_stop_once`] with the provenance already captured: the turn-end
    /// hook reads it synchronously, before spawning, so a slow `Stop` run for an earlier
    /// turn can never consume this turn's value.
    pub(super) async fn run_stop_once_with(
        &self,
        outcome: &RunOutcome,
        continuation: Continuation,
    ) {
        // A non-completed turn runs no `Stop` and opens no turn of its own.
        if !matches!(outcome.status, RunStatus::Completed) {
            return;
        }
        // The idle-continuation streak follows this turn's own outcome: a hook continuation
        // whose turn did no work adds one; real work (a tool call) resets it.
        self.count_idle_continuation(outcome, continuation);
        // Claude's field name: the last assistant message's text parts joined, so a user
        // script can run its own detection (backlog §B.6). Empty when the turn has none.
        let last_message = outcome
            .final_message
            .as_ref()
            .map(Message::text)
            .unwrap_or_default();
        let stdin = serde_json::json!({
            "hook_event_name": "Stop",
            "cwd": self.workspace.to_string_lossy(),
            "session_id": self.session_id,
            "last_assistant_message": last_message,
            "stop_hook_active": continuation.is_continuation(),
            "background_tasks": self.background_tasks(),
            "session_crons": self.session_crons(),
        });
        let mut reasons = Vec::new();
        // One run per command string, like the tool-call events (ADR 0045 slice B).
        let mut seen = std::collections::HashSet::new();
        for entry in &self.stop {
            for hook in &entry.hooks {
                if !seen.insert(dedup_key(hook)) {
                    continue;
                }
                // `background_tasks` is read per hook, so a sync hook later in the chain
                // sees an async hook that started just before it (ADR 0045 §5).
                let mut hook_stdin = stdin.clone();
                hook_stdin["background_tasks"] = serde_json::Value::Array(self.background_tasks());
                // An async `Stop` hook is not part of this turn's continuation chain and
                // is not counted by the idle-continuation counter (ADR 0045 §5): it starts
                // and the chain continues at once, delivering its own result later.
                if hook.is_async() {
                    self.spawn_async(hook, hook_stdin, "Stop", None);
                    continue;
                }
                match self.execute(hook, &hook_stdin, "Stop", None, None).await {
                    Ok(output) if output.code == Some(0) => {
                        if let Some(parsed) = self.parsed_output(&output.stdout, "Stop") {
                            reasons.extend(self.stop_output_reasons(parsed));
                        }
                    }
                    Ok(output) if output.code == Some(2) => {
                        let reason = short(&output.stderr);
                        reasons.push(if reason.trim().is_empty() {
                            "the Stop hook exited with code 2".to_owned()
                        } else {
                            reason
                        });
                    }
                    Ok(output) => {
                        // Fail closed (ADR 0017): a broken validator never continues the
                        // turn. The warning is the user-visible trace; the turn simply ends.
                        (self.warn)(&format!(
                            "Stop hook exited with code {}: {}",
                            output
                                .code
                                .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                            short(&output.stderr),
                        ));
                        return;
                    }
                    Err(error) => {
                        (self.warn)(&format!("Stop hook failed: {error}"));
                        return;
                    }
                }
            }
        }
        // The built-in validator joins the same single continuation (at most one per turn):
        // its fixed text rides after the hook reasons, and its presence marks the message
        // with the `intent-stop` origin.
        let hook_drove = !reasons.is_empty();
        let validator_drove = self.validator_verdict(outcome).await;
        if validator_drove {
            reasons.push(intent_stop::INTENT_STOP_TEXT.to_owned());
        }
        if !hook_drove && !validator_drove {
            return;
        }
        let text = if hook_drove {
            format!("Stop hook requested another turn:\n{}", reasons.join("\n"))
        } else {
            reasons.join("\n")
        };
        let mut message = Message::user(text);
        if validator_drove {
            match &mut message {
                Message::User { meta, .. }
                | Message::Assistant { meta, .. }
                | Message::Tool { meta, .. } => {
                    meta.origin = Some(intent_stop::intent_stop_origin());
                }
            }
        }
        // THE IDLE-CONTINUATION CAP (ADR 0045 §3): the
        // continuation that would be the 8th hook continuation in a row of no-work turns
        // does not open one. It takes the validator's two-in-a-row treatment: a marked
        // `inject` (journal-visible, no wake), one notice line through the host's
        // `hook stop` channel, and the TUI's `blocked` label. A merged validator
        // continuation rides the same single message, so it degrades with it.
        let idle_capped =
            hook_drove && self.hook_idle.load(Ordering::SeqCst) + 1 >= MAX_HOOK_IDLE_CONTINUATIONS;
        if idle_capped {
            if self
                .inbox
                .inject_degraded(message, InputSource::System)
                .await
                .is_ok()
            {
                self.degraded.fetch_add(1, Ordering::SeqCst);
            }
            (self.hook_stop_warn)(&hook_stop_cap_line());
            return;
        }
        // THE WAKE-GATE DEGRADE POINT (ADR 0037 decision 5): past the gate
        // the hook still runs, but an exit-2 verdict degrades to a marked
        // `inject` (no wake) with the wake-budget notice, the same degrade
        // as the background-job notifier (ADR 0045 §2) — a skipped run
        // would be silent, a degraded verdict waits visibly.
        // One continuation for every exit-2 verdict in the cycle, with the
        // reasons joined above: one wake, one degrade count at most. The
        // validator rides this same point: its continuation counts one wake.
        if self.gate.wake_allowed() {
            // The woken turn absorbs the waiting injects, so a wake also
            // ends the degrade burst. The continuation carries its own message id, and
            // that id is what the claim consumes (ADR 0045 §2): the turn it opens is the
            // only one that reports `stop_hook_active: true`, however late this run
            // finishes. A failed enqueue opened nothing and drops the id.
            let continuation = continuation_message_id();
            set_message_id(&mut message, continuation.clone());
            self.pending_continuations
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(continuation.clone(), hook_drove);
            if self
                .inbox
                .followup(message, InputSource::System)
                .await
                .is_ok()
            {
                self.gate.note_wake();
                self.degraded.store(0, Ordering::SeqCst);
            } else {
                self.pending_continuations
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&continuation);
            }
        } else if self
            .inbox
            .inject_degraded(message, InputSource::System)
            .await
            .is_ok()
        {
            let degraded = self.degraded.fetch_add(1, Ordering::SeqCst) + 1;
            (self.wake)(&crate::job_notify::wake_budget_line(degraded));
        }
    }

    /// The built-in validator's verdict for this turn end: `true` when its fixed text
    /// joins the continuation below. A hit with fewer than [`MAX_INTENT_STOP_CONSECUTIVE`]
    /// consecutive predecessors counts the hit and returns `true`. Anything else — the
    /// switch off, `/loop` or a goal owning the turn end, a background job running, a
    /// rule miss — clears the count and returns `false`.
    ///
    /// The third consecutive hit takes THE TWO-IN-A-ROW CAP instead: no third
    /// continuation opens. The verdict degrades to a marked `inject` (journal-visible,
    /// no wake) and the host reports the stop — the TUI shows one notice line, herdr
    /// reports `blocked` with the `intent stop` label until the next turn starts, and a
    /// user message clears it all. The count stays saturated, so further consecutive
    /// hits re-report (rewriting the same TUI line) instead of opening turns.
    pub(super) async fn validator_verdict(&self, outcome: &RunOutcome) -> bool {
        let hit = self.intent_stop
            && !self.loop_active.load(Ordering::SeqCst)
            && !self.goal_active()
            && self.background_running() == 0
            && outcome
                .final_message
                .as_ref()
                .is_some_and(intent_stop::matches_rule);
        if !hit {
            self.consecutive.store(0, Ordering::SeqCst);
            return false;
        }
        if self.consecutive.load(Ordering::SeqCst) >= MAX_INTENT_STOP_CONSECUTIVE {
            let message = intent_stop::intent_stop_message();
            if self
                .inbox
                .inject_degraded(message, InputSource::System)
                .await
                .is_ok()
            {
                self.degraded.fetch_add(1, Ordering::SeqCst);
            }
            (self.intent_stop_warn)(&intent_stop::intent_stop_cap_line());
            return false;
        }
        self.consecutive.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// The goal's status (`None` when no goal is set or the share was dropped).
    pub(super) fn goal_status(&self) -> Option<radian_session::GoalStatus> {
        self.goal
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .and_then(|goal| {
                goal.lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .as_ref()
                    .map(|state| state.status)
            })
    }

    /// Whether an `Active` or `Paused` goal owns the turn end (it re-posts itself, so
    /// the validator stays off). A missing goal reads as none.
    pub(super) fn goal_active(&self) -> bool {
        matches!(
            self.goal_status(),
            Some(radian_session::GoalStatus::Active | radian_session::GoalStatus::Paused)
        )
    }

    /// The background jobs and async hooks running when `Stop` runs, as its
    /// `background_tasks` (ADR 0045 §2/§5). A registry that was dropped, or never set, reads
    /// as no jobs running: the hook still runs and the field is `[]`. An async hook is
    /// listed with `type: "hook"`, a job with `type: "shell"`.
    pub(super) fn background_tasks(&self) -> Vec<serde_json::Value> {
        let jobs = {
            let slot = self
                .jobs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            slot.as_ref().and_then(Weak::upgrade)
        };
        let mut tasks: Vec<serde_json::Value> = Vec::new();
        if let Some(jobs) = jobs {
            tasks.extend(jobs.running_tasks().into_iter().map(|task| {
                serde_json::json!({
                    "id": task.id,
                    "type": "shell",
                    "status": "running",
                    "description": task.description,
                    "command": task.command,
                })
            }));
        }
        tasks.extend(self.running_async_hooks().into_iter().map(|hook| {
            serde_json::json!({
                "id": hook.id,
                "type": "hook",
                "status": "running",
                "description": first_line(&hook.command),
                "command": hook.command,
            })
        }));
        tasks
    }

    /// The session-level schedulers that open turns by themselves, as the `Stop` stdin
    /// `session_crons` (ADR 0045 §2): an armed `/loop`, then an `Active` or `Paused` goal.
    pub(super) fn session_crons(&self) -> Vec<serde_json::Value> {
        let mut crons = Vec::new();
        if self.loop_active.load(Ordering::SeqCst) {
            crons.push(serde_json::json!({"id": "loop", "type": "loop"}));
        }
        match self.goal_status() {
            Some(radian_session::GoalStatus::Active) => crons.push(serde_json::json!({
                "id": "goal",
                "type": "goal",
                "status": "active",
            })),
            Some(radian_session::GoalStatus::Paused) => crons.push(serde_json::json!({
                "id": "goal",
                "type": "goal",
                "status": "paused",
            })),
            _ => {}
        }
        crons
    }
}
