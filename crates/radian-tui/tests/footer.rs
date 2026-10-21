//! The footer's rows and items (ADR 0027 C2, slice 1): the `pi` default, the `classic`
//! preset, glyphs, thresholds, truncation and the warnings. Pure `App` state on a
//! `TestBackend`, so no pty is needed.

use radian_core::{BudgetReport, EventSink, UsageSource};
use radian_host::config::{TuiFooterConfig, TuiFooterItemConfig, TuiFooterRowConfig};
use radian_session::RuntimeEvent;
use radian_tui::App;
use radian_tui::app::Input;
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
        first.contains('*'),
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
    for (name, glyph) in [("unicode", '*'), ("ascii", '*')] {
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
        // it asks for `{bar}` explicitly.
        let warnings = app.apply_footer(&rows(vec![TuiFooterItemConfig {
            id: "context".to_string(),
            format: Some("{bar}".to_string()),
            ..Default::default()
        }]));
        assert!(warnings.is_empty(), "{warnings:?}");
        app.session.projection.set_context_window(Some(1000));
        app.session
            .projection
            .set_request_started(Some(BudgetReport {
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

/// `context`'s `{bar}` placeholder (ADR 0027 merged `context_bar`
/// into it): `width` cells, `fill` for the used part, `empty` for the rest. The old
/// `context_bar` id keeps working with exactly one warning.
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
    let warnings = app.apply_footer(&config);
    assert_eq!(
        warnings
            .iter()
            .filter(|w| w.contains("context_bar"))
            .count(),
        1,
        "the old id warns exactly once: {warnings:?}"
    );
    app.session.projection.set_context_window(Some(1000));
    app.session
        .projection
        .set_request_started(Some(BudgetReport {
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
        wide_text.contains("long-model-name") && wide_text.contains('*'),
        "{wide_text:?}"
    );
    let mid = render(&mut app, 20, 12);
    let mid_text = row(&mid, 11);
    assert!(
        mid_text.contains("long-model-name"),
        "the left group stays: {mid_text:?}"
    );
    assert!(
        !mid_text.contains('*'),
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
    app.session.git_branch = None;
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

/// The `jobs` component (ADR 0027's 2026-10-17 note, slice ③ item 5): the
/// running background jobs and the oldest one's age, fed by the start/settle events — never
/// by polling the journal file. Empty while none runs, so its row takes no space.
#[test]
fn jobs_shows_the_running_count_and_the_oldest_age() {
    fn started(id: &str) -> Input {
        Input::Runtime(RuntimeEvent::BackgroundJobStarted {
            job_id: id.to_string(),
            command: "make test".to_string(),
            output: "available through job".to_string(),
        })
    }
    fn settled(id: &str) -> Input {
        Input::Runtime(RuntimeEvent::BackgroundJobSettled {
            job_id: id.to_string(),
            state: "exited 0".to_string(),
            duration_ms: 1_000,
        })
    }
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![item("jobs")]));
    let start = std::time::Instant::now();
    app.set_now(start);
    assert!(
        app.footer.lines(&app, 100).is_empty(),
        "no running job, no row"
    );

    // One running job: the count and its age.
    app.update(started("job-1"));
    app.set_now(start + std::time::Duration::from_secs(18 * 60));
    let one = render(&mut app, 80, 12);
    assert!(
        row(&one, 11).contains("1 job") && row(&one, 11).contains("18m"),
        "one job with its age: {}",
        row(&one, 11)
    );

    // A second job: the count grows, the age stays the oldest one's.
    app.update(started("job-2"));
    let two = render(&mut app, 80, 12);
    assert!(
        row(&two, 11).contains("2 jobs") && row(&two, 11).contains("18m"),
        "two jobs, oldest age: {}",
        row(&two, 11)
    );

    // Settling the oldest leaves the younger one's count and age.
    app.update(settled("job-1"));
    let younger = render(&mut app, 80, 12);
    assert!(
        row(&younger, 11).contains("1 job") && row(&younger, 11).contains("0s"),
        "the younger job remains: {}",
        row(&younger, 11)
    );

    // Settling the last one empties the component: no text, no row, no gap.
    app.update(settled("job-2"));
    assert!(
        app.footer.lines(&app, 100).is_empty(),
        "no running job, no row"
    );
    let empty = render(&mut app, 80, 12);
    assert!(
        !screen(&empty).contains("job"),
        "nothing left behind:\n{}",
        screen(&empty)
    );
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

/// Rows positioned above/below the input draw around it (ADR 0027),
/// and an input-adjacent row reads `[tui.footer.items.<id>]` like a footer row does. (`status`
/// fixes its own text style from the contribution role, so the items-table colour shows on its
/// symbol; the old `widget_line` path read none of the items table.)
#[test]
fn rows_draw_above_and_below_the_input_with_items_config() {
    use radian_host::config::TuiFooterRowConfig;
    let mut config = items(&[(
        "status",
        TuiFooterItemConfig {
            id: "status".to_string(),
            icon: Some("● ".to_string()),
            symbol_style: Some("red".to_string()),
            ..Default::default()
        },
    )]);
    config.rows = vec![
        TuiFooterRowConfig {
            left: vec![item("status")],
            position: Some("above_input".to_string()),
            ..Default::default()
        },
        TuiFooterRowConfig {
            left: vec![item("status")],
            position: Some("below_input".to_string()),
            ..Default::default()
        },
    ];
    let mut app = App::new("mock", "s");
    let warnings = app.apply_footer(&config);
    assert!(warnings.is_empty(), "{warnings:?}");
    publish(&app, "status.thing", "EDGE-WIDGET");
    let terminal = render(&mut app, 80, 14);
    let text = screen(&terminal);
    let above = text.find("EDGE-WIDGET").expect("the above row");
    let input = text.find("(type a message").expect("the input hint");
    assert!(above < input, "the above row precedes the input:\n{text}");
    let below = text.rfind("EDGE-WIDGET").expect("the below row");
    assert!(below > input, "the below row follows the input:\n{text}");
    // The above row takes the items-table symbol colour (rev14 slice ③ acceptance).
    let height = 14;
    let mut coloured = false;
    for y in 0..height {
        if row(&terminal, y).contains("EDGE-WIDGET") && fg_at(&terminal, y, "●") == Some(Color::Red)
        {
            coloured = true;
        }
    }
    assert!(
        coloured,
        "the above/below row uses items.status red:\n{text}"
    );
}

/// The old `[tui.widgets]` block still works for one version: one warning, and its ids draw
/// above/below the input as rows (ADR 0027).
#[test]
fn legacy_widgets_convert_to_positioned_rows_with_one_warning() {
    let parsed = radian_host::config::parse(
        std::path::Path::new("/tmp/config.toml"),
        "schema_version = 1\n[tui.widgets]\nabove_input = [\"status\"]\nbelow_input = [\"status\"]\n",
    )
    .unwrap();
    let mut app = App::new("mock", "s");
    let warnings = app.apply_config(&parsed, false).unwrap();
    assert_eq!(
        warnings
            .iter()
            .filter(|w| w.contains("tui.widgets"))
            .count(),
        1,
        "exactly one widgets warning: {warnings:?}"
    );
    publish(&app, "status.thing", "LEGACY-WIDGET");
    let terminal = render(&mut app, 80, 14);
    let text = screen(&terminal);
    let above = text.find("LEGACY-WIDGET").expect("the converted above row");
    let input = text.find("(type a message").expect("the input hint");
    assert!(
        above < input,
        "the converted row precedes the input:\n{text}"
    );
    let below = text
        .rfind("LEGACY-WIDGET")
        .expect("the converted below row");
    assert!(
        below > input,
        "the converted row follows the input:\n{text}"
    );
}

/// A bad row position warns and falls back to the footer (never a refusal).
#[test]
fn a_bad_row_position_warns_and_falls_back_to_the_footer() {
    use radian_host::config::TuiFooterRowConfig;
    let config = TuiFooterConfig {
        rows: vec![TuiFooterRowConfig {
            left: vec![item("session")],
            position: Some("sideways".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut app = App::new("mock", "s");
    let warnings = app.apply_footer(&config);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("position"), "{warnings:?}");
    let terminal = render(&mut app, 80, 12);
    assert!(
        row(&terminal, 11).contains('s'),
        "the row draws in the footer:\n{}",
        screen(&terminal)
    );
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
    assert!(line.contains('R'), "the brand glyph: {line:?}");
    assert!(line.contains("@ Mac-mini"), "the host glyph: {line:?}");

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
    assert!(!line.contains('R'), "ascii has no brand mark: {line:?}");
}

/// `speed` and `elapsed` come from the injected clock (ADR 0027): the last finished
/// generation's output over its measured duration, and the live elapsed while one runs.
#[test]
fn the_speed_and_elapsed_items_use_the_injected_clock() {
    use std::time::{Duration, Instant};
    let base = Instant::now();
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![item("speed"), item("elapsed")]));
    app.session.timer.request_started(base);
    app.session
        .timer
        .response_finished(base + Duration::from_secs(2), Some(100));
    app.set_now(base + Duration::from_secs(9));
    let terminal = render(&mut app, 80, 12);
    let line = row(&terminal, 11);
    assert!(line.contains("50.0 tok/s"), "the finished speed: {line:?}");
    assert!(line.contains("2.0s"), "the finished elapsed: {line:?}");

    // A live generation ticks `elapsed` but leaves `speed` at the last finished value.
    let start = base + Duration::from_secs(20);
    app.session.timer.request_started(start);
    app.set_now(start + Duration::from_millis(1_500));
    let terminal = render(&mut app, 80, 12);
    let line = row(&terminal, 11);
    assert!(line.contains("1.5s"), "the live elapsed: {line:?}");
    assert!(line.contains("50.0 tok/s"), "the speed stays: {line:?}");
}

/// F2 (ADR 0018): a summarization call's usage reaches the footer's totals and fee
/// through the **real sink** (the one the driver emits into), not just by calling the
/// projection directly. Before this slice the compaction's spending never appeared at all.
#[test]
fn a_compaction_usage_reaches_the_footer_totals() {
    use std::sync::mpsc::channel;
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![item("tokens"), item("cost")]));
    app.set_model_price(Some(radian_host::ModelPrice {
        input: 3_000_000, // $3 / Mtok
        output: 15_000_000,
        cache_read: None,
        cache_write: None,
        tiers: Vec::new(),
    }));
    let (tx, _rx) = channel();
    let sink =
        radian_tui::events::ProjectionSink::new(tx, std::sync::Arc::clone(&app.session.projection));
    sink.emit(radian_core::AgentEvent::CompactionUsage {
        revision: 1,
        model: "current".into(),
        usage: radian_core::ModelUsage {
            input: Some(50_000),
            output: Some(2_000),
            input_includes_cache: Some(false),
            ..Default::default()
        },
    });

    // The projection holds the exact micro-dollars: 50000 x $3 + 2000 x $15 per Mtok = $0.18.
    assert_eq!(app.session.projection.read().cost_micros(), Some(180_000));
    let terminal = render(&mut app, 120, 12);
    let line = row(&terminal, 11);
    // The summary's tokens are in the totals (50K in, 2K out) ...
    assert!(line.contains("50K"), "the summary's input: {line:?}");
    // `format_tokens` keeps one decimal under 10 (ADR 0027): 2 000 is `2.0K`.
    assert!(line.contains("2.0K"), "the summary's output: {line:?}");
    // ... and in the fee.
    assert!(line.contains("$0.180"), "the summary's fee: {line:?}");

    // A conversation response then adds to the same totals through the same sink.
    sink.emit(radian_core::AgentEvent::ModelResponseReceived {
        stop_reason: radian_core::StopReason::EndTurn,
        tool_calls: 0,
        usage: Some(radian_core::ModelUsage {
            input: Some(50_000),
            input_includes_cache: Some(false),
            ..Default::default()
        }),
        limits: None,
    });
    assert_eq!(app.session.projection.read().usage_input(), Some(100_000));
    assert_eq!(
        app.session.projection.read().cost_micros(),
        Some(330_000),
        "$0.33"
    );
}

/// ADR 0027: a call with a non-zero counter the catalog cannot price
/// shows `≥` (a lower bound), not a number that looks exact.
#[test]
fn an_incomplete_fee_is_marked() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![item("cost")]));
    app.set_model_price(Some(radian_host::ModelPrice {
        input: 3_000_000,
        output: 15_000_000,
        cache_read: Some(300_000),
        cache_write: None, // the catalog lists no cache-write rate
        tiers: Vec::new(),
    }));
    app.session.projection.add_usage(&radian_core::ModelUsage {
        input: Some(10_000),
        cache_write: Some(10_000),
        input_includes_cache: Some(false),
        ..Default::default()
    });

    assert!(app.session.projection.read().cost_incomplete());
    let terminal = render(&mut app, 120, 12);
    let line = row(&terminal, 11);
    assert!(line.contains("≥$0.030"), "the incomplete marker: {line:?}");
}

/// ADR 0027: a total re-priced from an old record shows `~`, and the
/// estimate wins when the total is also incomplete (`~≥` would be unreadable).
#[test]
fn an_estimated_fee_shows_the_tilde_and_wins_over_incomplete() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![item("cost")]));
    let price = radian_host::ModelPrice {
        input: 3_000_000,
        output: 15_000_000,
        cache_read: Some(300_000),
        cache_write: None,
        tiers: Vec::new(),
    };
    // The resume path for an old record: re-priced with today's catalog and marked estimated.
    app.session.projection.add_usage_at(
        Some(&price),
        &radian_core::ModelUsage {
            input: Some(10_000),
            cache_write: Some(10_000),
            input_includes_cache: Some(false),
            ..Default::default()
        },
    );

    let read = app.session.projection.read();
    assert!(read.cost_estimated() && read.cost_incomplete());
    drop(read);
    let terminal = render(&mut app, 120, 12);
    let line = row(&terminal, 11);
    assert!(line.contains("~$0.030"), "the estimate marker: {line:?}");
    assert!(!line.contains("~≥"), "the estimate wins: {line:?}");
}

/// The `cost` item shows the fee accumulated **per call, at the price in force when the call
/// arrived** (ADR 0027). Without a price it hides rather than showing `$0.00`.
#[test]
fn the_cost_item_uses_the_model_price() {
    let mut app = App::new("mock", "s");
    // $3/Mtok in, $15/Mtok out: $3.00 + $1.50 = $4.50.
    app.set_model_price(Some(radian_host::ModelPrice {
        input: 3_000_000,
        output: 15_000_000,
        cache_read: Some(300_000),
        cache_write: None,
        tiers: Vec::new(),
    }));
    app.apply_footer(&rows(vec![item("cost")]));

    // No usage yet: hidden.
    let terminal = render(&mut app, 80, 12);
    assert!(!row(&terminal, 11).contains('$'), "{}", row(&terminal, 11));

    app.session.projection.add_usage(&radian_core::ModelUsage {
        input: Some(1_000_000),
        output: Some(100_000),
        input_includes_cache: Some(false),
        ..Default::default()
    });
    let terminal = render(&mut app, 80, 12);
    assert_eq!(row(&terminal, 11).trim_end(), "$4.500");

    // A session with no price at all hides, even with tokens counted.
    let mut unpriced = App::new("mock", "s");
    unpriced.apply_footer(&rows(vec![item("cost")]));
    unpriced
        .session
        .projection
        .add_usage(&radian_core::ModelUsage {
            input: Some(1_000_000),
            ..Default::default()
        });
    let terminal = render(&mut unpriced, 80, 12);
    assert!(!row(&terminal, 11).contains('$'), "{}", row(&terminal, 11));
}

/// F1 (the research's §3.2 numbers): on an OpenAI-compatible endpoint, 190 000 of a 200 000
/// token prompt are a cache hit. The hit must be billed **once, at the cache rate** — reading
/// `input` as reported charged it again at full price ($0.03177 instead of $0.00327, 9.7x).
#[test]
fn a_cache_hit_is_billed_once_at_the_cache_rate() {
    let mut app = App::new("mock", "s");
    // opencode-go / deepseek-v4.1-flash: $0.15 in, $0.60 out, $0.003 cache read.
    app.set_model_price(Some(radian_host::ModelPrice {
        input: 150_000,
        output: 600_000,
        cache_read: Some(3_000),
        cache_write: None,
        tiers: Vec::new(),
    }));
    app.apply_footer(&rows(vec![item("cost")]));
    app.session.projection.add_usage(&radian_core::ModelUsage {
        input: Some(200_000),
        output: Some(2_000),
        cache_read: Some(190_000),
        input_includes_cache: Some(true),
        ..Default::default()
    });
    let terminal = render(&mut app, 80, 12);
    // (200000-190000) x $0.15 + 2000 x $0.60 + 190000 x $0.003 = $0.00327.
    assert_eq!(row(&terminal, 11).trim_end(), "$0.003");
}

/// F4 (ADR 0026 D5): each call is priced at the rate it arrived under, so a `/model` switch
/// prices the calls after it with the new model and leaves the earlier ones alone.
#[test]
fn a_model_switch_prices_only_the_calls_after_it() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![item("cost")]));
    app.set_model_price(Some(radian_host::ModelPrice {
        input: 150_000, // the cheap model: $0.15 / Mtok
        output: 600_000,
        cache_read: None,
        cache_write: None,
        tiers: Vec::new(),
    }));
    let cheap = radian_core::ModelUsage {
        input: Some(1_000_000),
        input_includes_cache: Some(false),
        ..Default::default()
    };
    app.session.projection.add_usage(&cheap);
    app.set_model_price(Some(radian_host::ModelPrice {
        input: 3_000_000, // the expensive one: $3 / Mtok
        output: 15_000_000,
        cache_read: None,
        cache_write: None,
        tiers: Vec::new(),
    }));
    app.session.projection.add_usage(&cheap);

    let terminal = render(&mut app, 80, 12);
    // $0.15 (at the old price) + $3.00 (at the new one) — not 2M x $3 = $6.00.
    assert_eq!(row(&terminal, 11).trim_end(), "$3.150");
}

/// F7 (ADR 0027: never `$0.00`): a response that reports all zeros — an empty or
/// refused answer — has nothing to bill, so the item hides.
#[test]
fn an_all_zero_response_hides_the_cost() {
    let mut app = App::new("mock", "s");
    app.set_model_price(Some(radian_host::ModelPrice {
        input: 3_000_000,
        output: 15_000_000,
        cache_read: Some(300_000),
        cache_write: None,
        tiers: Vec::new(),
    }));
    app.apply_footer(&rows(vec![item("cost")]));
    app.session.projection.add_usage(&radian_core::ModelUsage {
        input: Some(0),
        output: Some(0),
        cache_read: Some(0),
        cache_write: Some(0),
        input_includes_cache: Some(true),
        ..Default::default()
    });
    let terminal = render(&mut app, 80, 12);
    let line = row(&terminal, 11);
    assert!(
        !line.contains('$'),
        "no $0.00 for a zero response: {line:?}"
    );

    // Once a real call lands, the fee appears (and the earlier zero added nothing).
    app.session.projection.add_usage(&radian_core::ModelUsage {
        input: Some(1_000_000),
        input_includes_cache: Some(false),
        ..Default::default()
    });
    let terminal = render(&mut app, 80, 12);
    assert_eq!(row(&terminal, 11).trim_end(), "$3.000");
}

/// ADR 0027: the cache-read total (ADR 0027 adds `{cache_write}`, off by
/// default), and `-` when the provider never reported one (never a fabricated 0).
#[test]
fn the_cache_tokens_are_shown() {
    let mut app = App::new("mock", "s");
    app.session.projection.add_usage(&radian_core::ModelUsage {
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
    none.session.projection.add_usage(&radian_core::ModelUsage {
        input: Some(5),
        ..Default::default()
    });
    none.apply_footer(&rows(vec![item("tokens")]));
    let terminal = render(&mut none, 80, 12);
    assert!(row(&terminal, 11).contains('-'), "{}", row(&terminal, 11));
}

/// F8 (ADR 0027): the `tokens` item has a `{cache_write}` placeholder, because a
/// cache write is billed at its own (often higher) rate and the `{cache}` total does not show
/// it. It is available but **not** in the default format: the numbers row must not get wider by
/// default.
#[test]
fn the_cache_write_tokens_show_only_when_the_format_asks_for_them() {
    let mut app = App::new("mock", "s");
    app.session.projection.add_usage(&radian_core::ModelUsage {
        input: Some(10_000),
        output: Some(760),
        cache_read: Some(33_000),
        cache_write: Some(12_000),
        ..Default::default()
    });

    // The default format: the cache write is nowhere on the line.
    app.apply_footer(&rows(vec![item("tokens")]));
    let terminal = render(&mut app, 80, 12);
    let line = row(&terminal, 11);
    assert!(line.contains("33K"), "the cache read: {line:?}");
    assert!(
        !line.contains("12K"),
        "the default format must not grow a cache-write field: {line:?}"
    );

    // A format that asks for it gets the number, and the config parses **without a warning**
    // (the placeholder list is what validates a format at startup).
    let warnings = app.apply_footer(&rows(vec![TuiFooterItemConfig {
        id: "tokens".to_string(),
        format: Some("{cache}/{cache_write}".to_string()),
        ..Default::default()
    }]));
    assert!(
        warnings.is_empty(),
        "`{{cache_write}}` is a known placeholder for `tokens`: {warnings:?}"
    );
    let terminal = render(&mut app, 80, 12);
    assert_eq!(row(&terminal, 11).trim_end(), "33K/12K");

    // A provider that never reported a write shows `-`, never a fabricated 0.
    let mut none = App::new("mock", "s");
    none.session.projection.add_usage(&radian_core::ModelUsage {
        input: Some(10_000),
        cache_read: Some(33_000),
        ..Default::default()
    });
    none.apply_footer(&rows(vec![TuiFooterItemConfig {
        id: "tokens".to_string(),
        format: Some("{cache_write}".to_string()),
        ..Default::default()
    }]));
    let terminal = render(&mut none, 80, 12);
    assert_eq!(row(&terminal, 11).trim_end(), "-");
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
    app.session.cwd = Some(std::path::PathBuf::from("/tmp/my-project"));
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
    app.session.projection.set_context_window(Some(1000));
    app.session
        .projection
        .set_request_started(Some(BudgetReport {
            source: UsageSource::Estimated,
            projected_tokens: 100,
            input_budget_tokens: 1000,
            request_bytes: 0,
            baseline_input_tokens: None,
            estimated_growth_tokens: None,
        }));
    app.set_auto_compact(true);
    let on = render(&mut app, 80, 12);
    assert!(row(&on, 11).contains('~'), "{}", row(&on, 11));
    app.set_auto_compact(false);
    let off = render(&mut app, 80, 12);
    assert!(!row(&off, 11).contains('~'), "{}", row(&off, 11));
}

/// Every new item carries a glyph in all three sets (ADR 0027), and the sets differ.
#[test]
fn the_new_items_have_glyphs_in_every_set() {
    // The unicode `elapsed` icon was dropped (ADR 0027: no covered glyph); the
    // emoji set (ADR 0027) draws from the system emoji font.
    for (set, host, speed, elapsed) in [
        ("unicode", '@', '»', None),
        ("nerd", '\u{f109}', '\u{f0e7}', Some('\u{f017}')),
        ("ascii", 'h', 's', Some('t')),
        ("emoji", '💻', '🚀', Some('⏱')),
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
        app.session.timer.request_started(base);
        app.session
            .timer
            .response_finished(base + std::time::Duration::from_secs(1), Some(10));
        app.set_now(base + std::time::Duration::from_secs(2));
        let terminal = render(&mut app, 80, 12);
        let line = row(&terminal, 11);
        assert!(line.contains(host), "{set} host glyph in {line:?}");
        assert!(line.contains(speed), "{set} speed glyph in {line:?}");
        if let Some(elapsed) = elapsed {
            assert!(line.contains(elapsed), "{set} elapsed glyph in {line:?}");
        }
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
    // Without a snapshot the row is **dropped**, not left blank: two rows, not three.
    assert_eq!(app.footer.lines(&app, 100).len(), 2, "no usage, no row");
    let without = render(&mut app, 100, 12);
    assert!(!screen(&without).contains("5h:"), "{}", screen(&without));
    publish_snapshot(
        &app,
        usage_snapshot(
            "opencode-go",
            "Usage",
            vec![window("5h", 98.0, None), window("7d", 81.0, None)],
            None,
        ),
    );
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

/// The `brand` is a bold accent `R` under the default unicode set (ADR 0027);
/// `rad` only under ascii.
#[test]
fn the_brand_is_the_unicode_glyph_by_default() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![item("brand")]));
    let terminal = render(&mut app, 40, 12);
    assert!(row(&terminal, 11).contains('R'), "{}", row(&terminal, 11));
}

/// ADR 0027: `tui.brand` replaces the glyph set's default in the footer's `brand`
/// item.
#[test]
fn the_brand_config_replaces_the_glyph_default() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![item("brand")]));
    app.brand = Some("ʀ".to_string());
    let terminal = render(&mut app, 40, 12);
    let line = row(&terminal, 11);
    assert!(line.contains('ʀ'), "{line:?}");
    assert!(
        !line.contains('R'),
        "the configured brand, not the default `R`: {line:?}"
    );
}

/// Without `tui.brand`, `App::brand_mark` is the glyph set's default (ADR 0027).
#[test]
fn the_brand_default_follows_the_glyph_set() {
    for (set, expected) in [("unicode", "R"), ("nerd", "\u{f0c20}"), ("ascii", "rad")] {
        let mut app = App::new("mock", "s");
        app.apply_footer(&with_glyphs(set));
        assert_eq!(app.brand_mark(), expected, "{set}");
    }
}

/// ADR 0027's sample: the deck's third row, whole, is
/// `↑15M ↓2.6M R1962M $14.510 41.3%/1.0M` (compact tokens, a 3-decimal cost, the context
/// share with one decimal and the window in `M`).
#[test]
fn the_deck_numbers_row_matches_the_sample() {
    let mut app = App::new("mock", "s");
    // The price first: the fee is accumulated **per call at the price in force then**
    // (ADR 0027), so a call arriving before the price is known contributes no fee
    // at all — unlike the old "session totals × the current price at draw time" formula.
    // 15M input at 967_333 micro-dollars per million = 14_509_995 micros = $14.510.
    app.set_model_price(Some(radian_host::ModelPrice {
        input: 967_333,
        output: 0,
        // Priced at zero, not absent: a **non-zero** cache read with no rate makes the fee
        // incomplete (`≥`, ADR 0027) and this sample row is the complete shape.
        cache_read: Some(0),
        cache_write: None,
        tiers: Vec::new(),
    }));
    app.session.projection.add_usage(&radian_core::ModelUsage {
        input: Some(15_000_000),
        output: Some(2_600_000),
        cache_read: Some(1_962_000_000),
        ..Default::default()
    });
    app.session.projection.set_context_window(Some(1_000_000));
    app.session
        .projection
        .set_request_started(Some(BudgetReport {
            source: UsageSource::Estimated,
            projected_tokens: 413_000,
            input_budget_tokens: 1_000_000,
            request_bytes: 0,
            baseline_input_tokens: None,
            estimated_growth_tokens: None,
        }));
    let terminal = render(&mut app, 100, 12);
    // The numbers row is right-aligned; the content itself is the sample.
    assert_eq!(
        row(&terminal, 11).trim(),
        "↑15M ↓2.6M R1962M $14.510 41.3%/1.0M"
    );
}

/// ADR 0027: the default unicode footer at 80 and 120 columns - the tier's token
/// arrows, the context gauge in the identity row's gap, and the ascii/nerd tiers keeping
/// `^`/`v`.
#[test]
fn the_unicode_footer_snapshot_at_80_and_120_columns() {
    for width in [80u16, 120] {
        let mut app = App::new("mock", "s");
        app.set_footer_facts(
            Some(std::path::PathBuf::from("/tmp/ws")),
            Some("Mock 5".to_string()),
            Some("opencode-go".to_string()),
            Some("high".to_string()),
        );
        app.set_git_branch(Some("main".to_string()));
        app.apply_footer(&TuiFooterConfig {
            context_line: Some("percentage".to_string()),
            ..Default::default()
        });
        app.session.projection.set_context_window(Some(1_000_000));
        app.session
            .projection
            .set_request_started(Some(BudgetReport {
                source: UsageSource::Estimated,
                projected_tokens: 413_000,
                input_budget_tokens: 1_000_000,
                request_bytes: 0,
                baseline_input_tokens: None,
                estimated_growth_tokens: None,
            }));
        let terminal = render(&mut app, width, 12);
        let identity = row(&terminal, 10);
        let numbers = row(&terminal, 11);
        // The identity row: the gauge fills the gap (41.3% used).
        assert!(
            identity.contains('─'),
            "the gauge at {width} columns: {identity:?}"
        );
        assert!(identity.contains('*'), "the reasoning glyph: {identity:?}");
        // The numbers row: the unicode arrows and the compact numbers, never wider than the
        // terminal.
        assert!(numbers.contains('↑'), "the in arrow: {numbers:?}");
        assert!(numbers.contains('↓'), "the out arrow: {numbers:?}");
        assert!(
            numbers.contains("41.3%"),
            "the context percent: {numbers:?}"
        );
        assert!(numbers.trim_end().chars().count() <= width as usize);
        assert!(
            !numbers.contains('^') && !numbers.contains('v'),
            "the unicode tier has no `^`/`v`: {numbers:?}"
        );
    }
}

/// The nerd and ascii tiers keep `^`/`v` (ADR 0027 item 2 changes unicode only).
#[test]
fn the_other_tiers_keep_the_ascii_token_arrows() {
    for set in ["nerd", "ascii"] {
        let mut app = App::new("mock", "s");
        app.apply_footer(&TuiFooterConfig {
            glyphs: Some(set.to_string()),
            rows: vec![TuiFooterRowConfig {
                left: vec![item("tokens")],
                ..Default::default()
            }],
            ..Default::default()
        });
        app.session.projection.add_usage(&radian_core::ModelUsage {
            input: Some(15_000_000),
            output: Some(2_600_000),
            ..Default::default()
        });
        let terminal = render(&mut app, 60, 8);
        let line = row(&terminal, 7);
        assert!(line.contains('^'), "{set}: {line:?}");
        assert!(line.contains('v'), "{set}: {line:?}");
    }
}

/// ADR 0027 item 5 (omp's `statusLine.transparent`): the footer inherits the
/// terminal's background - every footer cell's background is cleared, a configured item
/// style's included.
#[test]
fn transparent_clears_the_footer_background() {
    let config = |transparent: bool| TuiFooterConfig {
        transparent: Some(transparent),
        rows: vec![TuiFooterRowConfig {
            left: vec![TuiFooterItemConfig {
                id: "model".to_string(),
                // A starship style: two space-separated colors, the second is the
                // background (`red` alone is a role, which carries no background).
                style: Some("red #112233".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let background = Color::Rgb(0x11, 0x22, 0x33);

    let mut opaque = App::new("mock", "s");
    opaque.apply_footer(&config(false));
    let terminal = render(&mut opaque, 40, 8);
    assert_eq!(
        terminal.backend().buffer().cell((0, 7)).unwrap().bg,
        background,
        "the item's background is drawn by default"
    );

    let mut clear = App::new("mock", "s");
    clear.apply_footer(&config(true));
    let terminal = render(&mut clear, 40, 8);
    assert_eq!(
        terminal.backend().buffer().cell((0, 7)).unwrap().bg,
        Color::Reset,
        "transparent clears it"
    );
}

/// ADR 0027 item 4 (omp's `contextLine = percentage`): the gap between a row's
/// left and right groups is a `─` rule - the used fraction of the context window in the
/// `budget` role, the rest in the `footer_separator` (border) role.
#[test]
fn the_context_fill_draws_the_usage_in_the_gap() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&TuiFooterConfig {
        context_line: Some("percentage".to_string()),
        rows: vec![TuiFooterRowConfig {
            left: vec![item("model")],
            // A right group that always has data: `reasoning` hides without a thinking level.
            right: vec![item("context")],
            ..Default::default()
        }],
        ..Default::default()
    });
    // 250 of 1000 tokens: a quarter of the gap is the used part.
    app.session.projection.set_context_window(Some(1_000));
    app.session
        .projection
        .set_request_started(Some(BudgetReport {
            source: UsageSource::Estimated,
            projected_tokens: 250,
            input_budget_tokens: 1_000,
            request_bytes: 0,
            baseline_input_tokens: None,
            estimated_growth_tokens: None,
        }));
    let terminal = render(&mut app, 60, 8);
    let buffer = terminal.backend().buffer();
    // The footer is the layout's last row (the input box's own rule is `─` too).
    let gauge_row = 7u16;
    let columns: Vec<u16> = (0..60u16)
        .filter(|x| buffer.cell((*x, gauge_row)).unwrap().symbol() == "─")
        .collect();
    assert!(columns.len() > 4, "the gap is filled: {columns:?}");
    let first = columns[0];
    let last = *columns.last().unwrap();
    assert_eq!(
        Some(buffer.cell((first, gauge_row)).unwrap().fg),
        app.theme.budget.fg,
        "the used part is the budget role"
    );
    assert_eq!(
        Some(buffer.cell((last, gauge_row)).unwrap().fg),
        app.theme.footer_separator.fg,
        "the remaining part is the border role"
    );
    // The boundary: round(gap * 0.25) used cells.
    let used = ((columns.len() as f64) * 0.25).round() as u16;
    assert!(used > 0 && used < columns.len() as u16, "used={used}");
    assert_eq!(
        Some(buffer.cell((first + used - 1, gauge_row)).unwrap().fg),
        app.theme.budget.fg
    );
    assert_eq!(
        Some(buffer.cell((first + used, gauge_row)).unwrap().fg),
        app.theme.footer_separator.fg
    );
}

/// The default is `off`: the gap stays spaces (ADR 0027 item 4).
#[test]
fn the_context_fill_is_off_by_default() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&TuiFooterConfig {
        rows: vec![TuiFooterRowConfig {
            left: vec![item("model")],
            right: vec![item("context")],
            ..Default::default()
        }],
        ..Default::default()
    });
    app.session.projection.set_context_window(Some(1_000));
    app.session
        .projection
        .set_request_started(Some(BudgetReport {
            source: UsageSource::Estimated,
            projected_tokens: 250,
            input_budget_tokens: 1_000,
            request_bytes: 0,
            baseline_input_tokens: None,
            estimated_growth_tokens: None,
        }));
    let terminal = render(&mut app, 60, 8);
    assert!(!row(&terminal, 7).contains('─'), "no fill by default");
}

/// With no known budget the gap stays spaces: never a fabricated percentage (ADR 0027
/// item 4).
#[test]
fn the_context_fill_without_a_budget_is_spaces() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&TuiFooterConfig {
        context_line: Some("percentage".to_string()),
        rows: vec![TuiFooterRowConfig {
            left: vec![item("model")],
            right: vec![item("context")],
            ..Default::default()
        }],
        ..Default::default()
    });
    let terminal = render(&mut app, 60, 8);
    assert!(!row(&terminal, 7).contains('─'), "no budget, no gauge");
}

/// after a `/model` switch the driver emits a
/// model `ConfigChanged`. The footer's `model` item must then show the new model, so the
/// session's model facts follow the applied route the way startup leaves them: the model id
/// (`cli.rs` sets `app.session.model_name`), the provider from the applied route, and
/// `model_display = None` so `{display_name}` falls back to the id — startup passes `None` to
/// `set_footer_facts` too, so no catalog name is resolved anywhere today. Before this fix the
/// item kept reading the startup model (`old-model`).
#[test]
fn the_footer_model_follows_an_applied_switch() {
    let mut app = App::new("old-model", "s");
    app.set_footer_facts(
        None,
        None,
        Some("opencode-go".to_string()),
        Some("high".to_string()),
    );
    let before = render(&mut app, 80, 12);
    assert!(
        screen(&before).contains("old-model"),
        "the startup model shows first: {}",
        screen(&before)
    );
    app.update(radian_tui::app::Input::Runtime(
        radian_session::RuntimeEvent::ConfigChanged {
            revision: 1,
            change: radian_host::ModelRoute::new("anthropic", "claude-new").to_value(),
        },
    ));
    assert_eq!(app.session.model_name, "claude-new", "the model id follows");
    assert_eq!(
        app.session.provider.as_deref(),
        Some("anthropic"),
        "the provider follows"
    );
    // The switch resolves no catalog display name, exactly like startup: `model_display` stays
    // `None`, so `{display_name}` is the id.
    assert!(
        app.session.model_display.is_none(),
        "no catalog display name is resolved: {:?}",
        app.session.model_display
    );
    let after = render(&mut app, 80, 12);
    let text = screen(&after);
    assert!(
        text.contains("claude-new"),
        "the footer shows the switch: {text}"
    );
    assert!(!text.contains("old-model"), "the old model is gone: {text}");
}

/// ADR 0027: a contributor's value renders through the registry's dynamic
/// fallback even when the id is one the **user wrote** in a row. The old code was not uniformly
/// raw here: `item.known` was true for a preset id (or any id `item_is_known` accepted), and such
/// an item *did* reach the old `contribution_item`'s general store fallback. What is new is the
/// user-written id: the old `resolve_item` marked it `known = false`, so `render_item` returned
/// the raw id **before** consulting the store — publishing a value never changed the row. This
/// test fails on the old code at the second render: `left: "custom.thing", right: "hello"`.
#[test]
fn an_arbitrary_store_id_renders_through_the_dynamic_component() {
    let mut app = App::new("mock", "s");
    app.apply_footer(&rows(vec![item("custom.thing")]));
    // No value yet: an id the user wrote shows raw (ADR 0008), warned once at parse.
    let before = render(&mut app, 60, 12);
    assert!(
        row(&before, 11).contains("custom.thing"),
        "{}",
        row(&before, 11)
    );
    publish(&app, "custom.thing", "hello");
    let after = render(&mut app, 60, 12);
    assert_eq!(row(&after, 11).trim_end(), "hello");
}

/// ADR 0027: a compiled-in component claims its id first, so a store value
/// published under a built-in's name cannot hijack it — the dynamic fallback is only for ids no
/// built-in claims (`components::lookup`). This is a behaviour the old dispatch got wrong: the old
/// `contribution_item` checked only `CONTRIBUTION_ITEMS` before falling back to the store, so a
/// `model` value in the store was drawn instead of the model. On the old code the second render
/// reads `HIJACK`; here it must read the model.
#[test]
fn a_builtin_id_wins_over_the_store_fallback() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(None, Some("Mock 5".to_string()), None, None);
    app.apply_footer(&rows(vec![item("model")]));
    publish(&app, "model", "HIJACK");
    let terminal = render(&mut app, 60, 12);
    assert_eq!(
        row(&terminal, 11).trim_end(),
        "Mock 5",
        "the built-in component wins over a store value under its id"
    );
}

// ---- ADR 0027: the out-of-process `command` footer component ----

/// A footer config whose `weather` item is a command (no real process runs here; the value is
/// published straight into the store, exactly as the runner does).
fn command_item_config(id: &str, extra: TuiFooterItemConfig) -> TuiFooterConfig {
    let mut config = items(&[(
        id,
        TuiFooterItemConfig {
            id: id.to_string(),
            params: vec![("command".to_string(), "printf hi".to_string())],
            ..extra
        },
    )]);
    config.rows = vec![TuiFooterRowConfig {
        left: vec![item(id)],
        ..Default::default()
    }];
    config
}

/// Publish a command value with an optional symbol, the way `CommandContributor` writes it.
fn publish_contribution(
    app: &App,
    id: &str,
    text: &str,
    role: radian_host::SegmentRole,
    symbol: Option<&str>,
) {
    app.contributions
        .write_contribution(radian_host::Contribution {
            item: id.to_string(),
            segments: vec![radian_host::Segment::new(text, role)],
            stale: false,
            symbol: symbol.map(str::to_string),
            usage: None,
        });
}

/// A `command` id is a known item: no "unknown item" warning, and it appears wherever its id is
/// placed in a row. Before the producer's first value it hides (never shows its raw id).
#[test]
fn a_command_item_appears_where_its_id_is_placed() {
    let mut config = command_item_config("weather", TuiFooterItemConfig::default());
    config.rows = vec![TuiFooterRowConfig {
        left: vec![item("model"), item("weather")],
        ..Default::default()
    }];
    let (footer, warnings) = Footer::from_config(&config);
    assert!(
        warnings.is_empty(),
        "a command id is known, not an unknown id: {warnings:?}"
    );
    let mut app = App::new("mock", "s");
    app.set_footer_facts(None, Some("Mock 5".to_string()), None, None);
    app.footer = footer;

    // No value yet: the item hides rather than showing the raw id (unlike an unknown id).
    let before = render(&mut app, 60, 12);
    assert!(
        !row(&before, 11).contains("weather"),
        "{}",
        row(&before, 11)
    );

    publish_contribution(
        &app,
        "weather",
        "72F",
        radian_host::SegmentRole::Normal,
        None,
    );
    let after = render(&mut app, 60, 12);
    let line = row(&after, 11);
    let model = line.find("Mock 5").expect("the model");
    let weather = line.find("72F").expect("the command value");
    assert!(
        model < weather,
        "the value is where the id is placed: {line:?}"
    );
}

/// A JSON line's `style` sets the text's theme role; a configured `symbol_style` styles the
/// symbol; a configured `symbol` wins over the JSON `symbol`.
#[test]
fn a_command_json_style_and_symbol_render_and_the_symbol_style_applies() {
    let config = command_item_config(
        "weather",
        TuiFooterItemConfig {
            symbol_style: Some("red".to_string()),
            ..Default::default()
        },
    );
    let (footer, warnings) = Footer::from_config(&config);
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut app = App::new("mock", "s");
    app.footer = footer;
    publish_contribution(
        &app,
        "weather",
        "hi",
        radian_host::SegmentRole::Warn,
        // The symbol is drawn verbatim, so it carries its own trailing space (as the built-in
        // glyphs do).
        Some("! "),
    );
    let terminal = render(&mut app, 60, 12);
    let line = row(&terminal, 11);
    assert!(line.contains("! hi"), "{line:?}");
    // The JSON `style` is the warn role.
    assert_eq!(
        fg_at(&terminal, 11, "hi"),
        app.theme.budget_over.fg,
        "the JSON style is the text role"
    );
    // The configured `symbol_style` styles the JSON symbol too.
    assert_eq!(
        fg_at(&terminal, 11, "!"),
        Some(Color::Red),
        "the configured symbol_style applies to the JSON symbol"
    );

    // A configured `symbol` wins over the JSON `symbol`.
    let config = command_item_config(
        "weather",
        TuiFooterItemConfig {
            icon: Some("> ".to_string()),
            ..Default::default()
        },
    );
    let (footer, _) = Footer::from_config(&config);
    let mut app = App::new("mock", "s");
    app.footer = footer;
    publish_contribution(
        &app,
        "weather",
        "hi",
        radian_host::SegmentRole::Normal,
        Some("! "),
    );
    let terminal = render(&mut app, 60, 12);
    let line = row(&terminal, 11);
    assert!(line.contains("> hi"), "{line:?}");
    assert!(!line.contains('!'), "the configured symbol wins: {line:?}");
}

/// A command item may format its `{value}` without a "unknown placeholder" warning.
#[test]
fn a_command_item_accepts_the_value_placeholder() {
    let config = command_item_config(
        "weather",
        TuiFooterItemConfig {
            format: Some("[{value}](red)".to_string()),
            ..Default::default()
        },
    );
    let (footer, warnings) = Footer::from_config(&config);
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut app = App::new("mock", "s");
    app.footer = footer;
    publish_contribution(
        &app,
        "weather",
        "72F",
        radian_host::SegmentRole::Normal,
        None,
    );
    let terminal = render(&mut app, 60, 12);
    assert!(row(&terminal, 11).contains("72F"), "{}", row(&terminal, 11));
    assert_eq!(fg_at(&terminal, 11, "72F"), Some(Color::Red));
}

/// The brief's acceptance, end to end: a fixture `printf hi` command runs through the real
/// contributor and its first line appears in the footer where its id is placed.
#[test]
fn a_printf_fixture_appears_in_the_configured_footer_location() {
    let dir =
        std::env::temp_dir().join(format!("radian-tui-command-footer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("hi.sh");
    std::fs::write(&script, "#!/bin/sh\nprintf hi\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let store = radian_host::ContributionStore::new();
    let spec = radian_host::CommandSpec {
        id: "weather".to_string(),
        program: script.as_os_str().to_owned(),
        args: Vec::new(),
        period: Some(std::time::Duration::from_secs(60)),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let contributor = std::sync::Arc::new(radian_host::CommandContributor::new(
            spec,
            radian_host::FooterContext::new(),
            "sess-1",
            Some(dir.display().to_string()),
        ));
        let mut runner =
            radian_host::ContributorRunner::new(contributor, std::sync::Arc::clone(&store));
        runner.tick().await;
    });

    let mut app = App::new("mock", "s");
    app.set_footer_facts(None, Some("Mock 5".to_string()), None, None);
    app.set_contributions(store);
    let (footer, warnings) = Footer::from_config(&command_item_config(
        "weather",
        TuiFooterItemConfig::default(),
    ));
    assert!(warnings.is_empty(), "{warnings:?}");
    app.footer = footer;
    let terminal = render(&mut app, 60, 12);
    let line = row(&terminal, 11);
    assert!(
        line.contains("hi"),
        "the fixture's `printf hi` is in the configured location: {line:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- ADR 0027: the provider-related `usage` component ----

/// One usage window: `remaining` percent and an optional reset countdown.
fn window(label: &str, percent: f64, resets_at: Option<&str>) -> radian_host::UsageWindow {
    radian_host::UsageWindow {
        label: label.to_string(),
        remaining: Some(radian_host::Remaining::Percent(percent)),
        resets_at: resets_at.map(str::to_string),
    }
}

fn usage_snapshot(
    provider: &str,
    title: &str,
    windows: Vec<radian_host::UsageWindow>,
    plan: Option<&str>,
) -> radian_host::UsageSnapshot {
    radian_host::UsageSnapshot {
        provider: provider.to_string(),
        title: title.to_string(),
        windows,
        plan: plan.map(str::to_string),
    }
}

/// Publish the `usage` item's snapshot, the way a descriptor's `Endpoint` fetch writes it.
fn publish_snapshot(app: &App, snapshot: radian_host::UsageSnapshot) {
    app.contributions
        .write_contribution(radian_host::Contribution {
            item: radian_host::USAGE_ITEM.to_string(),
            segments: Vec::new(),
            stale: false,
            symbol: None,
            usage: Some(snapshot),
        });
}

/// Submit one `/…` line and return the notices the app printed.
fn submit_notices(app: &mut App, line: &str) -> Vec<String> {
    app.input = line.into();
    let enter = ratatui::crossterm::event::KeyEvent::new(
        ratatui::crossterm::event::KeyCode::Enter,
        ratatui::crossterm::event::KeyModifiers::NONE,
    );
    let _ = app.on_key(enter);
    app.session
        .entries
        .iter()
        .filter(|entry| entry.kind == radian_tui::EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

/// Acceptance ②: the drawn text for opencode-go is byte-for-byte the line the source built
/// before the move. The literal is `opencode_go::display`'s output for the response whose
/// snapshot is this one, captured on the pre-change tree.
#[test]
fn the_usage_row_is_the_pre_move_opencode_go_line() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(
        None,
        Some("Mock 5".to_string()),
        Some("opencode-go".to_string()),
        None,
    );
    publish_snapshot(
        &app,
        usage_snapshot(
            "opencode-go",
            "Usage",
            vec![window("5h", 98.0, Some("1h0m")), window("7d", 81.0, None)],
            None,
        ),
    );
    let terminal = render(&mut app, 100, 12);
    assert_eq!(
        row(&terminal, 9).trim_end(),
        "Usage (remaining): 5h: 98% · 7d: 81% · 5h ↺1h0m"
    );
}

/// The same for chatgpt, whose line carries its plan type (`ChatGPT usage (remaining): 5h: 95%
/// · 7d: 60% · 5h ↺1h0m · 7d ↺1d1h · pro`, the pre-move `chatgpt_usage::display` output).
#[test]
fn the_usage_row_is_the_pre_move_chatgpt_line() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(
        None,
        Some("Mock 5".to_string()),
        Some("chatgpt".to_string()),
        None,
    );
    publish_snapshot(
        &app,
        usage_snapshot(
            "chatgpt",
            "ChatGPT usage",
            vec![
                window("5h", 95.0, Some("1h0m")),
                window("7d", 60.0, Some("1d1h")),
            ],
            Some("pro"),
        ),
    );
    let terminal = render(&mut app, 100, 12);
    assert_eq!(
        row(&terminal, 9).trim_end(),
        "ChatGPT usage (remaining): 5h: 95% · 7d: 60% · 5h ↺1h0m · 7d ↺1d1h · pro"
    );
}

/// ADR 0027, the ADR's own mutation: a provider whose descriptor
/// declares no `usage` source draws nothing, even while the store holds a snapshot for it. With
/// the descriptor check gone, this row would appear. (Slice ④ gave `anthropic` a `Headers`
/// source, so the no-source example is the generic provider now.)
#[test]
fn a_provider_without_a_usage_source_draws_no_usage_row() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(
        None,
        Some("Mock 5".to_string()),
        Some("openai-compatible".to_string()),
        None,
    );
    publish_snapshot(
        &app,
        usage_snapshot(
            "openai-compatible",
            "Usage",
            vec![window("5h", 98.0, None)],
            None,
        ),
    );
    assert_eq!(app.footer.lines(&app, 100).len(), 2, "no usage row");
    let terminal = render(&mut app, 100, 12);
    assert!(!screen(&terminal).contains("5h:"), "{}", screen(&terminal));
}

/// The snapshot must belong to the **current** route's provider: a leftover value for another
/// provider does not draw (the `/model` switch removes it, this is the belt to that braces).
#[test]
fn a_snapshot_for_another_provider_does_not_draw() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(
        None,
        Some("Mock 5".to_string()),
        Some("chatgpt".to_string()),
        None,
    );
    publish_snapshot(
        &app,
        usage_snapshot("opencode-go", "Usage", vec![window("5h", 98.0, None)], None),
    );
    let terminal = render(&mut app, 100, 12);
    assert!(!screen(&terminal).contains("5h:"), "{}", screen(&terminal));
}

/// A stale snapshot is the last good value, drawn dim (ADR 0027 C3), exactly as the store
/// fallback drew a stale contribution before the move.
#[test]
fn a_stale_snapshot_is_drawn_dim() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(
        None,
        Some("Mock 5".to_string()),
        Some("opencode-go".to_string()),
        None,
    );
    publish_snapshot(
        &app,
        usage_snapshot("opencode-go", "Usage", vec![window("5h", 98.0, None)], None),
    );
    let fresh = render(&mut app, 100, 12);
    let fresh_fg = fg_at(&fresh, 9, "5h: 98%");
    app.contributions.mark_stale(radian_host::USAGE_ITEM);
    let stale = render(&mut app, 100, 12);
    assert!(row(&stale, 9).contains("5h: 98%"), "the value is kept");
    assert_ne!(
        fg_at(&stale, 9, "5h: 98%"),
        fresh_fg,
        "a stale snapshot is drawn dim"
    );
}

/// ADR 0027 + ADR 0008: an id an earlier version used for a provider's
/// usage row still renders (as `usage`), with one startup warning naming the new id — once,
/// however often the id is written.
#[test]
fn the_old_usage_ids_still_render_and_warn_once() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(
        None,
        Some("Mock 5".to_string()),
        Some("opencode-go".to_string()),
        None,
    );
    let mut config = items(&[("opencode-go.usage", TuiFooterItemConfig::default())]);
    config.rows = vec![
        TuiFooterRowConfig {
            left: vec![item("opencode-go.usage")],
            ..Default::default()
        },
        TuiFooterRowConfig {
            left: vec![item("opencode-go.usage")],
            ..Default::default()
        },
    ];
    let warnings = app.apply_footer(&config);
    assert_eq!(
        warnings.len(),
        1,
        "one warning for the id, however often it is written: {warnings:?}"
    );
    assert!(
        warnings[0].contains("\"opencode-go.usage\"") && warnings[0].contains("\"usage\""),
        "the warning names the old and the new id: {warnings:?}"
    );
    publish_snapshot(
        &app,
        usage_snapshot("opencode-go", "Usage", vec![window("5h", 98.0, None)], None),
    );
    let terminal = render(&mut app, 100, 12);
    assert!(
        screen(&terminal).contains("5h: 98%"),
        "the old id renders the usage line:\n{}",
        screen(&terminal)
    );
}

/// `/usage` prints **every** window of the current provider's snapshot, not the footer's
/// one-line form (ADR 0027).
#[test]
fn usage_prints_every_window() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(
        None,
        Some("Mock 5".to_string()),
        Some("opencode-go".to_string()),
        None,
    );
    publish_snapshot(
        &app,
        usage_snapshot(
            "opencode-go",
            "Usage",
            vec![
                window("5h", 98.0, Some("1h0m")),
                window("7d", 81.0, None),
                window("30d", 91.0, None),
            ],
            None,
        ),
    );
    let notices = submit_notices(&mut app, "/usage");
    assert!(notices[0].contains("opencode-go"), "{notices:?}");
    for expected in [
        "5h: 98% remaining, resets in 1h0m",
        "7d: 81% remaining",
        "30d: 91% remaining",
    ] {
        assert!(
            notices.iter().any(|notice| notice == expected),
            "`{expected}` must be a notice: {notices:?}"
        );
    }
}

/// `/usage` says so in one line when the routed provider has no usage source, and when there
/// is no snapshot yet (ADR 0027). Slice ④: `anthropic` reads its
/// rate-limit headers per response, so it has a source — with no response yet it says "no
/// snapshot yet", like `opencode-go` before its first poll.
#[test]
fn usage_says_why_there_is_nothing_to_show() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(None, None, Some("openai-compatible".to_string()), None);
    let notices = submit_notices(&mut app, "/usage");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("openai-compatible") && notices[0].contains("no usage source"),
        "{notices:?}"
    );

    let mut app = App::new("mock", "s");
    app.set_footer_facts(None, None, Some("anthropic".to_string()), None);
    let notices = submit_notices(&mut app, "/usage");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("anthropic") && notices[0].contains("no snapshot yet"),
        "{notices:?}"
    );

    let mut app = App::new("mock", "s");
    app.set_footer_facts(None, None, Some("opencode-go".to_string()), None);
    let notices = submit_notices(&mut app, "/usage");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("opencode-go") && notices[0].contains("no snapshot yet"),
        "{notices:?}"
    );
}

/// ADR 0027 slice ④: a `Headers` provider's rate-limit snapshot reaches the
/// `usage` row from the model response event — same rendering rules as the endpoint source —
/// and `/usage` prints its windows. A response without headers leaves the row absent.
#[test]
fn rate_limit_headers_reach_the_usage_row() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(None, None, Some("anthropic".to_string()), None);
    app.apply_footer(&rows(vec![item("usage")]));

    // No response yet: no snapshot, no row.
    let terminal = render(&mut app, 80, 12);
    assert!(
        !row(&terminal, 11).contains("remaining"),
        "no snapshot, no row: {:?}",
        row(&terminal, 11)
    );

    // A response carrying the headers publishes the snapshot; the row draws it.
    app.apply_event(&radian_core::AgentEvent::ModelResponseReceived {
        stop_reason: radian_core::StopReason::EndTurn,
        tool_calls: 0,
        usage: None,
        limits: Some(radian_core::RateLimits {
            headers: vec![
                (
                    "anthropic-ratelimit-requests-remaining".to_owned(),
                    "999".to_owned(),
                ),
                (
                    "anthropic-ratelimit-requests-reset".to_owned(),
                    "2030-01-01T00:00:00Z".to_owned(),
                ),
                (
                    "anthropic-ratelimit-tokens-remaining".to_owned(),
                    "99900".to_owned(),
                ),
            ],
        }),
    });
    let stored = app
        .contributions
        .get(radian_host::USAGE_ITEM)
        .expect("the headers publish a snapshot");
    let snapshot = stored.usage.as_ref().expect("a usage snapshot");
    assert_eq!(snapshot.provider, "anthropic");
    assert_eq!(snapshot.title, "Anthropic usage");
    assert_eq!(snapshot.windows.len(), 2);
    let terminal = render(&mut app, 80, 12);
    let line = row(&terminal, 11);
    assert!(
        line.contains("Anthropic usage (remaining):"),
        "the footer line: {line:?}"
    );
    assert!(line.contains("requests: 999"), "the count: {line:?}");
    assert!(line.contains("tokens: 99900"), "the count: {line:?}");
    assert!(line.contains("↺"), "the reset: {line:?}");

    // `/usage` prints the same snapshot's windows.
    let notices = submit_notices(&mut app, "/usage");
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("requests: 999 remaining")),
        "{notices:?}"
    );
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("tokens: 99900 remaining")),
        "{notices:?}"
    );
}

/// A response without headers changes nothing: with no earlier snapshot the row stays absent.
#[test]
fn a_response_without_headers_leaves_the_usage_row_absent() {
    let mut app = App::new("mock", "s");
    app.set_footer_facts(None, None, Some("openai".to_string()), None);
    app.apply_footer(&rows(vec![item("usage")]));
    app.apply_event(&radian_core::AgentEvent::ModelResponseReceived {
        stop_reason: radian_core::StopReason::EndTurn,
        tool_calls: 0,
        usage: None,
        limits: None,
    });
    assert!(
        app.contributions.get(radian_host::USAGE_ITEM).is_none(),
        "nothing invented without headers"
    );
    let terminal = render(&mut app, 80, 12);
    assert!(
        !row(&terminal, 11).contains("remaining"),
        "the row is absent: {:?}",
        row(&terminal, 11)
    );
}

/// `radian footer export` prints the preset file exactly, so the presets are user-facing
/// text: none of the four names an ADR.
/// Mutation: put `(ADR 0027)` back into one preset comment → this fails.
#[test]
fn exported_presets_name_no_adr() {
    for preset in ["deck", "full", "pi", "classic"] {
        let toml = radian_tui::footer::export_preset(preset).expect("a built-in preset");
        assert!(!toml.contains("ADR"), "{preset} names an ADR:\n{toml}");
    }
}

/// The user-facing docs never name an ADR: the root `README.md`, `crates/radian-sandbox/LIMITS.md` and
/// `docs/user/pi-migration.md` (crate READMEs and developer docs keep theirs).
/// Mutation: put `(ADR 0008)` back into one doc → this fails.
#[test]
fn user_docs_name_no_adr() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for doc in [
        "README.md",
        "crates/radian-sandbox/LIMITS.md",
        "docs/user/pi-migration.md",
    ] {
        let text = std::fs::read_to_string(root.join(doc)).expect("a user-facing doc exists");
        assert!(!text.contains("ADR"), "{doc} names an ADR");
    }
}
