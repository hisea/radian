//! The transcript is virtualized (dogfooding D9): a frame wraps only the entries that overlap
//! the visible window, and every other entry contributes a cached line count. These tests hold
//! the virtualized window to the full, unvirtualized wrap, and hold the scroll position past
//! the 65,535 lines a `u16` could count.

use agent_core::AgentEvent;
use radian_tui::app::App;
use radian_tui::render::{self, transcript_lines, visible_lines};
use radian_tui::theme::Theme;
use radian_tui::{Entry, EntryKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::text::Line;

use radian_tui::blocks::ReasoningMode;
use radian_tui::tools::{DisplayModes, ToolDisplay, ToolFold};

/// Both folds expanded, so a test's geometry does not depend on the fold defaults.
const EXPANDED: DisplayModes = DisplayModes {
    reasoning: ReasoningMode::Expanded,
    tools: ToolDisplay {
        fold: ToolFold::Expanded,
        preview_lines: 5,
    },
    mermaid: true,
    highlight: true,
    glyphs: radian_tui::footer::Glyphs::Unicode,
};

fn text(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

/// Entries of every shape the transcript renders: short, long enough to wrap many times, CJK,
/// multi-paragraph, interrupted, and every kind.
fn mixed_entries() -> Vec<Entry> {
    let mut entries = Vec::new();
    for i in 0..40 {
        let (kind, body) = match i % 6 {
            0 => (EntryKind::User, format!("question {i}")),
            1 => (EntryKind::Assistant, "word ".repeat(40 + i)),
            2 => (EntryKind::Reasoning, "思考中文内容".repeat(7)),
            3 => (
                EntryKind::ToolCall,
                format!("shell {{\"command\": \"ls {i}\"}}"),
            ),
            4 => (
                EntryKind::ToolResult,
                format!("line a\nline b\n{}", "x".repeat(90)),
            ),
            _ => (EntryKind::Notice, String::new()),
        };
        let mut entry = Entry::new(kind, body);
        entry.interrupted = i % 13 == 1;
        entries.push(entry);
    }
    entries
}

#[test]
fn every_window_matches_the_full_wrap() {
    for width in [12usize, 31, 80] {
        let mut entries = mixed_entries();
        let full = text(&transcript_lines(
            &entries,
            EXPANDED,
            width,
            &Theme::default(),
        ));
        for height in [1usize, 5, 17] {
            for start in 0..=full.len() {
                let (total, window) = visible_lines(
                    &mut entries,
                    EXPANDED,
                    width,
                    start,
                    height,
                    &Theme::default(),
                );
                assert_eq!(total, full.len(), "width {width}");
                let end = (start + height).min(full.len());
                assert_eq!(
                    text(&window),
                    full[start..end].to_vec(),
                    "width {width}, height {height}, start {start}"
                );
            }
        }
    }
}

/// A live entry grows between frames; its cached count must not survive the growth.
#[test]
fn a_growing_entry_is_measured_again() {
    let width = 20;
    let mut entries = mixed_entries();
    let _ = visible_lines(&mut entries, EXPANDED, width, 0, 10, &Theme::default());
    let last = entries.len() - 1;
    entries[last].text.push_str(&" more words".repeat(30));
    let full = text(&transcript_lines(
        &entries,
        EXPANDED,
        width,
        &Theme::default(),
    ));
    let (total, window) = visible_lines(
        &mut entries,
        EXPANDED,
        width,
        full.len() - 10,
        10,
        &Theme::default(),
    );
    assert_eq!(total, full.len(), "the total must include the growth");
    assert_eq!(text(&window), full[full.len() - 10..].to_vec());
}

/// A changed width (a resize) must re-measure, not reuse the old width's counts.
#[test]
fn a_resize_is_measured_again() {
    let mut entries = mixed_entries();
    let _ = visible_lines(&mut entries, EXPANDED, 80, 0, 10, &Theme::default());
    let (total, _) = visible_lines(&mut entries, EXPANDED, 15, 0, 10, &Theme::default());
    assert_eq!(
        total,
        transcript_lines(&entries, EXPANDED, 15, &Theme::default()).len()
    );
}

/// Past 65,535 wrapped lines the scroll position, the bottom and the visible text stay right.
/// With a `u16` line count the total wrapped and the bottom of the transcript was unreachable.
#[test]
fn a_transcript_past_u16_lines_scrolls_to_its_real_bottom() {
    let mut app = App::new("m", "s");
    // 700 messages of 100 lines each: 70,000 lines, past u16::MAX.
    let block = vec!["row"; 100].join("\n");
    for i in 0..700 {
        app.apply_event(&AgentEvent::TranscriptAppended {
            index: i,
            message: agent_core::Message::user(format!("{block}\nEND-{i}")),
        });
    }
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    assert!(
        app.session.total_lines > usize::from(u16::MAX),
        "the fixture must pass u16::MAX lines: {}",
        app.session.total_lines
    );
    assert_eq!(
        app.session.scroll, app.session.max_scroll,
        "following the bottom"
    );
    let screen: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        screen.contains("END-699"),
        "the last message is visible at the bottom"
    );
}
