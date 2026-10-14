//! The `/model` switch state machine (ADR 0026 D5/D7): the completion tree's two halves, the
//! direct queue when the context fits, the confirm modal when it does not, and the paths that
//! must never queue.

use radian_tui::app::{App, Effect, Input};
use radian_tui::command::{self, Outcome};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn parse(line: &str) -> Outcome {
    command::parse(line, false)
}

/// Two-level completion: the provider half, then the model half of the same word.
#[test]
fn the_completion_tree_offers_providers_then_models() {
    // `/model ` completes the provider half (the candidates need the host: `Needs`).
    let level = command::complete_in(command::tree(), "/model ", 7).expect("a completion");
    assert!(
        matches!(
            level.level_result,
            command::Level::Needs(command::Fetch::ModelRoute { provider: None, .. })
        ),
        "the provider half needs the host: {:?}",
        level.level_result
    );
    // After the slash, the same level is `Needs` **with** the provider named.
    let level =
        command::complete_in(command::tree(), "/model opencode-go/", 19).expect("a completion");
    match level.level_result {
        command::Level::Needs(command::Fetch::ModelRoute { provider, .. }) => {
            assert_eq!(provider.as_deref(), Some("opencode-go"));
        }
        other => panic!("the model half needs the host with the provider: {other:?}"),
    }
    // And the parse reads the same word.
    match parse("/model opencode-go/deepseek-v4.1-flash") {
        Outcome::Model(switch) => {
            assert_eq!(switch.provider, "opencode-go");
            assert_eq!(switch.model, "deepseek-v4.1-flash");
        }
        other => panic!("expected a switch, got {other:?}"),
    }
}

/// The model is never defaulted: a missing half is a usage notice, not a guess.
#[test]
fn a_partial_target_is_a_usage_notice() {
    match parse("/model opencode-go") {
        Outcome::Notices(notices) => {
            assert!(notices[0].contains("usage"), "{notices:?}");
        }
        other => panic!("expected a usage notice, got {other:?}"),
    }
}

#[test]
fn a_fitting_switch_queues_directly() {
    let mut app = App::new("m", "s");
    let result = app.update(Input::ModelChecked {
        provider: "anthropic".into(),
        model: "claude".into(),
        tokens: Some(100),
        budget: Some(200),
    });
    assert!(
        result.effects.iter().any(|effect| matches!(
            effect,
            Effect::QueueModel { provider, model }
                if provider == "anthropic" && model == "claude"
        )),
        "a fitting switch queues: {:?}",
        result.effects
    );
    assert_eq!(app.next_model.as_deref(), Some("anthropic/claude"));
    // The route is the whole change: no key, no header values anywhere in the effect.
}

/// `T > B'` asks first (D7): the modal is up and nothing is queued yet.
#[test]
fn an_over_budget_switch_asks_first() {
    let mut app = App::new("m", "s");
    let result = app.update(Input::ModelChecked {
        provider: "anthropic".into(),
        model: "claude".into(),
        tokens: Some(500),
        budget: Some(200),
    });
    assert!(
        result.effects.is_empty(),
        "nothing is queued before the yes: {:?}",
        result.effects
    );
    assert!(app.pending_command().is_some(), "the confirm modal is up");
    assert!(app.next_model.is_none());
    // y compacts first (D7), targeting the new model's budget.
    let result = app.update(Input::ConfirmCommand(true));
    assert!(result.effects.iter().any(|effect| matches!(
        effect,
        Effect::CompactForSwitch { provider, budget, .. }
            if provider == "anthropic" && *budget == 200
    )));
}

/// `n` cancels: no compaction, no queue.
#[test]
fn a_declined_confirm_queues_nothing() {
    let mut app = App::new("m", "s");
    app.update(Input::ModelChecked {
        provider: "anthropic".into(),
        model: "claude".into(),
        tokens: Some(500),
        budget: Some(200),
    });
    let result = app.update(Input::ConfirmCommand(false));
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert!(app.next_model.is_none());
}

/// A failed pre-switch compaction never queues (D7).
#[test]
fn a_failed_compaction_does_not_queue() {
    let mut app = App::new("m", "s");
    let result = app.update(Input::SwitchCompacted {
        provider: "anthropic".into(),
        model: "claude".into(),
        result: Err("the summarizer failed".into()),
    });
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert!(app.next_model.is_none());
    assert!(
        app.pending_command().is_none(),
        "no modal stays up after the failure"
    );
}

/// A succeeded pre-switch compaction queues (D7).
#[test]
fn a_succeeded_compaction_queues_the_switch() {
    let mut app = App::new("m", "s");
    let result = app.update(Input::SwitchCompacted {
        provider: "anthropic".into(),
        model: "claude".into(),
        result: Ok(agent_core::CompactionOutcome::Compacted {
            revision: 1,
            tokens_before: 500,
            tokens_after: 100,
        }),
    });
    assert!(
        result
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::QueueModel { .. })),
        "{:?}",
        result.effects
    );
}

/// An unknown budget (`B'` unknown) queues without a comparison: it cannot exceed what is
/// unknown, and refusing every switch on a catalog miss would make `/model` useless offline.
#[test]
fn an_unknown_budget_queues_without_a_comparison() {
    let mut app = App::new("m", "s");
    let result = app.update(Input::ModelChecked {
        provider: "opencode-go".into(),
        model: "deepseek-v4.1-flash".into(),
        tokens: Some(900),
        budget: None,
    });
    assert!(
        result
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::QueueModel { .. })),
        "{:?}",
        result.effects
    );
}

/// The queued `next:` marker clears when the driver applies the change.
#[test]
fn the_next_marker_clears_when_the_change_is_applied() {
    let mut app = App::new("m", "s");
    app.update(Input::ModelChecked {
        provider: "anthropic".into(),
        model: "claude".into(),
        tokens: None,
        budget: None,
    });
    assert!(app.next_model.is_some());
    let event = radian_session::RuntimeEvent::ConfigChanged {
        revision: 1,
        change: radian_host::ModelRoute::new("anthropic", "claude").to_value(),
    };
    app.update(Input::Runtime(event));
    assert!(app.next_model.is_none(), "the promise is spent");
}

/// The keys answer the confirm even though it is a switch: hardcoded y/n (ADR 0023 U4).
#[test]
fn the_confirm_keys_are_hardcoded() {
    let mut app = App::new("m", "s");
    app.update(Input::ModelChecked {
        provider: "anthropic".into(),
        model: "claude".into(),
        tokens: Some(500),
        budget: Some(200),
    });
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let result = app.on_key(key(KeyCode::Char('y')));
    assert!(
        result.effects.iter().any(|effect| matches!(
            effect,
            Effect::CompactForSwitch { provider, .. } if provider == "anthropic"
        )),
        "y compacts first (D7): {:?}",
        result.effects
    );
    // And the queue only happens once the compaction succeeds.
    assert!(app.next_model.is_none());
}
