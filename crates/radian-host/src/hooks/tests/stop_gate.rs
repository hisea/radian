use super::*;

/// `Stop` exit 2 opens another turn with the stderr as a `System` followup. Continuations
/// minutes apart never fill the rate gate: six in a row all open a turn; a fast loop
/// fills the window, and past the gate the hook still runs but its verdict degrades to
/// a marked inject (no wake) with the wake-budget notice, until the window passes.
#[tokio::test]
async fn stop_continuations_minutes_apart_are_never_gated() {
    tokio::time::pause();
    let config = HooksConfig {
        stop: vec![HookMatcher {
            matcher: None,
            hooks: vec![hook("echo keep-going >&2; exit 2")],
        }],
        ..HooksConfig::default()
    };
    let fixture = plain(&config);
    for i in 0..6 {
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        let entry = fixture
            .inbox
            .claim_next_turn()
            .await
            .unwrap_or_else(|| panic!("round {i}: the continuation opens a turn"));
        assert!(
            entry.message.text().contains("keep-going"),
            "the stderr is the input: {}",
            entry.message.text()
        );
        assert_eq!(entry.source, InputSource::System);
        tokio::time::advance(std::time::Duration::from_secs(180)).await;
    }
}

/// A fast `Stop` loop (every continuation ~1 s apart) fills the window: the 11th run
/// still runs the hook, but its exit-2 verdict degrades to a marked inject (no wake)
/// with the wake-budget notice; the window passing reopens followups with no user
/// message.
#[tokio::test]
async fn stop_fast_loop_is_gated_after_n_and_recovers_after_the_window() {
    tokio::time::pause();
    let config = HooksConfig {
        stop: vec![HookMatcher {
            matcher: None,
            hooks: vec![hook("echo keep-going >&2; exit 2")],
        }],
        ..HooksConfig::default()
    };
    let fixture = plain(&config);
    for i in 0..MAX_WAKES_PER_WINDOW {
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_some(),
            "continuation {i}: still inside the budget"
        );
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
    }
    assert_eq!(fixture.hooks.stop_wakes(), MAX_WAKES_PER_WINDOW);
    // The 11th run still runs the hook; its exit-2 verdict degrades to a marked
    // inject (no wake) with the wake-budget notice.
    fixture.hooks.run_stop_once(&completed_quiet()).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "the gated continuation must not open a turn"
    );
    let entry = fixture
        .inbox
        .claim_next_step()
        .await
        .expect("the gated hook injects the verdict");
    assert!(
        entry.message.text().contains("keep-going"),
        "the stderr is the input: {}",
        entry.message.text()
    );
    assert_eq!(fixture.hooks.degraded(), 1);
    assert_eq!(
        warned(&fixture.wake_notices).len(),
        1,
        "the degrade fires the wake notice: {:?}",
        warned(&fixture.wake_notices)
    );
    assert!(
        warned(&fixture.wake_notices)[0].starts_with("wake budget used up: 1 "),
        "the notice carries the degrade count: {:?}",
        warned(&fixture.wake_notices)
    );
    // The window passing alone reopens the gate: no user message needed; the later
    // wake ends the burst.
    tokio::time::advance(WAKE_WINDOW + std::time::Duration::from_secs(1)).await;
    fixture.hooks.run_stop_once(&completed_quiet()).await;
    assert_eq!(fixture.hooks.stop_wakes(), 1);
    assert_eq!(
        fixture.hooks.degraded(),
        0,
        "the later wake ends the degrade burst"
    );
    assert!(
        fixture.inbox.claim_next_turn().await.is_some(),
        "after the window: the continuation opens a turn again"
    );
}

/// A user message restarts the `Stop` budget at once.
#[tokio::test]
async fn stop_user_message_resets_the_budget() {
    tokio::time::pause();
    let config = HooksConfig {
        stop: vec![HookMatcher {
            matcher: None,
            hooks: vec![hook("echo keep-going >&2; exit 2")],
        }],
        ..HooksConfig::default()
    };
    let fixture = plain(&config);
    for _ in 0..3 {
        fixture.hooks.run_stop_once(&completed_quiet()).await;
    }
    assert_eq!(fixture.hooks.stop_wakes(), 3);
    // A user message restarts the budget.
    fixture.hooks.reset_wakes();
    assert_eq!(fixture.hooks.degraded(), 0);
    fixture.hooks.run_stop_once(&completed_quiet()).await;
    assert_eq!(fixture.hooks.stop_wakes(), 1);
}

/// A cancelled turn runs no Stop hook: an exit-2 hook after a user interrupt must not
/// open another turn.
#[tokio::test]
async fn stop_does_not_run_on_a_cancelled_turn() {
    let config = HooksConfig {
        stop: vec![HookMatcher {
            matcher: None,
            hooks: vec![hook("echo keep-going >&2; exit 2")],
        }],
        ..HooksConfig::default()
    };
    let fixture = plain(&config);
    fixture
        .hooks
        .run_stop_once(&completed_outcome(
            RunStatus::Cancelled(CancelReason::Requested),
            Some("Now the docs-only ADR updates."),
        ))
        .await;
    assert_eq!(fixture.hooks.stop_wakes(), 0);
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "a cancelled turn starts no new turn"
    );
}

/// A failed turn runs no Stop hook either.
#[tokio::test]
async fn stop_does_not_run_on_a_failed_turn() {
    let config = HooksConfig {
        stop: vec![HookMatcher {
            matcher: None,
            hooks: vec![hook("echo keep-going >&2; exit 2")],
        }],
        ..HooksConfig::default()
    };
    let fixture = plain(&config);
    fixture
        .hooks
        .run_stop_once(&completed_outcome(
            RunStatus::Failed,
            Some("Now the docs-only ADR updates."),
        ))
        .await;
    assert_eq!(fixture.hooks.stop_wakes(), 0);
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "a failed turn starts no new turn"
    );
}

/// `Stop` exit 0 ends the turn with no followup.
#[tokio::test]
async fn stop_exit_0_continues_nothing() {
    let config = HooksConfig {
        stop: vec![HookMatcher {
            matcher: None,
            hooks: vec![hook("exit 0")],
        }],
        ..HooksConfig::default()
    };
    let fixture = plain(&config);
    fixture.hooks.run_stop_once(&completed_quiet()).await;
    assert_eq!(fixture.hooks.stop_wakes(), 0);
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "exit 0 continues nothing"
    );
}

/// Edge case 5: a project-layer `additionalContext` and a user-layer exit 2 join one
/// continuation: one turn, one wake, one message carrying both reasons (project first).
/// Ignoring `additionalContext` loses the project reason and fails.
#[tokio::test]
async fn stop_additional_context_merges_with_another_layers_exit_2() {
    let project = stop_config(
        r#"printf '%s' '{"hookSpecificOutput":{"additionalContext":"project-context"}}'"#,
    );
    let user = stop_config("echo user-reason >&2; exit 2");
    let fixture = layered(&user, &project);
    fixture.hooks.run_stop_once(&completed_quiet()).await;
    let entry = fixture
        .inbox
        .claim_next_turn()
        .await
        .expect("the merged continuation opens a turn");
    let text = entry.message.text();
    let project_at = text
        .find("project-context")
        .expect("project reason: {text}");
    let user_at = text.find("user-reason").expect("user reason: {text}");
    assert!(
        project_at < user_at,
        "the project reason rides first: {text}"
    );
    assert_eq!(fixture.hooks.stop_wakes(), 1, "one wake, not two");
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "one continuation, not two"
    );
}

/// `decision: "block"` on `Stop` opens the same continuation as exit 2 (the JSON form).
#[tokio::test]
async fn stop_block_decision_opens_the_same_continuation() {
    let config = stop_config(r#"printf '%s' '{"decision":"block","reason":"keep-going"}'"#);
    let fixture = plain(&config);
    fixture.hooks.run_stop_once(&completed_quiet()).await;
    let entry = fixture
        .inbox
        .claim_next_turn()
        .await
        .expect("block opens a turn");
    assert!(entry.message.text().contains("keep-going"));
    assert_eq!(fixture.hooks.stop_wakes(), 1);
}

/// Edge case 6: `additionalContext` past the wake gate degrades to a marked `inject`
/// with the same three visible signals as exit 2 (inbox entry, degrade count, wake
/// notice). Ignoring the field opens no continuation and injects nothing, so the
/// step claim fails.
#[tokio::test]
async fn stop_additional_context_degrades_past_the_wake_gate() {
    tokio::time::pause();
    let config =
        stop_config(r#"printf '%s' '{"hookSpecificOutput":{"additionalContext":"keep-going"}}'"#);
    let fixture = plain(&config);
    for i in 0..MAX_WAKES_PER_WINDOW {
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_some(),
            "continuation {i}: still inside the budget"
        );
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
    }
    assert_eq!(fixture.hooks.stop_wakes(), MAX_WAKES_PER_WINDOW);
    fixture.hooks.run_stop_once(&completed_quiet()).await;
    assert!(
        fixture.inbox.claim_next_turn().await.is_none(),
        "the gated continuation must not open a turn"
    );
    let step = fixture
        .inbox
        .claim_next_step()
        .await
        .expect("the gated verdict injects");
    assert!(step.message.text().contains("keep-going"));
    assert_eq!(fixture.hooks.degraded(), 1);
    assert_eq!(
        warned(&fixture.wake_notices).len(),
        1,
        "the degrade fires the wake notice: {:?}",
        warned(&fixture.wake_notices)
    );
}

/// A `Stop` continuation past the gate runs the hook but injects the verdict with
/// the degrade mark: the durable `InboxEnqueued` says `degraded`, so the journal leg
/// matches the job notifier's (ADR 0045 §2).
#[tokio::test]
async fn stop_past_the_cap_injects_the_verdict_with_the_degrade_mark() {
    use radian_session::{EventData, Session, SessionCreatedData, SessionJournal};
    let root = TempRoot::new("stop-degraded");
    let journal = Arc::new(
        SessionJournal::create(
            root.dir.join("s.jsonl"),
            "sid",
            SessionCreatedData::new(
                "/work/example",
                serde_json::json!({"provider": "example", "model": "m-1"}),
            ),
        )
        .unwrap(),
    );
    let inbox = Inbox::durable(64, Arc::clone(&journal));
    let (warn, _) = warnings_collector();
    let (wake, wake_notices) = warnings_collector();
    let (intent, _) = warnings_collector();
    let (hook_stop, _) = warnings_collector();
    let config = HooksConfig {
        stop: vec![HookMatcher {
            matcher: None,
            hooks: vec![hook("echo keep-going >&2; exit 2")],
        }],
        ..HooksConfig::default()
    };
    let hooks = CommandHooks::new(
        &config,
        &HooksConfig::default(),
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
    for _ in 0..MAX_WAKES_PER_WINDOW {
        hooks.run_stop_once(&completed_quiet()).await;
    }
    hooks.run_stop_once(&completed_quiet()).await;
    assert_eq!(hooks.stop_wakes(), MAX_WAKES_PER_WINDOW);
    assert_eq!(hooks.degraded(), 1);

    // Waking followups record no degrade; the gated inject does.
    let session = Session::open(root.dir.join("s.jsonl")).unwrap();
    let degraded: Vec<bool> = session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::InboxEnqueued { degraded, .. } => Some(*degraded),
            _ => None,
        })
        .collect();
    assert_eq!(
        degraded,
        [vec![false; MAX_WAKES_PER_WINDOW as usize], vec![true]].concat()
    );

    // The wake notice names the one waiting continuation.
    let notices = wake_notices.lock().unwrap().clone();
    assert_eq!(
        notices,
        vec![
            "wake budget used up: 1 job notices are waiting — send any message to continue"
                .to_owned()
        ]
    );
}
