//! The startup screen: the leading block of entries, the `Ctrl-P` fold,
//! the safety lines that never fold, the Tip, and the wordmark's width fallback. Pure `App`
//! state on a `TestBackend`, so no pty is needed.

use radian_tui::app::{App, EntryKind};
use radian_tui::startup::{StartupScreen, TIPS, banner_rows, tip_for};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn ctrl(code: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL)
}

fn screen() -> StartupScreen {
    StartupScreen {
        banner: banner_rows("0.1.0", "mock-model", "~/Code/radian"),
        banner_narrow: "R radian 0.1.0".to_string(),
        trust_hint: None,
        summary: "workspace · macOS sandbox · context 1000 · memory off · ask_user on".to_string(),
        lines: vec![
            "profile: workspace".to_string(),
            "context: window 2000".to_string(),
        ],
        safety: vec!["workspace: ~/tmp (cwd is $HOME)".to_string()],
        tip: Some("Ctrl-F searches the transcript".to_string()),
        expanded: false,
    }
}

fn entry_texts(app: &App) -> Vec<String> {
    app.session
        .entries
        .iter()
        .map(|entry| entry.text.clone())
        .collect()
}

fn with_startup() -> App {
    let mut app = App::new("mock", "s");
    app.set_transcript_width(120);
    app.set_startup(screen());
    app
}

/// decision: the wordmark carries `untrusted project · /trust to trust
/// it` under it while the project has no trusted decision, and nothing once it is
/// trusted. Showing it always (or never) fails this test, so it is the mutation catcher
/// for both directions.
#[test]
fn the_trust_hint_shows_until_the_project_is_trusted() {
    use radian_host::trust::{Decision, Resolved, Setup, Source};
    use radian_tui::startup::trust_hint;
    use std::path::PathBuf;

    let untrusted = Setup::untrusted(&PathBuf::from("/work/example"), false);
    assert_eq!(
        trust_hint(&untrusted),
        Some("untrusted project · /trust to trust it".to_string())
    );
    let trusted = Setup {
        workspace: PathBuf::from("/work/example"),
        resolved: Resolved {
            decision: Decision::Trust,
            source: Source::Session,
        },
        needs_ask: false,
    };
    assert_eq!(trust_hint(&trusted), None);
}

/// The hint is its own entry directly under the wordmark rows, so it survives the
/// information fold; without it no such line exists.
#[test]
fn the_trust_hint_is_an_entry_under_the_wordmark() {
    let mut hinted = screen();
    hinted.trust_hint = Some("untrusted project · /trust to trust it".to_string());
    let mut app = App::new("mock", "s");
    app.set_transcript_width(120);
    app.set_startup(hinted);
    let texts = entry_texts(&app);
    let banner_len = radian_tui::startup::BANNER_WIDE.len();
    assert_eq!(
        texts[banner_len], "untrusted project · /trust to trust it",
        "{texts:?}"
    );
    assert_eq!(app.session.entries[banner_len].kind, EntryKind::Startup);

    let plain = with_startup();
    assert!(
        !entry_texts(&plain)
            .iter()
            .any(|text| text.contains("/trust to trust it")),
        "{:?}",
        entry_texts(&plain)
    );
}
/// The default is one collapsed line (plus the wordmark, safety and Tip); `Ctrl-P` expands
/// it to the full lines and folds it back.
#[test]
fn the_information_block_collapses_and_ctrl_p_toggles_it() {
    let mut app = with_startup();
    let collapsed = entry_texts(&app);
    assert!(
        collapsed
            .iter()
            .any(|text| text.starts_with("+ workspace · macOS sandbox")),
        "{collapsed:?}"
    );
    assert!(
        !collapsed
            .iter()
            .any(|text| text.contains("profile: workspace")),
        "the full lines are hidden while collapsed: {collapsed:?}"
    );

    app.on_key(ctrl('p'));
    let expanded = entry_texts(&app);
    assert!(
        expanded
            .iter()
            .any(|text| text.starts_with("- startup information")),
        "{expanded:?}"
    );
    assert!(
        expanded
            .iter()
            .any(|text| text.contains("profile: workspace")),
        "the full lines appear when expanded: {expanded:?}"
    );

    app.on_key(ctrl('p'));
    let back = entry_texts(&app);
    assert!(
        back.iter()
            .any(|text| text.starts_with("+ workspace · macOS sandbox")),
        "{back:?}"
    );
}

/// The safety lines are entries of their own: they survive a fold, and they carry the warning
/// kind.
#[test]
fn safety_lines_stay_visible_while_collapsed() {
    let mut app = with_startup();
    for expanded in [false, true] {
        if expanded {
            app.on_key(ctrl('p'));
        }
        let warnings: Vec<&str> = app
            .session
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Warning)
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(
            warnings,
            vec!["workspace: ~/tmp (cwd is $HOME)"],
            "expanded={expanded}"
        );
    }
}

/// The Tip is one entry with its own kind; with `tui.tips = false` the screen carries none.
#[test]
fn the_tip_is_one_entry_and_can_be_off() {
    let app = with_startup();
    let tips: Vec<&str> = app
        .session
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Tip)
        .map(|entry| entry.text.as_str())
        .collect();
    assert_eq!(tips, vec!["Ctrl-F searches the transcript"]);

    let mut off = screen();
    off.tip = None;
    let mut app = App::new("mock", "s");
    app.set_startup(off);
    assert!(
        !app.session
            .entries
            .iter()
            .any(|entry| entry.kind == EntryKind::Tip),
        "tui.tips = false shows no Tip"
    );
}

/// `tui.banner = false` shows no wordmark rows.
#[test]
fn the_banner_can_be_off() {
    let mut off = screen();
    off.banner.clear();
    let mut app = App::new("mock", "s");
    app.set_transcript_width(120);
    app.set_startup(off);
    assert!(
        !entry_texts(&app).iter().any(|text| text.contains('╭')),
        "{:?}",
        entry_texts(&app)
    );
}

/// Wide: the three wordmark rows, character for character. Narrow: the one-line fallback.
#[test]
fn the_banner_shows_three_rows_or_the_one_line_fallback() {
    let mut app = with_startup();
    let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, &mut app))
        .unwrap();
    let wide = buffer_text(terminal.backend().buffer());
    for row in radian_tui::startup::BANNER_WIDE {
        assert!(wide.contains(row), "missing {row:?} in:\n{wide}");
    }
    assert!(wide.contains("0.1.0"), "{wide}");

    // A narrow terminal (the wordmark is 21 columns plus the right column) drops to one line.
    let mut narrow_app = with_startup();
    let mut terminal = Terminal::new(TestBackend::new(24, 20)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, &mut narrow_app))
        .unwrap();
    let narrow = buffer_text(terminal.backend().buffer());
    assert!(
        narrow.contains("radian 0.1.0") && narrow.contains('R'),
        "the fallback is missing:\n{narrow}"
    );
    assert!(
        !narrow.contains('╭'),
        "no wordmark at 24 columns:\n{narrow}"
    );
}

/// The fallback line's width goes through `unicode-width` (ADR 0027). The unicode brand mark
/// is one column: revision 4 replaced the East-Asian-Wide `㎭` with `R`.
#[test]
fn the_banner_fallback_width_uses_unicode_width() {
    use unicode_width::UnicodeWidthStr;
    let mark = radian_tui::footer::brand_mark(radian_tui::footer::Glyphs::Unicode);
    assert_eq!(mark, "R");
    let line = format!("{mark} radian 0.1.0");
    assert_eq!(line.width(), 1 + " radian 0.1.0".width());
}

fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
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

/// Every tip names a feature that exists: a slash command on the tree, a key that is bound,
/// or a documented gesture. The structural check keeps the table honest.
#[test]
fn every_tip_names_a_real_feature() {
    use radian_tui::keys::Keymap;
    let tree = radian_tui::command::tree();
    let keymap = Keymap::default();
    for tip in TIPS {
        let first = tip.split_whitespace().next().expect("a non-empty tip");
        if let Some(command) = first.strip_prefix('/') {
            let name = command.trim_end_matches(|c: char| !c.is_alphanumeric());
            assert!(
                tree.iter().any(|entry| entry.name == name),
                "tip {tip:?} names /{name}, which is not on the command tree"
            );
        } else if let Some(chord) = first.strip_prefix("Ctrl-") {
            let key = KeyEvent::new(
                KeyCode::Char(chord.chars().next().unwrap().to_ascii_lowercase()),
                KeyModifiers::CONTROL,
            );
            assert!(
                keymap.action_for(&key).is_some(),
                "tip {tip:?} names {first}, which is not bound"
            );
        } else {
            assert!(
                // `!` is a real feature (ADR 0033): the input box's shell command.
                matches!(first, "@path" | "Shift+drag" | "!<command>"),
                "tip {tip:?} begins with {first:?}; add a real feature or allow it here"
            );
        }
    }
}

/// The tip is chosen from a hash of the session id: the same session resumes with the same
/// tip, and the choice is one of the table.
#[test]
fn the_tip_is_stable_per_session() {
    for id in ["session-a", "session-b", "abc123"] {
        let first = tip_for(id);
        assert_eq!(first, tip_for(id), "stable for {id}");
        assert!(TIPS.contains(&first), "{first:?} is in the table");
    }
}

/// Renders and returns the foreground of the first cell of `needle` and of the cell
/// immediately after it (0-based column scan).
fn fg_pair(app: &mut App, needle: &str) -> (ratatui::style::Color, ratatui::style::Color) {
    let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, app))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let area = *buffer.area();
    for y in area.top()..area.bottom() {
        let mut row = String::new();
        for x in area.left()..area.right() {
            row.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        if let Some(col) = row.find(needle) {
            let label = buffer.cell((col as u16, y)).map(|c| c.fg).unwrap();
            let body = buffer
                .cell(((col + needle.len()) as u16 + 1, y))
                .map(|c| c.fg)
                .unwrap();
            return (label, body);
        }
    }
    panic!("{needle:?} is not on screen");
}

/// The startup information's `label: value` lines follow blocks' `split_label` rule (ADR 0028,
/// / ADR 0029): the label is the accent role (`notice_label`) and
/// the value the secondary level (`notice_body`) — the same split a notice gets.
#[test]
fn startup_labels_use_the_notice_label_and_body_levels() {
    let mut app = with_startup();
    // Expand: the full lines carry the `label: value` pairs.
    app.on_key(ctrl('p'));
    let (label_fg, body_fg) = fg_pair(&mut app, "profile:");
    assert_eq!(
        label_fg,
        app.theme.notice_label.fg.expect("notice_label fg"),
        "the label is the accent role"
    );
    assert_eq!(
        body_fg,
        app.theme.notice_body.fg.expect("notice_body fg"),
        "the value is the secondary level"
    );
    assert_ne!(
        label_fg, body_fg,
        "the label and the value are distinguishable"
    );
}
