//! The persistent goal (ADR 0047): the `goal` tool and the `<goal>` prompt block.
//!
//! Both share the session's [`SharedGoal`] (`Arc<Mutex<Option<GoalState>>>`, folded once
//! from the journal at open): the tool is the model's write end (`progress`/`done`/`wait`),
//! the hook is the deterministic read end (the context-area block on every request). Every
//! change is journaled first and applied to the shared state second, so the state always
//! matches what [`radian_session::fold`] rebuilds.

use std::sync::{Arc, Mutex};

use agent_core::{
    HookContext, HookError, Hooks, ModelRequest, ToolCall, ToolConcurrency, ToolContext, ToolError,
    ToolFailure, ToolOutput, ToolProvider, ToolSession, ToolSpec,
};
use radian_session::{GoalState, GoalStatus, RuntimeEvent, SessionJournal};
use serde_json::{Value, json};

/// The session's shared goal: `None` when no goal exists (never set, or dropped).
pub type SharedGoal = Arc<Mutex<Option<GoalState>>>;

/// Folds a fresh share from already-open events (`None` for a new session).
pub fn fresh_share() -> SharedGoal {
    Arc::new(Mutex::new(None))
}

/// The `goal` tool's `wait` delay is clamped to this (ADR 0047: at most one hour).
pub const MAX_WAIT_SECS: u64 = 3600;

/// The `goal` spec: the model's write end of the persistent goal (ADR 0047). Its serialized
/// size follows the `job` rule (ADR 0037 decision 4): one sentence per rule, and no fact the
/// schema already states is repeated here.
pub(crate) fn goal_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "op": {
                "type": "string",
                "enum": ["progress", "done", "wait"],
                "description": "`progress` records a note. `done` ends the goal. `wait` delays the next goal message."
            },
            "note": {
                "type": "string",
                "description": "The progress or completion note."
            },
            "secs": {
                "type": "integer",
                "minimum": 1,
                "description": "Seconds `wait` delays the next goal message. Maximum: 3600."
            }
        },
        "required": ["op"],
        "additionalProperties": false
    })
}

pub(crate) fn goal_spec() -> ToolSpec {
    ToolSpec {
        name: "goal".into(),
        description: "Record goal progress, finish the goal, or delay its next step. \
                      `progress` needs `note` and replaces the latest progress. `done` ends \
                      the goal; the turn continues and no further goal message follows. \
                      `wait` needs `secs` and the next goal message waits that long. Outside \
                      an active goal every `op` is a `no_active_goal` error."
            .into(),
        input_schema: goal_schema(),
        concurrency: ToolConcurrency::Shared,
    }
}

#[derive(Clone)]
pub struct GoalTools {
    state: SharedGoal,
    journal: Arc<SessionJournal>,
}

impl GoalTools {
    pub fn new(state: SharedGoal, journal: Arc<SessionJournal>) -> Self {
        Self { state, journal }
    }
}

impl ToolProvider for GoalTools {
    type Session = GoalSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        Ok(GoalSession {
            state: Arc::clone(&self.state),
            journal: Arc::clone(&self.journal),
            specs: vec![goal_spec()],
        })
    }
}

pub struct GoalSession {
    state: SharedGoal,
    journal: Arc<SessionJournal>,
    specs: Vec<ToolSpec>,
}

impl ToolSession for GoalSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        call: &ToolCall,
        _cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        if call.name != "goal" {
            return Err(ToolFailure::new(
                "unknown_tool",
                format!("`{}` is not the goal tool", call.name),
            ));
        }
        let op = call
            .arguments
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolFailure::new(
                    "invalid_arguments",
                    "`op` is required and must be one of `progress`, `done` or `wait`",
                )
            })?;
        let note = match call.arguments.get("note") {
            None | Some(Value::Null) => None,
            Some(Value::String(note)) => Some(note.clone()),
            Some(_) => {
                return Err(ToolFailure::new(
                    "invalid_arguments",
                    "`note`, when given, must be a string",
                ));
            }
        };
        let secs = match call.arguments.get("secs") {
            None | Some(Value::Null) => None,
            Some(Value::Number(secs)) => secs
                .as_u64()
                .or_else(|| secs.as_i64().and_then(|secs| u64::try_from(secs).ok())),
            Some(_) => {
                return Err(ToolFailure::new(
                    "invalid_arguments",
                    "`secs`, when given, must be a positive integer",
                ));
            }
        };
        match op {
            "progress" => {
                let Some(note) = note.filter(|note| !note.trim().is_empty()) else {
                    return Err(ToolFailure::new(
                        "invalid_arguments",
                        "`progress` needs a non-empty `note`",
                    ));
                };
                self.require_active()?;
                self.journal
                    .append_runtime_event(RuntimeEvent::GoalProgress { note: note.clone() })
                    .await
                    .map_err(|error| {
                        ToolFailure::new(
                            "journal_error",
                            format!("the progress note was not recorded: {error}"),
                        )
                    })?;
                if let Some(goal) = self
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                {
                    goal.progress = Some(note.clone());
                }
                Ok(ToolOutput::text(format!("progress recorded: {note}\n")))
            }
            "done" => {
                self.require_active()?;
                self.journal
                    .append_runtime_event(RuntimeEvent::GoalDone { note: note.clone() })
                    .await
                    .map_err(|error| {
                        ToolFailure::new(
                            "journal_error",
                            format!("the goal was not marked done: {error}"),
                        )
                    })?;
                if let Some(goal) = self
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                {
                    if note.is_some() {
                        goal.progress = note.clone();
                    }
                    goal.status = GoalStatus::Done;
                }
                Ok(ToolOutput::text(
                    "goal done: no further goal message follows\n",
                ))
            }
            "wait" => {
                let Some(secs) = secs.filter(|secs| *secs > 0) else {
                    return Err(ToolFailure::new(
                        "invalid_arguments",
                        "`wait` needs a positive integer `secs`",
                    ));
                };
                let secs = secs.min(MAX_WAIT_SECS);
                self.require_active()?;
                self.journal
                    .append_runtime_event(RuntimeEvent::GoalWait { secs })
                    .await
                    .map_err(|error| {
                        ToolFailure::new(
                            "journal_error",
                            format!("the wait was not recorded: {error}"),
                        )
                    })?;
                if let Some(goal) = self
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                {
                    goal.pending_wait_secs = Some(secs);
                }
                Ok(ToolOutput::text(format!(
                    "the next goal message waits {secs}s\n"
                )))
            }
            other => Err(ToolFailure::new(
                "invalid_arguments",
                format!("unknown `op` {other:?}: use `progress`, `done` or `wait`"),
            )),
        }
    }
}

impl GoalSession {
    /// The goal must be running for the model to write it; anything else is the
    /// `no_active_goal` tool error (ADR 0047). The message names the state so a paused
    /// goal reads differently from no goal at all.
    fn require_active(&self) -> Result<(), ToolFailure> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.as_ref().map(|goal| goal.status) {
            Some(GoalStatus::Active) => Ok(()),
            Some(GoalStatus::Paused) => Err(ToolFailure::new(
                "no_active_goal",
                "the goal is paused; resume it with `/goal resume` before recording",
            )),
            Some(GoalStatus::Done) => Err(ToolFailure::new(
                "no_active_goal",
                "the goal is already done; set a new one with `/goal <objective>`",
            )),
            Some(GoalStatus::BudgetExhausted) => Err(ToolFailure::new(
                "no_active_goal",
                "the goal's budget is exhausted; raise it with `/goal budget …` or set a new goal",
            )),
            None => Err(ToolFailure::new(
                "no_active_goal",
                "there is no active goal; set one with `/goal <objective>`",
            )),
        }
    }
}

/// The `<goal>` block for one goal, or `None` when no block belongs on the request (no
/// goal, or a finished one). The block carries the objective only: never the latest
/// progress nor the budget use. The system prompt is the front of
/// the cache prefix, and anything that changes every turn there invalidates the whole
/// prefix. The latest progress rides the turn-end continuation message instead, rebuilt
/// from the durable records each time at the end of the request.
pub fn goal_block(goal: &GoalState) -> Option<String> {
    if !matches!(goal.status, GoalStatus::Active | GoalStatus::Paused) {
        return None;
    }
    let mut out = String::from("<goal>\n");
    out.push_str(&format!("Objective: {}\n", goal.objective));
    out.push_str(
        "When the objective is met, call the `goal` tool with `op: \"done\"`; \
         use `op: \"progress\"` to record what changed.\n</goal>",
    );
    Some(out)
}

/// The hook that appends the `<goal>` block **after** skills and memory (ADR 0047, the
/// context area of ADR 0036) and before the volatile `<session>` facts, so the static
/// prefix stays cacheable. While the goal is unchanged the block bytes do not change;
/// compaction never sees it (it only consumes transcript messages), so it can neither
/// summarize the goal away nor drop it.
pub struct GoalHooks {
    state: SharedGoal,
}

impl GoalHooks {
    pub fn new(state: SharedGoal) -> Self {
        Self { state }
    }
}

impl Hooks for GoalHooks {
    async fn before_model_call(
        &self,
        _cx: &HookContext,
        request: &mut ModelRequest,
    ) -> Result<(), HookError> {
        let block = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(goal_block);
        let Some(block) = block else {
            return Ok(());
        };
        match &mut request.system_prompt {
            Some(system) => {
                system.push('\n');
                system.push_str(&block);
            }
            None => request.system_prompt = Some(block),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{CancellationToken, ContentPart, Hooks, ToolCall};
    use serde_json::json;

    /// A share holding one active goal, for the tool tests below.
    fn active_share() -> SharedGoal {
        let mut goal = GoalState::fresh("ship it".into(), GoalState::default_budget());
        goal.turns_used = 3;
        goal.active_secs = 90;
        goal.tokens_used = 150;
        Arc::new(Mutex::new(Some(goal)))
    }

    fn call(op: &str, note: Option<&str>, secs: Option<u64>) -> ToolCall {
        let mut arguments = json!({"op": op});
        if let Some(note) = note {
            arguments["note"] = json!(note);
        }
        if let Some(secs) = secs {
            arguments["secs"] = json!(secs);
        }
        ToolCall {
            id: "call-1".into(),
            name: "goal".into(),
            arguments,
        }
    }

    fn cx<'a>(cancellation: &'a CancellationToken) -> ToolContext<'a> {
        ToolContext {
            cancellation: cancellation.clone(),
            events: &agent_core::NoopSink,
            index: 0,
        }
    }

    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    /// A live journal in a unique scratch directory (removed on drop), for the tool tests.
    struct TempJournal {
        _dir: std::path::PathBuf,
        journal: Arc<SessionJournal>,
    }

    impl TempJournal {
        fn new(label: &str) -> Self {
            let n = N.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir()
                .join(format!("radian-goal-{}-{label}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let journal = SessionJournal::create(
                dir.join("s.jsonl"),
                "sid",
                radian_session::SessionCreatedData::new(
                    "/work/example",
                    serde_json::json!({"model": {"provider": "example", "model": "m-1"}}),
                ),
            )
            .unwrap();
            Self {
                _dir: dir,
                journal: Arc::new(journal),
            }
        }
    }

    impl Drop for TempJournal {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self._dir);
        }
    }

    /// Say-each-fact-once (ADR 0037's rule, like the `skill` tool): no parameter
    /// description repeats a sentence of the `goal` description. Fragments under 24
    /// characters are too short to be a repeated fact.
    #[tokio::test]
    async fn no_parameter_description_repeats_a_sentence_of_the_description() {
        use agent_core::{ToolProvider, ToolSession};
        let temp = TempJournal::new("desc");
        let session = GoalTools::new(fresh_share(), Arc::clone(&temp.journal))
            .open_session()
            .await
            .unwrap();
        let spec = session
            .tool_specs()
            .iter()
            .find(|spec| spec.name == "goal")
            .expect("the goal spec");
        let sentences: Vec<&str> = spec
            .description
            .split(". ")
            .map(str::trim)
            .filter(|sentence| sentence.len() >= 24)
            .collect();
        let properties = spec.input_schema["properties"]
            .as_object()
            .expect("the goal properties");
        for sentence in sentences {
            for (name, property) in properties {
                if let Some(text) = property["description"].as_str() {
                    assert!(
                        !text.contains(sentence),
                        "the `{name}` field repeats {sentence:?}: {text:?}"
                    );
                }
            }
        }
    }

    /// `progress` journals first and replaces the latest note for the continuation
    /// message; the `<goal>` block bytes do not change.
    #[tokio::test]
    async fn progress_records_a_note_and_leaves_the_block_bytes_unchanged() {
        let temp = TempJournal::new("progress");
        let share = active_share();
        let before = goal_block(
            share
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .expect("still a goal"),
        )
        .expect("a block");
        let session = GoalTools::new(Arc::clone(&share), Arc::clone(&temp.journal))
            .open_session()
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let output = session
            .execute(&call("progress", Some("half there"), None), cx(&cancel))
            .await
            .unwrap();
        assert!(output.content.iter().any(|part| match part {
            ContentPart::Text { text, .. } => text.contains("half there"),
            _ => false,
        }));
        let guard = share.lock().unwrap_or_else(|e| e.into_inner());
        let goal = guard.as_ref().expect("still a goal");
        assert_eq!(goal.progress.as_deref(), Some("half there"));
        let after = goal_block(goal).expect("a block");
        assert_eq!(before, after, "a progress note must not touch the block");
        assert!(!after.contains("half there"), "{after}");
        assert!(!after.contains("Progress:"), "{after}");
        assert!(!after.contains("Budget:"), "{after}");
    }

    /// `done` ends the goal: the block goes quiet and the tool refuses afterwards.
    #[tokio::test]
    async fn done_ends_the_goal_and_later_calls_fail() {
        let temp = TempJournal::new("done");
        let share = active_share();
        let session = GoalTools::new(Arc::clone(&share), Arc::clone(&temp.journal))
            .open_session()
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        session
            .execute(&call("done", Some("shipped"), None), cx(&cancel))
            .await
            .unwrap();
        {
            let state = share.lock().unwrap_or_else(|e| e.into_inner());
            let goal = state.as_ref().expect("the goal row stays for status");
            assert_eq!(goal.status, GoalStatus::Done);
            assert!(goal_block(goal).is_none(), "no block after done");
        }
        let failure = session
            .execute(&call("progress", Some("more"), None), cx(&cancel))
            .await
            .unwrap_err();
        assert_eq!(failure.code, "no_active_goal", "{failure:?}");
    }

    /// `wait` arms the shared delay (the TUI's turn end consumes it) and clamps at one hour.
    #[tokio::test]
    async fn wait_arms_a_clamped_delay() {
        let temp = TempJournal::new("wait");
        let share = active_share();
        let session = GoalTools::new(Arc::clone(&share), Arc::clone(&temp.journal))
            .open_session()
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        session
            .execute(&call("wait", None, Some(600)), cx(&cancel))
            .await
            .unwrap();
        assert_eq!(
            share
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .expect("a goal")
                .pending_wait_secs,
            Some(600)
        );
        session
            .execute(&call("wait", None, Some(99999)), cx(&cancel))
            .await
            .unwrap();
        assert_eq!(
            share
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .expect("a goal")
                .pending_wait_secs,
            Some(MAX_WAIT_SECS)
        );
    }

    /// Outside a goal every op is the same `no_active_goal` error; bad shapes are
    /// `invalid_arguments`, never a panic.
    #[tokio::test]
    async fn without_a_goal_every_op_is_no_active_goal() {
        let temp = TempJournal::new("nogoal");
        let session = GoalTools::new(fresh_share(), Arc::clone(&temp.journal))
            .open_session()
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        for op in ["progress", "done", "wait"] {
            let failure = session
                .execute(&call(op, Some("x"), Some(5)), cx(&cancel))
                .await
                .unwrap_err();
            assert_eq!(failure.code, "no_active_goal", "{op}: {failure:?}");
        }
        let failure = session
            .execute(&call("nap", None, None), cx(&cancel))
            .await
            .unwrap_err();
        assert_eq!(failure.code, "invalid_arguments", "{failure:?}");
        let failure = session
            .execute(&call("progress", None, None), cx(&cancel))
            .await
            .unwrap_err();
        assert_eq!(failure.code, "invalid_arguments", "{failure:?}");
        let failure = session
            .execute(&call("wait", None, None), cx(&cancel))
            .await
            .unwrap_err();
        assert_eq!(failure.code, "invalid_arguments", "{failure:?}");
    }

    #[test]
    fn the_block_carries_only_the_objective() {
        let mut goal = GoalState::fresh("ship it".into(), GoalState::default_budget());
        goal.progress = Some("half there".into());
        goal.turns_used = 3;
        goal.active_secs = 90;
        goal.tokens_used = 150;
        let block = goal_block(&goal).expect("an active goal has a block");
        assert!(block.starts_with("<goal>"), "{block}");
        assert!(block.contains("Objective: ship it"), "{block}");
        assert!(block.contains("op: \"done\""), "{block}");
        assert!(!block.contains("half there"), "{block}");
        assert!(!block.contains("Progress:"), "{block}");
        assert!(!block.contains("Budget:"), "{block}");
        assert!(!block.contains("Status:"), "{block}");
        assert!(block.ends_with("</goal>"), "{block}");
        // Paused keeps the very same bytes: nothing in the block moves but the objective.
        let mut paused = goal.clone();
        paused.status = GoalStatus::Paused;
        assert_eq!(goal_block(&paused).as_deref(), Some(block.as_str()));
        // A finished goal has no block: the transcript already says it ended.
        let mut done = goal.clone();
        done.status = GoalStatus::Done;
        assert!(done.status == GoalStatus::Done && goal_block(&done).is_none());
    }

    /// The hook appends the block after the base prompt, and writes nothing without a goal.
    #[tokio::test]
    async fn the_hook_appends_the_block_and_stays_quiet_without_a_goal() {
        let hooks = GoalHooks::new(active_share());
        let mut request = ModelRequest {
            system_prompt: Some("base".into()),
            messages: Vec::new(),
            tools: Vec::new(),
            settings: json!({}),
        };
        hooks
            .before_model_call(
                &HookContext {
                    cancellation: CancellationToken::new(),
                    model_calls: 1,
                },
                &mut request,
            )
            .await
            .unwrap();
        let prompt = request.system_prompt.unwrap();
        assert!(prompt.starts_with("base"), "{prompt}");
        assert!(prompt.contains("<goal>"), "{prompt}");
        assert!(prompt.contains("ship it"), "{prompt}");

        let hooks = GoalHooks::new(fresh_share());
        let mut request = ModelRequest {
            system_prompt: Some("base".into()),
            messages: Vec::new(),
            tools: Vec::new(),
            settings: json!({}),
        };
        hooks
            .before_model_call(
                &HookContext {
                    cancellation: CancellationToken::new(),
                    model_calls: 1,
                },
                &mut request,
            )
            .await
            .unwrap();
        assert_eq!(request.system_prompt.unwrap(), "base");
    }

    /// Renders the full system prompt the hook produces for one shared goal.
    async fn rendered_prompt(share: &SharedGoal) -> String {
        let hooks = GoalHooks::new(Arc::clone(share));
        let mut request = ModelRequest {
            system_prompt: Some("base".into()),
            messages: Vec::new(),
            tools: Vec::new(),
            settings: json!({}),
        };
        hooks
            .before_model_call(
                &HookContext {
                    cancellation: CancellationToken::new(),
                    model_calls: 1,
                },
                &mut request,
            )
            .await
            .unwrap();
        request.system_prompt.unwrap()
    }

    /// Acceptance: after `goal progress` the system prompt bytes are
    /// unchanged \u2014 the block holds the objective only \u2014 while the shared state
    /// carries the new progress for the turn-end continuation message (the TUI half,
    /// pinned by `an_unfinished_turn_continues_with_the_latest_progress`).
    /// Mutation: put the progress back into the block and this fails.
    #[tokio::test]
    async fn progress_leaves_the_system_prompt_bytes_unchanged() {
        let temp = TempJournal::new("stable");
        let share = active_share();
        let before = rendered_prompt(&share).await;
        assert!(before.contains("Objective: ship it"), "{before}");
        let session = GoalTools::new(Arc::clone(&share), Arc::clone(&temp.journal))
            .open_session()
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        session
            .execute(&call("progress", Some("half there"), None), cx(&cancel))
            .await
            .unwrap();
        let after = rendered_prompt(&share).await;
        assert_eq!(
            before, after,
            "a progress note must not touch the system prompt"
        );
        assert!(!after.contains("half there"), "{after}");
    }
}
