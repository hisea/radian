//! `tui.bell`: one `\x07` for attention-class events, off by default.
//!
//! Pure `update` tests (ADR 0023 U3): a newly shown approval/question and a completed or
//! failed turn end produce exactly one `Effect::Bell` in every mode but `off`; a background
//! job's settle only in `all`. The loop runs the effect (one BEL per frame); a repeated
//! event for the same head never re-rings.

use radian_host::{PendingRequest, PendingView};
use radian_permissions::ApprovalRequest;
use radian_session::{RuntimeEvent, TurnOutcome};
use radian_tui::app::{App, Effect, Input};
use radian_tui::bell::BellMode;
use serde_json::json;

fn view(id: u64) -> PendingView<PendingRequest> {
    PendingView {
        request_id: id,
        origin: "session".into(),
        request: PendingRequest::Approval(ApprovalRequest::new(
            "perm-1",
            "call-1",
            "write",
            "sub/../out.txt",
            "/ws/out.txt",
            json!({"path": "sub/../out.txt", "content": "hello"}),
            None,
        )),
    }
}

fn pending(app: &mut App, id: u64, len: usize) -> Vec<Effect> {
    app.update(Input::Pending(Some(Box::new(view(id))), len))
        .effects
}

fn bells(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|effect| **effect == Effect::Bell)
        .count()
}

fn finished(outcome: TurnOutcome) -> Input {
    Input::Runtime(RuntimeEvent::TurnFinished {
        turn_id: "t-1".into(),
        outcome,
        reason: None,
        stop_reason: None,
    })
}

/// One approval prompt rings exactly once under `attention`: showing it rings, showing the
/// same head again does not, and the next request rings again.
#[test]
fn attention_rings_once_per_newly_shown_prompt() {
    let mut app = App::new("m", "s");
    app.bell = BellMode::Attention;
    assert_eq!(bells(&pending(&mut app, 1, 1)), 1, "a new prompt rings");
    assert_eq!(
        bells(&pending(&mut app, 1, 1)),
        0,
        "the same head never re-rings"
    );
    assert_eq!(bells(&pending(&mut app, 2, 2)), 1, "the next prompt rings");
}

/// `off` never rings — not for a prompt, not for a turn end. A mode that rings under `off`
/// (or a default that is not `off`) fails this test.
#[test]
fn off_never_rings() {
    let mut app = App::new("m", "s");
    assert_eq!(app.bell, BellMode::Off, "the default is off");
    assert_eq!(bells(&pending(&mut app, 1, 1)), 0);
    assert_eq!(
        bells(&app.update(finished(TurnOutcome::Completed)).effects),
        0
    );
    assert_eq!(bells(&app.update(finished(TurnOutcome::Failed)).effects), 0);
}

/// A completed or failed turn rings once; a cancelled turn needs no ring (the user pressed
/// the key themselves).
#[test]
fn a_turn_end_rings_for_completed_and_failed_but_not_cancelled() {
    let mut app = App::new("m", "s");
    app.bell = BellMode::Attention;
    assert_eq!(
        bells(&app.update(finished(TurnOutcome::Completed)).effects),
        1
    );
    assert_eq!(bells(&app.update(finished(TurnOutcome::Failed)).effects), 1);
    assert_eq!(
        bells(&app.update(finished(TurnOutcome::Cancelled)).effects),
        0
    );
}

/// A background job's settle rings only under `all`.
#[test]
fn a_job_settle_rings_only_under_all() {
    let settled = || {
        Input::Runtime(RuntimeEvent::BackgroundJobSettled {
            job_id: "job-1".into(),
            state: "exited".into(),
            duration_ms: 1000,
        })
    };
    let mut app = App::new("m", "s");
    app.bell = BellMode::Attention;
    assert_eq!(bells(&app.update(settled()).effects), 0);
    app.bell = BellMode::All;
    assert_eq!(bells(&app.update(settled()).effects), 1);
    app.bell = BellMode::Off;
    assert_eq!(bells(&app.update(settled()).effects), 0);
}

/// `tui.bell` folds through `apply_config`: every spelling lands, an unknown one warns and
/// falls back to `off`, and `NO_COLOR` does not affect it.
#[test]
fn the_bell_mode_folds_through_apply_config() {
    fn config(text: &str) -> radian_host::config::Config {
        radian_host::config::parse(std::path::Path::new("/tmp/config.toml"), text).unwrap()
    }
    for (spelling, expected) in [
        ("off", BellMode::Off),
        ("attention", BellMode::Attention),
        ("all", BellMode::All),
    ] {
        for no_color in [false, true] {
            let mut app = App::new("m", "s");
            let warnings = app
                .apply_config(
                    &config(&format!(
                        "schema_version = 1\n[tui]\nbell = \"{spelling}\"\n"
                    )),
                    no_color,
                )
                .unwrap();
            assert_eq!(app.bell, expected, "{spelling}");
            assert!(
                warnings.iter().all(|warning| !warning.contains("tui.bell")),
                "{spelling}: no warning: {warnings:?}"
            );
        }
    }

    let mut app = App::new("m", "s");
    let warnings = app
        .apply_config(
            &config("schema_version = 1\n[tui]\nbell = \"loud\"\n"),
            false,
        )
        .unwrap();
    assert_eq!(app.bell, BellMode::Off, "an unknown mode falls back to off");
    assert!(
        warnings.iter().any(|warning| warning.contains("tui.bell")),
        "{warnings:?}"
    );
}
