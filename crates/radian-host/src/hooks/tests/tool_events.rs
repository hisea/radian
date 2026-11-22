use super::*;

/// Exit 2 denies the call with the hook's stderr as the reason.
#[tokio::test]
async fn pre_tool_use_exit_2_denies_with_the_stderr_reason() {
    let fixture = plain(&pre_config("shell", "echo blocked-by-policy >&2; exit 2"));
    let decision = fixture
        .hooks
        .before_tool_call(
            &cx(),
            &call("shell", serde_json::json!({"command": "rm -rf /"})),
        )
        .await
        .unwrap();
    match decision {
        ToolDecision::Deny { reason } => assert!(
            reason.contains("blocked-by-policy"),
            "the stderr is the reason: {reason}"
        ),
        other => panic!("exit 2 must deny, got {other:?}"),
    }
}

/// `hookSpecificOutput.permissionDecision: "deny"` denies, with its reason.
#[tokio::test]
async fn permission_decision_deny_denies() {
    let script = r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"deny","reason":"custom-deny"}}'"#;
    let fixture = plain(&pre_config("write", script));
    let decision = fixture
        .hooks
        .before_tool_call(&cx(), &call("write", serde_json::json!({"path": "a"})))
        .await
        .unwrap();
    match decision {
        ToolDecision::Deny { reason } => {
            assert!(reason.contains("custom-deny"), "{reason}")
        }
        other => panic!("deny must deny, got {other:?}"),
    }
}

/// `"allow"` (and `"ask"`) are refused with a warning and treated as no decision: the
/// hook-level verdict stays `Allow`, and the permission rules below still decide.
#[tokio::test]
async fn allow_is_refused_and_treated_as_no_decision() {
    for decision in ["allow", "ask"] {
        let script = format!(
            r#"printf '%s' '{{"hookSpecificOutput":{{"permissionDecision":"{decision}"}}}}'"#
        );
        let fixture = plain(&pre_config("shell", &script));
        let verdict = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap();
        assert!(
            matches!(verdict, ToolDecision::Allow),
            "{decision} must not decide: {verdict:?}"
        );
        assert!(
            warned(&fixture.warnings)
                .iter()
                .any(|warning| warning.contains("refused") && warning.contains(decision)),
            "{decision} must warn: {:?}",
            warned(&fixture.warnings)
        );
    }
}

/// `updatedInput` rewrites the call's arguments (re-validated, hooks not re-run).
#[tokio::test]
async fn updated_input_rewrites_the_call() {
    let script = r#"printf '%s' '{"hookSpecificOutput":{"updatedInput":{"command":"echo safe"}}}'"#;
    let fixture = plain(&pre_config("shell", script));
    let decision = fixture
        .hooks
        .before_tool_call(
            &cx(),
            &call("shell", serde_json::json!({"command": "echo evil"})),
        )
        .await
        .unwrap();
    match decision {
        ToolDecision::Rewrite { arguments } => assert_eq!(
            arguments,
            serde_json::json!({"command": "echo safe"}),
            "the hook input replaces the arguments"
        ),
        other => panic!("updatedInput must rewrite, got {other:?}"),
    }
}

/// A non-object `updatedInput` is a hook error: failing closed beats running a call
/// with arguments the hook malformed.
#[tokio::test]
async fn non_object_updated_input_is_a_hook_error() {
    let script = r#"printf '%s' '{"hookSpecificOutput":{"updatedInput":["not","an","object"]}}'"#;
    let fixture = plain(&pre_config("shell", script));
    let error = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("updatedInput"), "{error}");
}

/// PostToolUse exit 2 feeds the hook's stderr back to the model with the result.
#[tokio::test]
async fn post_tool_use_exit_2_feeds_the_reason_back() {
    let config = HooksConfig {
        post_tool_use: vec![entry("patch", vec![hook("echo lint-broke >&2; exit 2")])],
        ..HooksConfig::default()
    };
    let fixture = plain(&config);
    let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
    fixture
        .hooks
        .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
        .await
        .unwrap();
    assert!(result.is_error, "feedback is an error signal");
    assert!(result.text().contains("patched"), "the result stays");
    assert!(
        result.text().contains("lint-broke"),
        "the reason rides along: {}",
        result.text()
    );
}

/// Edge case 1: exit-0 `additionalContext` rides back with the result and does **not**
/// mark it an error. Not reading stdout (the old PostToolUse behavior) leaves the result
/// as the tool wrote it and fails this.
#[tokio::test]
async fn post_tool_use_additional_context_feeds_back_without_an_error() {
    let script = r#"printf '%s' '{"hookSpecificOutput":{"additionalContext":"reformatted"}}'"#;
    let config = HooksConfig {
        post_tool_use: vec![entry("patch", vec![hook(script)])],
        ..HooksConfig::default()
    };
    let fixture = plain(&config);
    let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
    fixture
        .hooks
        .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
        .await
        .unwrap();
    assert!(
        !result.is_error,
        "additionalContext is not an error signal: {:?}",
        result
    );
    assert_eq!(
        result.text(),
        "patched[PostToolUse hook] reformatted",
        "the feedback appends to the result"
    );
}

/// Edge case 2: `decision: "block"` with a reason is the JSON form of exit 2: the same
/// result text and the same error mark. Ignoring the exit-0 object leaves the result
/// untouched and fails the equality.
#[tokio::test]
async fn post_tool_use_block_decision_matches_exit_2() {
    let config = HooksConfig {
        post_tool_use: vec![entry(
            "patch",
            vec![hook("printf '%s' 'lint-broke' >&2; exit 2")],
        )],
        ..HooksConfig::default()
    };
    let exit_2 = plain(&config);
    let mut from_exit = ToolResult::success("c1", vec![ContentPart::text("patched")]);
    exit_2
        .hooks
        .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut from_exit)
        .await
        .unwrap();

    let script = r#"printf '%s' '{"decision":"block","reason":"lint-broke"}'"#;
    let config = HooksConfig {
        post_tool_use: vec![entry("patch", vec![hook(script)])],
        ..HooksConfig::default()
    };
    let block = plain(&config);
    let mut from_block = ToolResult::success("c1", vec![ContentPart::text("patched")]);
    block
        .hooks
        .after_tool_call(
            &cx(),
            &call("patch", serde_json::json!({})),
            &mut from_block,
        )
        .await
        .unwrap();

    assert!(from_exit.is_error && from_block.is_error);
    assert_eq!(
        from_block.text(),
        from_exit.text(),
        "block and exit 2 carry the same feedback"
    );
}

/// Edge case 3 (unit half): the project layer's `updatedToolOutput` replaces first, so
/// the user layer sees the replaced result on stdin and its own replacement wins.
/// Ignoring `updatedToolOutput` leaves the user's stdin and the final result at the
/// tool's original text and fails both.
#[tokio::test]
async fn the_user_hook_sees_the_project_replaced_result() {
    let root = TempRoot::new("post-replace");
    let stdin_path = root.dir.join("user-stdin.json");
    let project = HooksConfig {
        post_tool_use: vec![entry(
            "patch",
            vec![hook(
                r#"printf '%s' '{"hookSpecificOutput":{"updatedToolOutput":"from-project"}}'"#,
            )],
        )],
        ..HooksConfig::default()
    };
    let user_script = format!(
        "input=$(cat); printf '%s' \"$input\" > {}; \
         printf '%s' '{{\"hookSpecificOutput\":{{\"updatedToolOutput\":\"from-user\"}}}}'",
        stdin_path.display()
    );
    let user = HooksConfig {
        post_tool_use: vec![entry("patch", vec![hook(&user_script)])],
        ..HooksConfig::default()
    };
    let fixture = layered_with_root(root, &user, &project, SandboxSlot::empty());
    let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
    fixture
        .hooks
        .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
        .await
        .unwrap();
    let stdin: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&stdin_path).unwrap()).unwrap();
    let seen = stdin["tool_response"].to_string();
    assert!(
        seen.contains("from-project"),
        "the user hook sees the project replacement: {seen}"
    );
    assert_eq!(
        result.text(),
        "from-user",
        "the user layer's replacement is the final result"
    );
    assert!(
        stdin["tool_response"]["is_error"] == serde_json::json!(false),
        "the replacement keeps is_error: {stdin}"
    );
}

/// Edge case 4: an `updatedToolOutput` that is not a string fails closed as a hook
/// error, like a non-zero exit. Treating it as text or ignoring it returns `Ok` here.
#[tokio::test]
async fn a_non_string_updated_tool_output_is_a_hook_error() {
    for value in ["42", "{\"body\":\"no\"}"] {
        let script =
            format!(r#"printf '%s' '{{"hookSpecificOutput":{{"updatedToolOutput":{value}}}}}'"#);
        let config = HooksConfig {
            post_tool_use: vec![entry("patch", vec![hook(&script)])],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
        let error = fixture
            .hooks
            .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("updatedToolOutput"), "{error}");
    }
}

/// Rework 2, item 1: a present `updatedToolOutput` of JSON `null` is a value, not an
/// absent field, and `null` is not a string, so it fails closed like any other
/// non-string. Deserializing the field into a plain `Option<Value>` conflates `null`
/// with absence and leaves the result unchanged instead.
#[tokio::test]
async fn a_null_updated_tool_output_is_a_hook_error() {
    let script = r#"printf '%s' '{"hookSpecificOutput":{"updatedToolOutput":null}}'"#;
    let config = HooksConfig {
        post_tool_use: vec![entry("patch", vec![hook(script)])],
        ..HooksConfig::default()
    };
    let fixture = plain(&config);
    let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
    let error = fixture
        .hooks
        .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("updatedToolOutput"), "{error}");
    assert_eq!(result.text(), "patched", "the result is not replaced");
}

/// Edge case 7: a 10,000-character `additionalContext` keeps the truncation marker and
/// stays within 2,000 characters in total. Forwarding it whole fails the length and the
/// marker; keeping 2,000 and then appending the marker overshoots by 12.
#[tokio::test]
async fn a_long_additional_context_is_truncated() {
    let long = "x".repeat(10_000);
    let script =
        format!(r#"printf '%s' '{{"hookSpecificOutput":{{"additionalContext":"{long}"}}}}'"#);
    let config = HooksConfig {
        post_tool_use: vec![entry("patch", vec![hook(&script)])],
        ..HooksConfig::default()
    };
    let fixture = plain(&config);
    let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
    fixture
        .hooks
        .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
        .await
        .unwrap();
    let truncated = format!(
        "{}…[truncated]",
        "x".repeat(REASON_CHAR_LIMIT - TRUNCATION_MARKER.chars().count())
    );
    assert_eq!(truncated.chars().count(), REASON_CHAR_LIMIT);
    assert_eq!(
        result.text(),
        format!("patched[PostToolUse hook] {truncated}")
    );
}

/// Edge case 8: exit-0 stdout that is not JSON warns once and leaves the result alone
/// (the old PostToolUse never read stdout, so there was no warning and this failed).
#[tokio::test]
async fn a_non_json_post_tool_use_stdout_warns_once_and_leaves_the_result_alone() {
    let config = HooksConfig {
        post_tool_use: vec![entry("patch", vec![hook("printf '%s' 'not json'")])],
        ..HooksConfig::default()
    };
    let fixture = plain(&config);
    let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
    fixture
        .hooks
        .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
        .await
        .unwrap();
    assert_eq!(result.text(), "patched");
    assert!(!result.is_error);
    let warnings = warned(&fixture.warnings);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("not JSON"), "{warnings:?}");
}

/// `permissionDecision` on `PostToolUse` and `permissionDecision`/`updatedInput` on
/// `Stop` are ignored with one warning line and change nothing.
#[tokio::test]
async fn fields_only_pre_tool_use_uses_warn_once_and_are_ignored() {
    let script = r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"deny"}}'"#;
    let config = HooksConfig {
        post_tool_use: vec![entry("patch", vec![hook(script)])],
        ..HooksConfig::default()
    };
    let fixture = plain(&config);
    let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
    fixture
        .hooks
        .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
        .await
        .unwrap();
    assert_eq!(result.text(), "patched");
    assert_eq!(
        warned(&fixture.warnings).len(),
        1,
        "{:?}",
        warned(&fixture.warnings)
    );

    let script = r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"deny","updatedInput":{"command":"x"}}}'"#;
    let fixture = plain(&stop_config(script));
    fixture.hooks.run_stop_once(&completed_quiet()).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "the ignored fields open no turn"
    );
    assert_eq!(
        warned(&fixture.warnings).len(),
        1,
        "{:?}",
        warned(&fixture.warnings)
    );
}

/// A hook that overruns its timeout fails closed.
#[tokio::test]
async fn a_hook_timeout_fails_closed() {
    let mut config = pre_config("shell", "sleep 30");
    config.pre_tool_use[0].hooks[0].timeout_secs = Some(1);
    let fixture = plain(&config);
    let error = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error}");
}

/// A matcher that does not match never runs the hook (an `exit 2` hook on another
/// tool leaves this call alone).
#[tokio::test]
async fn a_non_matching_matcher_does_not_run_the_hook() {
    let fixture = plain(&pre_config("shell", "exit 2"));
    let decision = fixture
        .hooks
        .before_tool_call(&cx(), &call("read", serde_json::json!({})))
        .await
        .unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "an unmatched hook must not decide: {decision:?}"
    );
}

/// `*` matches every tool.
#[tokio::test]
async fn star_matcher_matches_every_tool() {
    let fixture = plain(&pre_config("*", "exit 2"));
    let decision = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "`*` must match `shell`: {decision:?}"
    );
}

/// A regex matcher matches the tool it describes.
#[tokio::test]
async fn regex_matcher_matches_the_tool_it_describes() {
    let fixture = plain(&pre_config("wri.*", "exit 2"));
    let decision = fixture
        .hooks
        .before_tool_call(&cx(), &call("write", serde_json::json!({})))
        .await
        .unwrap();
    assert!(
        matches!(decision, ToolDecision::Deny { .. }),
        "`wri.*` must match `write`: {decision:?}"
    );
}

/// The match is anchored to the whole tool name: `sh` does not match `shell`.
#[tokio::test]
async fn an_unanchored_matcher_does_not_match_a_prefix() {
    let fixture = plain(&pre_config("sh", "exit 2"));
    let decision = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "`sh` must not match `shell` (anchored): {decision:?}"
    );
}

/// An invalid pattern warns once at load time and the hook never runs.
#[tokio::test]
async fn an_invalid_matcher_warns_once_and_never_runs() {
    let config = pre_config("shell(", "exit 2");
    let fixture = plain(&config);
    assert!(
        warned(&fixture.warnings)
            .iter()
            .any(|warning| warning.contains("not a valid regex") && warning.contains("shell(")),
        "an invalid pattern must warn at load: {:?}",
        warned(&fixture.warnings)
    );
    let decision = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap();
    assert!(
        matches!(decision, ToolDecision::Allow),
        "an invalid matcher must never run: {decision:?}"
    );
}

/// Claude's stdin field names reach the hook: event, tool, input, cwd, session id.
#[tokio::test]
async fn the_hook_sees_claudes_stdin_fields() {
    let root = TempRoot::new("stdin");
    let stdin_path = root.dir.join("stdin.json");
    let config = pre_config("shell", &format!("cat > {}", stdin_path.display()));
    let (warn, _) = warnings_collector();
    let (wake, _) = warnings_collector();
    let (intent, _) = warnings_collector();
    let (hook_stop, _) = warnings_collector();
    let inbox = Inbox::new(64);
    let hooks = CommandHooks::new(
        &config,
        &HooksConfig::default(),
        &root.ws,
        "sid-9",
        SandboxSlot::empty(),
        &inbox,
        &tokio::runtime::Handle::current(),
        warn,
        wake,
        intent,
        hook_stop,
    );
    hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({"command": "hi"})))
        .await
        .unwrap();
    let stdin: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&stdin_path).unwrap()).unwrap();
    assert_eq!(stdin["hook_event_name"], "PreToolUse");
    assert_eq!(stdin["tool_name"], "shell");
    assert_eq!(stdin["tool_input"]["command"], "hi");
    assert_eq!(stdin["session_id"], "sid-9");
    assert_eq!(stdin["cwd"], root.ws.to_string_lossy().as_ref());
}

/// Edge case 1: arguments reach the program exactly as written. `/bin/sh` would split
/// `a b`, expand `$(…)` and treat `;`/`*`; the exec form must do none of that, and no
/// file may be created by the ``$(touch …)`` argument.
#[cfg(unix)]
#[tokio::test]
async fn exec_args_reach_the_program_verbatim_without_a_shell() {
    let root = TempRoot::new("exec-args");
    let out = root.ws.join("args.txt");
    let script = root.ws.join("capture");
    write_script(
        &script,
        &format!("printf '%s\\n' \"$@\" > {}", out.display()),
    );
    let marker = root.outside("expanded");
    let touch = format!("$(touch {})", marker.display());
    let config = pre_hook(HookCommand {
        command: script.display().to_string(),
        args: Some(vec![
            "a b".to_owned(),
            touch.clone(),
            ";".to_owned(),
            "*".to_owned(),
        ]),
        timeout_secs: None,
        async_hook: false,
        async_rewake: false,
    });
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    let decision = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap();
    assert!(matches!(decision, ToolDecision::Allow), "{decision:?}");
    let seen = std::fs::read_to_string(&out).unwrap();
    assert_eq!(
        seen.lines().collect::<Vec<_>>(),
        vec!["a b", touch.as_str(), ";", "*"],
        "arguments arrive verbatim, with no word splitting or expansion"
    );
    assert!(!marker.exists(), "$(...) must not run");
}

/// Edge case 2: an exec-form hook in the `workspace` tier runs inside the sandbox, so a
/// write outside the workspace fails and lands nothing.
#[cfg(unix)]
#[tokio::test]
async fn exec_args_run_inside_the_workspace_sandbox() {
    let root = TempRoot::new("sandbox-exec-args");
    let outside = root.outside("escaped.txt");
    let script = root.ws.join("escape");
    write_script(
        &script,
        &format!("echo escaped > {}; exit 1", outside.display()),
    );
    let config = pre_hook(HookCommand {
        command: script.display().to_string(),
        args: Some(vec!["ignored".to_owned()]),
        timeout_secs: None,
        async_hook: false,
        async_rewake: false,
    });
    let sandbox =
        radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::WorkspaceWrite, &root.ws)
            .expect("the workspace sandbox probes clean on this machine");
    let fixture = layered_with_root(
        root,
        &config,
        &HooksConfig::default(),
        SandboxSlot::with(sandbox),
    );
    let error = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("exited with code"),
        "the sandbox denial is a hook error: {error}"
    );
    assert!(
        !outside.exists(),
        "the write outside the workspace must not land"
    );
}

/// Edge case 4: dedup keys on `command` plus `args` (ADR 0045 §1 item 3). The same
/// program with different arguments runs twice, and the same program with the same
/// arguments written in both layers runs once. A command-only key fails both halves.
#[cfg(unix)]
#[tokio::test]
async fn the_dedup_key_is_command_plus_args() {
    let root = TempRoot::new("dedup-args");
    let log = root.ws.join("runs.txt");
    let script = root.ws.join("log");
    write_script(&script, &format!("echo \"$1\" >> {}", log.display()));
    let hook = |arg: &str| HookCommand {
        command: script.display().to_string(),
        args: Some(vec![arg.to_owned()]),
        timeout_secs: None,
        async_hook: false,
        async_rewake: false,
    };
    let project = pre_hook(hook("one"));
    let user = HooksConfig {
        pre_tool_use: vec![entry("shell", vec![hook("one"), hook("two")])],
        ..HooksConfig::default()
    };
    let fixture = layered_with_root(root, &user, &project, SandboxSlot::empty());
    fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap();
    let seen = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        seen.lines().collect::<Vec<_>>(),
        vec!["one", "two"],
        "the duplicate (command, args) runs once; the other args still run"
    );
}

/// Edge case 5: a command that does not exist is a hook error (fail closed, unchanged)
/// and is recorded as `failed to start`.
#[tokio::test]
async fn a_missing_exec_command_fails_to_start_and_is_recorded() {
    let root = TempRoot::new("missing-exec");
    let config = pre_hook(HookCommand {
        command: root.ws.join("does-not-exist").display().to_string(),
        args: Some(Vec::new()),
        timeout_secs: None,
        async_hook: false,
        async_rewake: false,
    });
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    let error = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("could not start"),
        "the spawn failure stays a hook error: {error}"
    );
    let runs = fixture.hooks.hook_runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].end, HookRunEnd::FailedToStart);
    assert_eq!(runs[0].event, "PreToolUse");
    assert_eq!(runs[0].tool.as_deref(), Some("shell"));
    assert!(
        runs[0].line().contains("failed to start"),
        "{}",
        runs[0].line()
    );
}

/// Edge case 6: a timed-out hook is recorded as `timed out`, and its milliseconds are at
/// least the timeout.
#[tokio::test]
async fn a_timed_out_hook_is_recorded_as_timed_out() {
    let root = TempRoot::new("timed-out-record");
    let config = pre_hook(HookCommand {
        command: "/bin/sleep".to_owned(),
        args: Some(vec!["5".to_owned()]),
        timeout_secs: Some(1),
        async_hook: false,
        async_rewake: false,
    });
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    let error = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error}");
    let runs = fixture.hooks.hook_runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].end, HookRunEnd::TimedOut);
    assert!(
        runs[0].millis >= 1000,
        "the timeout's record must be at least 1000ms: {}",
        runs[0].millis
    );
}

/// A run that ends on a signal has no exit code; the closed set calls that `cancelled`.
#[tokio::test]
async fn a_hook_killed_by_a_signal_is_recorded_cancelled() {
    let root = TempRoot::new("cancelled-record");
    let config = pre_hook(HookCommand {
        command: "/bin/sh".to_owned(),
        args: Some(vec!["-c".to_owned(), "kill -KILL $$".to_owned()]),
        timeout_secs: None,
        async_hook: false,
        async_rewake: false,
    });
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    let error = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("signal"), "{error}");
    let runs = fixture.hooks.hook_runs();
    assert_eq!(runs[0].end, HookRunEnd::Cancelled);
}

/// Edge case 5's record shape, plus `Stop`'s missing tool name: the agreed one-line
/// record is `<event>:<tool> [<command first line>] <end> (<n>ms)`. A command shaped
/// `echo first` records only `echo first` even when the shell command has more lines.
#[tokio::test]
async fn every_run_records_the_agreed_line() {
    let root = TempRoot::new("record-line");
    let config = pre_config("shell", "true");
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap();
    let runs = fixture.hooks.hook_runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].end, HookRunEnd::Status(0));
    assert!(
        runs[0]
            .line()
            .starts_with("PreToolUse:shell [true] status 0 ("),
        "{}",
        runs[0].line()
    );
    assert!(runs[0].line().ends_with("ms)"), "{}", runs[0].line());
    assert!(runs[0].millis >= 1, "a run never reads 0ms");

    let root = TempRoot::new("record-line-stop");
    let config = HooksConfig {
        stop: vec![HookMatcher {
            matcher: None,
            hooks: vec![hook("true\nexit 0")],
        }],
        ..HooksConfig::default()
    };
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    fixture.hooks.run_stop_once(&completed_quiet()).await;
    let runs = fixture.hooks.hook_runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(
        runs[0].line().starts_with("Stop [true] status 0 ("),
        "Stop has no tool name, and only the command's first line shows: {}",
        runs[0].line()
    );
}

/// Edge case 7: 60 runs keep the last 50, newest first.
#[tokio::test]
async fn the_run_ring_keeps_the_last_50_newest_first() {
    let fixture = plain(&HooksConfig::default());
    for n in 0..60 {
        fixture.hooks.record_run(
            "Stop",
            None,
            &format!("cmd-{n}"),
            HookRunEnd::Status(0),
            Instant::now(),
        );
    }
    let runs = fixture.hooks.hook_runs();
    assert_eq!(runs.len(), HOOK_RUN_HISTORY, "the ring holds 50");
    assert_eq!(runs[0].command, "cmd-59", "newest first");
    assert_eq!(runs[49].command, "cmd-10", "the oldest kept is #10");
}

/// `workspace` runs the hook sandboxed: a hook writing outside the workspace fails
/// (fail closed), and nothing lands outside. The trailing `exit 1` pins the
/// verdict to the hook-error path on every shell: without it dash exits 2 on
/// a failed redirection (a `Deny`) while bash exits 1 (a hook error).
#[tokio::test]
async fn workspace_runs_the_hook_sandboxed() {
    let root = TempRoot::new("sandbox");
    let outside = root.outside("escaped.txt");
    let config = pre_config(
        "shell",
        &format!("echo escaped > {}; exit 1", outside.display()),
    );
    let sandbox =
        radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::WorkspaceWrite, &root.ws)
            .expect("the workspace sandbox probes clean on this machine");
    let fixture = layered_with_root(
        root,
        &config,
        &HooksConfig::default(),
        SandboxSlot::with(sandbox),
    );
    let error = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("exited with code"),
        "the sandbox denial is a hook error: {error}"
    );
    assert!(
        !outside.exists(),
        "the write outside the workspace must not land"
    );
}

/// The sandboxed hook can still write inside the workspace.
#[tokio::test]
async fn a_sandboxed_hook_writes_inside_the_workspace() {
    let root = TempRoot::new("sandbox-inside");
    let config = pre_config("shell", "echo ok > marker.txt");
    let sandbox =
        radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::WorkspaceWrite, &root.ws)
            .expect("the workspace sandbox probes clean on this machine");
    let ws = root.ws.clone();
    let fixture = layered_with_root(
        root,
        &config,
        &HooksConfig::default(),
        SandboxSlot::with(sandbox),
    );
    let decision = fixture
        .hooks
        .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
        .await
        .unwrap();
    assert!(matches!(decision, ToolDecision::Allow), "{decision:?}");
    assert!(ws.join("marker.txt").exists());
}
