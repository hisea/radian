//! Whole-screen snapshots of the `/context` grid view (spec `tui-context-grid`,
//! prototype B): the 10 × 20 grid beside the legend and the detail below, at
//! 120 and 80 columns under `dark`/`light`, plus the `ascii` tier. Same style
//! as `tests/thin.rs`: plain-text frames with the styling rules asserted on
//! the buffer beside them.

use radian_host::context_estimate::{Category, ContextReport, Details, FileDetail};
use radian_tui::app::{App, Input};
use radian_tui::footer::Glyphs;
use radian_tui::render;
use radian_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

/// The `lines` input area (rule + row + rule) and the one-row `classic` footer.
const CHROME: u16 = 4;

fn sample_report() -> ContextReport {
    let categories = [
        ("system prompt", 1_900),
        ("context files", 3_600),
        ("memory", 800),
        ("tool definitions", 2_900),
        ("skills", 700),
        ("MCP tools", 1_400),
        ("user messages", 4_100),
        ("assistant text", 9_800),
        ("tool results", 36_100),
    ];
    ContextReport {
        provider: Some("opencode".to_owned()),
        model: Some("muse-spark-1.3-contributor".to_owned()),
        categories: categories
            .iter()
            .map(|(name, tokens)| Category {
                name,
                tokens: *tokens,
                share: 0.0,
            })
            .collect(),
        total_tokens: 61_300,
        window: Some(200_000),
        threshold: Some(180_000),
        free_tokens: Some(118_700),
        buffer_tokens: Some(20_000),
        reported: Some(59_270),
        message_count: 58,
        compactions: 1,
        details: Details {
            context_files: vec![FileDetail {
                path: "AGENTS.md".to_owned(),
                tokens: 3_600,
            }],
            context_files_tokens: 3_600,
            skills_count: 9,
            skills_tokens: 700,
            builtin_tools_count: 12,
            builtin_tools_tokens: 2_900,
            mcp_servers: 1,
            mcp_tools_count: 16,
            mcp_tools_tokens: 1_400,
            memory_records: 4,
            memory_tokens: 800,
        },
    }
}

fn app_with_context(report: &ContextReport, all: bool, glyphs: Glyphs) -> App {
    let mut app = App::new("m", "s");
    app.apply_footer(&radian_host::config::TuiFooterConfig {
        preset: Some("classic".to_string()),
        glyphs: Some(
            match glyphs {
                Glyphs::Unicode => "unicode",
                Glyphs::Nerd => "nerd",
                Glyphs::Ascii => "ascii",
                Glyphs::Emoji => "emoji",
            }
            .to_string(),
        ),
        ..Default::default()
    });
    app.update(Input::ContextReady {
        report: Box::new(report.clone()),
        all,
    });
    app
}

fn render(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
}

fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    (area.top()..area.bottom())
        .map(|y| {
            (area.left()..area.right())
                .map(|x| {
                    buffer
                        .cell((x, y))
                        .map(|c| c.symbol())
                        .unwrap_or(" ")
                        .to_string()
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// The transcript region with the blank filler rows dropped.
fn transcript(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut rows = rows(&render(app, width, height));
    rows.truncate(usize::from(height - CHROME));
    while rows.last().is_some_and(|row| row.is_empty()) {
        rows.pop();
    }
    let first = rows
        .iter()
        .position(|row| !row.is_empty())
        .unwrap_or(rows.len());
    rows.drain(..first);
    rows
}

fn expect_transcript(app: &mut App, width: u16, height: u16, golden: &[&str]) {
    let got = transcript(app, width, height);
    let want: Vec<String> = golden.iter().map(|row| (*row).to_string()).collect();
    assert_eq!(
        got,
        want,
        "\n--- got ({width}x{height}) ---\n{}\n--- want ---\n{}",
        got.join("\n"),
        want.join("\n")
    );
}

/// The 80-column layout: the legend stacks under the grid.
const GOLDEN_80: &[&str] = &[
    "* ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼",
    "  ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼",
    "  ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼",
    "  ◼ ◼ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻",
    "  ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻",
    "  ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻",
    "  ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻",
    "  ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻",
    "  ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻",
    "  ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨",
    "  muse-spark-1.3-contributor",
    "  opencode",
    "  61K / 200K tokens (30.6%) estimated",
    "  59K input tokens  reported, last request",
    "",
    "  estimated by category",
    "  ◼ system prompt: 1.9K (0.9%)",
    "  ◼ context files: 3.6K (1.8%)",
    "  ◼ memory: 800 (0.4%)",
    "  ◼ tool definitions: 2.9K (1.4%)",
    "  ◼ skills: 700 (0.3%)",
    "  ◼ MCP tools: 1.4K (0.7%)",
    "  ◼ user messages: 4.1K (2.0%)",
    "  ◼ assistant text: 9.8K (4.9%)",
    "  ◼ tool results: 36K (18.1%)",
    "  ◻ free: 118K (59.4%)",
    "  ▨ compaction buffer: 20K (10.0%)",
    "",
    "  context files",
    "  AGENTS.md · 3.6K",
    "",
    "  MCP · /mcp",
    "  1 server · 16 tools · 1.4K",
    "",
    "  skills · /skills",
    "  9 skills · 700",
    "",
    "  memory · /memory",
    "  4 records · 800",
    "",
    "  58 messages · 1 compaction this session",
];

/// The 120-column layout: the grid beside the legend, detail below.
const GOLDEN_120: &[&str] = &[
    "* ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼   muse-spark-1.3-contributor",
    "  ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼   opencode",
    "  ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼ ◼   61K / 200K tokens (30.6%) estimated",
    "  ◼ ◼ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻   59K input tokens  reported, last request",
    "  ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻",
    "  ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻   estimated by category",
    "  ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻   ◼ system prompt: 1.9K (0.9%)",
    "  ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻   ◼ context files: 3.6K (1.8%)",
    "  ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻ ◻   ◼ memory: 800 (0.4%)",
    "  ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨ ▨   ◼ tool definitions: 2.9K (1.4%)",
    "                                            ◼ skills: 700 (0.3%)",
    "                                            ◼ MCP tools: 1.4K (0.7%)",
    "                                            ◼ user messages: 4.1K (2.0%)",
    "                                            ◼ assistant text: 9.8K (4.9%)",
    "                                            ◼ tool results: 36K (18.1%)",
    "                                            ◻ free: 118K (59.4%)",
    "                                            ▨ compaction buffer: 20K (10.0%)",
    "",
    "  context files",
    "  AGENTS.md · 3.6K",
    "",
    "  MCP · /mcp",
    "  1 server · 16 tools · 1.4K",
    "",
    "  skills · /skills",
    "  9 skills · 700",
    "",
    "  memory · /memory",
    "  4 records · 800",
    "",
    "  58 messages · 1 compaction this session",
];

/// The ascii tier draws its own glyph set.
const GOLDEN_ASCII: &[&str] = &[
    "* # # # # # # # # # # # # # # # # # # # #",
    "  # # # # # # # # # # # # # # # # # # # #",
    "  # # # # # # # # # # # # # # # # # # # #",
    "  # # - - - - - - - - - - - - - - - - - -",
    "  - - - - - - - - - - - - - - - - - - - -",
    "  - - - - - - - - - - - - - - - - - - - -",
    "  - - - - - - - - - - - - - - - - - - - -",
    "  - - - - - - - - - - - - - - - - - - - -",
    "  - - - - - - - - - - - - - - - - - - - -",
    "  = = = = = = = = = = = = = = = = = = = =",
    "  muse-spark-1.3-contributor",
    "  opencode",
    "  61K / 200K tokens (30.6%) estimated",
    "  59K input tokens  reported, last request",
    "",
    "  estimated by category",
    "  # system prompt: 1.9K (0.9%)",
    "  # context files: 3.6K (1.8%)",
    "  # memory: 800 (0.4%)",
    "  # tool definitions: 2.9K (1.4%)",
    "  # skills: 700 (0.3%)",
    "  # MCP tools: 1.4K (0.7%)",
    "  # user messages: 4.1K (2.0%)",
    "  # assistant text: 9.8K (4.9%)",
    "  # tool results: 36K (18.1%)",
    "  - free: 118K (59.4%)",
    "  = compaction buffer: 20K (10.0%)",
    "",
    "  context files",
    "  AGENTS.md · 3.6K",
    "",
    "  MCP · /mcp",
    "  1 server · 16 tools · 1.4K",
    "",
    "  skills · /skills",
    "  9 skills · 700",
    "",
    "  memory · /memory",
    "  4 records · 800",
    "",
    "  58 messages · 1 compaction this session",
];

/// The grid beside the legend with the detail below, at both widths and both
/// palettes (the text is the same under dark and light; only the colors move).
/// At 120 columns the legend sits beside the grid; at 80 it stacks below it.
#[test]
fn the_grid_snapshot_at_120_80_dark_light() {
    for theme in [Theme::dark(), Theme::light()] {
        let report = sample_report();
        let mut app = app_with_context(&report, false, Glyphs::Unicode);
        app.theme = theme.clone();
        expect_transcript(&mut app, 120, 48, GOLDEN_120);
        let mut app = app_with_context(&report, false, Glyphs::Unicode);
        app.theme = theme;
        expect_transcript(&mut app, 80, 48, GOLDEN_80);
    }
}

/// The ascii tier draws its own glyph set.
#[test]
fn the_grid_snapshot_ascii() {
    let report = sample_report();
    let mut app = app_with_context(&report, false, Glyphs::Ascii);
    app.theme = Theme::mono();
    expect_transcript(&mut app, 80, 48, GOLDEN_ASCII);
}

/// The grid's colors come from the category roles (ADR 0035): the first grid
/// cell is the system prompt's color, the buffer row the buffer's, and the
/// free legend glyph the frame line color.
#[test]
fn the_grid_cells_carry_their_category_roles() {
    let report = sample_report();
    let mut app = app_with_context(&report, false, Glyphs::Unicode);
    let theme = Theme::dark();
    app.theme = theme.clone();
    let terminal = render(&mut app, 120, 48);
    let buffer = terminal.backend().buffer();
    let cell = |x: u16, y: u16| buffer.cell((x, y)).expect("a cell").clone();
    // The transcript starts at row 0; the first body row's first grid cell is
    // the first category (system prompt). Marker `* ` occupies columns 0-1.
    assert_eq!(cell(0, 0).symbol(), "*");
    assert_eq!(cell(2, 0).symbol(), "◼");
    assert_eq!(
        cell(2, 0).fg,
        theme.ctx_system.fg.expect("system has a color")
    );
    // The second cell is the same category (2.9 → wait: allocation gives
    // system prompt 2 cells), still the system color.
    assert_eq!(cell(4, 0).symbol(), "◼");
    assert_eq!(
        cell(4, 0).fg,
        theme.ctx_system.fg.expect("system has a color")
    );
    // The last grid row is the compaction buffer.
    let grid_last_row = 9u16;
    assert_eq!(cell(2, grid_last_row).symbol(), "▨");
    assert_eq!(
        cell(2, grid_last_row).fg,
        theme.ctx_buffer.fg.expect("buffer has a color")
    );
    // No transcript cell carries a background (the thin rule).
    for y in 0..44u16 {
        for x in 0..120u16 {
            assert_eq!(
                cell(x, y).bg,
                Color::Reset,
                "row {y} col {x} carries a background"
            );
        }
    }
}

/// Too narrow for side-by-side: the legend stacks under the grid.
#[test]
fn a_narrow_terminal_stacks_the_legend_under_the_grid() {
    let report = sample_report();
    let mut app = app_with_context(&report, false, Glyphs::Unicode);
    let lines = transcript(&mut app, 60, 60);
    // Grid rows come first (10 rows of 20 cells), then the legend.
    assert_eq!(lines.len(), 10 + 17 + 1 + 13, "{}", lines.join("\n"));
    assert!(lines[0].starts_with("* ◼"), "{}", lines[0]);
    assert!(
        lines[10].contains("muse-spark-1.3-contributor"),
        "{}",
        lines[10]
    );
    assert!(
        lines.iter().all(|line| line.chars().count() <= 60),
        "nothing overflows"
    );
}

/// Too narrow for the grid at all: the plain-text fallback.
#[test]
fn a_very_narrow_terminal_falls_back_to_plain_text() {
    let report = sample_report();
    let mut app = app_with_context(&report, false, Glyphs::Unicode);
    let lines = transcript(&mut app, 30, 60);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("context (estimated):")),
        "{}",
        lines.join("\n")
    );
    assert!(
        lines
            .iter()
            .all(|line| !line.contains('◼') && !line.contains('▨')),
        "no grid art: {}",
        lines.join("\n")
    );
}

/// `/context all` lists every context file; the default truncates.
#[test]
fn context_all_lists_every_file() {
    let mut report = sample_report();
    for i in 0..10 {
        report.details.context_files.push(FileDetail {
            path: format!("file{i}.md"),
            tokens: 100,
        });
    }
    let short = transcript(
        &mut app_with_context(&report, false, Glyphs::Unicode),
        120,
        60,
    );
    assert!(
        short
            .iter()
            .any(|line| line.contains("+3 more · /context all")),
        "{}",
        short.join("\n")
    );
    let full = transcript(
        &mut app_with_context(&report, true, Glyphs::Unicode),
        120,
        60,
    );
    assert!(
        full.iter().any(|line| line.contains("file9.md")),
        "{}",
        full.join("\n")
    );
    assert!(
        full.iter()
            .all(|line| !line.contains("more · /context all")),
        "{}",
        full.join("\n")
    );
}
