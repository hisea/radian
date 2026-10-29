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
                args: None,
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
                args: None,
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
                args: None,
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
                args: None,
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
                args: None,
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

// ---------- `PermissionRequest` (ADR 0045 §4) ----------

use radian_host::pending::{PendingQueue, QueuedApprover};
use radian_permissions::{PermissionRequestHooks, RecordingAudit, ResolvedDecision};
use std::time::Duration;

/// The `radian exec -p` approver: there is no terminal to ask in, so every `ask` is a
/// denial. The counter proves the hook answered first when the call runs.
struct NoTerminalApprover {
    calls: AtomicUsize,
}

impl NoTerminalApprover {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Approver for NoTerminalApprover {
    fn decide(
        &self,
        _request: ApprovalRequest,
    ) -> BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(ApprovalOutcome::Deny {
                reason: Some(
                    "not approved: this run has no terminal to ask in; \
                     pass --yes to allow tool calls"
                        .into(),
                ),
            })
        })
    }
}

fn asking_profile(ws: &std::path::Path) -> Profile {
    // No rules: every call is `ask` (the conservative default).
    Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::OnAsk,
        ws,
        Vec::new(),
    )
    .unwrap()
}

/// One `PermissionRequest` entry for the `echo` tool, as the host reads it.
fn request_commands(
    ws: &std::path::Path,
    project: Option<&str>,
    user: &str,
    timeout_secs: Option<u64>,
) -> Arc<CommandHooks> {
    let entry = |script: &str| radian_host::config::HookMatcher {
        matcher: Some("*".into()),
        hooks: vec![radian_host::config::HookCommand {
            command: script.to_owned(),
            args: None,
            timeout_secs,
        }],
    };
    let user = radian_host::config::HooksConfig {
        permission_request: vec![entry(user)],
        ..Default::default()
    };
    let project = radian_host::config::HooksConfig {
        permission_request: project.map(entry).into_iter().collect(),
        ..Default::default()
    };
    let inbox = radian_runtime::Inbox::new(64);
    Arc::new(CommandHooks::new(
        &user,
        &project,
        ws,
        "test-session",
        radian_sandbox::SandboxSlot::empty(),
        &inbox,
        &tokio::runtime::Handle::current(),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
    ))
}

fn install_request_hooks(permissions: &Arc<PermissionHooks>, commands: &Arc<CommandHooks>) {
    permissions.set_permission_request_hooks(Some(
        Arc::clone(commands) as Arc<dyn PermissionRequestHooks>
    ));
}

/// Runs one journaled turn whose model asks for one `echo` call, through the production
/// chain, and returns the call's tool result.
async fn run_echo_turn(
    ws: &std::path::Path,
    permissions: Arc<PermissionHooks>,
    commands: Arc<CommandHooks>,
) -> radian_core::ToolResult {
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
    let hooks = production_chain(ws, permissions, commands);
    let agent = Arc::new(Agent::new(model, EchoTool).with_hooks(hooks));
    let journal = Arc::new(
        SessionJournal::create(
            ws.join("turn.jsonl"),
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
    let result = chat
        .transcript()
        .iter()
        .find_map(|message| match message {
            Message::Tool { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("a tool result");
    chat.close().await.expect("the session closes");
    result
}

const ALLOW_SCRIPT: &str =
    r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow"}}}'"#;
const DENY_SCRIPT: &str = r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"deny","message":"no-writes-by-script"}}}'"#;

/// §4 seam: `open_session` is the one place where the two sides meet — it installs the
/// command hooks as the permission layer's `PermissionRequest` chain, so `radian-permissions`
/// never depends on `radian-host`. Deleting the wiring leaves a headless or non-human ask on
/// today's deny path; this structural check fails then (the behavior tests install the chain
/// by hand, the way the host does).
#[test]
fn open_session_installs_the_permission_request_chain() {
    let source =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/session/open.rs"))
            .expect("session/open.rs is readable");
    assert!(
        source.contains("set_permission_request_hooks("),
        "open_session must install the command hooks as the PermissionRequest chain"
    );
    assert!(
        source.contains("as Arc<dyn PermissionRequestHooks>"),
        "the chain is the host's CommandHooks, through the trait only"
    );
}

/// Edge case 1: `radian exec -p` has no terminal, so the approver would deny. The
/// `PermissionRequest` hook answers first with `allow`, so the call runs. Asking the
/// approver (the old path) leaves the call denied and fails every assertion here.
#[tokio::test]
async fn an_exec_without_a_terminal_lets_the_permission_request_hook_run_the_call() {
    let ws = scratch("request-exec-allow");
    let approver = NoTerminalApprover::new();
    let permissions = Arc::new(PermissionHooks::new(
        asking_profile(&ws),
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::new(NoopAudit),
    ));
    permissions.set_turn_source(Some(InputSource::User));
    let commands = request_commands(&ws, None, ALLOW_SCRIPT, None);
    install_request_hooks(&permissions, &commands);
    let result = run_echo_turn(&ws, permissions, commands).await;
    assert!(
        !result.is_error,
        "the hook's allow runs the call: {result:?}"
    );
    assert_eq!(result.text(), "original");
    assert_eq!(approver.calls(), 0, "the terminal approver is never asked");
}

/// Edge case 2: the hook's `deny` with `message` reaches the model as the call's error
/// result, with reason code `denied`; the approver is never asked.
#[tokio::test]
async fn an_exec_without_a_terminal_returns_the_permission_request_deny_message() {
    let ws = scratch("request-exec-deny");
    let approver = NoTerminalApprover::new();
    let permissions = Arc::new(PermissionHooks::new(
        asking_profile(&ws),
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::new(NoopAudit),
    ));
    permissions.set_turn_source(Some(InputSource::User));
    let commands = request_commands(&ws, None, DENY_SCRIPT, None);
    install_request_hooks(&permissions, &commands);
    let result = run_echo_turn(&ws, permissions, commands).await;
    assert!(
        result.is_error,
        "a hook deny is an error result: {result:?}"
    );
    assert_eq!(result.error_code.as_deref(), Some("denied"));
    assert!(
        result.text().contains("no-writes-by-script"),
        "the denied result carries the message: {}",
        result.text()
    );
    assert_eq!(approver.calls(), 0, "the terminal approver is never asked");
}

/// Edge case 3: a hook that times out, exits non-2, or prints non-JSON fails closed: the
/// call gets a `hook_error` result, and no prompt is ever created for it (the approver is
/// the prompt's only source, so `calls() == 0`).
#[tokio::test]
async fn a_failed_permission_request_hook_denies_with_hook_error_and_shows_no_prompt() {
    for (n, (script, timeout)) in [
        ("printf '%s' 'hello'", None),
        ("exit 1", None),
        ("sleep 30", Some(1)),
    ]
    .into_iter()
    .enumerate()
    {
        let ws = scratch(&format!("request-error-{n}"));
        let approver = NoTerminalApprover::new();
        let permissions = Arc::new(PermissionHooks::new(
            asking_profile(&ws),
            Some(Arc::clone(&approver) as Arc<dyn Approver>),
            Arc::new(NoopAudit),
        ));
        permissions.set_turn_source(Some(InputSource::User));
        let commands = request_commands(&ws, None, script, timeout);
        install_request_hooks(&permissions, &commands);
        let result = run_echo_turn(&ws, permissions, commands).await;
        assert!(
            result.is_error,
            "{script}: a broken hook fails closed: {result:?}"
        );
        assert_eq!(
            result.error_code.as_deref(),
            Some("hook_error"),
            "{script}: {}",
            result.text()
        );
        assert_eq!(
            approver.calls(),
            0,
            "{script}: a hook error never falls back to a prompt"
        );
    }
}

/// Edge case 4: a hook that prints nothing leaves today's path in place — the approver is
/// asked (the TUI's modal is the approval queue's head); a hook `allow` answers it first,
/// so no head ever appears.
#[tokio::test]
async fn an_undecided_permission_request_hook_falls_back_to_the_prompt() {
    // (a) No decision: the queued approver is asked and the modal's head appears.
    let ws = scratch("request-prompt");
    let queue = Arc::new(PendingQueue::<ApprovalRequest, ApprovalOutcome>::new());
    let permissions = Arc::new(PermissionHooks::new(
        asking_profile(&ws),
        Some(Arc::new(QueuedApprover::new(
            Arc::clone(&queue),
            "test-session",
        ))),
        Arc::new(NoopAudit),
    ));
    permissions.set_turn_source(Some(InputSource::User));
    let commands = request_commands(&ws, None, "exit 0", None);
    install_request_hooks(&permissions, &commands);
    let cx = context();
    let task_permissions = Arc::clone(&permissions);
    let task = tokio::spawn(async move {
        Hooks::before_tool_call(&*task_permissions, &cx, &shell_call()).await
    });
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(view) = queue.head() {
                break view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("an undecided hook leaves the ordinary prompt in place");
    queue
        .answer(
            view.request_id,
            ApprovalOutcome::Deny {
                reason: Some("asked-the-human".into()),
            },
        )
        .unwrap();
    let decision = task.await.unwrap().unwrap();
    assert!(
        matches!(&decision, ToolDecision::Deny { reason } if reason.contains("asked-the-human")),
        "the prompt's answer stands: {decision:?}"
    );

    // (b) A hook `allow`: no prompt is ever created.
    let ws = scratch("request-allow-no-prompt");
    let queue = Arc::new(PendingQueue::<ApprovalRequest, ApprovalOutcome>::new());
    let permissions = Arc::new(PermissionHooks::new(
        asking_profile(&ws),
        Some(Arc::new(QueuedApprover::new(
            Arc::clone(&queue),
            "test-session",
        ))),
        Arc::new(NoopAudit),
    ));
    permissions.set_turn_source(Some(InputSource::User));
    let commands = request_commands(&ws, None, ALLOW_SCRIPT, None);
    install_request_hooks(&permissions, &commands);
    let chain = production_chain(&ws, permissions, commands);
    let decision = chain
        .before_tool_call(&context(), &shell_call())
        .await
        .unwrap();
    assert!(matches!(decision, ToolDecision::Allow), "{decision:?}");
    assert!(
        queue.head().is_none(),
        "an answered ask never shows the approval modal"
    );
}

/// Edge case 9 (process half): cancelling the turn while the hook runs kills the hook's
/// whole process group, and the call ends as a denial — never as the hook's late `allow`.
/// Without the kill-on-drop guard the slept process writes its marker and the test fails.
#[tokio::test]
async fn cancelling_the_turn_kills_the_permission_request_hook_group() {
    let ws = scratch("request-cancel-process");
    let started = ws.join("started");
    let late = ws.join("late");
    let script = format!(
        "touch {started}; sleep 1; echo late > {late}; \
         printf '%s' '{{\"hookSpecificOutput\":{{\"decision\":{{\"behavior\":\"allow\"}}}}}}'",
        started = started.display(),
        late = late.display(),
    );
    let approver = NoTerminalApprover::new();
    let permissions = Arc::new(PermissionHooks::new(
        asking_profile(&ws),
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::new(NoopAudit),
    ));
    permissions.set_turn_source(Some(InputSource::User));
    let commands = request_commands(&ws, None, &script, None);
    install_request_hooks(&permissions, &commands);

    let cx = context();
    let token = cx.cancellation.clone();
    let task_permissions = Arc::clone(&permissions);
    let task = tokio::spawn(async move {
        Hooks::before_tool_call(&*task_permissions, &cx, &shell_call()).await
    });
    // Wait for the hook's own marker, so the cancel lands while it sleeps.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the hook started");
    token.cancel();
    let decision = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("cancellation interrupts the hook")
        .expect("the task joins");
    assert!(
        matches!(decision, Ok(ToolDecision::Deny { .. })),
        "a cancelled ask is a denial: {decision:?}"
    );
    // Long enough that an un-killed hook would have reached its marker.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !late.exists(),
        "the hook's process group must be killed on cancellation"
    );
    assert_eq!(approver.calls(), 0);
}

/// §4 (layers through the production chain): the project entry runs first and a user
/// `deny` still wins over a project `allow`.
#[tokio::test]
async fn a_user_permission_request_deny_beats_a_project_allow() {
    let ws = scratch("request-layers");
    let queue = Arc::new(PendingQueue::<ApprovalRequest, ApprovalOutcome>::new());
    let permissions = Arc::new(PermissionHooks::new(
        asking_profile(&ws),
        Some(Arc::new(QueuedApprover::new(
            Arc::clone(&queue),
            "test-session",
        ))),
        Arc::new(NoopAudit),
    ));
    permissions.set_turn_source(Some(InputSource::User));
    let commands = request_commands(&ws, Some(ALLOW_SCRIPT), DENY_SCRIPT, None);
    install_request_hooks(&permissions, &commands);
    let chain = production_chain(&ws, permissions, commands);
    let decision = chain
        .before_tool_call(&context(), &shell_call())
        .await
        .unwrap();
    assert!(
        matches!(&decision, ToolDecision::Deny { reason } if reason.contains("no-writes-by-script")),
        "the user deny stands over the project allow: {decision:?}"
    );
    assert!(queue.head().is_none(), "no prompt for an answered ask");

    // The audit says the hook answered as the user, once.
    let audit = Arc::new(RecordingAudit::default());
    let approver = NoTerminalApprover::new();
    let permissions = Arc::new(PermissionHooks::new(
        asking_profile(&ws),
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::clone(&audit) as Arc<dyn radian_permissions::AuditSink>,
    ));
    permissions.set_turn_source(Some(InputSource::User));
    let commands = request_commands(&ws, None, DENY_SCRIPT, None);
    install_request_hooks(&permissions, &commands);
    let _ = Hooks::before_tool_call(&*permissions, &context(), &shell_call())
        .await
        .unwrap();
    let events = audit.events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].decision, ResolvedDecision::Deny);
    assert_eq!(events[0].approver, radian_permissions::AuditApprover::User);
}

/// Edge case 1 (process half) and §3: an allow with the one accepted `updatedPermissions`
/// item records a session grant. The second identical call is allowed by the grant, so the
/// hook never runs again — the marker counts one run — and the approver is never asked.
#[tokio::test]
async fn a_hook_session_grant_answers_the_next_call_without_running_the_hook() {
    let ws = scratch("request-grant-session");
    let marker = ws.join("runs");
    let script = format!(
        "echo ran >> {}\nprintf '%s' '{{\"hookSpecificOutput\":{{\"decision\":{{\"behavior\":\"allow\",\
         \"updatedPermissions\":[{{\"type\":\"addRules\",\"destination\":\"session\",\
         \"behavior\":\"allow\"}}]}}}}}}'",
        marker.display()
    );
    let approver = NoTerminalApprover::new();
    let permissions = Arc::new(PermissionHooks::new(
        asking_profile(&ws),
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::new(NoopAudit),
    ));
    permissions.set_turn_source(Some(InputSource::User));
    let commands = request_commands(&ws, None, &script, None);
    install_request_hooks(&permissions, &commands);
    let chain = production_chain(&ws, permissions, commands);

    for n in 0..2 {
        let decision = chain
            .before_tool_call(&context(), &shell_call())
            .await
            .unwrap();
        assert!(
            matches!(decision, ToolDecision::Allow),
            "call {n}: {decision:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().lines().count(),
        1,
        "the grant answers the second call, so the hook runs once"
    );
    assert_eq!(approver.calls(), 0, "the terminal approver is never asked");
}

/// Edge case 3: `destination: "projectSettings"` warns one line and records no grant; the
/// call is still allowed once, and the second identical call runs the hook again.
#[tokio::test]
async fn an_unsupported_updated_permissions_destination_allows_once_without_a_grant() {
    let ws = scratch("request-grant-destination");
    let marker = ws.join("runs");
    let script = format!(
        "echo ran >> {}\nprintf '%s' '{{\"hookSpecificOutput\":{{\"decision\":{{\"behavior\":\"allow\",\
         \"updatedPermissions\":[{{\"type\":\"addRules\",\"destination\":\"projectSettings\",\
         \"behavior\":\"allow\"}}]}}}}}}'",
        marker.display()
    );
    let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&warnings);
    let approver = NoTerminalApprover::new();
    let permissions = Arc::new(PermissionHooks::new(
        asking_profile(&ws),
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::new(NoopAudit),
    ));
    permissions.set_turn_source(Some(InputSource::User));
    let entry = radian_host::config::HookMatcher {
        matcher: Some("*".into()),
        hooks: vec![radian_host::config::HookCommand {
            command: script,
            args: None,
            timeout_secs: None,
        }],
    };
    let user = radian_host::config::HooksConfig {
        permission_request: vec![entry],
        ..Default::default()
    };
    let commands = Arc::new(CommandHooks::new(
        &user,
        &radian_host::config::HooksConfig::default(),
        &ws,
        "test-session",
        radian_sandbox::SandboxSlot::empty(),
        &radian_runtime::Inbox::new(64),
        &tokio::runtime::Handle::current(),
        Arc::new(move |message: &str| sink.lock().unwrap().push(message.to_owned())),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
        Arc::new(|_message: &str| {}),
    ));
    install_request_hooks(&permissions, &commands);
    let chain = production_chain(&ws, permissions, commands);

    for n in 0..2 {
        let decision = chain
            .before_tool_call(&context(), &shell_call())
            .await
            .unwrap();
        assert!(
            matches!(decision, ToolDecision::Allow),
            "call {n}: {decision:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().lines().count(),
        2,
        "an unsupported destination records no grant, so each call runs the hook"
    );
    assert_eq!(approver.calls(), 0, "the hook still allows each call");
    let warnings = warnings.lock().unwrap();
    assert_eq!(warnings.len(), 2, "one warning per run: {warnings:?}");
    assert!(
        warnings[0].contains("updatedPermissions") && warnings[0].contains("session"),
        "{warnings:?}"
    );
}

/// Edge case 4: a project layer `allow` with an `addRules` grant, then a user layer
/// `deny`: the final decision is a denial, so no grant is recorded and the next identical
/// call runs both layers again (the project marker counts two runs).
#[tokio::test]
async fn a_user_deny_discards_a_project_updated_permissions_grant() {
    let ws = scratch("request-grant-project-deny");
    let marker = ws.join("project-runs");
    let project_script = format!(
        "echo ran >> {}\nprintf '%s' '{{\"hookSpecificOutput\":{{\"decision\":{{\"behavior\":\"allow\",\
         \"updatedPermissions\":[{{\"type\":\"addRules\",\"destination\":\"session\",\
         \"behavior\":\"allow\"}}]}}}}}}'",
        marker.display()
    );
    let approver = NoTerminalApprover::new();
    let permissions = Arc::new(PermissionHooks::new(
        asking_profile(&ws),
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::new(NoopAudit),
    ));
    permissions.set_turn_source(Some(InputSource::User));
    let commands = request_commands(&ws, Some(&project_script), DENY_SCRIPT, None);
    install_request_hooks(&permissions, &commands);
    let chain = production_chain(&ws, permissions, commands);

    for n in 0..2 {
        let decision = chain
            .before_tool_call(&context(), &shell_call())
            .await
            .unwrap();
        assert!(
            matches!(&decision, ToolDecision::Deny { reason } if reason.contains("no-writes-by-script")),
            "call {n}: the user deny stands: {decision:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().lines().count(),
        2,
        "a denied call records no grant, so the project hook runs on each call"
    );
    assert_eq!(approver.calls(), 0, "the hooks answer every ask");
}
