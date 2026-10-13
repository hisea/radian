//! The context-budget stop: the wording of the three usage sources, and the keys that answer
//! it. Pure `App` state plus the formatting functions, so it runs everywhere.

use agent_core::{BudgetReport, UsageSource};
use radian_tui::app::{budget_status_text, budget_text};
use radian_tui::{App, Send};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn report(source: UsageSource) -> BudgetReport {
    BudgetReport {
        source,
        projected_tokens: 12340,
        window_tokens: 16000,
        threshold_percent: 80,
        request_bytes: 40000,
        baseline_input_tokens: match source {
            UsageSource::Estimated => None,
            _ => Some(12000),
        },
        estimated_growth_tokens: match source {
            UsageSource::Estimated => None,
            UsageSource::Reported => Some(0),
            UsageSource::ReportedPlusEstimate => Some(340),
        },
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn the_three_usage_sources_read_differently() {
    // ADR 0015 C1: a report, a report plus an estimate, and a pure estimate must be
    // distinguishable, and the estimate must never look like a precise report.
    let reported = budget_text(&report(UsageSource::Reported));
    assert!(reported.contains("12340"), "{reported}");
    assert!(reported.contains("(reported)"), "{reported}");
    assert!(!reported.contains("estimated"), "{reported}");
    assert!(!reported.contains('~'), "{reported}");

    let mixed = budget_text(&report(UsageSource::ReportedPlusEstimate));
    assert!(
        mixed.contains("(reported 12000 + estimated 340)"),
        "{mixed}"
    );
    assert!(
        !mixed.contains("(reported)"),
        "a mixed value must not be labelled a report: {mixed}"
    );

    let estimated = budget_text(&report(UsageSource::Estimated));
    assert!(estimated.contains("~12340"), "{estimated}");
    assert!(estimated.contains("(estimated)"), "{estimated}");
    assert!(!estimated.contains("(reported"), "{estimated}");
}

#[test]
fn no_window_says_the_guard_is_off_and_claims_no_number() {
    let off = budget_status_text(None, Some(&report(UsageSource::Estimated)));
    assert!(off.contains("off"), "{off}");
    assert!(off.contains("no window"), "{off}");
    assert!(
        !off.chars().any(|c| c.is_ascii_digit()),
        "the guard is off; no number may be shown: {off}"
    );
    let on = budget_status_text(Some(16000), None);
    assert!(on.contains("on") && on.contains("16000"), "{on}");
}

#[test]
fn the_stop_keys_continue_or_dismiss_and_never_swallow_typing() {
    let mut app = App::new("m", "s");
    app.set_context_window(Some(16000));
    app.budget_stop = Some(report(UsageSource::Estimated));

    // `c` continues once and clears the prompt.
    assert_eq!(
        app.on_key(key(KeyCode::Char('c'))),
        Some(Send::ContinueBudget)
    );
    assert!(app.budget_stop.is_none());

    // Esc dismisses without sending anything.
    app.budget_stop = Some(report(UsageSource::Estimated));
    assert_eq!(app.on_key(key(KeyCode::Esc)), None);
    assert!(app.budget_stop.is_none());

    // A typed message is never swallowed by the prompt keys: with text in the box, `c` types.
    app.budget_stop = Some(report(UsageSource::Estimated));
    app.on_key(key(KeyCode::Char('h')));
    assert_eq!(app.on_key(key(KeyCode::Char('c'))), None);
    assert_eq!(app.input, "hc", "the key was swallowed by the prompt");
    assert!(app.budget_stop.is_some(), "the stop is still up");
}

#[test]
fn a_budget_stop_is_cleared_when_the_next_turn_starts() {
    let mut app = App::new("m", "s");
    app.budget_stop = Some(report(UsageSource::Estimated));
    app.apply_event(&agent_core::AgentEvent::TurnStarted);
    assert!(
        app.budget_stop.is_none(),
        "a new turn makes the stop prompt stale"
    );
}
