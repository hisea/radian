//! ADR 0045 slice A: the production hook chain runs the user-level command hooks
//! **after** the permissions, and a hook's `"allow"` never bypasses the permission rules.
//!
//! Both tests compose the chain through [`radian_host::session_hooks`] — the one function
//! `open_session` uses — with stub role/memory/skills/facts layers, so a reorder inside it
//! fails here (the unit tests in `hooks.rs` cover each hook behavior on its own).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use std::collections::VecDeque;
use std::sync::Mutex;

use futures_util::future::BoxFuture;
use radian_core::{
    Agent, CancellationToken, ContentPart, HookContext, Hooks, InputSource, Message, Model,
    ModelContext, ModelError, ModelRequest, ModelResponse, NoopSink, StopReason, ToolCall,
    ToolContext, ToolDecision, ToolError, ToolFailure, ToolOutput, ToolProvider, ToolSession,
    ToolSpec, TurnOptions,
};
use radian_host::memory::InjectionConfig;
use radian_host::{CommandHooks, session_hooks};
use radian_permissions::{
    Action, ApprovalError, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, Approver, NoopAudit,
    PermissionHooks, Preset, Profile, ResourceMatcher, Rule, ToolMatcher,
};
use radian_session::{EventData, Session, SessionCreatedData, SessionJournal};
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

/// ADR 0045 §2: `stop_hook_active` belongs to the turn the continuation opened, not to
/// whichever turn's `Stop` happens to reach the cycle next. A `Stop` for an earlier turn
/// that exits 2 while a user's turn is open must not make the user's turn report `true`,
/// and the continuation that verdict queued must still report `true`. A session-global
/// pending flag fails both: the user's turn consumes the value and the continuation sees
/// `false`.
#[tokio::test]
async fn a_late_stop_verdict_belongs_to_the_turn_it_opens() {
    use radian_core::{Message, RunOutcome, RunStatus};

    fn completed(text: &str) -> RunOutcome {
        RunOutcome {
            status: RunStatus::Completed,
            final_message: Some(Message::assistant(text)),
            unapplied_steering: Vec::new(),
            model_calls: 0,
        }
    }

    async fn wait_for(path: &std::path::Path) {
        for _ in 0..600 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("{} never appeared", path.display());
    }

    async fn wait_for_json(path: &std::path::Path) -> serde_json::Value {
        wait_for(path).await;
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Releases a blocked `Stop` hook on the failure path (see the unit test's copy).
    struct Release {
        path: std::path::PathBuf,
        armed: bool,
    }

    impl Release {
        fn new(path: std::path::PathBuf) -> Self {
            Self { path, armed: true }
        }

        fn disarm(&mut self) {
            self.armed = false;
        }
    }

    impl Drop for Release {
        fn drop(&mut self) {
            if self.armed {
                let _ = std::fs::write(&self.path, b"go");
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }

    let ws = scratch("late-verdict");
    let a_stdin = ws.join("a.json");
    let b_stdin = ws.join("b.json");
    let c_stdin = ws.join("c.json");
    let started = ws.join("a-started");
    let release = ws.join("release");
    let script = format!(
        "stdin=$(cat); case \"$stdin\" in \
         *'\"last_assistant_message\":\"A\"'*) printf '%s' \"$stdin\" > '{a}'; \
         touch '{started}'; while [ ! -f '{release}' ]; do sleep 0.01; done; \
         echo keep-going >&2; exit 2 ;; \
         *'\"last_assistant_message\":\"B\"'*) printf '%s' \"$stdin\" > '{b}' ;; \
         *) printf '%s' \"$stdin\" > '{c}' ;; esac",
        a = a_stdin.display(),
        b = b_stdin.display(),
        c = c_stdin.display(),
        started = started.display(),
        release = release.display(),
    );
    let config = radian_host::config::HooksConfig {
        stop: vec![radian_host::config::HookMatcher {
            matcher: None,
            hooks: vec![radian_host::config::HookCommand {
                command: script,
                timeout_secs: None,
            }],
        }],
        ..Default::default()
    };
    let inbox = radian_runtime::Inbox::new(64);
    let mut release_guard = Release::new(release.clone());
    let hooks = Arc::new(CommandHooks::new(
        &config,
        &radian_host::config::HooksConfig::default(),
        &ws,
        "test-session",
        radian_sandbox::SandboxSlot::empty(),
        &inbox,
        &tokio::runtime::Handle::current(),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
    ));

    // Turn A ends; its Stop hook blocks before exiting 2.
    hooks.on_turn_end(&context(), &completed("A"));
    wait_for(&started).await;

    // The user speaks while A's hook still runs: turn B opens, opened by the user.
    hooks.reset_wakes();
    inbox.followup("B", InputSource::User).await.unwrap();
    let b = inbox.claim_next_turn().await.expect("B opens");
    assert_eq!(b.message.text(), "B");
    assert!(
        !hooks.stop_hook_active(),
        "a user turn is not a continuation"
    );

    // A's hook now exits 2 and queues its continuation; B ends after that verdict.
    std::fs::write(&release, b"go").unwrap();
    for _ in 0..600 {
        if hooks.stop_wakes() > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        hooks.stop_wakes(),
        1,
        "the late verdict queued a continuation"
    );
    release_guard.disarm();
    hooks.on_turn_end(&context(), &completed("B"));
    let b_stdin = wait_for_json(&b_stdin).await;
    assert_eq!(
        b_stdin["stop_hook_active"],
        json!(false),
        "the user's turn must not consume the other turn's continuation: {b_stdin}"
    );

    // The continuation the late verdict queued is the one that reports true.
    inbox
        .claim_next_turn()
        .await
        .expect("the late verdict queued a continuation");
    assert!(
        hooks.stop_hook_active(),
        "the claimed continuation marks its turn"
    );
    hooks.on_turn_end(&context(), &completed("C"));
    let c_stdin = wait_for_json(&c_stdin).await;
    assert_eq!(
        c_stdin["stop_hook_active"],
        json!(true),
        "the continuation itself says so: {c_stdin}"
    );
}

/// A denied call never reaches the command hooks: the hook's marker stays absent and the
/// verdict is the permission layer's `Deny`. Moving `commands` before `permissions` runs
/// the hook first, so the marker appears and this fails.
#[tokio::test]
async fn permissions_deny_short_circuits_before_command_hooks_run() {
    let ws = scratch("deny-first");
    let marker = ws.join("hook-ran");
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
        &ws,
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

/// Returns its scripted responses in order, with no network.
struct ScriptedModel(Mutex<VecDeque<ModelResponse>>);

impl ScriptedModel {
    fn new(replies: Vec<ModelResponse>) -> Self {
        Self(Mutex::new(replies.into()))
    }
}

impl Model for ScriptedModel {
    async fn complete(
        &self,
        _request: ModelRequest,
        _cx: ModelContext<'_>,
    ) -> Result<ModelResponse, ModelError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted reply"))
    }
}

/// One `echo` tool that always returns `original`.
struct EchoTool;

struct EchoSession {
    specs: Vec<ToolSpec>,
}

impl ToolProvider for EchoTool {
    type Session = EchoSession;

    async fn open_session(&self) -> Result<EchoSession, ToolError> {
        Ok(EchoSession {
            specs: vec![ToolSpec {
                name: "echo".into(),
                description: "echo".into(),
                input_schema: json!({"type": "object"}),
                concurrency: Default::default(),
            }],
        })
    }
}

impl ToolSession for EchoSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        _call: &ToolCall,
        _cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        Ok(ToolOutput::text("original"))
    }
}

fn no_limits() -> Option<radian_core::RateLimits> {
    None
}

/// Edge case 3 (journal half, run under `--features journal-selfcheck`): with
/// `updatedToolOutput` in both layers, the user hook's stdin `tool_response` is the
/// project-replaced result, and the final transcript tool message and the journal's
/// `ToolExecutionSettled` both hold the user layer's text. Ignoring `updatedToolOutput`
/// (the old behavior) leaves every one of the three at `original` and fails.
#[tokio::test]
async fn updated_tool_output_reaches_the_next_layer_the_transcript_and_the_journal() {
    let ws = scratch("post-replace-journal");
    let user_stdin = ws.join("user-stdin.json");
    let project = radian_host::config::HooksConfig {
        post_tool_use: vec![radian_host::config::HookMatcher {
            matcher: Some("echo".into()),
            hooks: vec![radian_host::config::HookCommand {
                command:
                    r#"printf '%s' '{"hookSpecificOutput":{"updatedToolOutput":"from-project"}}'"#
                        .into(),
                timeout_secs: None,
            }],
        }],
        ..Default::default()
    };
    let user = radian_host::config::HooksConfig {
        post_tool_use: vec![radian_host::config::HookMatcher {
            matcher: Some("echo".into()),
            hooks: vec![radian_host::config::HookCommand {
                command: format!(
                    "input=$(cat); printf '%s' \"$input\" > {}; \
                     printf '%s' '{{\"hookSpecificOutput\":{{\"updatedToolOutput\":\"from-user\"}}}}'",
                    user_stdin.display()
                ),
                timeout_secs: None,
            }],
        }],
        ..Default::default()
    };
    let inbox = radian_runtime::Inbox::new(64);
    let hooks = Arc::new(CommandHooks::new(
        &user,
        &project,
        &ws,
        "test-session",
        radian_sandbox::SandboxSlot::empty(),
        &inbox,
        &tokio::runtime::Handle::current(),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
    ));
    let model = ScriptedModel::new(vec![
        ModelResponse {
            content: Vec::new(),
            reasoning: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                arguments: json!({}),
            }],
            provider_payload: None,
            stop_reason: StopReason::ToolUse,
            usage: None,
            limits: no_limits(),
        },
        ModelResponse {
            content: vec![ContentPart::text("done")],
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            provider_payload: None,
            stop_reason: StopReason::EndTurn,
            usage: None,
            limits: no_limits(),
        },
    ]);
    let agent = Arc::new(Agent::new(model, EchoTool).with_hooks(hooks));
    let journal_path = ws.join("journal.jsonl");
    let journal = Arc::new(
        SessionJournal::create(
            &journal_path,
            "sid",
            SessionCreatedData::new("/work/example", json!({"provider": "example"})),
        )
        .unwrap(),
    );
    let mut chat = agent
        .open_session_with_journal(Vec::new(), journal)
        .await
        .expect("a journaled session");
    chat.run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .expect("the turn finishes");
    chat.close().await.expect("the session closes");

    let tool = chat
        .transcript()
        .iter()
        .find_map(|message| match message {
            Message::Tool { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("a tool result");
    assert_eq!(tool.text(), "from-user", "the user layer's text is final");

    let stdin: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&user_stdin).unwrap()).unwrap();
    let seen = stdin["tool_response"].to_string();
    assert!(
        seen.contains("from-project"),
        "the user hook sees the project replacement: {seen}"
    );
    assert!(
        !seen.contains("original"),
        "the project replacement, not the tool's text: {seen}"
    );

    let session = Session::open(&journal_path).unwrap();
    let settled: Vec<_> = session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::ToolExecutionSettled { result, .. } => Some(result.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(settled.len(), 1, "one settled record");
    assert_eq!(
        settled[0].text(),
        "from-user",
        "the journal holds the replaced result"
    );
}

/// A hook's `permissionDecision: "allow"` is refused: the call still follows the permission
/// rules, so a would-ask call still asks (and here is denied by the scripted approver).
/// Accepting the `allow` — by mapping it to a decision or by running hooks before the
/// rules — skips the approver, so `calls() == 0` and this fails.
#[tokio::test]
async fn allow_output_does_not_bypass_the_permission_rules() {
    let ws = scratch("allow-refused");
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
        &ws,
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
