//! Compound shell commands: allow only when every segment is allowed.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{ScriptedApprover, TempDir, call, ctx};
use radian_core::{Hooks, InputSource, ToolDecision};
use radian_permissions::*;
use serde_json::json;

/// A workspace `ws` with `sub/f.txt` inside, `outside/passwd` next to it, and a
/// `sub/link -> outside` symlink: `cd sub && cat link/passwd` reads outside the
/// workspace when paths resolve from the `cd` target, and looks innocent when
/// they resolve from the workspace root.
struct Ws {
    base: PathBuf,
    ws: PathBuf,
}

impl Ws {
    fn new(label: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("radian-compound-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ws = base.join("ws");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("sub").join("f.txt"), "x").unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::write(base.join("outside").join("passwd"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(base.join("outside"), ws.join("sub").join("link")).unwrap();
        let ws = std::fs::canonicalize(&ws).unwrap();
        Self { base, ws }
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// A host-style profile: the tier's blanket shell `ask` replaced by the allowlist
/// (see `radian-exec/src/profile.rs`), plus any named rules the test needs.
fn profile_with_allow(
    ws: &Path,
    preset: Preset,
    approval: ApprovalPolicy,
    allow: &[&str],
    extra: Vec<Rule>,
) -> Profile {
    let base = match preset {
        Preset::WorkspaceWrite => Profile::workspace_write(ws).unwrap(),
        Preset::FullAccess => Profile::full_access(ws).unwrap(),
    };
    let mut rules: Vec<Rule> = base
        .rules()
        .iter()
        .filter(|rule| {
            !(matches!(&rule.tool, ToolMatcher::Name(name) if name == "shell")
                && rule.action == Action::Ask)
        })
        .cloned()
        .collect();
    for raw in allow {
        let prefix: Vec<String> = raw.split_whitespace().map(str::to_owned).collect();
        assert!(!prefix.is_empty(), "empty allow prefix");
        rules.push(Rule::new(
            ToolMatcher::Name("shell".into()),
            ResourceMatcher::CommandPrefix(prefix),
            Action::Allow,
        ));
    }
    rules.extend(extra);
    Profile::custom(preset, approval, ws, rules).unwrap()
}

fn workspace_with_allow(ws: &Path, allow: &[&str]) -> Profile {
    profile_with_allow(
        ws,
        Preset::WorkspaceWrite,
        ApprovalPolicy::OnAsk,
        allow,
        vec![],
    )
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

async fn decide(
    hooks: &PermissionHooks,
    command: &str,
) -> Result<ToolDecision, radian_core::HookError> {
    let call = call("shell", json!({ "command": command }));
    Hooks::before_tool_call(hooks, &ctx(), &call).await
}

fn deny_answer() -> Vec<ApprovalOutcome> {
    vec![ApprovalOutcome::Deny { reason: None }]
}

/// `cargo fmt && cargo test` with both allow rules is allowed without a prompt.
#[tokio::test]
async fn compound_allows_when_every_segment_is_allowed() {
    let dir = TempDir::new("compound-allow");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        workspace_with_allow(dir.path(), &["cargo fmt", "cargo test"]),
        vec![],
        &audit,
    );
    for command in [
        "cargo fmt && cargo test",
        "cargo fmt&&cargo test",
        "cargo fmt || cargo test",
        "cargo fmt; cargo test",
        // Mixed separators, mixed sources: the rules cover `cargo`, the table
        // covers `git status`.
        "cargo fmt && cargo test || git status",
    ] {
        let decision = decide(&hooks, command).await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Allow),
            "{command:?} must be allowed, got {decision:?}"
        );
    }
    assert_eq!(approver.calls(), 0, "no segment must ask");
    let events = audit.events();
    assert!(!events.is_empty());
    for event in &events {
        assert_eq!(event.decision, ResolvedDecision::Allow);
        assert_eq!(event.approver, AuditApprover::Policy);
    }
}

/// `cargo fmt && cargo test` with a rule for `cargo fmt` only asks: one allowed
/// segment is not enough. Replacing the every-segment check with an
/// any-segment check allows this command, so this test is that mutation's catcher.
#[tokio::test]
async fn compound_allows_only_when_every_segment_is_allowed() {
    let dir = TempDir::new("compound-every");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        workspace_with_allow(dir.path(), &["cargo fmt"]),
        deny_answer(),
        &audit,
    );
    let decision = decide(&hooks, "cargo fmt && cargo test").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "a segment without a rule must ask, got {decision:?}"
    );
    assert_eq!(
        approver.calls(),
        1,
        "the missing rule must reach the approver"
    );
    assert_eq!(audit.events()[0].approver, AuditApprover::User);
}

/// `ls | xargs rm` asks even with allow rules for both: `xargs` runs its
/// arguments, so no prefix rule and no table entry covers that segment. Dropping
/// the runs-its-arguments table allows this command, so this test is that
/// mutation's catcher.
#[tokio::test]
async fn pipe_into_a_wrapper_always_asks() {
    for command in ["ls | xargs rm", "xargs rm", "ls | sudo ls"] {
        let dir = TempDir::new("compound-wrapper");
        let audit = Arc::new(RecordingAudit::default());
        let (hooks, approver) = hooks_with(
            workspace_with_allow(dir.path(), &["ls", "xargs", "sudo"]),
            deny_answer(),
            &audit,
        );
        let decision = decide(&hooks, command).await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "{command:?} must ask, got {decision:?}"
        );
        assert_eq!(approver.calls(), 1, "{command:?} must reach the approver");
        assert_eq!(audit.events()[0].approver, AuditApprover::User);
    }
}

/// A separator inside quotes never splits into allow-able pieces: the quotes
/// survive in the segment, so the command is not simple and asks.
#[tokio::test]
async fn quoted_separators_never_allow() {
    for command in [
        "echo 'a && b'",
        "cargo fmt && echo \"a && b\"",
        "git status && echo 'x'",
    ] {
        let dir = TempDir::new("compound-quotes");
        let audit = Arc::new(RecordingAudit::default());
        let (hooks, approver) = hooks_with(
            workspace_with_allow(dir.path(), &["cargo fmt", "echo", "git status"]),
            deny_answer(),
            &audit,
        );
        let decision = decide(&hooks, command).await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "{command:?} must ask, got {decision:?}"
        );
        assert_eq!(approver.calls(), 1, "{command:?} must reach the approver");
    }
}

/// `cd sub && cargo test` is allowed in the `workspace` tier when `sub` is
/// inside the workspace and both segments have allow rules.
#[tokio::test]
async fn cd_into_the_workspace_allows_with_a_rule() {
    let ws = Ws::new("cd-inside");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        workspace_with_allow(&ws.ws, &["cd", "cargo test"]),
        vec![],
        &audit,
    );
    let decision = decide(&hooks, "cd sub && cargo test").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "cd into the workspace must be allowed, got {decision:?}"
    );
    assert_eq!(approver.calls(), 0);
    assert_eq!(audit.events()[0].approver, AuditApprover::Policy);
}

/// `cd .. && cargo test` from the workspace root asks: the target leaves the
/// workspace, so the `cd` segment has no cover.
#[tokio::test]
async fn cd_out_of_the_workspace_asks() {
    let ws = Ws::new("cd-outside");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        workspace_with_allow(&ws.ws, &["cd", "cargo test"]),
        deny_answer(),
        &audit,
    );
    let decision = decide(&hooks, "cd .. && cargo test").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "cd out of the workspace must ask, got {decision:?}"
    );
    assert_eq!(approver.calls(), 1);
}

/// Paths after a `cd` resolve from the `cd` target, not from the workspace root:
/// `sub/link` points outside, so `cd sub && cat link/passwd` must ask, while
/// `cd sub && cat f.txt` stays allowed. Resolving the later paths from the root
/// instead allows the first command, so this test is that mutation's catcher.
#[tokio::test]
async fn cd_resolves_later_paths_from_the_cd_target() {
    #[cfg(not(unix))]
    return;
    // The `cd` needs a rule; `cat` is judged by the read-only table against the
    // effective directory.
    let ws = Ws::new("cd-target");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) =
        hooks_with(workspace_with_allow(&ws.ws, &["cd"]), deny_answer(), &audit);
    let decision = decide(&hooks, "cd sub && cat link/passwd").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "cat through a symlink out must ask, got {decision:?}"
    );
    assert_eq!(approver.calls(), 1);
    assert_eq!(audit.events()[0].approver, AuditApprover::User);

    // The control: a path that stays inside from the `cd` target is allowed.
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(workspace_with_allow(&ws.ws, &["cd"]), vec![], &audit);
    let decision = decide(&hooks, "cd sub && cat f.txt").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "cat inside the cd target must be allowed, got {decision:?}"
    );
    assert_eq!(approver.calls(), 0);
}

/// A table segment in a compound command is rewritten in place: `cargo fmt`
/// keeps its text (it came from an allow rule) while the `git log` segment
/// gains the two flags, and every separator stays as it was.
#[tokio::test]
async fn compound_rewrite_keeps_separators_and_flags() {
    let dir = TempDir::new("compound-rewrite");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        workspace_with_allow(dir.path(), &["cargo fmt"]),
        vec![],
        &audit,
    );
    let decision = decide(&hooks, "cargo fmt && git log -p").await.unwrap();
    let ToolDecision::Rewrite { arguments } = decision else {
        panic!("the git log segment must be rewritten, got {decision:?}");
    };
    assert_eq!(
        arguments.get("command").and_then(|v| v.as_str()),
        Some("cargo fmt && git log --no-ext-diff --no-textconv -p")
    );
    assert_eq!(approver.calls(), 0, "a rewrite must not ask");
    let events = audit.events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].decision, ResolvedDecision::Rewrite);
    assert_eq!(events[0].approver, AuditApprover::Policy);
}

/// `full` trusts the environment: a `cd` segment is judged like any other there — the
/// tier's blanket allow covers it, in both `on-ask` and `always` (where the blanket
/// allow is still reviewed). The `workspace` tier's `cd` restrictions are unchanged.
#[tokio::test]
async fn cd_allows_in_the_full_tier() {
    let ws = Ws::new("cd-full");
    for approval in [ApprovalPolicy::OnAsk, ApprovalPolicy::Always] {
        let audit = Arc::new(RecordingAudit::default());
        let answers = match approval {
            ApprovalPolicy::Always => vec![ApprovalOutcome::AllowOnce],
            _ => deny_answer(),
        };
        let (hooks, approver) = hooks_with(
            profile_with_allow(
                &ws.ws,
                Preset::FullAccess,
                approval,
                &["cd", "cargo test"],
                vec![],
            ),
            answers,
            &audit,
        );
        let decision = decide(&hooks, "cd sub && cargo test").await.unwrap();
        assert!(
            matches!(decision, ToolDecision::Allow),
            "{approval:?}: cd in the full tier is allowed, got {decision:?}"
        );
        if approval == ApprovalPolicy::Always {
            assert_eq!(approver.calls(), 1, "{approval:?}: must still be reviewed");
            assert_eq!(audit.events()[0].approver, AuditApprover::User);
        } else {
            assert_eq!(approver.calls(), 0, "{approval:?}: must not ask");
            assert_eq!(audit.events()[0].approver, AuditApprover::Policy);
        }
    }
}

/// Read-only segments of a compound command bypass `approval = always`, the way
/// a single table hit does — but a rule-allowed segment next to them is still
/// reviewed.
#[tokio::test]
async fn compound_table_hits_bypass_always() {
    // Pure table: `git status && ls -la` stays allowed under `always`, with the
    // tier's blanket shell `ask` left in place.
    let dir = TempDir::new("compound-always-table");
    let profile = Profile::workspace_write(dir.path())
        .unwrap()
        .with_approval(ApprovalPolicy::Always);
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(profile, vec![], &audit);
    let decision = decide(&hooks, "git status && ls -la").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "pure table compound must stay allowed under always, got {decision:?}"
    );
    assert_eq!(approver.calls(), 0);

    // Mixed: `cargo fmt` comes from an allow rule, so `always` reviews it.
    let dir = TempDir::new("compound-always-mixed");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        profile_with_allow(
            dir.path(),
            Preset::WorkspaceWrite,
            ApprovalPolicy::Always,
            &["cargo fmt"],
            vec![],
        ),
        deny_answer(),
        &audit,
    );
    let decision = decide(&hooks, "cargo fmt && git status").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "a rule-allowed segment must be reviewed under always, got {decision:?}"
    );
    assert_eq!(approver.calls(), 1);
}

/// A rule denial still wins over every segment: deny runs first, unchanged.
#[tokio::test]
async fn deny_still_denies_a_compound_first() {
    let dir = TempDir::new("compound-deny");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        profile_with_allow(
            dir.path(),
            Preset::WorkspaceWrite,
            ApprovalPolicy::OnAsk,
            &["cargo fmt"],
            vec![Rule::new(
                ToolMatcher::Name("shell".into()),
                ResourceMatcher::CommandPrefix(vec!["rm".into()]),
                Action::Deny,
            )],
        ),
        vec![],
        &audit,
    );
    let decision = decide(&hooks, "cargo fmt && rm -f x").await.unwrap();
    let ToolDecision::Deny { reason } = decision else {
        panic!("a deny rule must deny the compound, got {decision:?}");
    };
    assert!(!reason.is_empty());
    assert_eq!(approver.calls(), 0, "a deny must not reach the approver");
    assert_eq!(audit.events()[0].approver, AuditApprover::Policy);
}

/// A named user `ask` rule blocks its segment: the whole command prompts.
#[tokio::test]
async fn named_user_ask_blocks_its_segment() {
    let dir = TempDir::new("compound-user-ask");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        profile_with_allow(
            dir.path(),
            Preset::WorkspaceWrite,
            ApprovalPolicy::OnAsk,
            &["cargo fmt"],
            vec![
                Rule::new(
                    ToolMatcher::Name("shell".into()),
                    ResourceMatcher::CommandPrefix(vec!["cargo".into(), "test".into()]),
                    Action::Ask,
                )
                .with_reason("the user wants to review tests"),
            ],
        ),
        vec![ApprovalOutcome::AllowOnce],
        &audit,
    );
    let decision = decide(&hooks, "cargo fmt && cargo test").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "the approver's answer decides, got {decision:?}"
    );
    assert_eq!(approver.calls(), 1, "a user ask rule must prompt");
    assert_eq!(audit.events()[0].approver, AuditApprover::User);
}

/// A simple command is one segment, so the wrapper and `cd` restrictions hold
/// for it too: `xargs` never allows by prefix rule, `cd` only into the workspace.
#[tokio::test]
async fn single_commands_take_the_same_segment_path() {
    let ws = Ws::new("single-segments");
    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(
        workspace_with_allow(&ws.ws, &["xargs", "cd"]),
        deny_answer(),
        &audit,
    );
    let decision = decide(&hooks, "xargs rm x").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "a wrapper segment must ask, got {decision:?}"
    );
    assert_eq!(approver.calls(), 1);

    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) = hooks_with(workspace_with_allow(&ws.ws, &["cd"]), vec![], &audit);
    let decision = decide(&hooks, "cd sub").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "cd into the workspace must be allowed, got {decision:?}"
    );
    assert_eq!(approver.calls(), 0);

    let audit = Arc::new(RecordingAudit::default());
    let (hooks, approver) =
        hooks_with(workspace_with_allow(&ws.ws, &["cd"]), deny_answer(), &audit);
    let decision = decide(&hooks, "cd ..").await.unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "cd out of the workspace must ask, got {decision:?}"
    );
    assert_eq!(approver.calls(), 1);
}
