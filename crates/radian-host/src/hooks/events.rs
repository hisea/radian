//! Interpreting each deciding event output: `PreToolUse`, `PostToolUse`, `PermissionRequest`.

use super::*;

impl CommandHooks {
    /// One `PreToolUse` hook's verdict. `Err` is a hook error (fail closed: Core denies the
    /// call with `hook_error`).
    pub(super) async fn run_pre_hook(
        &self,
        hook: &HookCommand,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<PreVerdict, HookError> {
        let stdin = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": tool,
            "tool_input": input,
            "cwd": self.workspace.to_string_lossy(),
            "session_id": self.session_id,
        });
        let output = self
            .execute(hook, &stdin, "PreToolUse", Some(tool), None)
            .await?;
        match output.code {
            Some(2) => {
                let reason = short(&output.stderr);
                Ok(PreVerdict::Deny(if reason.trim().is_empty() {
                    "the hook blocked the call with no message".to_owned()
                } else {
                    reason
                }))
            }
            Some(0) => {
                let Some(parsed) = self.parsed_output(&output.stdout, "PreToolUse") else {
                    return Ok(PreVerdict::Continue);
                };
                let specific = parsed.hookSpecificOutput.unwrap_or_default();
                match specific.permissionDecision.as_deref() {
                    Some("deny") => {
                        let reason = specific
                            .reason
                            .or(parsed.reason)
                            .filter(|reason| !reason.trim().is_empty())
                            .unwrap_or_else(|| "the hook denied the call".to_owned());
                        Ok(PreVerdict::Deny(reason))
                    }
                    // A hook can never widen permissions (ADR 0012 P1, Codex parity): the
                    // decision is refused with a warning and treated as no decision. The
                    // call still follows the permission rules below.
                    Some("allow") | Some("ask") => {
                        let decision = specific.permissionDecision.unwrap_or_default();
                        (self.warn)(&format!(
                            "PreToolUse hook returned permissionDecision \"{decision}\", which is \
                             refused: a hook cannot widen permissions; the call still follows \
                             the permission rules"
                        ));
                        self.updated_input(specific.updatedInput, "PreToolUse")
                    }
                    Some(other) => {
                        (self.warn)(&format!(
                            "PreToolUse hook returned unknown permissionDecision \"{other}\"; ignoring it"
                        ));
                        self.updated_input(specific.updatedInput, "PreToolUse")
                    }
                    None => self.updated_input(specific.updatedInput, "PreToolUse"),
                }
            }
            _ => Err(HookError::new(format!(
                "the PreToolUse hook exited with code {}: {}",
                output
                    .code
                    .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                short(&output.stderr),
            ))),
        }
    }

    /// The `updatedInput` half of an exit-0 verdict: an object replaces the tool input
    /// (threaded into the following hooks), anything else shaped is a hook error.
    pub(super) fn updated_input(
        &self,
        updated: Option<serde_json::Value>,
        event: &str,
    ) -> Result<PreVerdict, HookError> {
        match updated {
            None | Some(serde_json::Value::Null) => Ok(PreVerdict::Continue),
            Some(serde_json::Value::Object(_)) => {
                Ok(PreVerdict::Updated(updated.expect("matched as an object")))
            }
            Some(_) => Err(HookError::new(format!(
                "the {event} hook returned a non-object updatedInput"
            ))),
        }
    }

    /// Applies one exit-0 `PostToolUse` output to the result. An `updatedToolOutput` string
    /// replaces the content first (one text part; `is_error` and `error_code` stay), then a
    /// `decision: "block"` reason is appended as the error feedback, then
    /// `additionalContext` as non-error feedback. A non-string `updatedToolOutput` is a hook
    /// error (fail closed).
    pub(super) fn apply_post_output(
        &self,
        parsed: HookOutput,
        result: &mut ToolResult,
    ) -> Result<(), HookError> {
        let specific = parsed.hookSpecificOutput.unwrap_or_default();
        if specific.permissionDecision.is_some() {
            (self.warn)(
                "PostToolUse hook returned permissionDecision, which only PreToolUse uses; \
                 ignoring it",
            );
        }
        if let Some(updated) = specific.updatedToolOutput {
            match updated {
                Some(serde_json::Value::String(text)) => {
                    result.content = vec![ContentPart::text(short(&text))];
                }
                // A present `null` lands here too: `null` is a value, not an omitted field,
                // and it is not a string.
                _ => {
                    return Err(HookError::new(
                        "the PostToolUse hook returned a non-string updatedToolOutput",
                    ));
                }
            }
        }
        if parsed.decision.as_deref() == Some("block") {
            let reason = specific.reason.or(parsed.reason).unwrap_or_default();
            let reason = if reason.trim().is_empty() {
                "the PostToolUse hook reported an error with no message".to_owned()
            } else {
                short(&reason)
            };
            result
                .content
                .push(ContentPart::text(format!("[PostToolUse hook] {reason}")));
            result.is_error = true;
        }
        if let Some(context) = specific.additionalContext {
            result.content.push(ContentPart::text(format!(
                "[PostToolUse hook] {}",
                short(&context)
            )));
        }
        Ok(())
    }

    /// The reasons one exit-0 `Stop` output contributes to the turn's single continuation:
    /// `decision: "block"` first, then `additionalContext` (ADR 0045 §2). Fields only
    /// `PreToolUse` uses warn once and are ignored.
    pub(super) fn stop_output_reasons(&self, parsed: HookOutput) -> Vec<String> {
        let mut reasons = Vec::new();
        let specific = parsed.hookSpecificOutput.unwrap_or_default();
        if specific.permissionDecision.is_some() || specific.updatedInput.is_some() {
            (self.warn)(
                "Stop hook returned permissionDecision or updatedInput, which only PreToolUse \
                 uses; ignoring them",
            );
        }
        if parsed.decision.as_deref() == Some("block") {
            let reason = specific.reason.or(parsed.reason).unwrap_or_default();
            reasons.push(if reason.trim().is_empty() {
                "the Stop hook requested another turn".to_owned()
            } else {
                short(&reason)
            });
        }
        if let Some(context) = specific.additionalContext {
            reasons.push(short(&context));
        }
        reasons
    }

    /// Parses an exit-0 stdout as JSON per Claude: empty output continues silently, garbage
    /// warns and continues (the hook exited successfully, so there is nothing to fail
    /// closed on).
    pub(super) fn parsed_output(&self, stdout: &str, event: &str) -> Option<HookOutput> {
        if stdout.trim().is_empty() {
            return None;
        }
        match serde_json::from_str::<HookOutput>(stdout) {
            Ok(parsed) => Some(parsed),
            Err(_) => {
                (self.warn)(&format!(
                    "the {event} hook exited 0 but its stdout was not JSON; ignoring it"
                ));
                None
            }
        }
    }

    /// One `PermissionRequest` hook's verdict (ADR 0045 §4). Unlike the other events, an
    /// exit-0 stdout that is not JSON fails closed: a script that was meant to answer the
    /// question and misspoke must not fall through to a prompt.
    ///
    /// `updatedPermissions` is read only for a layer that decides `allow`: an item
    /// `{ "type": "addRules", "destination": "session", "behavior": "allow" }`
    /// asks for a session grant, exactly like the approval prompt's "allow for this
    /// session" (ADR 0045 §4). The item's `rules` are not parsed. Any other `type`,
    /// `destination` or `behavior` warns one line and records nothing, and so does a
    /// present value that is not an array (a JSON `null` included: it is present, so it
    /// is not the same as the field's absence).
    pub(super) fn session_grant_requested(
        &self,
        updated: Option<&Option<serde_json::Value>>,
    ) -> bool {
        let Some(updated) = updated else {
            return false;
        };
        let Some(items) = updated.as_ref().and_then(serde_json::Value::as_array) else {
            (self.warn)(
                "the PermissionRequest hook returned updatedPermissions that is not an \
                 array; ignoring it",
            );
            return false;
        };
        let mut granted = false;
        for item in items {
            let accepted = item.as_object().is_some_and(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some("addRules")
                    && item.get("destination").and_then(serde_json::Value::as_str)
                        == Some("session")
                    && item.get("behavior").and_then(serde_json::Value::as_str) == Some("allow")
            });
            if accepted {
                granted = true;
            } else {
                (self.warn)(
                    "the PermissionRequest hook returned an updatedPermissions item that \
                     is ignored: only { \"type\": \"addRules\", \"destination\": \"session\", \
                     \"behavior\": \"allow\" } is supported",
                );
            }
        }
        granted
    }

    pub(super) async fn run_permission_request_hook(
        &self,
        hook: &HookCommand,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<PermissionVerdict, HookError> {
        let stdin = serde_json::json!({
            "hook_event_name": "PermissionRequest",
            "tool_name": tool,
            "tool_input": input,
            "cwd": self.workspace.to_string_lossy(),
            "session_id": self.session_id,
        });
        let output = self
            .execute(hook, &stdin, "PermissionRequest", Some(tool), None)
            .await?;
        match output.code {
            Some(2) => {
                let reason = short(&output.stderr);
                Ok(PermissionVerdict::Deny(Some(if reason.trim().is_empty() {
                    "the hook blocked the call with no message".to_owned()
                } else {
                    reason
                })))
            }
            Some(0) => {
                if output.stdout.trim().is_empty() {
                    return Ok(PermissionVerdict::NoDecision);
                }
                let parsed: HookOutput = serde_json::from_str(&output.stdout).map_err(|_| {
                    HookError::new(
                        "the PermissionRequest hook exited 0 but its stdout was not JSON; \
                             the call is denied",
                    )
                })?;
                let Some(decision) = parsed
                    .hookSpecificOutput
                    .and_then(|specific| specific.decision)
                else {
                    // No decision: `updatedPermissions` alone contributes nothing (items
                    // count only from a layer that decides `allow`).
                    return Ok(PermissionVerdict::NoDecision);
                };
                if decision.updatedInput.is_some() || decision.interrupt.is_some() {
                    (self.warn)(
                        "PermissionRequest hook returned updatedInput or interrupt, which this \
                         version does not support; ignoring them",
                    );
                }
                match decision.behavior.as_deref() {
                    Some("allow") => Ok(PermissionVerdict::Allow {
                        session_grant: self
                            .session_grant_requested(decision.updatedPermissions.as_ref()),
                    }),
                    // Bounded like stderr: hook output enters the model's context.
                    Some("deny") => Ok(PermissionVerdict::Deny(
                        decision.message.map(|message| short(&message)),
                    )),
                    Some(other) => {
                        (self.warn)(&format!(
                            "PermissionRequest hook returned unknown decision.behavior \
                             \"{other}\"; ignoring it"
                        ));
                        Ok(PermissionVerdict::NoDecision)
                    }
                    None => {
                        (self.warn)(
                            "PermissionRequest hook returned a decision without behavior; \
                             ignoring it",
                        );
                        Ok(PermissionVerdict::NoDecision)
                    }
                }
            }
            _ => Err(HookError::new(format!(
                "the PermissionRequest hook exited with code {}: {}",
                output
                    .code
                    .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                short(&output.stderr),
            ))),
        }
    }
}
