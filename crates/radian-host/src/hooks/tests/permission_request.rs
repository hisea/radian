use super::*;

// ---------- `PermissionRequest` (ADR 0045 §4) ----------

pub(super) fn permission_request_config(matcher: &str, command: &str) -> HooksConfig {
    HooksConfig {
        permission_request: vec![entry(matcher, vec![hook(command)])],
        ..HooksConfig::default()
    }
}

pub(super) async fn request(
    fixture: &Fixture,
    call: &ToolCall,
) -> Result<PermissionRequestDecision, HookError> {
    PermissionRequestHooks::permission_request(&fixture.hooks, call).await
}

/// §3: exit 0 with `decision.behavior` answers the ask; a deny carries its `message`.
#[tokio::test]
async fn permission_request_allow_and_deny_come_from_the_decision() {
    let script = r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow"}}}'"#;
    let fixture = plain(&permission_request_config("shell", script));
    assert_eq!(
        request(&fixture, &call("shell", serde_json::json!({})))
            .await
            .unwrap(),
        PermissionRequestDecision::Allow {
            session_grant: false
        }
    );

    let script = r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"deny","message":"no-writes"}}}'"#;
    let fixture = plain(&permission_request_config("shell", script));
    assert_eq!(
        request(&fixture, &call("shell", serde_json::json!({})))
            .await
            .unwrap(),
        PermissionRequestDecision::Deny {
            reason: Some("no-writes".into())
        }
    );
}

/// §2: the stdin carries the tool fields and `cwd`/`session_id`, and never
/// `permission_suggestions` (radian has no text rule syntax).
#[tokio::test]
async fn permission_request_stdin_has_the_tool_fields_and_no_suggestions() {
    let root = TempRoot::new("permission-stdin");
    let stdin_path = root.dir.join("stdin.json");
    let config = permission_request_config("shell", &format!("cat > {}", stdin_path.display()));
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    let _ = request(
        &fixture,
        &call("shell", serde_json::json!({"command": "make check"})),
    )
    .await;
    let stdin: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&stdin_path).unwrap()).unwrap();
    assert_eq!(stdin["hook_event_name"], "PermissionRequest");
    assert_eq!(stdin["tool_name"], "shell");
    assert_eq!(stdin["tool_input"]["command"], "make check");
    assert_eq!(stdin["session_id"], "test-session");
    assert_eq!(stdin["cwd"], fixture._root.ws.to_string_lossy().as_ref());
    assert!(
        stdin.get("permission_suggestions").is_none(),
        "there are no text suggestions: {stdin}"
    );
}

/// §3: exit 2 denies with stderr as the reason (the `PreToolUse` caliber).
#[tokio::test]
async fn permission_request_exit_2_denies_with_the_stderr_reason() {
    let fixture = plain(&permission_request_config(
        "shell",
        "echo blocked-by-script >&2; exit 2",
    ));
    assert_eq!(
        request(&fixture, &call("shell", serde_json::json!({})))
            .await
            .unwrap(),
        PermissionRequestDecision::Deny {
            reason: Some("blocked-by-script\n".into())
        }
    );
}

/// §3: empty stdout, an unknown `behavior`, and a decision without `behavior` all mean
/// "no decision"; the last two warn one line and the ordinary path follows.
#[tokio::test]
async fn permission_request_undecided_outputs_follow_the_ordinary_path() {
    let fixture = plain(&permission_request_config("shell", "exit 0"));
    assert_eq!(
        request(&fixture, &call("shell", serde_json::json!({})))
            .await
            .unwrap(),
        PermissionRequestDecision::NoDecision
    );
    assert!(warned(&fixture.warnings).is_empty());

    let script = r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"maybe"}}}'"#;
    let fixture = plain(&permission_request_config("shell", script));
    assert_eq!(
        request(&fixture, &call("shell", serde_json::json!({})))
            .await
            .unwrap(),
        PermissionRequestDecision::NoDecision
    );
    assert!(
        warned(&fixture.warnings)
            .iter()
            .any(|warning| warning.contains("unknown decision.behavior")
                && warning.contains("maybe")),
        "{:?}",
        warned(&fixture.warnings)
    );

    let script = r#"printf '%s' '{"hookSpecificOutput":{"decision":{"updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow"}]}}}'"#;
    let fixture = plain(&permission_request_config("shell", script));
    assert_eq!(
        request(&fixture, &call("shell", serde_json::json!({})))
            .await
            .unwrap(),
        PermissionRequestDecision::NoDecision,
        "items count only from a layer that decides allow"
    );
    assert!(
        warned(&fixture.warnings)
            .iter()
            .any(|warning| warning.contains("without behavior")),
        "{:?}",
        warned(&fixture.warnings)
    );

    let script = r#"printf '%s' '{"hookSpecificOutput":{"decision":{"message":"x"}}}'"#;
    let fixture = plain(&permission_request_config("shell", script));
    assert_eq!(
        request(&fixture, &call("shell", serde_json::json!({})))
            .await
            .unwrap(),
        PermissionRequestDecision::NoDecision
    );
    assert!(
        warned(&fixture.warnings)
            .iter()
            .any(|warning| warning.contains("without behavior")),
        "{:?}",
        warned(&fixture.warnings)
    );
}

/// §3 / edge case 3: a non-zero non-2 exit, exit-0 stdout that is not JSON, and a
/// timeout are hook errors — denied with `hook_error`, never "no decision".
#[tokio::test]
async fn permission_request_hook_errors_fail_closed() {
    for (command, timeout) in [
        ("exit 1", None),
        ("printf '%s' 'hello'", None),
        ("sleep 30", Some(1)),
    ] {
        let mut config = permission_request_config("shell", command);
        config.permission_request[0].hooks[0].timeout_secs = timeout;
        let fixture = plain(&config);
        let error = request(&fixture, &call("shell", serde_json::json!({})))
            .await
            .expect_err("a broken hook must fail closed, not decide");
        assert!(
            error.to_string().contains("PermissionRequest hook"),
            "{command}: {error}"
        );
    }
}

/// §3: `updatedInput` and `interrupt` are ignored with one warning line, and the
/// decision still stands.
#[tokio::test]
async fn permission_request_ignores_unsupported_decision_fields() {
    for field in [r#""updatedInput":{"command":"x"}"#, r#""interrupt":true"#] {
        let script = format!(
            r#"printf '%s' '{{"hookSpecificOutput":{{"decision":{{"behavior":"allow",{field}}}}}}}'"#
        );
        let fixture = plain(&permission_request_config("shell", &script));
        assert_eq!(
            request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .unwrap(),
            PermissionRequestDecision::Allow {
                session_grant: false
            }
        );
        let warnings = warned(&fixture.warnings);
        assert_eq!(warnings.len(), 1, "{field}: {warnings:?}");
        assert!(
            warnings[0].contains("updatedInput") && warnings[0].contains("interrupt"),
            "{field}: {warnings:?}"
        );
    }
}

/// §3: the one accepted `updatedPermissions` item asks for a session grant; other
/// `type`/`destination`/`behavior` values warn one line and record nothing, and the
/// allow still stands. `rules` inside an accepted item are not parsed.
#[tokio::test]
async fn permission_request_accepts_only_the_session_add_rules_item() {
    let accepted = r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow","rules":["Bash(git push)"]}]}}}'"#;
    for (script, session_grant, warnings) in [
        // The one accepted shape: a session grant, whatever `rules` holds.
        (accepted, true, 0usize),
        // Another destination: a warning, and the allow is still one call only.
        (
            r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedPermissions":[{"type":"addRules","destination":"projectSettings","behavior":"allow"}]}}}'"#,
            false,
            1,
        ),
        // Another type or behavior: a warning and no grant.
        (
            r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedPermissions":[{"type":"setMode","destination":"session","behavior":"allow"}]}}}'"#,
            false,
            1,
        ),
        (
            r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedPermissions":[{"type":"addRules","destination":"session","behavior":"deny"}]}}}'"#,
            false,
            1,
        ),
    ] {
        let fixture = plain(&permission_request_config("shell", script));
        assert_eq!(
            request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .unwrap(),
            PermissionRequestDecision::Allow { session_grant },
            "{script}"
        );
        let warned = warned(&fixture.warnings);
        assert_eq!(warned.len(), warnings, "{script}: {warned:?}");
        if warnings > 0 {
            assert!(
                warned[0].contains("updatedPermissions"),
                "{script}: {warned:?}"
            );
        }
    }
}

/// §3 / rework 2: a present `updatedPermissions` that is not an array warns one line and
/// records nothing, and the allow still stands. A JSON `null` is present, not an absent
/// field: deserializing it into a plain `Option<Value>` conflates the two and loses the
/// warning.
#[tokio::test]
async fn a_non_array_updated_permissions_warns_including_null() {
    for value in ["null", r#""session""#, "{}", "42"] {
        let script = format!(
            r#"printf '%s' '{{"hookSpecificOutput":{{"decision":{{"behavior":"allow","updatedPermissions":{value}}}}}}}'"#
        );
        let fixture = plain(&permission_request_config("shell", &script));
        assert_eq!(
            request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .unwrap(),
            PermissionRequestDecision::Allow {
                session_grant: false
            },
            "{value}"
        );
        let warned = warned(&fixture.warnings);
        assert_eq!(warned.len(), 1, "{value}: {warned:?}");
        assert!(
            warned[0].contains("updatedPermissions"),
            "{value}: {warned:?}"
        );
    }
}

/// §4: items count only from a layer that decides `allow`: a `deny` layer's
/// `updatedPermissions` is never read, and the chain's answer stays a denial.
#[tokio::test]
async fn a_denying_layer_never_records_an_updated_permissions_grant() {
    let project = permission_request_config(
        "shell",
        r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow"}]}}}'"#,
    );
    let user = permission_request_config(
        "shell",
        r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"deny","message":"no-writes","updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow"}]}}}'"#,
    );
    let fixture = layered(&user, &project);
    assert_eq!(
        request(&fixture, &call("shell", serde_json::json!({})))
            .await
            .unwrap(),
        PermissionRequestDecision::Deny {
            reason: Some("no-writes".into())
        }
    );
}

/// §4: the project layer runs first; a user `deny` beats a project `allow`; a project
/// `deny` stops the chain (the user's marker never appears); and a project `allow`
/// with a silent user layer allows. An identical command in both layers runs once.
#[tokio::test]
async fn permission_request_layers_and_deny_stop_the_chain() {
    // Project allow, user deny: denied.
    let project = permission_request_config(
        "shell",
        r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow"}}}'"#,
    );
    let user = permission_request_config("shell", "echo user-deny >&2; exit 2");
    let fixture = layered(&user, &project);
    match request(&fixture, &call("shell", serde_json::json!({})))
        .await
        .unwrap()
    {
        PermissionRequestDecision::Deny { reason } => assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains("user-deny")),
            "the user deny stands: {reason:?}"
        ),
        other => panic!("the user deny stands: {other:?}"),
    }

    // Project deny: the user hook never starts.
    let root = TempRoot::new("permission-deny-stops");
    let marker = root.dir.join("user-ran");
    let project = permission_request_config("shell", "echo project-deny >&2; exit 2");
    let user = permission_request_config("shell", &format!("touch {}; exit 0", marker.display()));
    let fixture = layered_with_root(root, &user, &project, SandboxSlot::empty());
    match request(&fixture, &call("shell", serde_json::json!({})))
        .await
        .unwrap()
    {
        PermissionRequestDecision::Deny { reason } => assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains("project-deny")),
            "the project deny stands: {reason:?}"
        ),
        other => panic!("the project deny stands: {other:?}"),
    }
    assert!(
        !marker.exists(),
        "a deny only narrows: the later layer must not run"
    );

    // Project allow, user silent: allowed.
    let project = permission_request_config(
        "shell",
        r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow"}}}'"#,
    );
    let user = permission_request_config("shell", "exit 0");
    let fixture = layered(&user, &project);
    assert_eq!(
        request(&fixture, &call("shell", serde_json::json!({})))
            .await
            .unwrap(),
        PermissionRequestDecision::Allow {
            session_grant: false
        }
    );

    // The same command in both layers runs once.
    let root = TempRoot::new("permission-dedupe");
    let counter = root.dir.join("runs");
    let script = format!(
        "echo ran >> {}; printf '%s' '{{\"hookSpecificOutput\":{{\"decision\":{{\"behavior\":\"allow\"}}}}}}'",
        counter.display()
    );
    let project = permission_request_config("shell", &script);
    let user = permission_request_config("shell", &script);
    let fixture = layered_with_root(root, &user, &project, SandboxSlot::empty());
    assert_eq!(
        request(&fixture, &call("shell", serde_json::json!({})))
            .await
            .unwrap(),
        PermissionRequestDecision::Allow {
            session_grant: false
        }
    );
    assert_eq!(
        std::fs::read_to_string(&counter).unwrap().lines().count(),
        1,
        "one run for two identical namings"
    );
}
