//! The `/model` switch state machine (ADR 0026 D5/D7): the completion tree's two halves, the
//! direct queue when the context fits, the confirm modal when it does not, and the paths that
//! must never queue.

use radian_tui::app::{App, Effect, Input};
use radian_tui::command::{self, Outcome};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn parse(line: &str) -> Outcome {
    command::parse(line, false)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Two-level completion: the provider half, then the model half of the same word.
#[test]
fn the_completion_tree_offers_providers_then_models() {
    // `/model ` offers the flat list of every logged-in provider's models (ADR 0035
    // revision 2); the candidates need the host, so the level is `Needs`.
    let level = command::complete_in(command::tree(), "/model ", 7).expect("a completion");
    assert!(
        matches!(
            level.level_result,
            command::Level::Needs(command::Fetch::ModelRoute { provider: None, .. })
        ),
        "the flat list needs the host: {:?}",
        level.level_result
    );
    // The direct `provider/model` form is the same flat level; the typed word is the partial.
    let level =
        command::complete_in(command::tree(), "/model opencode-go/", 19).expect("a completion");
    match level.level_result {
        command::Level::Needs(command::Fetch::ModelRoute {
            provider: None,
            partial,
        }) => {
            assert_eq!(partial, "opencode-go/");
        }
        other => panic!("the flat list needs the host: {other:?}"),
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

// ---------- /thinking (ADR 0026 D11) ----------

/// The completion offers only the levels the **current** model supports; the loop resolves
/// them from the route's map.
#[test]
fn the_thinking_completion_needs_the_host() {
    let level = command::complete_in(command::tree(), "/thinking ", 10).expect("a completion");
    assert!(
        matches!(
            level.level_result,
            command::Level::Needs(command::Fetch::ThinkingLevels { .. })
        ),
        "{:?}",
        level.level_result
    );
}

/// A clamped level queues, and the clamp is named with the supported list.
#[test]
fn a_thinking_level_queues_after_the_clamp() {
    let mut app = App::new("m", "s");
    let result = app.update(Input::ThinkingChecked {
        requested: "medium".into(),
        applied: Some("high".into()),
        supported: vec!["low".into(), "high".into(), "max".into()],
    });
    assert!(
        result.effects.iter().any(|effect| matches!(
            effect,
            Effect::QueueThinking { level } if level == "high"
        )),
        "{:?}",
        result.effects
    );
    assert_eq!(app.next_thinking.as_deref(), Some("high"));
}

/// A model with no supported level changes nothing.
#[test]
fn a_levelless_model_changes_nothing() {
    let mut app = App::new("m", "s");
    let result = app.update(Input::ThinkingChecked {
        requested: "high".into(),
        applied: None,
        supported: Vec::new(),
    });
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert!(app.next_thinking.is_none());
}

/// The queued level clears when the driver applies it (same rule as `next:`).
#[test]
fn the_thinking_marker_clears_when_applied() {
    let mut app = App::new("m", "s");
    app.update(Input::ThinkingChecked {
        requested: "high".into(),
        applied: Some("high".into()),
        supported: vec!["high".into()],
    });
    assert!(app.next_thinking.is_some());
    app.update(Input::Runtime(
        radian_session::RuntimeEvent::ConfigChanged {
            revision: 3,
            change: serde_json::json!({"thinking": "high"}),
        },
    ));
    assert!(app.next_thinking.is_none());
}

/// p4's provider completion (ADR 0026 D10): the list a switch could name comes from the
/// built-ins, the config's declarations and the usable `auth.json` entries — each resolved
/// with `resolve_provider`, so the completion and the switch agree.
#[test]
fn the_provider_list_resolves_built_ins_config_and_auth() {
    // The pure part of the rule is `resolve_provider`: a built-in resolves; an unknown name
    // does not; a config declaration resolves to a named provider.
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "acme".to_owned(),
        radian_host::ProviderDecl {
            kind: "openai-compatible".to_owned(),
            base_url: "https://acme.example/v1".to_owned(),
            reasoning_replay: None,
        },
    );
    for name in [
        "opencode-go",
        "anthropic",
        "openai",
        "openai-compatible",
        "acme",
    ] {
        assert!(
            radian_host::resolve_provider(name, &providers, None).is_ok(),
            "{name} must resolve"
        );
    }
    assert!(
        radian_host::resolve_provider("nope", &providers, None).is_err(),
        "an unknown name must not be offered"
    );
}

/// ADR 0035: the flat `/model` list. Typing a provider name matches that provider's
/// rows (the insert is `provider/model`, so both halves are searchable), and accepting inserts
/// the pair.
#[test]
fn the_flat_model_list_filters_by_provider_and_accepts_the_pair() {
    let flat = vec![
        command::Candidate {
            insert: "opencode-go/deepseek-v4.1-flash".into(),
            description: "opencode-go".into(),
        },
        command::Candidate {
            insert: "openai/gpt-5".into(),
            description: "openai".into(),
        },
        command::Candidate {
            insert: "anthropic/claude-4".into(),
            description: "anthropic".into(),
        },
    ];
    // Typing `opencode` keeps only that provider's rows; a model name still matches.
    let ranked = command::rank(flat.clone(), "opencode");
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].insert, "opencode-go/deepseek-v4.1-flash");
    let ranked = command::rank(flat.clone(), "gpt");
    assert_eq!(ranked[0].insert, "openai/gpt-5");

    // The app menu shows the provider column and accepts the `provider/model` pair.
    let mut app = App::new("m", "s");
    for c in "/model ".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.update(Input::CompletionsReady {
        fetch: command::Fetch::ModelRoute {
            provider: None,
            partial: String::new(),
        },
        candidates: flat,
    });
    let rows = radian_tui::render::completion_rows(&app, 80, 10);
    let text: String = rows
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("deepseek-v4.1-flash"), "{text}");
    assert!(text.contains("opencode-go"), "the provider column: {text}");
    let result = app.on_key(key(KeyCode::Enter));
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert_eq!(app.input, "/model opencode-go/deepseek-v4.1-flash ");
}
