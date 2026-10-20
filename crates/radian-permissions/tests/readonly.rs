//! The built-in read-only shell table: what it allows, what still asks, the git
//! rewrite, and its precedence (deny first, then the user's rules, then the
//! table, then the tier's blanket shell rule).

mod common;

use std::sync::Arc;

use agent_core::{Hooks, InputSource, ToolDecision};
use common::{ScriptedApprover, TempDir, call, ctx};
use radian_permissions::*;
use serde_json::json;

async fn decide(
    hooks: &PermissionHooks,
    command: &str,
) -> Result<ToolDecision, agent_core::HookError> {
    let call = call("shell", json!({ "command": command }));
    Hooks::before_tool_call(hooks, &ctx(), &call).await
}

fn hooks_with(
    profile: Profile,
    answers: Vec<ApprovalOutcome>,
    audit: &Arc<RecordingAudit>,
) -> (Arc<PermissionHooks>, Arc<ScriptedApprover>) {
    let approver = ScriptedApprover::scripted(answers);
    let hooks = Arc::new(PermissionHooks::new(
        profile,
        Some(Arc::clone(&approver) as Arc<dyn Approver>),
        Arc::clone(audit) as Arc<dyn AuditSink>,
    ));
    hooks.set_turn_source(Some(InputSource::User));
    (hooks, approver)
}

fn allow_events(audit: &Arc<RecordingAudit>) -> Vec<PermissionResolved> {
    audit.events()
}

/// `git status`, `rg foo crates/`, `ls -la` are allowed with `approver = policy`
/// in both tiers, without consulting the approver.
#[tokio::test]
async fn readonly_table_allows_without_a_prompt() {
    for command in ["git status", "rg foo crates/", "ls -la"] {
        for preset in ["workspace", "full"] {
            let dir = TempDir::new("readonly-allow");
            let profile = match preset {
                "workspace" => Profile::workspace_write(dir.path()).unwrap(),
                _ => Profile::full_access(dir.path()).unwrap(),
            };
            let audit = Arc::new(RecordingAudit::default());
            let (hooks, approver) = hooks_with(profile, vec![], &audit);
            let decision = decide(&hooks, command).await.unwrap();
            assert!(
                matches!(decision, ToolDecision::Allow),
                "{preset}: {command:?} must be allowed, got {decision:?}"
            );
            assert_eq!(approver.calls(), 0, "{preset}: {command:?} must not ask");
            let events = allow_events(&audit);
            assert_eq!(events.len(), 1, "{preset}: {command:?}: {events:?}");
            assert_eq!(events[0].decision, ResolvedDecision::Allow);
            assert_eq!(events[0].approver, AuditApprover::Policy);
        }
    }
}

/// Read-only commands are not worth reviewing: `approval = always` still allows
/// them without a prompt.
#[tokio::test]
async fn readonly_table_allows_under_always() {
    for command in ["git status", "rg foo crates/", "ls -la", "git log -p"] {
        let dir = TempDir::new("readonly-always");
        let profile = Profile::workspace_write(dir.path())
            .unwrap()
            .with_approval(ApprovalPolicy::Always);
        let audit = Arc::new(RecordingAudit::default());
        let (hooks, approver) = hooks_with(profile, vec![], &audit);
        let decision = decide(&hooks, command).await.unwrap();
        let allowed = matches!(decision, ToolDecision::Allow | ToolDecision::Rewrite { .. });
        assert!(
            allowed,
            "always: {command:?} must stay allowed, got {decision:?}"
        );
        assert_eq!(approver.calls(), 0, "always: {command:?} must not ask");
        assert_eq!(audit.events()[0].approver, AuditApprover::Policy);
    }
}

/// Paths outside the workspace still ask. Dropping the path check lets
/// `cat ../x` through, so this test is its mutation catcher.
#[tokio::test]
async fn paths_outside_the_workspace_still_ask() {
    let mut commands = vec![
        "cat ../x".to_owned(),
        "cat /etc/passwd".to_owned(),
        "cat sub/../../outside".to_owned(),
        "grep -rn foo /etc".to_owned(),
        "cargo metadata --manifest-path /etc/x".to_owned(),
    ];
    #[cfg(unix)]
    {
        // A symlink out of the workspace: resolved, not string-matched. The
        // workspace is `base/ws`, so `base/outside` is genuinely outside it.
        let dir = TempDir::new("readonly-link");
        let ws = dir.path().join("ws");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, ws.join("link")).unwrap();
        let audit = Arc::new(RecordingAudit::default());
        let (hooks, approver) = hooks_with(
            Profile::workspace_write(&ws).unwrap(),
            vec![ApprovalOutcome::Deny { reason: None }],
            &audit,
        );
        let decision = decide(&hooks, "cat link/s.txt").await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "a symlink out of the workspace must ask, got {decision:?}"
        );
        assert_eq!(approver.calls(), 1);
        assert_eq!(audit.events()[0].approver, AuditApprover::User);
    }
    for command in commands.drain(..) {
        let dir = TempDir::new("readonly-paths");
        let audit = Arc::new(RecordingAudit::default());
        let (hooks, approver) = hooks_with(
            Profile::workspace_write(dir.path()).unwrap(),
            vec![ApprovalOutcome::Deny { reason: None }],
            &audit,
        );
        let decision = decide(&hooks, &command).await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "{command:?} must ask, got {decision:?}"
        );
        assert_eq!(approver.calls(), 1, "{command:?} must reach the approver");
        let events = allow_events(&audit);
        assert_eq!(events.len(), 1, "{command:?}: {events:?}");
        assert_eq!(events[0].decision, ResolvedDecision::Deny);
        assert_eq!(
            events[0].approver,
            AuditApprover::User,
            "{command:?} must be denied by the user, not the policy"
        );
    }
}

/// Executable, long-lived and file-writing options still ask. Dropping the
/// option check lets `find . -exec rm +` through, so this test is its mutation
/// catcher. (`find . -exec rm {} \;` is already non-simple; the `+` form is the
/// simple one the option check has to catch.)
#[tokio::test]
async fn executable_and_writing_options_still_ask() {
    for command in [
        "find . -exec rm +",
        "find . -execdir rm +",
        "find . -ok rm +",
        "find . -delete",
        "find . -fls out",
        "find . -L",
        "git -c core.pager=x log",
        "git -C .. status",
        "git diff --output=x",
        "git log --ext-diff",
        "tail -f log",
        "tail -F log",
        "tail --follow=name log",
        "grep -R foo .",
        "cat ~/.ssh/id_rsa",
    ] {
        let dir = TempDir::new("readonly-opts");
        let audit = Arc::new(RecordingAudit::default());
        let (hooks, approver) = hooks_with(
            Profile::workspace_write(dir.path()).unwrap(),
            vec![ApprovalOutcome::Deny { reason: None }],
            &audit,
        );
        let decision = decide(&hooks, command).await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "{command:?} must ask, got {decision:?}"
        );
        assert_eq!(approver.calls(), 1, "{command:?} must reach the approver");
        let events = allow_events(&audit);
        assert_eq!(events.len(), 1, "{command:?}: {events:?}");
        assert_eq!(events[0].approver, AuditApprover::User);
    }
}

/// The git subcommand shapes are exact: `branch` takes no arguments or `--list`,
/// `remote` takes only `-v`, `stash` takes only `list`, and unknown options
/// over-ask.
#[tokio::test]
async fn git_subcommand_shapes_are_exact() {
    for command in [
        "git branch -a",
        "git branch foo",
        "git remote",
        "git remote add x y",
        "git stash",
        "git stash push",
        "git stash list --all",
        "git push",
        "git status --bogus",
        "ls -- .",
    ] {
        let dir = TempDir::new("readonly-shapes");
        let audit = Arc::new(RecordingAudit::default());
        let (hooks, approver) = hooks_with(
            Profile::workspace_write(dir.path()).unwrap(),
            vec![ApprovalOutcome::Deny { reason: None }],
            &audit,
        );
        let decision = decide(&hooks, command).await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "{command:?} must ask, got {decision:?}"
        );
        assert_eq!(approver.calls(), 1, "{command:?} must reach the approver");
    }
    // The exact shapes still allow.
    for command in [
        "git branch",
        "git branch --list",
        "git remote -v",
        "git stash list",
    ] {
        let dir = TempDir::new("readonly-shapes-allow");
        let audit = Arc::new(RecordingAudit::default());
        let (hooks, approver) = hooks_with(
            Profile::workspace_write(dir.path()).unwrap(),
            vec![],
            &audit,
        );
        assert!(
            matches!(decide(&hooks, command).await.unwrap(), ToolDecision::Allow),
            "{command:?} must be allowed"
        );
        assert_eq!(approver.calls(), 0);
    }
}

/// `git log -p` is rewritten with the two flags right after the subcommand, and
/// the rewrite is audited as a policy decision. Dropping the flag rewrite keeps
/// the plain command, so this test is its mutation catcher.
#[tokio::test]
async fn git_log_is_rewritten_with_both_flags() {
    let dir = TempDir::new("readonly-rewrite");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        Profile::workspace_write(dir.path()).unwrap(),
        vec![],
        &audit,
    );
    let decision = decide(&hooks, "git log -p").await.unwrap();
    let ToolDecision::Rewrite { arguments } = decision else {
        panic!("git log -p must be rewritten, got {decision:?}");
    };
    assert_eq!(
        arguments.get("command").and_then(|v| v.as_str()),
        Some("git log --no-ext-diff --no-textconv -p")
    );
    assert_eq!(approver.calls(), 0, "a rewrite must not ask");
    let events = allow_events(&audit);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].decision, ResolvedDecision::Rewrite);
    assert_eq!(events[0].approver, AuditApprover::Policy);
}

/// A rule denial still wins over the table.
#[tokio::test]
async fn deny_still_denies_a_table_command() {
    let dir = TempDir::new("readonly-deny");
    let profile = Profile::workspace_write(dir.path())
        .unwrap()
        .with_denied_tools(["shell"]);
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(profile, vec![], &audit);
    let decision = decide(&hooks, "git status").await.unwrap();
    let ToolDecision::Deny { reason } = decision else {
        panic!("a deny rule must deny a table command, got {decision:?}");
    };
    assert!(!reason.is_empty());
    assert_eq!(approver.calls(), 0, "a deny must not reach the approver");
    let events = allow_events(&audit);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].decision, ResolvedDecision::Deny);
    assert_eq!(events[0].approver, AuditApprover::Policy);
}

/// A named `ask` rule is an explicit user rule: it outranks the table and the
/// call prompts.
#[tokio::test]
async fn named_user_ask_outranks_the_table() {
    let dir = TempDir::new("readonly-user-ask");
    let profile = Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::OnAsk,
        dir.path(),
        vec![
            Rule::new(
                ToolMatcher::Name("shell".into()),
                ResourceMatcher::CommandPrefix(vec!["git".into(), "status".into()]),
                Action::Ask,
            )
            .with_reason("the user wants to review git"),
        ],
    )
    .unwrap();
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(profile, vec![ApprovalOutcome::AllowOnce], &audit);
    let decision = decide(&hooks, "git status").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "the approver's answer decides, got {decision:?}"
    );
    assert_eq!(approver.calls(), 1, "a user ask rule must prompt");
    let events = allow_events(&audit);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].approver, AuditApprover::User);
}

/// A user `allow` on the same command does not lose the git flag rewrite.
#[tokio::test]
async fn user_allow_keeps_the_git_rewrite() {
    let dir = TempDir::new("readonly-user-allow");
    let mut rules = Profile::workspace_write(dir.path())
        .unwrap()
        .rules()
        .to_vec();
    rules.push(
        Rule::new(
            ToolMatcher::Name("shell".into()),
            ResourceMatcher::CommandPrefix(vec!["git".into(), "log".into()]),
            Action::Allow,
        )
        .with_reason("allowed by --allow-shell-command"),
    );
    let profile = Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::OnAsk,
        dir.path(),
        rules,
    )
    .unwrap();
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(profile, vec![], &audit);
    let decision = decide(&hooks, "git log -p").await.unwrap();
    let ToolDecision::Rewrite { arguments } = decision else {
        panic!("a user-allowed git log must still gain the flags, got {decision:?}");
    };
    assert_eq!(
        arguments.get("command").and_then(|v| v.as_str()),
        Some("git log --no-ext-diff --no-textconv -p")
    );
    assert_eq!(approver.calls(), 0);
}

/// Headless turns no longer lose read-only commands: a non-human turn and the
/// `never` policy both still allow the table (anything else still denies under
/// `never`).
#[tokio::test]
async fn readonly_table_holds_for_nonhuman_turns_and_never() {
    // A non-human turn: allowed by policy, approver untouched.
    {
        let dir = TempDir::new("readonly-nonhuman");
        let audit = Arc::new(RecordingAudit::default());
        let approver = ScriptedApprover::scripted(vec![]);
        let hooks = Arc::new(PermissionHooks::new(
            Profile::workspace_write(dir.path()).unwrap(),
            Some(Arc::clone(&approver) as Arc<dyn Approver>),
            Arc::clone(&audit) as Arc<dyn AuditSink>,
        ));
        hooks.set_turn_source(Some(InputSource::Subagent));
        let decision = decide(&hooks, "git status").await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Allow),
            "a non-human turn must keep read-only commands, got {decision:?}"
        );
        assert_eq!(approver.calls(), 0);
        assert_eq!(audit.events()[0].approver, AuditApprover::Policy);
    }
    // The `never` policy: the table bypasses it, the rest still denies.
    {
        let dir = TempDir::new("readonly-never");
        let profile = Profile::workspace_write(dir.path())
            .unwrap()
            .with_approval(ApprovalPolicy::Never);
        let audit = Arc::new(RecordingAudit::default());
        let (hooks, approver) = hooks_with(profile, vec![], &audit);
        let decision = decide(&hooks, "git status").await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Allow),
            "never must keep read-only commands, got {decision:?}"
        );
        assert_eq!(approver.calls(), 0);
        let decision = decide(&hooks, "rm -rf x").await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "never must still deny the rest, got {decision:?}"
        );
    }
}
