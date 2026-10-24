//! ADR 0045 slice A: the production hook chain runs the user-level command hooks
//! **after** the permissions, and a hook's `"allow"` never bypasses the permission rules.
//!
//! Both tests compose the chain through [`radian_host::session_hooks`] — the one function
//! `open_session` uses — with stub role/memory/skills/facts layers, so a reorder inside it
//! fails here (the unit tests in `hooks.rs` cover each hook behavior on its own).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::future::BoxFuture;
use radian_core::{CancellationToken, HookContext, Hooks, InputSource, ToolCall, ToolDecision};
use radian_host::memory::InjectionConfig;
use radian_host::{CommandHooks, session_hooks};
use radian_permissions::{
    Action, ApprovalError, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, Approver, NoopAudit,
    PermissionHooks, Preset, Profile, ResourceMatcher, Rule, ToolMatcher,
};
use radian_session::{SessionCreatedData, SessionJournal};
use serde_json::json;

fn scratch(label: &str) -> common::TempDir {
    common::TempDir::canonical(&format!("radian-command-hooks-{label}"))
}

/// A counting approver: every `ask` gets the scripted answer.
struct ScriptedApprover {
    calls: AtomicUsize,
}

impl ScriptedApprover {
    fn denying() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Approver for ScriptedApprover {
    fn decide(
        &self,
        _request: ApprovalRequest,
    ) -> BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(ApprovalOutcome::Deny { reason: None }) })
    }
}

fn context() -> HookContext {
    HookContext {
        cancellation: CancellationToken::new(),
        model_calls: 0,
    }
}

fn shell_call() -> ToolCall {
    ToolCall {
        id: "call-shell".into(),
        name: "shell".into(),
        arguments: json!({"command": "git push"}),
    }
}

fn command_hooks(
    dir: &std::path::Path,
    ws: &std::path::Path,
    pre: Vec<radian_host::config::HookMatcher>,
) -> Arc<CommandHooks> {
    let _ = dir;
    let config = radian_host::config::HooksConfig {
        pre_tool_use: pre,
        ..Default::default()
    };
    Arc::new(CommandHooks::new(
        &config,
        &radian_host::config::HooksConfig::default(),
        ws,
        "test-session",
        radian_sandbox::SandboxSlot::empty(),
        &radian_runtime::Inbox::new(64),
        &tokio::runtime::Handle::current(),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
    ))
}

/// The production chain with stub prompt layers and the given permission/command layers.
fn production_chain(
    ws: &std::path::Path,
    permissions: Arc<PermissionHooks>,
    commands: Arc<CommandHooks>,
) -> impl Hooks {
    let journal = Arc::new(
        SessionJournal::create(
            ws.join("journal.jsonl"),
            "sid",
            SessionCreatedData::new("/work/example", json!({"provider": "example"})),
        )
        .unwrap(),
    );
    let role = Arc::new(radian_host::RoleHooks::new("", "", None));
    let memory = Arc::new(radian_host::memory::MemoryHooks::new(
        None,
        journal,
        InjectionConfig::default(),
        Arc::new(|_message: &str| {}),
    ));
    let skills = Arc::new(radian_host::SkillsHooks::new(&[]));
    // ADR 0047: the goal block rides between skills and the session facts; empty here, so
    // it writes nothing and the permission/command order this test pins is unaffected.
    let goal = Arc::new(radian_host::GoalHooks::new(radian_host::fresh_share()));
    let facts = Arc::new(radian_host::prompt::SessionFactsHooks::new(ws, "m-1", None));
    session_hooks(role, permissions, commands, memory, skills, goal, facts)
}

/// A denied call never reaches the command hooks: the hook's marker stays absent and the
/// verdict is the permission layer's `Deny`. Moving `commands` before `permissions` runs
/// the hook first, so the marker appears and this fails.
#[tokio::test]
async fn permissions_deny_short_circuits_before_command_hooks_run() {
    let ws = scratch("deny-first");
    let dir = ws.parent().unwrap().to_path_buf();
    let marker = dir.join("hook-ran");
    let profile = Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::OnAsk,
        &ws,
        vec![Rule::new(
            ToolMatcher::Name("shell".into()),
            ResourceMatcher::Any,
            Action::Deny,
        )],
    )
    .unwrap();
    let permissions = Arc::new(PermissionHooks::new(profile, None, Arc::new(NoopAudit)));
    permissions.set_turn_source(Some(InputSource::User));
    let commands = command_hooks(
        &dir,
        &ws,
        vec![radian_host::config::HookMatcher {
            matcher: Some("shell".into()),
            hooks: vec![radian_host::config::HookCommand {
                command: format!("touch {}", marker.display()),
                timeout_secs: None,
            }],
        }],
    );
    let chain = production_chain(&ws, permissions, commands);
    let decision = chain
        .before_tool_call(&context(), &shell_call())
        .await
        .unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "the permission denial stands: {decision:?}"
    );
    assert!(!marker.exists(), "a denied call must not run the hook");
}

/// A hook's `permissionDecision: "allow"` is refused: the call still follows the permission
/// rules, so a would-ask call still asks (and here is denied by the scripted approver).
/// Accepting the `allow` — by mapping it to a decision or by running hooks before the
/// rules — skips the approver, so `calls() == 0` and this fails.
#[tokio::test]
async fn allow_output_does_not_bypass_the_permission_rules() {
    let ws = scratch("allow-refused");
    let dir = ws.parent().unwrap().to_path_buf();
    // No rules: every call asks (the conservative default).
    let profile = Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::OnAsk,
        &ws,
        Vec::new(),
    )
    .unwrap();
    let approver = ScriptedApprover::denying();
    let permissions = Arc::new(PermissionHooks::new(
        profile,
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::new(NoopAudit),
    ));
    permissions.set_turn_source(Some(InputSource::User));
    let script = r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"allow"}}'"#;
    let commands = command_hooks(
        &dir,
        &ws,
        vec![radian_host::config::HookMatcher {
            matcher: Some("shell".into()),
            hooks: vec![radian_host::config::HookCommand {
                command: script.into(),
                timeout_secs: None,
            }],
        }],
    );
    let chain = production_chain(&ws, permissions, commands);
    let decision = chain
        .before_tool_call(&context(), &shell_call())
        .await
        .unwrap();
    assert_eq!(
        approver.calls(),
        1,
        "the allow output must not skip the permission rules"
    );
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "the approver's denial stands: {decision:?}"
    );
}
