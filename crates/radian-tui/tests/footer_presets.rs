//! The four built-in presets render byte-identical (ADR 0027:
//! presets are TOML now, parsed by the user's config parser).
//!
//! The golden screens in `tests/golden/footer/` were generated from the pristine `525daf0`
//! tree (before the Rust row tables were removed) with the same fixed `App` this test builds:
//! every preset × every glyph set × widths 40/60/80/100/120, no running jobs. `full` with no
//! running job stays byte-identical; `full` with a job is covered by
//! `full_shows_jobs_while_deck_does_not` below.
//!
//! The fixture populates **every** component a preset can show (p1 review of 83cb172: the old
//! fixture left `speed`/`cost` empty, so a swap of those two items stayed green). Tokens
//! in/out/cache(/write), speed, cost, context (percent/budget/auto), elapsed, reasoning,
//! host, session, a dirty branch, the repo name, the worktree, and a usage snapshot all carry
//! distinct non-empty values, so every item of every preset draws at the widest width.

use agent_core::{BudgetReport, UsageSource};
use radian_host::config::TuiFooterConfig;
use radian_session::RuntimeEvent;
use radian_tui::App;
use radian_tui::app::Input;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const WIDTHS: [u16; 5] = [40, 60, 80, 100, 120];
const GLYPHS: [&str; 4] = ["unicode", "nerd", "ascii", "emoji"];
const PRESETS: [&str; 4] = ["deck", "full", "pi", "classic"];

/// One normal contribution segment, the way the background contributors publish.
fn publish(app: &App, item: &str, text: &str) {
    app.contributions.write(
        item,
        vec![radian_host::Segment::new(
            text,
            radian_host::SegmentRole::Normal,
        )],
    );
}

/// One usage window: `remaining` percent and an optional reset countdown.
fn window(label: &str, percent: f64, resets_at: Option<&str>) -> radian_host::UsageWindow {
    radian_host::UsageWindow {
        label: label.to_string(),
        remaining: Some(radian_host::Remaining::Percent(percent)),
        resets_at: resets_at.map(str::to_string),
    }
}

/// The `usage` item's snapshot, the way a descriptor's `Endpoint` fetch writes it.
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

/// The fixed `App` both the golden generation and this test render: every component a preset
/// can show carries a distinct non-empty value, so at the widest width every item draws. The
/// same builder runs on the old tree (the generator) and the new one (this test); the host
/// name is fixed because `full` shows `host`.
fn fixed_app(preset: &str, glyphs: &str) -> App {
    let mut app = App::new("mock", "s");
    app.set_hostname(Some("testhost".to_string()));
    app.set_footer_facts(
        Some(std::path::PathBuf::from("/tmp/ws")),
        Some("Mock 5".to_string()),
        Some("opencode-go".to_string()),
        Some("high".to_string()),
    );
    // A dirty contributor branch, the repo name and the worktree: `git_branch` draws
    // `{branch}{dirty}`, `cwd` draws `{repo}`, `worktree` draws its contributor value.
    publish(&app, "git_branch", "feature/fixture");
    publish(&app, "git_modified", "2");
    publish(&app, "git_untracked", "1");
    publish(&app, "git_root", "fixture-repo");
    publish(&app, "worktree", "fixture-wt");
    // The session's usage totals and fee: priced at $3/Mtok in, $15/Mtok out, $0.30/Mtok
    // cache-read and $1.50/Mtok cache-write, so every `tokens`/`cost` placeholder is filled.
    app.set_model_price(Some(radian_host::ModelPrice {
        input: 3_000_000,
        output: 15_000_000,
        cache_read: Some(300_000),
        cache_write: Some(1_500_000),
        tiers: Vec::new(),
    }));
    app.session.projection.add_usage(&agent_core::ModelUsage {
        input: Some(19_000),
        output: Some(760),
        cache_read: Some(33_000),
        cache_write: Some(12_000),
        input_includes_cache: Some(false),
        ..Default::default()
    });
    // The context window and this request's report: 413K of 1M is 41.3%, and auto-compact
    // is on so `{auto}` draws too.
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
    app.set_auto_compact(true);
    // One finished generation on the injected clock: 100 tokens in 2s is 50.0 tok/s, and the
    // elapsed item shows the finished 2.0s.
    let base = std::time::Instant::now();
    app.session.timer.request_started(base);
    app.session
        .timer
        .response_finished(base + std::time::Duration::from_secs(2), Some(100));
    app.set_now(base + std::time::Duration::from_secs(9));
    // The current provider's usage snapshot: `deck`'s first row draws it as one line.
    publish_snapshot(
        &app,
        radian_host::UsageSnapshot {
            provider: "opencode-go".to_string(),
            title: "Usage".to_string(),
            windows: vec![window("5h", 98.0, Some("1h0m")), window("7d", 81.0, None)],
            plan: None,
        },
    );
    let warnings = app.apply_footer(&TuiFooterConfig {
        preset: Some(preset.to_string()),
        glyphs: Some(glyphs.to_string()),
        ..Default::default()
    });
    assert!(warnings.is_empty(), "{preset}/{glyphs}: {warnings:?}");
    app
}

fn screen(app: &mut App, width: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, app))
        .unwrap();
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

/// Every preset × every glyph set × several widths renders exactly as at `525daf0`.
#[test]
fn the_four_presets_render_byte_identical() {
    for preset in PRESETS {
        for glyphs in GLYPHS {
            for width in WIDTHS {
                let mut app = fixed_app(preset, glyphs);
                let rendered = screen(&mut app, width);
                let expected = golden_for(preset, glyphs, width);
                assert_eq!(
                    rendered, expected,
                    "screen changed for preset={preset} glyphs={glyphs} width={width}:\n{rendered}"
                );
            }
        }
    }
}

/// `full` includes `jobs`, the other three presets do not. With one
/// running background job `full` shows it; `deck` with a running job stays byte-identical to
/// its no-job golden (no `jobs` row, nothing shown).
#[test]
fn full_shows_jobs_while_deck_does_not() {
    let mut full = fixed_app("full", "unicode");
    start_job(&mut full);
    let rendered = screen(&mut full, 100);
    assert!(
        rendered.contains("1 job") && rendered.contains("18m"),
        "full shows the running job: {rendered:?}"
    );

    // `deck` has no `jobs` row: a running job changes nothing on screen.
    let mut deck = fixed_app("deck", "unicode");
    start_job(&mut deck);
    assert_eq!(
        screen(&mut deck, 100),
        golden_for("deck", "unicode", 100),
        "deck with a running job stays byte-identical"
    );
}

fn start_job(app: &mut App) {
    let start = std::time::Instant::now();
    app.set_now(start);
    app.update(Input::Runtime(RuntimeEvent::BackgroundJobStarted {
        job_id: "job-1".to_string(),
        command: "make test".to_string(),
        output: "available through job".to_string(),
    }));
    app.set_now(start + std::time::Duration::from_secs(18 * 60));
}

/// One waiting followup, the way the runtime reports it: `queued` counts the `next_turn`
/// inputs without a matching admission.
fn enqueue(app: &mut App, id: &str) {
    app.update(Input::Runtime(RuntimeEvent::InboxEnqueued {
        message: agent_core::Message::user("queued text"),
        message_id: agent_core::MessageId(id.to_string()),
        lane: radian_session::InboxLane::NextTurn,
        source: agent_core::InputSource::User,
        wake: true,
    }));
}

/// Every item named in each preset file appears in its widest golden (p1 review of 83cb172:
/// the old fixture left `speed`/`cost` empty, so a `speed`/`cost` swap stayed green). The
/// marker is the fixture value that proves the item drew; an id with no marker is a hard
/// failure, so a preset change without fixture coverage fails loudly. `jobs` hides with no
/// job running, so it is checked against a with-job screen instead.
#[test]
fn every_preset_item_appears_in_its_widest_golden() {
    // (preset, item id, marker): the marker is the fixture value that proves the item drew
    // in the unicode width-120 golden.
    let markers: &[(&str, &str, &str)] = &[
        ("deck", "usage", "Usage (remaining): 5h: 98%"),
        ("deck", "model", "Mock 5"),
        ("deck", "cwd", "fixture-repo"),
        ("deck", "git_branch", "feature/fixture ?3"),
        ("deck", "worktree", "worktree fixture-wt"),
        ("deck", "context", "41.3%"),
        ("deck", "reasoning", "* high"),
        ("deck", "tokens", "R33K"),
        ("deck", "cost", "$0.096"),
        ("full", "brand", "R · @ testhost"),
        ("full", "host", "@ testhost"),
        ("full", "model", "Mock 5"),
        ("full", "reasoning", "* high"),
        ("full", "cwd", "fixture-repo"),
        ("full", "git_branch", "feature/fixture ?3"),
        ("full", "session", "# s"),
        ("full", "tokens", "↓760"),
        ("full", "speed", "50.0 tok/s"),
        ("full", "cost", "$0.096"),
        ("full", "context", "41.3%/1.0M ~"),
        ("full", "elapsed", "2.0s"),
        ("pi", "model", "Mock 5"),
        ("pi", "cwd", "/tmp/ws"),
        ("pi", "git_branch", "feature/fixture"),
        ("pi", "context", "41.3%"),
        ("pi", "reasoning", "* high"),
        ("pi", "tokens", "R33K"),
        ("pi", "cost", "$0.096"),
    ];
    for preset in PRESETS {
        let exported = radian_tui::footer::export_preset(preset).expect("a shipped preset");
        let text = format!("schema_version = 1\n{exported}");
        let config = radian_host::config::parse(std::path::Path::new("guard"), &text)
            .expect("a built-in preset parses");
        for row in &config.tui.footer.rows {
            for item in row.left.iter().chain(row.right.iter()) {
                if item.id == "jobs" {
                    let mut app = fixed_app(preset, "unicode");
                    start_job(&mut app);
                    let rendered = screen(&mut app, 120);
                    assert!(
                        rendered.contains("1 job"),
                        "{preset}: the with-job screen shows the `jobs` item"
                    );
                    continue;
                }
                if item.id == "queued" {
                    // `queued` hides with nothing waiting, like `jobs` with no job: the
                    // guard renders with two followups waiting instead of the golden.
                    let mut app = fixed_app(preset, "unicode");
                    enqueue(&mut app, "q-1");
                    enqueue(&mut app, "q-2");
                    let rendered = screen(&mut app, 120);
                    assert!(
                        rendered.contains("2 queued"),
                        "{preset}: the with-queue screen shows the `queued` item"
                    );
                    continue;
                }
                let (_, _, marker) = markers
                    .iter()
                    .find(|(p, id, _)| *p == preset && *id == item.id)
                    .unwrap_or_else(|| {
                        panic!(
                            "{preset}: item {:?} has no guard marker — extend the marker table and the fixture",
                            item.id
                        )
                    });
                assert!(
                    golden_for(preset, "unicode", 120).contains(marker),
                    "{preset}: item {:?} (marker {marker:?}) is missing from the widest golden",
                    item.id
                );
            }
        }
    }
}

/// `radian footer export <preset>` prints the preset's TOML exactly (item 3): using the
/// export as the config's `[tui.footer]` section gives the same footer the preset resolves to.
#[test]
fn exported_presets_round_trip_through_the_user_config() {
    for preset in PRESETS {
        let exported = radian_tui::footer::export_preset(preset).expect("a shipped preset");
        let text = format!("schema_version = 1\n{exported}");
        let path = std::path::Path::new("round-trip");
        let config =
            radian_host::config::parse(path, &text).expect("the export parses as a user config");
        assert!(
            config.skipped.is_empty(),
            "{preset}: the export has no skipped keys: {:?}",
            config.skipped
        );
        let mut via_export = App::new("mock", "s");
        via_export.apply_footer(&config.tui.footer);
        let mut direct = App::new("mock", "s");
        direct.apply_footer(&TuiFooterConfig {
            preset: Some(preset.to_string()),
            ..Default::default()
        });
        // The export carries the rows, not the preset name (so `preset` differs): what must
        // match is the footer itself — the same rows, the same rendered lines.
        assert_eq!(
            via_export.footer, direct.footer,
            "{preset}: the export resolves to the preset's rows"
        );
        for width in [40usize, 100] {
            assert_eq!(
                via_export.footer.lines(&via_export, width),
                direct.footer.lines(&direct, width),
                "{preset}: the export draws the preset's lines at width {width}"
            );
        }
    }
}

/// An unknown preset is an error naming the four (item 3).
#[test]
fn an_unknown_preset_names_the_four() {
    let error = radian_tui::footer::export_preset("bogus").unwrap_err();
    for preset in PRESETS {
        assert!(
            error.contains(preset),
            "the error names {preset}: {error:?}"
        );
    }
}

/// A parse error in a built-in preset is a test failure, never a runtime surprise (item 4):
/// all four ship parseable by the user's config parser, with no skipped keys.
#[test]
fn all_four_built_in_presets_parse() {
    for preset in PRESETS {
        let exported = radian_tui::footer::export_preset(preset).expect("a shipped preset");
        let text = format!("schema_version = 1\n{exported}");
        let path = std::path::Path::new("built-in");
        let config = radian_host::config::parse(path, &text).expect("a built-in preset parses");
        assert!(
            config.skipped.is_empty(),
            "{preset}: no skipped keys: {:?}",
            config.skipped
        );
        // The file declares its own preset: `classic`'s two keys are covered too, so a swap
        // of their values fails here (and in the export round trip below).
        assert_eq!(
            config.tui.footer.preset.as_deref(),
            Some(preset),
            "{preset}: the file declares its own preset"
        );
        if preset == "classic" {
            assert!(config.tui.footer.rows.is_empty(), "classic carries no rows");
        } else {
            assert!(!config.tui.footer.rows.is_empty(), "{preset} carries rows");
        }
    }
}

fn golden_for(preset: &str, glyphs: &str, width: u16) -> &'static str {
    match (preset, glyphs, width) {
        ("deck", "unicode", 40) => include_str!("golden/footer/deck-unicode-40.txt"),
        ("deck", "unicode", 60) => include_str!("golden/footer/deck-unicode-60.txt"),
        ("deck", "unicode", 80) => include_str!("golden/footer/deck-unicode-80.txt"),
        ("deck", "unicode", 100) => include_str!("golden/footer/deck-unicode-100.txt"),
        ("deck", "unicode", 120) => include_str!("golden/footer/deck-unicode-120.txt"),
        ("deck", "nerd", 40) => include_str!("golden/footer/deck-nerd-40.txt"),
        ("deck", "nerd", 60) => include_str!("golden/footer/deck-nerd-60.txt"),
        ("deck", "nerd", 80) => include_str!("golden/footer/deck-nerd-80.txt"),
        ("deck", "nerd", 100) => include_str!("golden/footer/deck-nerd-100.txt"),
        ("deck", "nerd", 120) => include_str!("golden/footer/deck-nerd-120.txt"),
        ("deck", "ascii", 40) => include_str!("golden/footer/deck-ascii-40.txt"),
        ("deck", "ascii", 60) => include_str!("golden/footer/deck-ascii-60.txt"),
        ("deck", "ascii", 80) => include_str!("golden/footer/deck-ascii-80.txt"),
        ("deck", "ascii", 100) => include_str!("golden/footer/deck-ascii-100.txt"),
        ("deck", "ascii", 120) => include_str!("golden/footer/deck-ascii-120.txt"),
        ("deck", "emoji", 40) => include_str!("golden/footer/deck-emoji-40.txt"),
        ("deck", "emoji", 60) => include_str!("golden/footer/deck-emoji-60.txt"),
        ("deck", "emoji", 80) => include_str!("golden/footer/deck-emoji-80.txt"),
        ("deck", "emoji", 100) => include_str!("golden/footer/deck-emoji-100.txt"),
        ("deck", "emoji", 120) => include_str!("golden/footer/deck-emoji-120.txt"),
        ("full", "unicode", 40) => include_str!("golden/footer/full-unicode-40.txt"),
        ("full", "unicode", 60) => include_str!("golden/footer/full-unicode-60.txt"),
        ("full", "unicode", 80) => include_str!("golden/footer/full-unicode-80.txt"),
        ("full", "unicode", 100) => include_str!("golden/footer/full-unicode-100.txt"),
        ("full", "unicode", 120) => include_str!("golden/footer/full-unicode-120.txt"),
        ("full", "nerd", 40) => include_str!("golden/footer/full-nerd-40.txt"),
        ("full", "nerd", 60) => include_str!("golden/footer/full-nerd-60.txt"),
        ("full", "nerd", 80) => include_str!("golden/footer/full-nerd-80.txt"),
        ("full", "nerd", 100) => include_str!("golden/footer/full-nerd-100.txt"),
        ("full", "nerd", 120) => include_str!("golden/footer/full-nerd-120.txt"),
        ("full", "ascii", 40) => include_str!("golden/footer/full-ascii-40.txt"),
        ("full", "ascii", 60) => include_str!("golden/footer/full-ascii-60.txt"),
        ("full", "ascii", 80) => include_str!("golden/footer/full-ascii-80.txt"),
        ("full", "ascii", 100) => include_str!("golden/footer/full-ascii-100.txt"),
        ("full", "ascii", 120) => include_str!("golden/footer/full-ascii-120.txt"),
        ("full", "emoji", 40) => include_str!("golden/footer/full-emoji-40.txt"),
        ("full", "emoji", 60) => include_str!("golden/footer/full-emoji-60.txt"),
        ("full", "emoji", 80) => include_str!("golden/footer/full-emoji-80.txt"),
        ("full", "emoji", 100) => include_str!("golden/footer/full-emoji-100.txt"),
        ("full", "emoji", 120) => include_str!("golden/footer/full-emoji-120.txt"),
        ("pi", "unicode", 40) => include_str!("golden/footer/pi-unicode-40.txt"),
        ("pi", "unicode", 60) => include_str!("golden/footer/pi-unicode-60.txt"),
        ("pi", "unicode", 80) => include_str!("golden/footer/pi-unicode-80.txt"),
        ("pi", "unicode", 100) => include_str!("golden/footer/pi-unicode-100.txt"),
        ("pi", "unicode", 120) => include_str!("golden/footer/pi-unicode-120.txt"),
        ("pi", "nerd", 40) => include_str!("golden/footer/pi-nerd-40.txt"),
        ("pi", "nerd", 60) => include_str!("golden/footer/pi-nerd-60.txt"),
        ("pi", "nerd", 80) => include_str!("golden/footer/pi-nerd-80.txt"),
        ("pi", "nerd", 100) => include_str!("golden/footer/pi-nerd-100.txt"),
        ("pi", "nerd", 120) => include_str!("golden/footer/pi-nerd-120.txt"),
        ("pi", "ascii", 40) => include_str!("golden/footer/pi-ascii-40.txt"),
        ("pi", "ascii", 60) => include_str!("golden/footer/pi-ascii-60.txt"),
        ("pi", "ascii", 80) => include_str!("golden/footer/pi-ascii-80.txt"),
        ("pi", "ascii", 100) => include_str!("golden/footer/pi-ascii-100.txt"),
        ("pi", "ascii", 120) => include_str!("golden/footer/pi-ascii-120.txt"),
        ("pi", "emoji", 40) => include_str!("golden/footer/pi-emoji-40.txt"),
        ("pi", "emoji", 60) => include_str!("golden/footer/pi-emoji-60.txt"),
        ("pi", "emoji", 80) => include_str!("golden/footer/pi-emoji-80.txt"),
        ("pi", "emoji", 100) => include_str!("golden/footer/pi-emoji-100.txt"),
        ("pi", "emoji", 120) => include_str!("golden/footer/pi-emoji-120.txt"),
        ("classic", "unicode", 40) => include_str!("golden/footer/classic-unicode-40.txt"),
        ("classic", "unicode", 60) => include_str!("golden/footer/classic-unicode-60.txt"),
        ("classic", "unicode", 80) => include_str!("golden/footer/classic-unicode-80.txt"),
        ("classic", "unicode", 100) => include_str!("golden/footer/classic-unicode-100.txt"),
        ("classic", "unicode", 120) => include_str!("golden/footer/classic-unicode-120.txt"),
        ("classic", "nerd", 40) => include_str!("golden/footer/classic-nerd-40.txt"),
        ("classic", "nerd", 60) => include_str!("golden/footer/classic-nerd-60.txt"),
        ("classic", "nerd", 80) => include_str!("golden/footer/classic-nerd-80.txt"),
        ("classic", "nerd", 100) => include_str!("golden/footer/classic-nerd-100.txt"),
        ("classic", "nerd", 120) => include_str!("golden/footer/classic-nerd-120.txt"),
        ("classic", "ascii", 40) => include_str!("golden/footer/classic-ascii-40.txt"),
        ("classic", "ascii", 60) => include_str!("golden/footer/classic-ascii-60.txt"),
        ("classic", "ascii", 80) => include_str!("golden/footer/classic-ascii-80.txt"),
        ("classic", "ascii", 100) => include_str!("golden/footer/classic-ascii-100.txt"),
        ("classic", "ascii", 120) => include_str!("golden/footer/classic-ascii-120.txt"),
        ("classic", "emoji", 40) => include_str!("golden/footer/classic-emoji-40.txt"),
        ("classic", "emoji", 60) => include_str!("golden/footer/classic-emoji-60.txt"),
        ("classic", "emoji", 80) => include_str!("golden/footer/classic-emoji-80.txt"),
        ("classic", "emoji", 100) => include_str!("golden/footer/classic-emoji-100.txt"),
        ("classic", "emoji", 120) => include_str!("golden/footer/classic-emoji-120.txt"),
        _ => panic!("no golden for preset={preset} glyphs={glyphs} width={width}"),
    }
}
