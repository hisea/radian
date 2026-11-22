use super::*;

/// `StopFailure` runs only the entry whose matcher names the failed turn's kind (or `*`),
/// and its stdin carries the kind, the details and the last assistant text (ADR 0045 §6).
#[tokio::test]
async fn stop_failure_selects_the_kind_and_carries_its_stdin() {
    let root = TempRoot::new("stop-failure");
    let hit = root.outside("hit.json");
    let miss = root.outside("miss.json");
    let config = HooksConfig {
        stop_failure: vec![
            entry(
                "usage_limit",
                vec![hook(&format!("cat > '{}'", hit.display()))],
            ),
            entry(
                "transport",
                vec![hook(&format!("cat > '{}'", miss.display()))],
            ),
        ],
        ..HooksConfig::default()
    };
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    fixture.hooks.run_stop_failure(
        &radian_core::ModelError::UsageLimit {
            message: "the plan window is used up".into(),
            resets_in: None,
        },
        "I was mid-sentence",
    );
    fixture
        .hooks
        .wait_observe(std::time::Duration::from_secs(10))
        .await;
    let text = std::fs::read_to_string(&hit).expect("the matching hook ran");
    assert!(text.contains("\"error\":\"usage_limit\""), "{text}");
    assert!(
        text.contains("\"error_details\":\"model usage limit"),
        "{text}"
    );
    assert!(
        text.contains("\"last_assistant_message\":\"I was mid-sentence\""),
        "{text}"
    );
    assert!(
        text.contains("\"hook_event_name\":\"StopFailure\""),
        "{text}"
    );
    assert!(!miss.exists(), "the non-matching entry never runs");
}

/// `error_details` is documented as "at most 2000 characters" (ADR 0045 §6), and the
/// truncation marker is part of that bound. Keeping 2000 and then appending the marker
/// made the field 2012 characters long.
#[tokio::test]
async fn stop_failure_error_details_stays_within_the_bound() {
    let root = TempRoot::new("stop-failure-long");
    let hit = root.outside("hit.json");
    let config = HooksConfig {
        stop_failure: vec![entry(
            "*",
            vec![hook(&format!("cat > '{}'", hit.display()))],
        )],
        ..HooksConfig::default()
    };
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    fixture.hooks.run_stop_failure(
        &radian_core::ModelError::UsageLimit {
            message: "x".repeat(5_000),
            resets_in: None,
        },
        "",
    );
    fixture
        .hooks
        .wait_observe(std::time::Duration::from_secs(10))
        .await;
    let text = std::fs::read_to_string(&hit).expect("the matching hook ran");
    let stdin: serde_json::Value = serde_json::from_str(&text).expect("stdin is one JSON object");
    let details = stdin["error_details"].as_str().expect("a string field");
    assert_eq!(
        details.chars().count(),
        REASON_CHAR_LIMIT,
        "the field stays at the documented maximum: {}",
        details.len()
    );
    assert!(details.ends_with(TRUNCATION_MARKER), "{details}");
}

/// A `StopFailure` matcher outside `MODEL_ERROR_KINDS` warns once at load and that entry
/// never runs (ADR 0045 §6). `rate_limit` is Claude's name, not a radian kind.
#[tokio::test]
async fn an_out_of_set_stop_failure_matcher_warns_and_never_runs() {
    let root = TempRoot::new("stop-failure-bad");
    let hit = root.outside("never.json");
    let config = HooksConfig {
        stop_failure: vec![entry(
            "rate_limit",
            vec![hook(&format!("cat > '{}'", hit.display()))],
        )],
        ..HooksConfig::default()
    };
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    let warnings = warned(&fixture.warnings);
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("rate_limit") && warning.contains("will never run")),
        "{warnings:?}"
    );
    fixture.hooks.run_stop_failure(
        &radian_core::ModelError::UsageLimit {
            message: "quota".into(),
            resets_in: None,
        },
        "",
    );
    fixture
        .hooks
        .wait_observe(std::time::Duration::from_secs(1))
        .await;
    assert!(!hit.exists(), "an outside-the-set matcher never runs");
    assert!(fixture.hooks.hook_runs().is_empty());
}

/// A `Notification` hook receives its type, title and message, and the message names the
/// tool but never its arguments (ADR 0045 §6).
#[tokio::test]
async fn notification_carries_the_type_and_a_tool_name_only() {
    let root = TempRoot::new("notification");
    let permission = root.outside("permission.json");
    let idle = root.outside("idle.json");
    let config = HooksConfig {
        notification: vec![
            entry(
                "permission_prompt",
                vec![hook(&format!("cat > '{}'", permission.display()))],
            ),
            entry(
                "idle_prompt",
                vec![hook(&format!("cat > '{}'", idle.display()))],
            ),
        ],
        ..HooksConfig::default()
    };
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    NotificationHooks::notification(&fixture.hooks, "permission_prompt", Some("shell"));
    let text = wait_notification(&permission).await;
    assert!(
        text.contains("\"notification_type\":\"permission_prompt\""),
        "{text}"
    );
    assert!(text.contains("\"title\":\"radian\""), "{text}");
    assert!(
        text.contains("\"message\":\"radian needs your permission to use shell\""),
        "{text}"
    );
    assert!(
        !text.contains("tool_input"),
        "arguments never leave: {text}"
    );
    assert!(
        !idle.exists(),
        "the idle entry never runs for a permission prompt"
    );

    fixture.hooks.notify_idle();
    let text = wait_notification(&idle).await;
    assert!(
        text.contains("\"notification_type\":\"idle_prompt\""),
        "{text}"
    );
    assert!(
        text.contains("\"message\":\"radian is waiting for your input\""),
        "{text}"
    );
}

/// Both observe-only events are always background: `async` is accepted and inert, and
/// the default timeout stays the synchronous 60 s. A `wait_observe` deadline kills the
/// `StopFailure` run's process group (the `exec -p` 5 s rule); a `Notification` run is
/// never waited for.
#[tokio::test]
async fn observe_hooks_are_async_and_a_wait_deadline_kills_them() {
    let root = TempRoot::new("observe-async");
    let pidfile = root.outside("pid");
    let mut sleeping = hook(&format!("echo $$ > '{}'; sleep 30", pidfile.display()));
    sleeping.async_hook = true;
    let config = HooksConfig {
        stop_failure: vec![HookMatcher {
            matcher: None,
            hooks: vec![sleeping],
        }],
        ..HooksConfig::default()
    };
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    fixture.hooks.run_stop_failure(
        &radian_core::ModelError::Transport("stream ended".into()),
        "",
    );
    // The run is registered before `run_stop_failure` returns, so the pid appears while
    // the wait below is still running.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pidfile)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the hook never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    let started = std::time::Instant::now();
    fixture
        .hooks
        .wait_observe(std::time::Duration::from_secs(1))
        .await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "wait_observe waited past its deadline"
    );
    let mut gone = false;
    for _ in 0..100 {
        // SAFETY: signal 0 only probes existence.
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        gone,
        "the observe run's process group survived the deadline"
    );
    // The killed run's recent-runs entry says `cancelled` (ADR 0045 §6): the deadline
    // kills it and its end is recorded, never delivered.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if fixture
            .hooks
            .hook_runs()
            .iter()
            .any(|run| run.end == HookRunEnd::Cancelled)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the killed observe run was not recorded cancelled: {:?}",
            fixture.hooks.hook_runs()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// A `Notification` hook never postpones an exit: `wait_observe` returns without waiting
/// for it (the session end kills it). Only `StopFailure` is waited for (ADR 0045 §6).
#[tokio::test]
async fn notification_does_not_delay_the_exit_wait() {
    let root = TempRoot::new("notification-no-wait");
    let config = HooksConfig {
        notification: vec![entry("idle_prompt", vec![hook("sleep 5")])],
        ..HooksConfig::default()
    };
    let fixture = layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
    fixture.hooks.notify_idle();
    let started = std::time::Instant::now();
    fixture
        .hooks
        .wait_observe(std::time::Duration::from_secs(5))
        .await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "a Notification hook held the exit wait: {:?}",
        started.elapsed()
    );
    fixture.hooks.end_session().await;
}
