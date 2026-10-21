//! ADR 0025 S6: the composed hook chain appends the **memory** section before the
//! **skills** section.
//!
//! `the_memory_section_precedes_the_skills_section` in `skills/mod.rs` composes its own hooks,
//! so it does not prove the production order. This test builds the three **real** hooks and
//! calls [`radian_host::session_hooks`] — the one function `open_session` uses — so swapping
//! `memory` and `skills` inside it fails here.

use std::sync::Arc;

use agent_core::{CancellationToken, HookContext, Hooks, Message, ModelRequest};
use radian_host::memory::{InjectionConfig, MemoryHooks, MemoryService, WriteScope};
use radian_host::{Skill, SkillsHooks, session_hooks};
use radian_memory::Kind;
use radian_permissions::{NoopAudit, PermissionHooks, Profile};
use radian_session::{SessionCreatedData, SessionJournal};
use serde_json::json;

fn scratch(label: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "radian-skills-order-{}-{label}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

/// The same three-hook composition `open_session` uses, with a memory record and one skill, so
/// both sections are non-empty.
#[tokio::test]
async fn the_production_hook_chain_puts_memory_before_skills() {
    let dir = scratch("order");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    // A real memory service with one record: the section only renders when it has one.
    let service = MemoryService::open(&ws, dir.join("memory"));
    service.create_project().unwrap();
    service
        .add(
            WriteScope::Project,
            Kind::Preference,
            Some("style".into()),
            "tabs over spaces".into(),
        )
        .unwrap();
    let memory = Arc::new(MemoryHooks::new(
        Some(service),
        journal(&dir),
        InjectionConfig::default(),
        Arc::new(|_message: &str| {}),
    ));

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

    // The volatile `<session>` facts (ADR 0036 D1): appended last.
    let facts = Arc::new(radian_host::prompt::SessionFactsHooks::new(
        &ws, "m-1", None,
    ));

    // The production function, called exactly as `open_session` calls it (with an empty
    // command-hooks layer: this test is about the memory/skills order, and `commands`
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
    ));
    let hooks = session_hooks(role, permissions, commands, memory, skills_hooks, facts);
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

    let memory_at = prompt
        .find("## Memory (ADR 0021)")
        .unwrap_or_else(|| panic!("no memory section in {prompt}"));
    let skills_at = prompt
        .find("## Skills (ADR 0025)")
        .unwrap_or_else(|| panic!("no skills section in {prompt}"));
    assert!(
        memory_at < skills_at,
        "the memory section must come before the skills section:\n{prompt}"
    );
    assert!(prompt.contains("tabs over spaces"), "{prompt}");
    assert!(prompt.contains("Say hello."), "{prompt}");
    // The `<session>` layer is last, after skills (ADR 0036 D1: the static prefix stays
    // cacheable).
    let session_at = prompt
        .find("<session>")
        .unwrap_or_else(|| panic!("no session layer in {prompt}"));
    assert!(
        skills_at < session_at,
        "the <session> layer must come after the skills section:\n{prompt}"
    );
    assert!(prompt.trim_end().ends_with("</session>"), "{prompt}");
}
