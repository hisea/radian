//! Rule model, priority, the shell bypass list, composite deny, presets, and resolved-path
//! matching.

mod common;

use common::{TempDir, call};
use radian_permissions::*;
use serde_json::{Value, json};

fn rule(tool: &str, resource: ResourceMatcher, action: Action) -> Rule {
    Rule::new(ToolMatcher::Name(tool.into()), resource, action)
}

fn profile(dir: &TempDir, rules: Vec<Rule>) -> Profile {
    Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::OnAsk,
        dir.path(),
        rules,
    )
    .unwrap()
}

fn action_of(profile: &Profile, name: &str, args: Value) -> Action {
    evaluate(profile.rules(), &profile.workspace, &call(name, args)).action
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|w| (*w).to_owned()).collect()
}

/// Test 1: `deny` beats `ask` beats `allow`, and no match is `ask`.
#[test]
fn priority_deny_beats_ask_beats_allow_and_default_is_ask() {
    let dir = TempDir::new("priority");
    let profile = profile(
        &dir,
        vec![
            rule("write", ResourceMatcher::Any, Action::Allow),
            rule("write", ResourceMatcher::Any, Action::Deny),
            rule("shell", ResourceMatcher::Any, Action::Allow),
            rule("shell", ResourceMatcher::Any, Action::Ask),
        ],
    );
    assert_eq!(
        action_of(&profile, "write", json!({"path": "a.txt"})),
        Action::Deny,
        "deny wins over allow"
    );
    assert_eq!(
        action_of(&profile, "shell", json!({"command": "ls"})),
        Action::Ask,
        "ask wins over allow"
    );
    assert_eq!(
        action_of(&profile, "read", json!({"path": "a.txt"})),
        Action::Ask,
        "no matching rule means ask"
    );
}

/// Test 2: an `allow git status` rule never matches a command with a metacharacter.
#[test]
fn shell_metacharacters_are_never_allowed() {
    let dir = TempDir::new("metachars");
    let profile = profile(
        &dir,
        vec![rule(
            "shell",
            ResourceMatcher::CommandPrefix(words(&["git", "status"])),
            Action::Allow,
        )],
    );
    assert_eq!(
        action_of(&profile, "shell", json!({"command": "git status --short"})),
        Action::Allow,
        "a simple command is allowed"
    );
    // The exact list from `radian_permissions::shell::METACHARACTERS`, plus the pairs.
    let suffixes = [
        "; rm -rf /",
        " && rm -rf /",
        " || rm -rf /",
        " | sh",
        " `touch /tmp/x`",
        " $(touch /tmp/x)",
        " $HOME",
        " > /tmp/x",
        " >> /tmp/x",
        " < /etc/passwd",
        "\nrm -rf /",
        " (rm -rf /)",
        " & rm -rf /",
    ];
    for suffix in suffixes {
        let command = format!("git status{suffix}");
        let action = action_of(&profile, "shell", json!({"command": command}));
        assert_ne!(action, Action::Allow, "{command:?} must not be allowed");
        assert_eq!(action, Action::Ask, "{command:?} must fall through to ask");
    }
    // The metacharacter constant the docs point at stays in sync with the test list.
    assert!(shell::METACHARACTERS.contains(&";"));
    assert!(shell::METACHARACTERS.contains(&"$("));
}

/// Test 3: any segment hitting `deny` denies the whole composite command.
#[test]
fn a_deny_segment_denies_the_whole_composite_command() {
    let dir = TempDir::new("composite");
    let profile = profile(
        &dir,
        vec![
            rule(
                "shell",
                ResourceMatcher::CommandPrefix(words(&["rm"])),
                Action::Deny,
            ),
            rule(
                "shell",
                ResourceMatcher::CommandPrefix(words(&["git", "status"])),
                Action::Allow,
            ),
        ],
    );
    for command in [
        "git status; rm -rf /",
        "git status && rm -rf /",
        "git status || rm -rf /",
        "git status | rm -rf /",
        "echo $(rm -rf /)",
        "sh -c 'rm -rf /'",
        "/bin/rm -rf /",
    ] {
        assert_eq!(
            action_of(&profile, "shell", json!({"command": command})),
            Action::Deny,
            "{command:?}"
        );
    }
    assert_eq!(
        action_of(&profile, "shell", json!({"command": "git status"})),
        Action::Allow
    );
}

/// Test 10: the presets differ — `workspace` asks for MCP and `full-access` allows it, while
/// both allow a workspace write. The preset and the sandbox mode travel together.
#[test]
fn workspace_asks_for_mcp_where_full_access_allows_it() {
    let dir = TempDir::new("presets");
    let workspace = Profile::workspace_write(dir.path()).unwrap();
    let full = Profile::full_access(dir.path()).unwrap();
    let write = json!({"path": "a.txt"});

    assert_eq!(action_of(&workspace, "write", write.clone()), Action::Allow);
    assert_eq!(
        action_of(&workspace, "shell", json!({"command": "ls"})),
        Action::Ask
    );
    assert_eq!(action_of(&full, "write", write.clone()), Action::Allow);
    assert_eq!(
        action_of(&full, "shell", json!({"command": "ls"})),
        Action::Ask
    );
    // MCP tools ask in `workspace` and are allowed in `full-access`.
    assert_eq!(
        action_of(&workspace, "server__tool", json!({})),
        Action::Ask
    );
    assert_eq!(action_of(&full, "server__tool", json!({})), Action::Allow);
    // Read-only built-ins are allowed everywhere.
    assert_eq!(action_of(&workspace, "read", write.clone()), Action::Allow);
    // The preset and the sandbox mode travel together.
    assert_eq!(
        workspace.sandbox,
        radian_sandbox::SandboxMode::WorkspaceWrite
    );
    assert_eq!(full.sandbox, radian_sandbox::SandboxMode::FullAccess);
}

/// Test 11: a path rule matches the resolved path; `..` and symlinks cannot bypass it.
#[test]
fn rules_match_resolved_paths_so_dotdot_and_symlinks_cannot_bypass() {
    let dir = TempDir::new("paths");
    let ws = dir.path().join("ws");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, ws.join("link")).unwrap();

    // The profile's workspace is `ws` so the rule window and the resolution root agree.
    let profile = Profile::custom(
        Preset::WorkspaceWrite,
        ApprovalPolicy::OnAsk,
        &ws,
        vec![rule(
            "write",
            ResourceMatcher::PathUnder(ws.join("sub")),
            Action::Allow,
        )],
    )
    .unwrap();
    let _ = profile;

    // Within `sub` → allow.
    assert_eq!(
        evaluate(
            profile.rules(),
            &profile.workspace,
            &call("write", json!({"path": "sub/a.txt"}))
        )
        .action,
        Action::Allow
    );
    // `..` inside then back → still under `sub` → allow.
    assert_eq!(
        evaluate(
            profile.rules(),
            &profile.workspace,
            &call("write", json!({"path": "sub/../sub/b.txt"}))
        )
        .action,
        Action::Allow
    );
    // `..` out of `sub` → no match → ask.
    assert_eq!(
        evaluate(
            profile.rules(),
            &profile.workspace,
            &call("write", json!({"path": "sub/../../outside/a.txt"}))
        )
        .action,
        Action::Ask
    );
    // Absolute path outside → ask.
    assert_eq!(
        evaluate(
            profile.rules(),
            &profile.workspace,
            &call(
                "write",
                json!({"path": outside.join("a.txt").to_string_lossy()})
            )
        )
        .action,
        Action::Ask
    );
    #[cfg(unix)]
    {
        // A symlink inside the workspace pointing outside `sub` → resolves outside → ask.
        assert_eq!(
            evaluate(
                profile.rules(),
                &profile.workspace,
                &call("write", json!({"path": "link/a.txt"}))
            )
            .action,
            Action::Ask
        );
    }
}
