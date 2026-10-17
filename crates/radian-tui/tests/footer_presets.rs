//! The four built-in presets render byte-identical (ADR 0027:
//! presets are TOML now, parsed by the user's config parser).
//!
//! The golden screens in `tests/golden/footer/` were generated from the pristine `525daf0`
//! tree (before the Rust row tables were removed) with the same fixed `App` this test builds:
//! every preset × every glyph set × widths 40/60/80/100/120, no running jobs. `full` with no
//! running job stays byte-identical; `full` with a job is covered by
//! `full_shows_jobs_while_deck_does_not` below.

use radian_host::config::TuiFooterConfig;
use radian_session::RuntimeEvent;
use radian_tui::App;
use radian_tui::app::Input;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const WIDTHS: [u16; 5] = [40, 60, 80, 100, 120];
const GLYPHS: [&str; 4] = ["unicode", "nerd", "ascii", "emoji"];
const PRESETS: [&str; 4] = ["deck", "full", "pi", "classic"];

/// The fixed `App` both the golden generation and this test render: the same facts the
/// existing footer tests use, plus a fixed hostname (the `full` preset shows `host`, which
/// would otherwise bake in the machine's name).
fn fixed_app(preset: &str, glyphs: &str) -> App {
    let mut app = App::new("mock", "s");
    app.set_hostname(Some("testhost".to_string()));
    app.set_footer_facts(
        Some(std::path::PathBuf::from("/tmp/ws")),
        Some("Mock 5".to_string()),
        Some("opencode-go".to_string()),
        Some("high".to_string()),
    );
    app.set_git_branch(Some("main".to_string()));
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
    fn start_job(app: &mut App) {
        let start = std::time::Instant::now();
        app.set_now(start);
        app.update(Input::Runtime(RuntimeEvent::BackgroundJobStarted {
            job_id: "job-1".to_string(),
            command: "make test".to_string(),
            output: "available through job_output".to_string(),
        }));
        app.set_now(start + std::time::Duration::from_secs(18 * 60));
    }

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
