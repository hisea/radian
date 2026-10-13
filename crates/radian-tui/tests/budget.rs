//! The context-budget projection in the status bar (ADR 0015 wording, ADR 0018 `T / B`).
//! Pure `App` state plus the formatting functions, so it runs everywhere.

use agent_core::{AgentEvent, BudgetReport, EventSink, UsageSource};
use radian_tui::App;
use radian_tui::app::{budget_status_text, budget_text};

fn report(source: UsageSource) -> BudgetReport {
    BudgetReport {
        source,
        projected_tokens: 12340,
        input_budget_tokens: 16000,
        request_bytes: 40000,
        baseline_input_tokens: match source {
            UsageSource::Estimated => None,
            _ => Some(12_000),
        },
        estimated_growth_tokens: match source {
            UsageSource::Estimated => None,
            UsageSource::Reported => Some(0),
            UsageSource::ReportedPlusEstimate => Some(340),
        },
    }
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
fn the_status_bar_shows_the_projection_over_the_input_budget() {
    // ADR 0018: `T / B`.
    let text = budget_text(&report(UsageSource::ReportedPlusEstimate));
    assert!(
        text.contains("/ 16000"),
        "the input budget must be shown: {text}"
    );
    assert!(text.contains("12340"), "{text}");
}

#[test]
fn no_budget_says_off_and_claims_no_number() {
    let off = budget_status_text(None, Some(&report(UsageSource::Estimated)));
    assert!(off.contains("off"), "{off}");
    assert!(off.contains("no window"), "{off}");
    assert!(
        !off.chars().any(|c| c.is_ascii_digit()),
        "no budget is configured; no number may be shown: {off}"
    );
    let configured = budget_status_text(Some(16_000), None);
    assert!(
        configured.contains("B 16000") && configured.contains("no request"),
        "{configured}"
    );
}

/// ADR 0023 U5: the request's budget is a **session fact**, so the event sink writes it into
/// the projection and `App` never holds it. The status bar then reads the projection.
#[test]
fn a_request_started_event_sets_the_status_bar_report() {
    use std::sync::Arc;
    use std::sync::mpsc::channel;

    let app = App::new("m", "s");
    app.projection.set_context_window(Some(16_000));
    let report = report(UsageSource::ReportedPlusEstimate);
    let (tx, _rx) = channel();
    let sink = radian_tui::events::ProjectionSink::new(tx, Arc::clone(&app.projection));
    sink.emit(AgentEvent::ModelRequestStarted {
        model_call: 1,
        budget: Some(report.clone()),
    });
    let session = app.projection.read();
    assert_eq!(session.budget(), Some(&report));
    assert_eq!(session.turn(), radian_tui::TurnState::Running);
    let text = budget_status_text(session.context_window(), session.budget());
    assert!(text.contains("12340"), "{text}");
    assert!(text.contains("/ 16000"), "{text}");
}
