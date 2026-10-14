//! The footer's rows and items (ADR 0027 C2, slice 1): the `pi` default, the `classic`
//! preset, glyphs, thresholds, truncation and the warnings. Pure `App` state on a
//! `TestBackend`, so no pty is needed.

use agent_core::{BudgetReport, UsageSource};
use radian_host::config::{TuiFooterConfig, TuiFooterItemConfig, TuiFooterRowConfig};
use radian_tui::App;
use radian_tui::footer::{Footer, Preset};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

fn render(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, app))
        .unwrap();
    terminal
}

fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    (area.left()..area.right())
        .map(|x| buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
        .collect()
}

fn fg_at(terminal: &Terminal<TestBackend>, y: u16, needle: &str) -> Option<Color> {
    let buffer = terminal.backend().buffer();
    let text = row(terminal, y);
    let col = text.find(needle)?;
    buffer.cell((col as u16, y)).map(|c| c.fg)
}

fn item(id: &str) -> TuiFooterItemConfig {
    TuiFooterItemConfig {
        id: id.to_string(),
        ..Default::default()
    }
}

/// `preset = classic` config.
fn classic() -> TuiFooterConfig {
    TuiFooterConfig {
        preset: Some("classic".to_string()),
        ..Default::default()
    }
}

/// A config with the glyph set filled in (a struct literal, so clippy's
/// `field_reassign_with_default` stays quiet).
fn with_glyphs(name: &str) -> TuiFooterConfig {
    TuiFooterConfig {
        glyphs: Some(name.to_string()),
        ..Default::default()
    }
}

/// The default is the two-row `pi` layout: one row with the model, one with the tokens.
#[test]
fn the_default_footer_is_two_rows() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(
        Some(std::path::PathBuf::from("/tmp/ws")),
        Some("Mock 5".to_string()),
        Some("opencode-go".to_string()),
        Some("high".to_string()),
    );
    app.set_git_branch(Some("main".to_string()));
    let terminal = render(&mut app, 100, 12);
    let first = row(&terminal, 10);
    let second = row(&terminal, 11);
    assert!(first.contains("Mock 5"), "{first:?}");
    assert!(
        first.contains("main"),
        "the branch is on the first row: {first:?}"
    );
    assert!(
        first.contains('◐'),
        "the reasoning glyph is on the first row: {first:?}"
    );
    assert!(
        second.contains('↑'),
        "the tokens row is the second: {second:?}"
    );
    assert_eq!(app.footer.preset(), Preset::Pi);
}

/// `preset = classic` is the pre-ADR one-line status bar: one row, the classic segments.
#[test]
fn classic_is_the_one_line_status_bar() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&classic());
    let terminal = render(&mut app, 140, 12);
    let line = row(&terminal, 11);
    assert!(line.contains("mock"), "{line:?}");
    assert!(line.contains("usage none"), "{line:?}");
    assert!(line.contains("mcp off"), "{line:?}");
    assert!(
        line.contains("context budget off (no window configured)"),
        "{line:?}"
    );
    // The row above is the input rule, not a second footer row.
    assert!(
        !row(&terminal, 10).contains("usage none"),
        "{}",
        row(&terminal, 10)
    );
}

/// The three glyph sets differ, and the `reasoning` item's icon follows the set.
#[test]
fn the_glyph_sets_differ() {
    for (name, glyph) in [("unicode", '◐'), ("ascii", '*')] {
        let mut app = App::new("mock", "s");
        app.apply_footer(&with_glyphs(name));
        app.set_footer_facts(None, None, None, Some("high".to_string()));
        let terminal = render(&mut app, 80, 12);
        assert!(
            row(&terminal, 10).contains(glyph),
            "{name}: {}",
            row(&terminal, 10)
        );
    }
    // Nerd Font: the private-use code point, not the unicode one.
    let mut app = App::new("mock", "s");
    app.apply_footer(&with_glyphs("nerd"));
    app.set_footer_facts(None, None, None, Some("high".to_string()));
    let terminal = render(&mut app, 80, 12);
    assert!(
        row(&terminal, 10).contains('\u{f0eb}'),
        "{}",
        row(&terminal, 10)
    );
}

/// The 70/90 thresholds: a context at 75% uses the warning role, at 95% the error role.
#[test]
fn the_context_thresholds_change_the_style() {
    for (tokens, expected_role) in [(750u64, "warn"), (950, "error")] {
        let mut app = App::new("mock", "s");
        app.projection.set_context_window(Some(1000));
        app.projection.set_request_started(Some(BudgetReport {
            source: UsageSource::Estimated,
            projected_tokens: tokens,
            input_budget_tokens: 1000,
            request_bytes: 0,
            baseline_input_tokens: None,
            estimated_growth_tokens: None,
        }));
        let terminal = render(&mut app, 80, 12);
        let cell = fg_at(&terminal, 10, "━").expect("the filled bar cell");
        let expected = match expected_role {
            "warn" => app.theme.budget_over.fg,
            _ => app.theme.tool_result_error.fg,
        };
        assert_eq!(Some(cell), expected, "{expected_role} at {tokens} tokens");
    }
}

/// `context_bar`'s own parameters: `width` cells, `fill` for the used part, `empty` for the
/// rest.
#[test]
fn the_context_bar_takes_width_fill_and_empty() {
    let config = TuiFooterConfig {
        rows: vec![TuiFooterRowConfig {
            left: vec![TuiFooterItemConfig {
                id: "context_bar".to_string(),
                format: Some("{bar}".to_string()),
                params: vec![
                    ("width".to_string(), "4".to_string()),
                    ("fill".to_string(), "=".to_string()),
                    ("empty".to_string(), ".".to_string()),
                ],
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut app = App::new("mock", "s");
    app.apply_footer(&config);
    app.projection.set_context_window(Some(1000));
    app.projection.set_request_started(Some(BudgetReport {
        source: UsageSource::Estimated,
        projected_tokens: 500,
        input_budget_tokens: 1000,
        request_bytes: 0,
        baseline_input_tokens: None,
        estimated_growth_tokens: None,
    }));
    let terminal = render(&mut app, 40, 12);
    assert!(
        row(&terminal, 11).contains("==.."),
        "{}",
        row(&terminal, 11)
    );
}

/// A narrow terminal drops the right group first, then the left group from its end; a lower
/// `priority` drops first.
#[test]
fn truncation_drops_the_right_group_then_the_left_and_priority_wins() {
    // Group order: left = model, right = reasoning. One item must go at a middling width; the
    // right group goes, never the left.
    let config = TuiFooterConfig {
        rows: vec![TuiFooterRowConfig {
            left: vec![item("model")],
            right: vec![item("reasoning")],
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut app = App::new("long-model-name", "s");
    app.apply_footer(&config);
    app.set_footer_facts(None, None, None, Some("high".to_string()));
    let wide = render(&mut app, 120, 12);
    let wide_text = row(&wide, 11);
    assert!(
        wide_text.contains("long-model-name") && wide_text.contains('◐'),
        "{wide_text:?}"
    );
    let mid = render(&mut app, 20, 12);
    let mid_text = row(&mid, 11);
    assert!(
        mid_text.contains("long-model-name"),
        "the left group stays: {mid_text:?}"
    );
    assert!(
        !mid_text.contains('◐'),
        "the right group drops first: {mid_text:?}"
    );

    // Priority: within the left group, the lower priority drops first (the model here).
    let config = TuiFooterConfig {
        rows: vec![TuiFooterRowConfig {
            left: vec![
                TuiFooterItemConfig {
                    id: "model".to_string(),
                    priority: Some(1),
                    ..Default::default()
                },
                TuiFooterItemConfig {
                    id: "session".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut app = App::new("long-model-name", "s");
    app.apply_footer(&config);
    let terminal = render(&mut app, 14, 12);
    let text = row(&terminal, 11);
    assert!(
        !text.contains("long-model-name"),
        "the low-priority item drops: {text:?}"
    );
    assert!(text.contains('s'), "the session item stays: {text:?}");
}

/// An unknown item id, placeholder or style is shown raw with one warning (never a refusal).
#[test]
fn unknown_items_placeholders_and_styles_warn_once_and_show_raw() {
    let config = TuiFooterConfig {
        rows: vec![TuiFooterRowConfig {
            left: vec![
                TuiFooterItemConfig {
                    id: "future_item".to_string(),
                    ..Default::default()
                },
                TuiFooterItemConfig {
                    id: "model".to_string(),
                    format: Some("{future_placeholder}".to_string()),
                    style: Some("neon".to_string()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    };
    let (footer, warnings) = Footer::from_config(&config);
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    assert!(
        warnings.iter().any(|w| w.contains("future_item")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w.contains("future_placeholder")),
        "{warnings:?}"
    );
    assert!(warnings.iter().any(|w| w.contains("neon")), "{warnings:?}");
    let mut app = App::new("mock", "s");
    app.footer = footer;
    let terminal = render(&mut app, 120, 12);
    let text = row(&terminal, 11);
    assert!(
        text.contains("future_item"),
        "the unknown id shows raw: {text:?}"
    );
    assert!(
        text.contains("{future_placeholder}"),
        "the unknown placeholder shows raw: {text:?}"
    );
}

/// The draw path never calls git (ADR 0023 U2/U3): the branch is a fact on `App`.
#[test]
fn the_draw_path_does_not_call_git() {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/footer.rs"))
        .expect("footer.rs");
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in ["std::process", "Command::new", "git "] {
        assert!(
            !code.contains(forbidden),
            "footer.rs must not call {forbidden}"
        );
    }
}
