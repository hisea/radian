//! `/context` with a `<goal>` block (fix 1): the goal block folds into the
//! `system prompt` category, and the category sum still equals the estimator
//! total byte for byte with a goal set.
//!
//! The goal block is static system-prompt prefix bytes (ADR 0047 keeps it out
//! of the per-turn writes so the prefix stays cacheable), so it belongs to
//! the system prompt — not its own grid cell. The estimate's remainder rule
//! attributes prompt bytes outside the named layers to the base, and the base
//! minus skills is the `system prompt` category: nothing new to split, so the
//! sum cannot drift.

mod common;

use std::path::Path;
use std::sync::Arc;

use radian_core::{NoopSink, TurnOptions};
use radian_host::{
    ApprovalRouting, HostBindings, LocalSession, SessionCapability, SessionFacade, SessionId,
    SessionSetup, build_model, open_session,
};
use radian_permissions::{NoopAudit, Preset, Profile};
use serde_json::{Value, json};

fn scratch(label: &str) -> common::TempDir {
    common::TempDir::named(&format!("radian-ctxgoal-{label}"))
}

/// Opens with a paused goal already in the journal: `GoalSet` then
/// `GoalPaused`, so the fold parks it and the `<goal>` block rides the next
/// request's system prompt.
async fn open_with_paused_goal(label: &str, scratch: &Path, objective: &str) -> Arc<LocalSession> {
    let workspace = scratch.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = scratch.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let journal = radian_session::SessionJournal::create(
        session_dir.join(format!("{label}.jsonl")),
        label,
        radian_session::SessionCreatedData::new(workspace.to_str().unwrap(), json!({})),
    )
    .expect("create the session file");
    let budget = radian_session::GoalState::default_budget();
    journal
        .append_runtime_event(radian_session::RuntimeEvent::GoalSet {
            objective: objective.to_owned(),
            max_turns: budget.max_turns,
            max_duration_secs: budget.max_duration_secs,
            max_tokens: budget.max_tokens,
        })
        .await
        .expect("seed the goal");
    journal
        .append_runtime_event(radian_session::RuntimeEvent::GoalPaused {})
        .await
        .expect("pause the goal");
    drop(journal);
    open_inner(label, scratch, true).await
}

#[allow(clippy::too_many_lines)]
async fn open_inner(label: &str, scratch: &Path, resume: bool) -> Arc<LocalSession> {
    let workspace = scratch.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = scratch.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let slot = radian_runtime::ConfigSlot::new(Value::Null);
    let initial = build_model(radian_host::ModelArgs {
        extra_body: Default::default(),
        provider: radian_host::Provider::OpenAiCompatible,
        base_url: "http://127.0.0.1:1".to_owned(),
        model: "mock".into(),
        api_key: Some("sk-test".into()),
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 1024,
        thinking: radian_host::Thinking::Off,
        reasoning_effort: None,
        reasoning_replay: None,
        max_output_tokens: None,
        chatgpt_oauth: false,
    })
    .unwrap();
    let model = radian_host::SwitchableModel::new(
        initial,
        radian_host::ModelRoute::new("openai-compatible", "mock"),
        Arc::clone(&slot),
        radian_host::RouteContext {
            extra_body: Default::default(),
            providers: std::collections::BTreeMap::new(),
            session_id: label.to_owned(),
            user_headers: Vec::new(),
            include_usage: None,
            max_tokens: 1024,
            thinking: "medium".to_owned(),
            thinking_budget: None,
        },
    );
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new(label),
            session_dir: session_dir.clone(),
            workspace: workspace.clone(),
            coding_root: None,
            model,
            config_slot: slot,
            config_queue: radian_runtime::ConfigQueue::new(),
            budget_for: None,
            price_for: None,
            servers: Vec::new(),
            mcp_config_path: None,
            system_prompt: None,
            context_budget: None,
            window_context: None,
            auto_compact: false,
            compaction_settings: Value::Null,
            compaction_settings_for: None,
            retries: 0,
            sandbox: None,
            tier: Preset::FullAccess,
            profile: Profile::full_access(&workspace).unwrap(),
            access_builder: None,
            parent: None,
            subagent: None,
            memory_root: None,
            skills_extra_paths: Vec::new(),
            skills_builtin: false,
            skills_disabled: Vec::new(),
            tools_allow: None,
            extra_deny_read: Vec::new(),
            skills_allow: None,
            memory_injection: radian_host::memory::InjectionConfig::default(),
            hooks: Default::default(),
            project_hooks: Default::default(),
            trust: radian_host::trust::Setup::untrusted(&workspace, false),
            resume,
            config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: vec![SessionCapability::Observe, SessionCapability::SendMessage],
        },
        HostBindings {
            approvals: ApprovalRouting::Queued,
            shared_queue: None,
            ask_user: false,
            audit: Arc::new(|_journal| {
                Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
            }),
            events: Arc::new(NoopSink),
            projection: Arc::new(radian_host::SessionProjection::new()),
            runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
            driver_stopped: Arc::new(|_| {}),
            before_driver: None,
            memory_warning: Arc::new(|_message: &str| {}),
            hooks_warning: Arc::new(|_message: &str| {}),
            wake_warning: Arc::new(|_message: &str| {}),
            notice_lost_warning: Arc::new(|_message: &str| {}),
            intent_stop_warning: Arc::new(|_message: &str| {}),
            hook_stop_warning: Arc::new(|_message: &str| {}),
        },
    )
    .await
    .expect("open the session");
    Arc::new(session)
}

/// With a paused goal the `<goal>` block rides the system prompt: the report's
/// categories still sum to the total exactly, the goal's bytes land in
/// `system prompt`, and every other category moves by at most the estimator's
/// one rounding token (the token follows the largest fractional part, so a
/// longer system prompt can move it to a different category).
#[tokio::test]
async fn the_goal_block_folds_into_system_prompt_and_sums_exactly() {
    let scratch = scratch("goal-sum");
    let session = open_with_paused_goal("goal-sum", &scratch, "GOAL-OBJECTIVE-7").await;
    let shape = session.next_request_shape().expect("the shape builds");
    let prompt = shape.system_prompt.clone().unwrap_or_default();
    assert!(
        prompt.contains("<goal>") && prompt.contains("GOAL-OBJECTIVE-7"),
        "the paused goal rides the prompt: {prompt}"
    );
    let with_goal = session.context_report().expect("the report builds");
    let sum: u64 = with_goal.categories.iter().map(|c| c.tokens).sum();
    assert_eq!(sum, with_goal.total_tokens, "the sum is the total");

    // Clearing the shared goal removes exactly the block (same session, so
    // every other byte is identical).
    *session.goal().lock().unwrap_or_else(|e| e.into_inner()) = None;
    let shape = session.next_request_shape().expect("the shape builds");
    assert!(
        !shape
            .system_prompt
            .clone()
            .unwrap_or_default()
            .contains("<goal>"),
        "no goal, no block"
    );
    let without_goal = session.context_report().expect("the report builds");
    let plain_sum: u64 = without_goal.categories.iter().map(|c| c.tokens).sum();
    assert_eq!(plain_sum, without_goal.total_tokens);
    assert!(
        with_goal.total_tokens > without_goal.total_tokens,
        "the goal's bytes are counted: {} vs {}",
        with_goal.total_tokens,
        without_goal.total_tokens
    );
    let system = |report: &radian_host::context_estimate::ContextReport| {
        report
            .categories
            .iter()
            .find(|c| c.name == "system prompt")
            .map(|c| c.tokens)
            .unwrap_or(0)
    };
    assert!(
        system(&with_goal) > system(&without_goal),
        "the goal folds into system prompt"
    );
    for category in &without_goal.categories {
        if category.name == "system prompt" {
            continue;
        }
        let other = with_goal
            .categories
            .iter()
            .find(|c| c.name == category.name)
            .map(|c| c.tokens)
            .unwrap_or(u64::MAX);
        // The estimator hands its one rounding token to the largest fractional
        // part: when the system prompt grows, the token can land on a different
        // category, so any other category may move by exactly that token.
        assert!(
            other.abs_diff(category.tokens) <= 1,
            "only the rounding token moves elsewhere: {} ({} vs {})",
            category.name,
            category.tokens,
            other
        );
    }
    session.stop();
    let _ = session.join().await;
    let _ = std::fs::remove_dir_all(&scratch);
}
