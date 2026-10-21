//! Slice 2 of `sandbox-auto-allow-shell`: a "session" grant for a `shell` call widens
//! from the exact command string to the command's prefix — only for a simple command.
//! Compounds, wrappers, `eval`/`source`, and the never-prefix list keep exact-string
//! grants (or ask), as before.

mod common;

use std::sync::Arc;

use common::{ScriptedApprover, TempDir, call, ctx};
use radian_core::{Hooks, InputSource, ToolCall, ToolDecision};
use radian_permissions::*;
use radian_sandbox::Sandbox;
use serde_json::json;

fn hooks_with(
    profile: Profile,
    answers: Vec<ApprovalOutcome>,
) -> (Arc<PermissionHooks>, Arc<ScriptedApprover>) {
    let approver = ScriptedApprover::scripted(answers);
    let hooks = Arc::new(PermissionHooks::new(
        profile,
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::new(RecordingAudit::default()),
    ));
    hooks.set_turn_source(Some(InputSource::User));
    (hooks, approver)
}

fn asking_profile(dir: &TempDir) -> Profile {
    // No rules: every call is `ask` (the conservative default).
    Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::OnAsk,
        dir.path(),
        Vec::new(),
    )
    .unwrap()
}

async fn decide_shell(
    hooks: &PermissionHooks,
    command: &str,
) -> Result<ToolDecision, radian_core::HookError> {
    Hooks::before_tool_call(hooks, &ctx(), &call("shell", json!({ "command": command }))).await
}

/// An enforced sandbox without probing: `Sandbox::inherited` has mode
/// `WorkspaceWrite`, so it is proof a sandbox confines the session (no probe,
/// deterministic on macOS and Linux).
fn enforced(dir: &TempDir) -> Sandbox {
    Sandbox::inherited(dir.path())
}

/// After "allow for this session" on `cargo test -p a`, `cargo test -p b` does not ask —
/// the grant widened to the `cargo test` prefix — while `cargo build` still asks.
/// Granting exact strings only (no prefix widening) fails this test.
#[tokio::test]
async fn a_session_grant_widens_to_the_command_prefix() {
    let dir = TempDir::new("prefix-cargo");
    let (hooks, approver) = hooks_with(
        asking_profile(&dir),
        vec![
            ApprovalOutcome::AllowForSession,
            ApprovalOutcome::Deny { reason: None },
        ],
    );
    let first = decide_shell(&hooks, "cargo test -p a").await.unwrap();
    assert!(
        matches!(first, ToolDecision::Allow),
        "the granted call must be allowed, got {first:?}"
    );
    assert_eq!(approver.calls(), 1);
    // Same prefix, different package: covered, no second question.
    let second = decide_shell(&hooks, "cargo test -p b").await.unwrap();
    assert!(
        matches!(second, ToolDecision::Allow),
        "the same prefix must be covered, got {second:?}"
    );
    assert_eq!(
        approver.calls(),
        1,
        "the prefix grant must answer without asking"
    );
    // A different subcommand: still asks.
    let third = decide_shell(&hooks, "cargo build -p b").await.unwrap();
    assert!(
        matches!(third, ToolDecision::Deny { .. }),
        "a different subcommand must ask, got {third:?}"
    );
    assert_eq!(approver.calls(), 2, "a different subcommand must ask");
}

/// A bare program grants the program (`make check` covers `make test`), while a
/// subcommand program keeps its subcommand (`npm run` never covers `npm test`).
#[tokio::test]
async fn programs_without_a_subcommand_grant_the_program_alone() {
    let dir = TempDir::new("prefix-program");
    let (hooks, approver) = hooks_with(
        asking_profile(&dir),
        vec![
            ApprovalOutcome::AllowForSession,
            ApprovalOutcome::AllowForSession,
            ApprovalOutcome::Deny { reason: None },
        ],
    );
    assert!(matches!(
        decide_shell(&hooks, "make check").await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 1);
    assert!(
        matches!(
            decide_shell(&hooks, "make test").await.unwrap(),
            ToolDecision::Allow
        ),
        "the program-alone prefix must cover new arguments"
    );
    assert_eq!(approver.calls(), 1, "`make test` must not ask");

    assert!(matches!(
        decide_shell(&hooks, "npm run build").await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 2);
    assert!(
        matches!(
            decide_shell(&hooks, "npm run lint").await.unwrap(),
            ToolDecision::Allow
        ),
        "the subcommand prefix must cover new arguments"
    );
    assert_eq!(approver.calls(), 2, "`npm run lint` must not ask");
    assert!(
        matches!(
            decide_shell(&hooks, "npm test").await.unwrap(),
            ToolDecision::Deny { .. }
        ),
        "a different subcommand must ask"
    );
    assert_eq!(approver.calls(), 3, "`npm test` must ask");
}

/// A compound command neither earns nor borrows a prefix grant: granting
/// `cargo test && rm -rf x` covers that exact line and nothing else, and a `cargo test`
/// prefix grant never answers a compound. Prefix-granting a compound fails this test.
#[tokio::test]
async fn a_compound_command_neither_earns_nor_borrows_a_prefix_grant() {
    let dir = TempDir::new("prefix-compound");
    let (hooks, approver) = hooks_with(
        asking_profile(&dir),
        vec![
            ApprovalOutcome::AllowForSession,
            ApprovalOutcome::Deny { reason: None },
            ApprovalOutcome::Deny { reason: None },
        ],
    );
    let granted = "cargo test && rm -rf x";
    assert!(matches!(
        decide_shell(&hooks, granted).await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 1);
    // The exact line is covered by the exact grant.
    assert!(matches!(
        decide_shell(&hooks, granted).await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 1, "the exact line must not ask again");
    // A different compound with the same head still asks.
    assert!(matches!(
        decide_shell(&hooks, "cargo test && echo hi").await.unwrap(),
        ToolDecision::Deny { .. }
    ));
    assert_eq!(approver.calls(), 2, "a different compound must ask");
    // So does a simple command sharing the head: no prefix was stored.
    assert!(matches!(
        decide_shell(&hooks, "cargo test -p b").await.unwrap(),
        ToolDecision::Deny { .. }
    ));
    assert_eq!(
        approver.calls(),
        3,
        "no prefix grant may come from a compound"
    );

    // The other direction: a prefix grant never answers a compound.
    let dir = TempDir::new("prefix-compound-borrow");
    let (hooks, approver) = hooks_with(
        asking_profile(&dir),
        vec![
            ApprovalOutcome::AllowForSession,
            ApprovalOutcome::Deny { reason: None },
        ],
    );
    assert!(matches!(
        decide_shell(&hooks, "cargo test -p a").await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 1);
    assert!(matches!(
        decide_shell(&hooks, "cargo test && rm -rf x")
            .await
            .unwrap(),
        ToolDecision::Deny { .. }
    ));
    assert_eq!(
        approver.calls(),
        2,
        "a compound must not borrow a prefix grant"
    );
}

/// The never-prefix list keeps exact-string grants: `rm a` never covers `rm b`,
/// `git push` never covers another `git push`, `docker run` never covers another
/// `docker run`. Dropping the list (prefix-granting these) fails this test.
#[tokio::test]
async fn a_never_prefix_command_keeps_an_exact_grant() {
    let dir = TempDir::new("prefix-never");
    let (hooks, approver) = hooks_with(
        asking_profile(&dir),
        vec![
            ApprovalOutcome::AllowForSession,
            ApprovalOutcome::Deny { reason: None },
            ApprovalOutcome::AllowForSession,
            ApprovalOutcome::Deny { reason: None },
            ApprovalOutcome::AllowForSession,
            ApprovalOutcome::Deny { reason: None },
        ],
    );
    assert!(matches!(
        decide_shell(&hooks, "rm a").await.unwrap(),
        ToolDecision::Allow
    ));
    assert!(matches!(
        decide_shell(&hooks, "rm a").await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 1, "the exact line must not ask again");
    assert!(matches!(
        decide_shell(&hooks, "rm b").await.unwrap(),
        ToolDecision::Deny { .. }
    ));
    assert_eq!(approver.calls(), 2, "`rm b` must ask");

    assert!(matches!(
        decide_shell(&hooks, "git push origin main").await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 3);
    assert!(matches!(
        decide_shell(&hooks, "git push origin other").await.unwrap(),
        ToolDecision::Deny { .. }
    ));
    assert_eq!(approver.calls(), 4, "another `git push` must ask");

    assert!(matches!(
        decide_shell(&hooks, "docker run --rm img").await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 5);
    assert!(matches!(
        decide_shell(&hooks, "docker run --rm img2").await.unwrap(),
        ToolDecision::Deny { .. }
    ));
    assert_eq!(approver.calls(), 6, "another `docker run` must ask");
}

/// Prefix grants are per-session real-time state (ADR 0012): a fresh instance built from
/// the same profile — what a resume, or a project-config-only load, looks like — has
/// none, so the same prefix asks again. A process-wide grant store (the mutation that
/// would let a project config plant a prefix grant) fails this test.
#[tokio::test]
async fn a_project_config_cannot_create_a_prefix_grant() {
    let dir = TempDir::new("prefix-project");
    // The project-narrowed tier: a project config can only ever narrow to this.
    let profile = Profile::workspace_write(dir.path()).unwrap();
    let approver = ScriptedApprover::scripted(vec![ApprovalOutcome::AllowForSession]);
    let hooks = Arc::new(PermissionHooks::new(
        profile.clone(),
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::new(RecordingAudit::default()),
    ));
    hooks.set_turn_source(Some(InputSource::User));
    assert!(matches!(
        decide_shell(&hooks, "cargo test -p a").await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 1);

    // A fresh instance from the same profile has no grant: the prefix asks again.
    let resumed = PermissionHooks::new(
        profile,
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::new(RecordingAudit::default()),
    );
    resumed.set_turn_source(Some(InputSource::User));
    assert!(matches!(
        decide_shell(&resumed, "cargo test -p b").await.unwrap(),
        ToolDecision::Deny { .. }
    ));
    assert_eq!(
        approver.calls(),
        2,
        "a fresh instance must ask: no grant survives in the profile"
    );
}

/// A non-human turn cannot borrow a prefix grant either (unchanged): the grant is earned
/// in a human turn and stays unavailable to every other source.
#[tokio::test]
async fn a_non_human_turn_cannot_borrow_a_prefix_grant() {
    let dir = TempDir::new("prefix-source");
    let (hooks, approver) =
        hooks_with(asking_profile(&dir), vec![ApprovalOutcome::AllowForSession]);
    assert!(matches!(
        decide_shell(&hooks, "cargo test -p a").await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(approver.calls(), 1);

    hooks.set_turn_source(Some(InputSource::Subagent));
    assert!(matches!(
        decide_shell(&hooks, "cargo test -p b").await.unwrap(),
        ToolDecision::Deny { .. }
    ));
    assert_eq!(
        approver.calls(),
        1,
        "a non-human turn must not ask or borrow the prefix grant"
    );
}

/// A prefix grant never crosses the sandbox boundary: earned leaving the sandbox, it
/// covers later outside calls with the same prefix — and nothing inside.
#[tokio::test]
async fn a_prefix_grant_never_crosses_the_sandbox_boundary() {
    let dir = TempDir::new("prefix-outside");
    let (hooks, approver) = hooks_with(
        Profile::workspace_write_sandboxed(dir.path(), &enforced(&dir)).unwrap(),
        vec![ApprovalOutcome::AllowForSession, ApprovalOutcome::AllowOnce],
    );
    let outside = |command: &str| ToolCall {
        id: format!("call-{command}"),
        name: "shell".into(),
        arguments: json!({
            "command": command,
            "outside_sandbox": true,
            "outside_sandbox_reason": "needs the real home directory",
        }),
    };
    let first = Hooks::before_tool_call(&hooks, &ctx(), &outside("cargo test -p a"))
        .await
        .unwrap();
    assert!(matches!(first, ToolDecision::Allow), "{first:?}");
    assert_eq!(approver.calls(), 1);
    // The same prefix outside: covered, no second question.
    let second = Hooks::before_tool_call(&hooks, &ctx(), &outside("cargo test -p b"))
        .await
        .unwrap();
    assert!(matches!(second, ToolDecision::Allow), "{second:?}");
    assert_eq!(
        approver.calls(),
        1,
        "the outside prefix grant must cover the same prefix"
    );
    // The same prefix inside (under a profile without the blanket allow) asks again:
    // the outside grant must not cover it.
    hooks.set_profile(Profile::workspace_write(dir.path()).unwrap());
    assert!(matches!(
        decide_shell(&hooks, "cargo test -p c").await.unwrap(),
        ToolDecision::Allow
    ));
    assert_eq!(
        approver.calls(),
        2,
        "the outside prefix grant must not cover the same command inside"
    );
}
