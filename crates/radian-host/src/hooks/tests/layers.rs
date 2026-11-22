use super::*;

/// Slice B: the project layer runs first, so the user's hook receives the arguments the
/// project hook rewrote — and the call rewrites to exactly that. Swapping the layer
/// order fails this: the user hook would see the original input.
#[tokio::test]
async fn the_project_layer_runs_first_and_threads_updated_input_into_the_user_hook() {
    let root = TempRoot::new("layer-order");
    let marker = root.dir.join("project-ran");
    let user_stdin = root.dir.join("user-stdin.json");
    let project = pre_config(
        "shell",
        &format!(
            "echo ran >> {}; printf '%s' '{{\"hookSpecificOutput\":{{\"updatedInput\":{{\"command\":\"from-project\"}}}}}}'",
            marker.display()
        ),
    );
    let user = pre_config("shell", &format!("cat > {}", user_stdin.display()));
    let fixture = layered(&user, &project);
    let decision = fixture
        .hooks
        .before_tool_call(
            &cx(),
            &call("shell", serde_json::json!({"command": "orig"})),
        )
        .await
        .unwrap();
    assert!(marker.exists(), "the project hook runs");
    let stdin: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&user_stdin).unwrap()).unwrap();
    assert_eq!(
        stdin["tool_input"],
        serde_json::json!({"command": "from-project"}),
        "the user hook sees the project-rewritten arguments: {stdin}"
    );
    match decision {
        ToolDecision::Rewrite { arguments } => {
            assert_eq!(arguments, serde_json::json!({"command": "from-project"}),)
        }
        other => panic!("the rewrite stands: {other:?}"),
    }
    let _ = root;
}

/// Slice B: a command string identical in both layers runs once per event. Dropping
/// the dedupe runs it twice and fails the count.
#[tokio::test]
async fn an_identical_command_in_both_layers_runs_once() {
    let root = TempRoot::new("dedupe");
    let counter = root.dir.join("runs");
    let script = format!("echo ran >> {}; exit 0", counter.display());
    let project = pre_config("shell", &script);
    let user = pre_config("shell", &script);
    let fixture = layered(&user, &project);
    let decision = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap();
    assert!(matches!(decision, ToolDecision::Allow), "{decision:?}");
    assert_eq!(
        std::fs::read_to_string(&counter).unwrap().lines().count(),
        1,
        "one run for two identical namings"
    );
}

/// Slice B: a deny from any layer stops the rest — a project deny never reaches the
/// user's hooks. Letting later hooks run after a deny touches the marker and fails.
#[tokio::test]
async fn a_project_deny_stops_the_user_hooks() {
    let root = TempRoot::new("deny-stops");
    let marker = root.dir.join("user-ran");
    let project = pre_config("shell", "echo denied-by-project >&2; exit 2");
    let user = pre_config("shell", &format!("touch {}", marker.display()));
    let fixture = layered(&user, &project);
    let decision = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap();
    match decision {
        ToolDecision::Deny { reason } => assert!(
            reason.contains("denied-by-project"),
            "the project reason stands: {reason}"
        ),
        other => panic!("the deny stands: {other:?}"),
    }
    assert!(
        !marker.exists(),
        "a deny stops the later hooks: the user hook must not run"
    );
    let _ = root;
}

/// Slice B: two `Stop` exit-2 verdicts open a single continuation with both reasons
/// joined in one message, counted as one wake. Opening two turns (or two wakes)
/// fails the claim and the count.
#[tokio::test]
async fn two_stop_exit_2_verdicts_open_one_continuation_with_both_reasons() {
    let project = stop_config("echo project-reason >&2; exit 2");
    let user = stop_config("echo user-reason >&2; exit 2");
    let fixture = layered(&user, &project);
    fixture.hooks.run_stop_once(&completed_quiet()).await;
    let entry = fixture
        .inbox
        .claim_next_turn()
        .await
        .expect("the one continuation opens a turn");
    assert!(
        entry.message.text().contains("project-reason"),
        "the project reason rides along: {}",
        entry.message.text()
    );
    assert!(
        entry.message.text().contains("user-reason"),
        "the user reason joins it in the same message: {}",
        entry.message.text()
    );
    assert_eq!(fixture.hooks.stop_wakes(), 1, "one wake, not two");
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "no second continuation opens"
    );
}

/// Slice B: identical `Stop` commands run once: one turn, one wake, one execution.
#[tokio::test]
async fn an_identical_stop_command_runs_once() {
    let root = TempRoot::new("stop-dedupe");
    let counter = root.dir.join("runs");
    let script = format!(
        "echo ran >> {}; echo same-reason >&2; exit 2",
        counter.display()
    );
    let project = stop_config(&script);
    let user = stop_config(&script);
    let (warn, _) = warnings_collector();
    let (wake, _) = warnings_collector();
    let (intent, _) = warnings_collector();
    let (hook_stop, _) = warnings_collector();
    let inbox = Inbox::new(64);
    let hooks = CommandHooks::new(
        &user,
        &project,
        &root.ws,
        "test-session",
        SandboxSlot::empty(),
        &inbox,
        &tokio::runtime::Handle::current(),
        warn,
        wake,
        intent,
        hook_stop,
    );
    hooks.run_stop_once(&completed_quiet()).await;
    let entry = inbox
        .claim_next_turn()
        .await
        .expect("the one continuation opens a turn");
    assert_eq!(
        entry.message.text().matches("same-reason").count(),
        1,
        "the reason appears once: {}",
        entry.message.text()
    );
    assert_eq!(hooks.stop_wakes(), 1);
    assert_eq!(
        std::fs::read_to_string(&counter).unwrap().lines().count(),
        1,
        "one execution for two identical namings"
    );
}

/// Slice B (project check-after-edit): without a project `PostToolUse` an edit starts
/// no hook — the result passes through untouched.
#[tokio::test]
async fn without_a_project_post_tool_use_an_edit_starts_no_hook() {
    let project = HooksConfig {
        post_tool_use: vec![entry("shell", vec![hook("echo no-edit >&2; exit 2")])],
        ..HooksConfig::default()
    };
    let fixture = layered(&HooksConfig::default(), &project);
    let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
    fixture
        .hooks
        .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.text(), "patched");
}

/// Slice B (project check-after-edit): a project `PostToolUse` on an edit runs
/// synchronously, once per edit, its feedback riding directly after the edit's result —
/// before the user's own feedback. Swapping the layers fails the order.
#[tokio::test]
async fn a_project_post_tool_use_runs_once_per_edit_with_its_feedback_first() {
    let project = HooksConfig {
        post_tool_use: vec![entry("patch", vec![hook("echo project-lint >&2; exit 2")])],
        ..HooksConfig::default()
    };
    let user = HooksConfig {
        post_tool_use: vec![entry("patch", vec![hook("echo user-lint >&2; exit 2")])],
        ..HooksConfig::default()
    };
    let fixture = layered(&user, &project);
    let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
    fixture
        .hooks
        .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
        .await
        .unwrap();
    assert!(result.is_error, "feedback is an error signal");
    let text = result.text();
    assert!(text.contains("patched"), "the result stays: {text}");
    let project_at = text.find("project-lint").expect("project feedback: {text}");
    let user_at = text.find("user-lint").expect("user feedback: {text}");
    assert!(
        project_at < user_at,
        "the project feedback rides first: {text}"
    );
}
