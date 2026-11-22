//! ADR 0025 S6, ADR 0047, ADR 0036: the composed hook chain appends the **skills** section, then
//! the `<goal>` block, then the volatile `<session>` facts.
//!
//! This test builds the **real** hooks and calls [`radian_host::session_hooks`] — the one
//! function `open_session` uses — so reordering the hooks inside it fails here.

mod common;

use std::sync::Arc;

use radian_core::{CancellationToken, HookContext, Hooks, Message, ModelRequest};
use radian_host::{Skill, SkillsHooks, session_hooks};
use radian_permissions::{NoopAudit, PermissionHooks, Profile};
use radian_session::{SessionCreatedData, SessionJournal};
use serde_json::json;

fn scratch(label: &str) -> common::TempDir {
    common::TempDir::named(&format!("radian-skills-order-{label}"))
}

fn journal(dir: &std::path::Path) -> Arc<SessionJournal> {
    Arc::new(
        SessionJournal::create(
            dir.join("s.jsonl"),
            "sid",
            SessionCreatedData::new(
                "/work/example",
                json!({"provider": "example", "model": "m"}),
            ),
        )
        .unwrap(),
    )
}

fn context() -> HookContext {
    HookContext {
        cancellation: CancellationToken::new(),
        model_calls: 1,
    }
}

/// The same hook composition `open_session` uses, with one skill and an active goal, so every
/// section is non-empty.
#[tokio::test]
async fn the_production_hook_chain_orders_skills_goal_and_session() {
    let dir = scratch("order");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let journal = journal(&dir);

    // One skill, constructed directly: the section needs only a name, a description and paths.
    let skill_dir = dir.join("skills/greet");
    let skills = vec![Skill {
        name: "greet".into(),
        description: "Say hello.".into(),
        path: skill_dir.join("SKILL.md"),
        dir: skill_dir,
        disable_model_invocation: false,
        source: radian_host::skills::SkillSource::User,
        body: None,
    }];
    let skills_hooks = Arc::new(SkillsHooks::new(&skills));
    // ADR 0047: an active goal, so the block renders and its position is pinned too.
    let share = radian_host::fresh_share();
    *share.lock().unwrap() = Some(radian_session::GoalState::fresh(
        "ship it".into(),
        radian_session::GoalState::default_budget(),
    ));
    let goal = Arc::new(radian_host::GoalHooks::new(share));
    let permissions = Arc::new(PermissionHooks::new(
        Profile::workspace_write(&ws).unwrap(),
        None,
        Arc::new(NoopAudit),
    ));

    // The role hook (ADR 0036) writes layers 0a, 1 and 2 and runs first; an empty
    // project and the built-in role are enough here, because this test is about the order of
    // the sections the other hooks append.
    let role = Arc::new(radian_host::RoleHooks::new(
        "<runtime>\n</runtime>",
        "",
        None,
    ));

    // The volatile `<session>` facts (ADR 0036): appended last.
    let facts = Arc::new(radian_host::prompt::SessionFactsHooks::new(
        &ws, "m-1", None,
    ));

    // The production function, called exactly as `open_session` calls it (with an empty
    // command-hooks layer: this test is about the section order, and `commands`
    // neither writes the prompt nor decides tools when unconfigured).
    let commands = Arc::new(radian_host::CommandHooks::new(
        &Default::default(),
        &Default::default(),
        &ws,
        "test-session",
        radian_sandbox::SandboxSlot::empty(),
        &radian_runtime::Inbox::new(64),
        &tokio::runtime::Handle::current(),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
    ));
    // ADR 0036: the subtree context hook. This test has no tool call, so an empty state is
    // enough; the hook is in the chain to pin its place.
    let context_hooks = Arc::new(radian_host::ContextHooks::new(
        Arc::new(radian_host::SubtreeContext::new(&ws, [])),
        radian_runtime::Inbox::new(64),
        journal,
        Arc::new(|_message: &str| {}),
        true,
    ));
    let hooks = session_hooks(
        role,
        permissions,
        commands,
        skills_hooks,
        goal,
        facts,
        context_hooks,
    );
    let mut request = ModelRequest {
        system_prompt: None,
        messages: vec![Message::user("what style?")],
        tools: Vec::new(),
        settings: json!({}),
    };
    hooks
        .before_model_call(&context(), &mut request)
        .await
        .unwrap();
    let prompt = request.system_prompt.expect("the hooks set the prompt");

    let skills_at = prompt
        .find("## Skills (ADR 0025)")
        .unwrap_or_else(|| panic!("no skills section in {prompt}"));
    assert!(
        prompt.find("<role>").unwrap() < skills_at,
        "the skills section must come after the base prompt:\n{prompt}"
    );
    assert!(prompt.contains("Say hello."), "{prompt}");
    // The `<session>` layer is last, after skills (ADR 0036: the static prefix stays
    // cacheable).
    let session_at = prompt
        .find("<session>")
        .unwrap_or_else(|| panic!("no session layer in {prompt}"));
    assert!(
        skills_at < session_at,
        "the <session> layer must come after the skills section:\n{prompt}"
    );
    // ADR 0047: the goal block rides after skills and before the session facts.
    let goal_at = prompt
        .find("<goal>")
        .unwrap_or_else(|| panic!("no goal block in {prompt}"));
    assert!(
        skills_at < goal_at && goal_at < session_at,
        "the <goal> block must sit between skills and <session>:\n{prompt}"
    );
    assert!(prompt.trim_end().ends_with("</session>"), "{prompt}");
}
