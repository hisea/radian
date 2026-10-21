//! The thinking channel (ADR 0028): the `◻ thinking · <duration>`
//! header, the `│ ` rail with a single-colour Markdown body, the folded one-liner
//! `◼ thinking · <duration> · N lines`, and the duration that only an entry measured live
//! has (a resumed one shows none). The fold pair follows ADR 0035's 2026-10-16
//! "折叠标记统一" note (`◼`/`◻`, ascii `+`/`-`), shared with an expandable tool head.
//!
//! These assert the **rendered** frame (TestBackend), because the shapes are the point: the
//! marker column, the rail in columns 1-2, the body from column 3, and the one colour.

use radian_core::{AgentEvent, ContentPart, Message};
use radian_tui::app::{App, Entry, EntryKind};
use radian_tui::blocks::{self, ReasoningMode};
use radian_tui::projection::{SessionProjection, StreamKind};
use radian_tui::render;
use radian_tui::tools::{DisplayModes, ToolDisplay, ToolFold};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Modifier;
use std::time::{Duration, Instant};

/// Puts a test on the one-line status bar (`preset = classic`), so the transcript geometry is
/// the same as the pre-deck tests.
fn classic_footer(app: &mut App) {
    app.apply_footer(&radian_host::config::TuiFooterConfig {
        preset: Some("classic".to_string()),
        ..Default::default()
    });
}

/// The display modes the other reasoning tests render but assert only counts with.
fn thinking_modes(reasoning: ReasoningMode) -> DisplayModes {
    DisplayModes {
        reasoning,
        tools: ToolDisplay {
            fold: ToolFold::Expanded,
            preview_lines: 5,
        },
        mermaid: true,
        highlight: true,
        glyphs: radian_tui::footer::Glyphs::Unicode,
    }
}

fn render(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
}

fn screen(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let symbol = buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(symbol);
        }
        out.push('\n');
    }
    out
}

/// Row `y` as a string, trailing cells kept so the rail and the body columns are visible.
fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    (area.left()..area.right())
        .map(|x| {
            buffer
                .cell((x, y))
                .map(|c| c.symbol())
                .unwrap_or(" ")
                .to_string()
        })
        .collect()
}

fn row_of(terminal: &Terminal<TestBackend>, needle: &str) -> Option<usize> {
    let area = *terminal.backend().buffer().area();
    (area.top()..area.bottom())
        .find(|y| row(terminal, *y).contains(needle))
        .map(usize::from)
}

fn cell_style(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> ratatui::style::Style {
    terminal
        .backend()
        .buffer()
        .cell((x, y))
        .map(|cell| cell.style())
        .expect("a cell inside the buffer")
}

fn style_of(terminal: &Terminal<TestBackend>, needle: &str) -> Option<ratatui::style::Style> {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    let mut line = String::new();
    for y in area.top()..area.bottom() {
        line.clear();
        for x in area.left()..area.right() {
            line.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        if let Some(col) = line.find(needle) {
            let x = area.left() + unicode_width::UnicodeWidthStr::width(&line[..col]) as u16;
            return buffer.cell((x, y)).map(|c| c.style());
        }
    }
    None
}

fn assistant_with(content: &str, reasoning: &str) -> Message {
    Message::Assistant {
        content: if content.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(content)]
        },
        tool_calls: Vec::new(),
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
        reasoning: if reasoning.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(reasoning)]
        },
    }
}

/// A resumed reasoning entry: pushed straight into the transcript, so it never had a live
/// view and (the point) no wall clock on it.
fn resumed(app: &mut App, reasoning: &str) {
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: assistant_with("", reasoning),
    });
}

/// One streaming chunk the way the loop does it (projection then snapshot).
fn stream(app: &mut App, projection: &SessionProjection, kind: StreamKind, text: &str) {
    projection.push_delta(kind, text);
    app.update(radian_tui::app::Input::Stream(projection.snapshot()));
}

/// The expanded header is `◻ thinking · …` and every body row carries the `│ ` rail in
/// columns 1-2, at 80 and 40 columns.
#[test]
fn the_expanded_thinking_has_a_header_and_a_rail() {
    for width in [80u16, 40] {
        let mut app = App::new("m", "s");
        classic_footer(&mut app);
        app.reasoning_mode = ReasoningMode::Expanded;
        resumed(&mut app, "first thought\nsecond thought");
        let terminal = render(&mut app, width, 12);
        let text = screen(&terminal);
        let header = row_of(&terminal, "◻ thinking").expect("the expanded header");
        assert!(
            row(&terminal, header as u16).starts_with("◻ thinking"),
            "the header sits in the marker column: {text}"
        );
        let body = row_of(&terminal, "first thought").expect("the body");
        assert!(
            row(&terminal, body as u16).starts_with("│ first thought"),
            "the body starts with the rail in columns 1-2 and the text at column 3: {text}"
        );
    }
}

/// Folded, the channel is one line: `◼ thinking · N lines` (the tier's fold marker), and the
/// body is not drawn.
#[test]
fn the_folded_thinking_is_one_line_with_the_line_count() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    resumed(&mut app, "first thought\nsecond thought");
    let text = screen(&render(&mut app, 80, 10));
    assert!(text.contains("◼ thinking · 2 lines"), "{text}");
    assert!(!text.contains("first thought"), "{text}");
}

/// Streaming shows the spinner frame, never a duration and never the line count.
#[test]
fn streaming_shows_the_spinner_not_the_duration() {
    let base = Instant::now();
    for mode in [ReasoningMode::Expanded, ReasoningMode::Collapsed] {
        let mut app = App::new("m", "s");
        classic_footer(&mut app);
        app.reasoning_mode = mode;
        app.set_now(base);
        let projection = SessionProjection::new();
        stream(&mut app, &projection, StreamKind::Reasoning, "a thought");
        app.set_now(base);
        let glyph = app.spinner_frame().glyph;
        let text = screen(&render(&mut app, 80, 10));
        let marker = if mode == ReasoningMode::Expanded {
            "◻"
        } else {
            "◼"
        };
        assert!(
            text.contains(&format!("{marker} thinking · {glyph}")),
            "{mode:?}: the header carries the spinner frame: {text}"
        );
        assert!(!text.contains("lines"), "{mode:?}: {text}");
    }
}

/// The duration appears for an entry measured live, and not for a resumed one (ADI 0028
/// revision 5 decision 4). Driven through the app's clock seam: no sleeps.
#[test]
fn the_duration_is_shown_for_a_live_entry_and_absent_for_a_resumed_one() {
    let base = Instant::now();
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.reasoning_mode = ReasoningMode::Expanded;
    app.set_now(base);
    let projection = SessionProjection::new();
    stream(&mut app, &projection, StreamKind::Reasoning, "a thought");
    // The commit is the entry's end (the live view is replaced by the committed message).
    app.set_now(base + Duration::from_millis(3200));
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: assistant_with("", "a thought"),
    });
    let text = screen(&render(&mut app, 80, 10));
    assert!(
        text.contains("◻ thinking · 3.2s"),
        "a live entry keeps its measured duration: {text}"
    );

    // A resumed entry never had a live view: no duration, just the plain header.
    let mut resumed_app = App::new("m", "s");
    classic_footer(&mut resumed_app);
    resumed_app.reasoning_mode = ReasoningMode::Expanded;
    resumed(&mut resumed_app, "a thought");
    let terminal = render(&mut resumed_app, 80, 10);
    let text = screen(&terminal);
    let header = row_of(&terminal, "thinking").expect("the resumed header");
    assert_eq!(
        row(&terminal, header as u16).trim_end(),
        "◻ thinking",
        "a resumed entry has no duration: {text}"
    );
    assert!(
        !text.contains("3.2s") && !text.contains("0.0s"),
        "a resumed entry has no duration: {text}"
    );
}

/// The body is Markdown laid out in the reasoning colour: `**bold**` loses its asterisks and
/// keeps the BOLD attribute, never another role's colour.
#[test]
fn the_thinking_body_is_single_colour_markdown() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.reasoning_mode = ReasoningMode::Expanded;
    resumed(&mut app, "# a heading\n\nplain **bold** and `code` text");
    let terminal = render(&mut app, 80, 12);
    let text = screen(&terminal);
    assert!(!text.contains("**"), "no markup is shown: {text}");
    assert!(!text.contains('`'), "no backticks are shown: {text}");
    let bold = style_of(&terminal, "bold").expect("the bold word");
    assert!(
        bold.add_modifier.contains(Modifier::BOLD),
        "the bold attribute survives: {bold:?}"
    );
    assert_eq!(bold.fg, app.theme.reasoning.fg, "in the reasoning colour");
    assert!(
        !bold.add_modifier.contains(Modifier::ITALIC),
        "the body is not italic (ADR 0029): {bold:?}"
    );
}

/// No body cell carries a role other than `reasoning`: a Markdown body with a heading, a
/// list, a quote, a code block and a table still has one colour (the `│ ` rail in columns
/// 1-2 is the `frame` role, not a body cell).
#[test]
fn every_thinking_body_cell_is_the_reasoning_colour() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.reasoning_mode = ReasoningMode::Expanded;
    resumed(
        &mut app,
        "# heading\n\nplain **bold** `code` [link](https://x)\n\n- item one\n- item two\n\n> quoted\n\n```rust\nlet x = 1;\n```\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n",
    );
    let terminal = render(&mut app, 80, 40);
    let reasoning = app.theme.reasoning.fg;
    let area = *terminal.backend().buffer().area();
    let mut body_rows = 0;
    for y in area.top()..area.bottom() {
        let line = row(&terminal, y);
        if !line.starts_with("│ ") {
            continue;
        }
        body_rows += 1;
        for x in 2..area.right() {
            let ch = terminal
                .backend()
                .buffer()
                .cell((x, y))
                .map(|c| c.symbol().to_string())
                .unwrap_or_default();
            if ch.trim().is_empty() {
                continue;
            }
            assert_eq!(
                cell_style(&terminal, x, y).fg,
                reasoning,
                "row {y} col {x} ({ch:?}) is not the reasoning colour"
            );
        }
    }
    assert!(body_rows > 5, "the body was drawn: {body_rows} rows");
}

/// ADR 0035's 2026-10-16 "折叠标记统一" note: the thinking header carries the same `◼`/`◻` pair
/// as an expandable tool head, at 80 columns, in unicode and ascii.
#[test]
fn the_thinking_fold_marker_is_the_shared_pair() {
    for (tier, folded, open) in [
        ("unicode", "◼ thinking · 2 lines", "◻ thinking"),
        ("ascii", "+ thinking · 2 lines", "- thinking"),
    ] {
        let mut app = App::new("m", "s");
        app.apply_footer(&radian_host::config::TuiFooterConfig {
            glyphs: Some(tier.to_string()),
            preset: Some("classic".to_string()),
            ..Default::default()
        });
        resumed(&mut app, "first thought\nsecond thought");
        let text = screen(&render(&mut app, 80, 10));
        assert!(text.contains(folded), "{tier}: folded: {text}");
        assert!(
            !text.contains("first thought"),
            "{tier}: folded hides the body: {text}"
        );

        app.reasoning_mode = ReasoningMode::Expanded;
        let text = screen(&render(&mut app, 80, 12));
        assert!(text.contains(open), "{tier}: expanded: {text}");
        assert!(
            text.contains("first thought"),
            "{tier}: expanded shows the body: {text}"
        );
    }
}

/// Under `ascii`, the thinking markers are `-`/`+` and the rail is `|` (ADR 0035's 2026-10-16
/// "折叠标记统一" note: `◼`/`◻`→`+`/`-`, `│→|`; it supersedes revision 6 decision 11's `▼`/`▶`→
/// `v`/`>`).
#[test]
fn the_thinking_markers_follow_the_ascii_glyph_tier() {
    let mut app = App::new("m", "s");
    app.apply_footer(&radian_host::config::TuiFooterConfig {
        glyphs: Some("ascii".to_string()),
        preset: Some("classic".to_string()),
        ..Default::default()
    });
    app.reasoning_mode = ReasoningMode::Expanded;
    resumed(&mut app, "a thought");
    let terminal = render(&mut app, 80, 10);
    let text = screen(&terminal);
    assert!(text.contains("- thinking"), "{text}");
    let body = row_of(&terminal, "a thought").expect("the body");
    assert!(row(&terminal, body as u16).starts_with("| "), "{text}");

    let mut folded = App::new("m", "s");
    folded.apply_footer(&radian_host::config::TuiFooterConfig {
        glyphs: Some("ascii".to_string()),
        preset: Some("classic".to_string()),
        ..Default::default()
    });
    resumed(&mut folded, "a thought");
    let text = screen(&render(&mut folded, 80, 10));
    assert!(text.contains("+ thinking · 1 lines"), "{text}");
}

/// A fenced code block in the thinking body draws the same open frame as a reply (ADR 0035
/// ): the `╭─ <lang>` top line, the `│ ` rail on the code rows and the
/// `╰──` bottom line — and the counted lines equal the drawn ones at two widths.
#[test]
fn a_thinking_code_block_draws_the_open_frame_and_counts_the_drawn_lines() {
    let source = "before the block\n\n```rust\nlet x = 1;\n```";
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.reasoning_mode = ReasoningMode::Expanded;
    resumed(&mut app, source);
    let terminal = render(&mut app, 60, 16);
    let text = screen(&terminal);
    assert!(
        text.contains("╭─ rust "),
        "the open frame's titled top line: {text}"
    );
    assert!(text.contains("╰──"), "the open frame's bottom line: {text}");
    let code = row_of(&terminal, "let x = 1;").expect("the code row");
    assert!(
        row(&terminal, code as u16).starts_with("│ let x = 1;"),
        "the code row keeps the frame's rail in columns 1-2: {text}"
    );

    // The counted lines equal the drawn ones: the mirror and the layout must agree, or the
    // transcript's total height (and the scroll position) drifts.
    let entry = &app.session.entries[0];
    assert_eq!(entry.kind, EntryKind::Reasoning);
    let parsed = blocks::parse_blocks(&entry.text);
    for width in [60usize, 24] {
        let modes = thinking_modes(ReasoningMode::Expanded);
        assert_eq!(
            blocks::line_count(entry, modes, width),
            blocks::layout(entry, &parsed, modes, width).len(),
            "width {width}"
        );
    }
}

/// The `~ ` marker of the old thinking channel is gone (ADR 0028).
#[test]
fn the_old_tilde_marker_is_gone() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.reasoning_mode = ReasoningMode::Expanded;
    resumed(&mut app, "a thought");
    let text = screen(&render(&mut app, 80, 10));
    assert!(!text.contains("~ thinking"), "{text}");
    assert!(!text.contains("~ a thought"), "{text}");
}

/// A marker/entry sanity check: `Entry` still carries the reasoning text unchanged (the
/// display is a view; nothing is cut).
#[test]
fn the_reasoning_text_is_kept_for_the_projection() {
    let entry = Entry::new(EntryKind::Reasoning, "raw **text**");
    assert_eq!(entry.text, "raw **text**");
}
