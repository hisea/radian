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

fn screen(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            out.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        out.push('\n');
    }
    out
}

fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    (area.left()..area.right())
        .map(|x| buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
        .collect()
}

/// The foreground of the cell where `needle` starts. The byte index of `find` is mapped back
/// to the **cell** column: a wide glyph (the brand `㎭`) is several bytes but one cell, so the
/// raw byte index would point at the wrong cell.
fn fg_at(terminal: &Terminal<TestBackend>, y: u16, needle: &str) -> Option<Color> {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    let mut text = String::new();
    let mut cells: Vec<u16> = Vec::new();
    for x in area.left()..area.right() {
        let symbol = buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
        text.push_str(symbol);
        for _ in 0..symbol.len() {
            cells.push(x);
        }
    }
    let byte = text.find(needle)?;
    buffer.cell((*cells.get(byte)?, y)).map(|c| c.fg)
}

fn item(id: &str) -> TuiFooterItemConfig {
    TuiFooterItemConfig {
        id: id.to_string(),
        ..Default::default()
    }
}

/// `preset = classic` config.
/// A config whose single row is `left`.
fn rows(left: Vec<TuiFooterItemConfig>) -> TuiFooterConfig {
    TuiFooterConfig {
        rows: vec![TuiFooterRowConfig {
            left,
            ..Default::default()
        }],
        ..Default::default()
    }
}

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

/// The default is the `deck` layout (ADR 0027): the usage row (dropped with no
/// data), the identity row, then the numbers row.
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
    assert_eq!(app.footer.preset(), Preset::Deck);
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
        // The default `full` preset uses the text `context`; this test is about the bar, so
        // it asks for `context_bar` explicitly.
        app.apply_footer(&rows(vec![item("context_bar")]));
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
        // One configured row: the footer is the last line.
        let cell = fg_at(&terminal, 11, "━").expect("the filled bar cell");
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

/// The draw path never calls git (ADR 0023 U2/U3, ADR 0027 C3): the git work lives in
/// `radian-host`'s contributor, behind a background task. A positive control pins that the
/// call is there, so this test cannot pass because the call moved nowhere.
#[test]
fn the_draw_path_does_not_call_git() {
    let strip = |source: &str| -> String {
        source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    for file in ["/src/footer.rs", "/src/render.rs"] {
        let source = std::fs::read_to_string(format!("{}{}", env!("CARGO_MANIFEST_DIR"), file))
            .expect("the source is readable");
        let code = strip(&source);
        for forbidden in ["std::process", "Command::new"] {
            assert!(
                !code.contains(forbidden),
                "{file} must not call {forbidden} (the draw path does no I/O)"
            );
        }
    }
    // The positive control: the git calls live in the contributor, not in the TUI.
    let contrib = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../radian-host/src/contrib.rs"
    ))
    .expect("contrib.rs is readable");
    assert!(
        contrib.contains("Command::new(\"git\")"),
        "the git contributor is where the git call belongs"
    );
}

/// A helper: a contribution with one normal segment.
fn publish(app: &App, item: &str, text: &str) {
    app.contributions.write(
        item,
        vec![radian_host::Segment::new(
            text,
            radian_host::SegmentRole::Normal,
        )],
    );
}

/// The `worktree` item (ADR 0027 C2/C3) shows only when the contributor published one.
#[test]
fn the_worktree_item_shows_only_when_the_store_has_one() {
    let mut app = App::new("mock", "s");
    app.git_branch = None;
    app.apply_footer(&rows(vec![item("model"), item("worktree")]));
    let before = render(&mut app, 80, 12);
    assert!(
        !row(&before, 11).contains("worktree"),
        "{}",
        row(&before, 11)
    );

    publish(&app, "worktree", "worktree my-branch");
    let after = render(&mut app, 80, 12);
    assert!(
        row(&after, 11).contains("my-branch"),
        "the worktree appears: {}",
        row(&after, 11)
    );
}

/// The `status` item lists the `status.*` contributions, filtered by `keys`.
#[test]
fn the_status_item_lists_status_contributions_and_filters_by_keys() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![item("status")]));
    publish(&app, "status.alpha", "alpha-text");
    publish(&app, "status.beta", "beta-text");
    let all = render(&mut app, 80, 12);
    let text = row(&all, 11);
    assert!(
        text.contains("alpha-text") && text.contains("beta-text"),
        "{text:?}"
    );

    let mut filtered = App::new("mock", "s");
    filtered.apply_footer(&rows(vec![TuiFooterItemConfig {
        id: "status".to_string(),
        params: vec![("keys".to_string(), "alpha".to_string())],
        ..Default::default()
    }]));
    publish(&filtered, "status.alpha", "alpha-text");
    publish(&filtered, "status.beta", "beta-text");
    let only = render(&mut filtered, 80, 12);
    let text = row(&only, 11);
    assert!(text.contains("alpha-text"), "{text:?}");
    assert!(!text.contains("beta-text"), "{text:?}");
}

/// A failed refresh keeps the value and draws it dim (the stale marker).
#[test]
fn a_stale_contribution_draws_with_the_dim_role() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![item("git_branch")]));
    publish(&app, "git_branch", "main");
    let fresh = render(&mut app, 80, 12);
    assert_eq!(
        fg_at(&fresh, 11, "main"),
        app.theme.text.fg,
        "a fresh value is the primary level"
    );
    app.contributions.mark_stale("git_branch");
    let stale = render(&mut app, 80, 12);
    assert_eq!(
        fg_at(&stale, 11, "main"),
        app.theme.truncation.fg,
        "a stale value is dim"
    );
}

/// `[tui.widgets]` puts a contributor line above and/or below the input box (ADR 0027 C3).
#[test]
fn widgets_render_above_and_below_the_input() {
    let mut app = App::new("mock", "s");
    app.widgets = radian_host::config::TuiWidgetsConfig {
        above_input: vec!["status".to_string()],
        below_input: Vec::new(),
    };
    publish(&app, "status.thing", "ABOVE-WIDGET");
    let terminal = render(&mut app, 80, 14);
    let text = screen(&terminal);
    let above = text.find("ABOVE-WIDGET").expect("the above widget");
    let input = text.find("(type a message").expect("the input hint");
    assert!(
        above < input,
        "the above widget precedes the input:\n{text}"
    );

    let mut app = App::new("mock", "s");
    app.widgets = radian_host::config::TuiWidgetsConfig {
        above_input: Vec::new(),
        below_input: vec!["status".to_string()],
    };
    publish(&app, "status.thing", "BELOW-WIDGET");
    let terminal = render(&mut app, 80, 14);
    let text = screen(&terminal);
    let input = text.find("(type a message").expect("the input hint");
    let below = text.find("BELOW-WIDGET").expect("the below widget");
    assert!(input < below, "the below widget follows the input:\n{text}");
}

/// The `brand` and `host` items (ADR 0027): the `㎭` wordmark (or `rad` under the
/// ASCII glyph set) and the machine's hostname.
#[test]
fn the_brand_and_host_items_show_their_values() {
    let mut app = App::new("mock", "s");
    app.set_hostname(Some("Mac-mini".to_string()));
    app.apply_footer(&rows(vec![item("brand"), item("host")]));
    let terminal = render(&mut app, 80, 12);
    let line = row(&terminal, 11);
    assert!(line.contains('㎭'), "the brand glyph: {line:?}");
    assert!(line.contains("Mac-mini"), "the hostname: {line:?}");

    // The ASCII glyph set falls back to `rad` (ADR 0027's 标志 section).
    let mut ascii = App::new("mock", "s");
    ascii.set_hostname(Some("Mac-mini".to_string()));
    ascii.apply_footer(&TuiFooterConfig {
        glyphs: Some("ascii".to_string()),
        rows: vec![TuiFooterRowConfig {
            left: vec![item("brand"), item("host")],
            ..Default::default()
        }],
        ..Default::default()
    });
    let terminal = render(&mut ascii, 80, 12);
    let line = row(&terminal, 11);
    assert!(line.contains("rad"), "{line:?}");
    assert!(!line.contains('㎭'), "ascii has no wide glyph: {line:?}");
}

/// `speed` and `elapsed` come from the injected clock (ADR 0027): the last finished
/// generation's output over its measured duration, and the live elapsed while one runs.
#[test]
fn the_speed_and_elapsed_items_use_the_injected_clock() {
    use std::time::{Duration, Instant};
    let base = Instant::now();
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![item("speed"), item("elapsed")]));
    app.timer.request_started(base);
    app.timer
        .response_finished(base + Duration::from_secs(2), Some(100));
    app.set_now(base + Duration::from_secs(9));
    let terminal = render(&mut app, 80, 12);
    let line = row(&terminal, 11);
    assert!(line.contains("50.0 tok/s"), "the finished speed: {line:?}");
    assert!(line.contains("2.0s"), "the finished elapsed: {line:?}");

    // A live generation ticks `elapsed` but leaves `speed` at the last finished value.
    let start = base + Duration::from_secs(20);
    app.timer.request_started(start);
    app.set_now(start + Duration::from_millis(1_500));
    let terminal = render(&mut app, 80, 12);
    let line = row(&terminal, 11);
    assert!(line.contains("1.5s"), "the live elapsed: {line:?}");
    assert!(line.contains("50.0 tok/s"), "the speed stays: {line:?}");
}

/// The `cost` item uses the model's catalog price and the session's totals; without a price it
/// hides rather than showing `$0.00` (ADR 0027).
#[test]
fn the_cost_item_uses_the_model_price() {
    let mut app = App::new("mock", "s");
    app.projection.add_usage(&agent_core::ModelUsage {
        input: Some(1_000_000),
        output: Some(100_000),
        ..Default::default()
    });
    app.apply_footer(&rows(vec![item("cost")]));
    // No price: hidden.
    let terminal = render(&mut app, 80, 12);
    assert!(!row(&terminal, 11).contains('$'), "{}", row(&terminal, 11));

    // $3/Mtok in, $15/Mtok out: $3.00 + $1.50 = $4.50.
    app.set_model_price(Some(radian_host::ModelPrice {
        input: 3_000_000,
        output: 15_000_000,
        cache_read: Some(300_000),
        cache_write: None,
    }));
    let terminal = render(&mut app, 80, 12);
    assert_eq!(row(&terminal, 11).trim_end(), "$4.500");
}

/// The `tokens` item shows the cache-read total (ADR 0027), and `-` when the
/// provider never reported one (never a fabricated 0).
#[test]
fn the_cache_tokens_are_shown() {
    let mut app = App::new("mock", "s");
    app.projection.add_usage(&agent_core::ModelUsage {
        input: Some(19_000),
        output: Some(760),
        cache_read: Some(33_000),
        ..Default::default()
    });
    app.apply_footer(&rows(vec![item("tokens")]));
    let terminal = render(&mut app, 80, 12);
    let line = row(&terminal, 11);
    assert!(line.contains("19K"), "{line:?}");
    assert!(line.contains("760"), "{line:?}");
    assert!(line.contains("33K"), "the cache total: {line:?}");

    let mut none = App::new("mock", "s");
    none.projection.add_usage(&agent_core::ModelUsage {
        input: Some(5),
        ..Default::default()
    });
    none.apply_footer(&rows(vec![item("tokens")]));
    let terminal = render(&mut none, 80, 12);
    assert!(row(&terminal, 11).contains('-'), "{}", row(&terminal, 11));
}

/// The `git_branch` item shows the change counts from the contributor store: `{dirty}` is the
/// total with a leading marker, and `{modified}`/`{untracked}` are the parts.
#[test]
fn the_git_branch_item_shows_the_change_counts() {
    let mut app = App::new("mock", "s");
    publish(&app, "git_branch", "main");
    publish(&app, "git_modified", "2");
    publish(&app, "git_untracked", "1");
    app.apply_footer(&rows(vec![TuiFooterItemConfig {
        id: "git_branch".to_string(),
        format: Some("{branch}{dirty} ({modified}m {untracked}u)".to_string()),
        ..Default::default()
    }]));
    let terminal = render(&mut app, 80, 12);
    assert_eq!(row(&terminal, 11).trim_end(), "main ?3 (2m 1u)");

    // A clean tree has no `{dirty}` marker.
    let mut clean = App::new("mock", "s");
    publish(&clean, "git_branch", "main");
    publish(&clean, "git_modified", "0");
    publish(&clean, "git_untracked", "0");
    clean.apply_footer(&rows(vec![TuiFooterItemConfig {
        id: "git_branch".to_string(),
        format: Some("{branch}{dirty}".to_string()),
        ..Default::default()
    }]));
    let terminal = render(&mut clean, 80, 12);
    assert_eq!(row(&terminal, 11).trim_end(), "main");
}

/// `{repo}` and `{worktree}` (ADR 0027): the repo name falls back to the directory
/// name outside a repository, and `{worktree}` is the contributor's current worktree name.
#[test]
fn the_cwd_repo_and_worktree_placeholders() {
    let mut app = App::new("mock", "s");
    app.cwd = Some(std::path::PathBuf::from("/tmp/my-project"));
    app.apply_footer(&rows(vec![TuiFooterItemConfig {
        id: "cwd".to_string(),
        format: Some("{repo}/{worktree}".to_string()),
        ..Default::default()
    }]));
    let terminal = render(&mut app, 80, 12);
    // No contributor: `{repo}` falls back to the basename, `{worktree}` is empty.
    assert_eq!(row(&terminal, 11).trim_end(), "my-project/");

    publish(&app, "git_root", "radian");
    publish(&app, "worktree", "dogfood");
    let terminal = render(&mut app, 80, 12);
    assert_eq!(row(&terminal, 11).trim_end(), "radian/dogfood");
}

/// The `{auto}` marker follows the auto-compaction switch (ADR 0027): shown while
/// it is on, gone while it is off.
#[test]
fn the_context_auto_marker_follows_the_switch() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![TuiFooterItemConfig {
        id: "context".to_string(),
        format: Some("{percent}%{auto}".to_string()),
        ..Default::default()
    }]));
    app.projection.set_context_window(Some(1000));
    app.projection.set_request_started(Some(BudgetReport {
        source: UsageSource::Estimated,
        projected_tokens: 100,
        input_budget_tokens: 1000,
        request_bytes: 0,
        baseline_input_tokens: None,
        estimated_growth_tokens: None,
    }));
    app.set_auto_compact(true);
    let on = render(&mut app, 80, 12);
    assert!(row(&on, 11).contains('⟲'), "{}", row(&on, 11));
    app.set_auto_compact(false);
    let off = render(&mut app, 80, 12);
    assert!(!row(&off, 11).contains('⟲'), "{}", row(&off, 11));
}

/// Every new item carries a glyph in all three sets (ADR 0027), and the sets differ.
#[test]
fn the_new_items_have_glyphs_in_all_three_sets() {
    for (set, host, speed, elapsed) in [
        ("unicode", '⌂', '»', '◷'),
        ("nerd", '\u{f109}', '\u{f0e7}', '\u{f017}'),
        ("ascii", 'h', 's', 't'),
    ] {
        let mut app = App::new("mock", "s");
        app.set_hostname(Some("box".to_string()));
        let config = TuiFooterConfig {
            glyphs: Some(set.to_string()),
            rows: vec![TuiFooterRowConfig {
                left: vec![item("host"), item("speed"), item("elapsed")],
                ..Default::default()
            }],
            ..Default::default()
        };
        app.apply_footer(&config);
        // Give speed/elapsed a value so they render.
        let base = std::time::Instant::now();
        app.timer.request_started(base);
        app.timer
            .response_finished(base + std::time::Duration::from_secs(1), Some(10));
        app.set_now(base + std::time::Duration::from_secs(2));
        let terminal = render(&mut app, 80, 12);
        let line = row(&terminal, 11);
        assert!(line.contains(host), "{set} host glyph in {line:?}");
        assert!(line.contains(speed), "{set} speed glyph in {line:?}");
        assert!(line.contains(elapsed), "{set} elapsed glyph in {line:?}");
    }
}

// ---- ADR 0027: deck, separator, per-field config, colors ----

use std::collections::BTreeMap;

fn items(pairs: &[(&str, TuiFooterItemConfig)]) -> TuiFooterConfig {
    TuiFooterConfig {
        items: pairs
            .iter()
            .map(|(id, config)| (id.to_string(), config.clone()))
            .collect::<BTreeMap<_, _>>(),
        ..Default::default()
    }
}

/// The deck preset has a usage row that disappears entirely without data.
#[test]
fn the_deck_usage_row_appears_only_with_data() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(
        Some(std::path::PathBuf::from("/tmp/ws")),
        Some("Mock 5".to_string()),
        Some("opencode-go".to_string()),
        Some("high".to_string()),
    );
    // Without data the row is **dropped**, not left blank: two rows, not three.
    assert_eq!(app.footer.lines(&app, 100).len(), 2, "no usage, no row");
    let without = render(&mut app, 100, 12);
    assert!(!screen(&without).contains("5h:"), "{}", screen(&without));
    publish(&app, "opencode-go.usage", "5h: 98% · 7d: 81%");
    assert_eq!(app.footer.lines(&app, 100).len(), 3, "usage adds a row");
    let with = render(&mut app, 100, 12);
    assert!(
        row(&with, 9).contains("5h: 98%"),
        "the usage row is the first of three:\n{}",
        screen(&with)
    );
}

/// The separator is configurable and uses the `footer_separator` role.
#[test]
fn the_separator_is_configurable_and_styled() {
    let mut app = App::new("mock", "s");
    let mut config = rows(vec![item("model"), item("session")]);
    config.separator = Some(" · ".to_string());
    app.apply_footer(&config);
    let terminal = render(&mut app, 80, 12);
    let line = row(&terminal, 11);
    assert!(line.contains(" · "), "{line:?}");
    assert_eq!(
        fg_at(&terminal, 11, "·"),
        app.theme.footer_separator.fg,
        "the separator uses its own role"
    );
}

/// `[tui.footer.items.<id>]` gives a bare id its symbol, style and format.
#[test]
fn items_config_sets_symbol_style_and_format() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(None, Some("Mock 5".to_string()), None, None);
    let mut config = items(&[(
        "model",
        TuiFooterItemConfig {
            id: "model".to_string(),
            icon: Some(">> ".to_string()),
            format: Some("[{display_name}](red)".to_string()),
            ..Default::default()
        },
    )]);
    config.rows = vec![TuiFooterRowConfig {
        left: vec![item("model")],
        ..Default::default()
    }];
    app.apply_footer(&config);
    let terminal = render(&mut app, 80, 12);
    let line = row(&terminal, 11);
    assert!(line.contains(">> Mock 5"), "{line:?}");
    // `[..](red)` is a fixed color.
    assert_eq!(fg_at(&terminal, 11, "Mock 5"), Some(Color::Red));
}

/// An inline table wins over `[tui.footer.items.<id>]`.
#[test]
fn an_inline_table_wins_over_items() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(None, Some("Mock 5".to_string()), None, None);
    let mut config = items(&[(
        "model",
        TuiFooterItemConfig {
            id: "model".to_string(),
            format: Some("{id}".to_string()),
            ..Default::default()
        },
    )]);
    config.rows = vec![TuiFooterRowConfig {
        left: vec![TuiFooterItemConfig {
            id: "model".to_string(),
            format: Some("{display_name}".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    }];
    app.apply_footer(&config);
    let terminal = render(&mut app, 80, 12);
    assert_eq!(row(&terminal, 11).trim_end(), "Mock 5");
}

/// `disabled = true` hides an item everywhere it is named.
#[test]
fn disabled_hides_the_item() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(None, Some("Mock 5".to_string()), None, None);
    let mut config = items(&[(
        "model",
        TuiFooterItemConfig {
            id: "model".to_string(),
            disabled: true,
            ..Default::default()
        },
    )]);
    config.rows = vec![TuiFooterRowConfig {
        left: vec![item("model"), item("session")],
        ..Default::default()
    }];
    app.apply_footer(&config);
    let terminal = render(&mut app, 80, 12);
    assert!(
        !row(&terminal, 11).contains("Mock 5"),
        "{}",
        row(&terminal, 11)
    );
}

/// A fixed color does not follow the theme; a role does.
#[test]
fn a_fixed_color_is_theme_independent_but_a_role_is_not() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(None, Some("Mock 5".to_string()), None, None);
    let mut config = items(&[(
        "model",
        TuiFooterItemConfig {
            id: "model".to_string(),
            style: Some("red".to_string()),
            ..Default::default()
        },
    )]);
    config.rows = vec![TuiFooterRowConfig {
        left: vec![item("model")],
        ..Default::default()
    }];
    app.apply_footer(&config);
    let dark = render(&mut app, 80, 12);
    assert_eq!(fg_at(&dark, 11, "Mock 5"), Some(Color::Red));

    let role = items(&[(
        "model",
        TuiFooterItemConfig {
            id: "model".to_string(),
            style: Some("model".to_string()),
            ..Default::default()
        },
    )]);
    let mut role_app = App::new("mock", "s");
    role_app.set_footer_facts(None, Some("Mock 5".to_string()), None, None);
    let mut role_config = role;
    role_config.rows = vec![TuiFooterRowConfig {
        left: vec![item("model")],
        ..Default::default()
    }];
    role_app.apply_footer(&role_config);
    let dark_role = render(&mut role_app, 80, 12);
    assert_eq!(fg_at(&dark_role, 11, "Mock 5"), role_app.theme.model.fg);
    role_app.update(radian_tui::app::Input::ThemeReady {
        theme: Some(Box::new(radian_tui::theme::Theme::light())),
        notices: Vec::new(),
    });
    let light_role = render(&mut role_app, 80, 12);
    assert_eq!(fg_at(&light_role, 11, "Mock 5"), role_app.theme.model.fg);
}

/// A bad color is reported, not silently ignored.
#[test]
fn a_bad_color_is_a_warning() {
    let config = items(&[(
        "model",
        TuiFooterItemConfig {
            id: "model".to_string(),
            style: Some("not_a_color".to_string()),
            ..Default::default()
        },
    )]);
    let (_, warnings) = Footer::from_config(&config);
    assert!(
        warnings.iter().any(|warning| warning.contains("bad style")),
        "{warnings:?}"
    );
}

/// `bold #7e9cd8`, a named color and a 256 index all parse.
#[test]
fn starship_colors_parse() {
    let parse = |text: &str| {
        let config = items(&[(
            "model",
            TuiFooterItemConfig {
                id: "model".to_string(),
                style: Some(text.to_string()),
                ..Default::default()
            },
        )]);
        let (_, warnings) = Footer::from_config(&config);
        assert!(warnings.is_empty(), "{text:?}: {warnings:?}");
    };
    parse("bold #7e9cd8");
    parse("bright_blue");
    parse("196");
    parse("italic red");
}

/// The `brand` is `㎭` under the default unicode set (`rad` only under ascii).
#[test]
fn the_brand_is_the_unicode_glyph_by_default() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![item("brand")]));
    let terminal = render(&mut app, 40, 12);
    assert!(row(&terminal, 11).contains('㎭'), "{}", row(&terminal, 11));
}
