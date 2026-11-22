//! The notification, permission-request and core hook trait impls.

use super::*;

impl NotificationHooks for CommandHooks {
    /// Fires the `Notification` observers (ADR 0045 §6): `permission_prompt` when a human is
    /// about to be asked, `idle_prompt` from the TUI's idle timer. The chain starts every
    /// matching hook in the background and returns at once, so it never delays a prompt; the
    /// message carries the tool name only, never its arguments.
    fn notification(&self, notification_type: &str, tool: Option<&str>) {
        if self.notification.is_empty() {
            return;
        }
        let message = match notification_type {
            NOTIFICATION_PERMISSION_PROMPT => format!(
                "radian needs your permission to use {}",
                tool.unwrap_or_default()
            ),
            NOTIFICATION_IDLE_PROMPT => "radian is waiting for your input".to_owned(),
            // Not a value of the closed set: nothing observes it.
            _ => return,
        };
        let stdin = serde_json::json!({
            "hook_event_name": "Notification",
            "cwd": self.workspace.to_string_lossy(),
            "session_id": self.session_id,
            "notification_type": notification_type,
            "title": "radian",
            "message": message,
        });
        self.observe_entries(
            "Notification",
            &self.notification,
            notification_type,
            &stdin,
        );
    }
}

impl PermissionRequestHooks for CommandHooks {
    /// Runs the `PermissionRequest` chain (ADR 0045 §4): project entries first, the user's
    /// after; any `deny` stops it; one `allow` stands unless a later hook denies; the same
    /// `command` + `args` runs once. `Err` is a hook error and denies the call with
    /// `hook_error`.
    fn permission_request<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> futures_util::future::BoxFuture<'a, Result<PermissionRequestDecision, HookError>> {
        Box::pin(async move {
            if self.permission_request.is_empty() {
                return Ok(PermissionRequestDecision::NoDecision);
            }
            // One run per command string, like the other events (ADR 0045 slice B).
            let mut seen = std::collections::HashSet::new();
            let mut allowed = false;
            let mut session_grant = false;
            for entry in &self.permission_request {
                if !Self::matches(&entry.matcher, &call.name) {
                    continue;
                }
                for hook in &entry.hooks {
                    if !seen.insert(dedup_key(hook)) {
                        continue;
                    }
                    match self
                        .run_permission_request_hook(hook, &call.name, &call.arguments)
                        .await?
                    {
                        PermissionVerdict::NoDecision => {}
                        PermissionVerdict::Allow {
                            session_grant: granted,
                        } => {
                            allowed = true;
                            // Items count only when the final decision is an allow; a
                            // later `deny` returns before this ever leaves the chain.
                            session_grant |= granted;
                        }
                        // A deny only narrows: the rest of the chain never runs.
                        PermissionVerdict::Deny(reason) => {
                            return Ok(PermissionRequestDecision::Deny { reason });
                        }
                    }
                }
            }
            Ok(if allowed {
                PermissionRequestDecision::Allow { session_grant }
            } else {
                PermissionRequestDecision::NoDecision
            })
        })
    }
}

impl Hooks for CommandHooks {
    async fn before_tool_call(
        &self,
        _cx: &HookContext,
        call: &ToolCall,
    ) -> Result<ToolDecision, HookError> {
        if self.pre.is_empty() {
            return Ok(ToolDecision::Allow);
        }
        let mut input = call.arguments.clone();
        let mut changed = false;
        // One run per command string: both layers may name the same hook, and the
        // second naming is skipped (ADR 0045 slice B).
        let mut seen = std::collections::HashSet::new();
        for entry in &self.pre {
            if !Self::matches(&entry.matcher, &call.name) {
                continue;
            }
            for hook in &entry.hooks {
                if !seen.insert(dedup_key(hook)) {
                    continue;
                }
                match self.run_pre_hook(hook, &call.name, &input).await? {
                    PreVerdict::Continue => {}
                    PreVerdict::Updated(next) => {
                        input = next;
                        changed = true;
                    }
                    PreVerdict::Deny(reason) => return Ok(ToolDecision::Deny { reason }),
                }
            }
        }
        if changed {
            // Re-validated by Core; hooks are not re-run (ADR 0012 P3).
            Ok(ToolDecision::Rewrite { arguments: input })
        } else {
            Ok(ToolDecision::Allow)
        }
    }

    async fn after_tool_call(
        &self,
        _cx: &HookContext,
        call: &ToolCall,
        result: &mut ToolResult,
    ) -> Result<(), HookError> {
        if self.post.is_empty() {
            return Ok(());
        }
        // One run per command string, like `PreToolUse` (ADR 0045 slice B).
        let mut seen = std::collections::HashSet::new();
        for entry in &self.post {
            if !Self::matches(&entry.matcher, &call.name) {
                continue;
            }
            for hook in &entry.hooks {
                if !seen.insert(dedup_key(hook)) {
                    continue;
                }
                let stdin = serde_json::json!({
                    "hook_event_name": "PostToolUse",
                    "tool_name": call.name,
                    "tool_input": call.arguments,
                    "tool_response": serde_json::to_value(&result).unwrap_or(serde_json::Value::Null),
                    "cwd": self.workspace.to_string_lossy(),
                    "session_id": self.session_id,
                });
                // An async hook starts the same way and the result continues at once
                // (ADR 0045 §5): its output is delivered through `inject`/`followup` later.
                if hook.is_async() {
                    self.spawn_async(hook, stdin, "PostToolUse", Some(&call.name));
                    continue;
                }
                let output = self
                    .execute(hook, &stdin, "PostToolUse", Some(&call.name), None)
                    .await?;
                match output.code {
                    Some(0) => {
                        if let Some(parsed) = self.parsed_output(&output.stdout, "PostToolUse") {
                            self.apply_post_output(parsed, result)?;
                        }
                    }
                    // The reason is fed back to the model with the result: the tool already
                    // ran, so there is nothing to deny — the feedback rides along instead.
                    Some(2) => {
                        let reason = short(&output.stderr);
                        let reason = if reason.trim().is_empty() {
                            "the PostToolUse hook reported an error with no message".to_owned()
                        } else {
                            reason
                        };
                        result
                            .content
                            .push(ContentPart::text(format!("[PostToolUse hook] {reason}")));
                        result.is_error = true;
                    }
                    _ => {
                        return Err(HookError::new(format!(
                            "the PostToolUse hook exited with code {}: {}",
                            output
                                .code
                                .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                            short(&output.stderr),
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// A completed turn runs the Stop hooks once, after `RunFinished`, plus the built-in
    /// intent-stop validator (which runs even with no `Stop` hook configured); any other
    /// terminal status (cancelled, failed, budget, incomplete, refused, poisoned) runs
    /// nothing. A hook asking for more (exit 2) opens another turn through the inbox. The
    /// work spawns onto the session runtime so this sync callback never blocks a turn.
    fn on_turn_end(&self, _cx: &HookContext, outcome: &RunOutcome) {
        // Read the turn's provenance here, synchronously, at its own boundary: the value
        // belongs to this turn, so a `Stop` run for an earlier turn (still in flight) can
        // never consume it and a later claim can never overwrite it (ADR 0045 §2).
        let continuation = self.take_turn_continuation();
        if !matches!(outcome.status, RunStatus::Completed) {
            // A non-completed turn breaks the validator's chain (like a rule miss) and the
            // hook idle-continuation streak (the same reset point), ends any `Stop`
            // continuation it was opened by, and no Stop run will come for this turn: drop
            // the turn-start count.
            self.consecutive.store(0, Ordering::SeqCst);
            self.hook_idle.store(0, Ordering::SeqCst);
            // Saturating: sessions that never called `note_turn_started` (only the
            // hosts that read `pending_stops` wire it) must not wrap the counter.
            self.pending
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    Some(n.saturating_sub(1))
                })
                .ok();
            self.pending_signal.notify_one();
            return;
        }
        // The turn-start count covers the spawned run; it drops when the run ends.
        let this = self.clone();
        let outcome = outcome.clone();
        self.runtime.spawn(async move {
            this.run_stop_once_with(&outcome, continuation).await;
            // Paired with `note_turn_started`; saturating for the same reason.
            this.pending
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    Some(n.saturating_sub(1))
                })
                .ok();
            this.pending_signal.notify_one();
        });
    }
}
