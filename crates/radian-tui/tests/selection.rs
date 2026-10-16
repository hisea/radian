//! The transcript selection as pure `App::update` state (ADR 0023 U3): press/drag/release
//! produce the copy effect, a click clears, a press outside the transcript or on a modal
//! starts nothing, `tui.mouse = false` removes the whole feature, and the drag's auto-scroll
//! is one tick of app state.

use agent_core::Message;
use radian_host::{PendingRequest, PendingView};
use radian_permissions::ApprovalRequest;
use radian_tui::app::{AUTO_SCROLL_TICK, App, Effect, Input, UpdateResult};
use radian_tui::blocks::ReasoningMode;
use radian_tui::clip::MAX_CLIPBOARD_BYTES;
use radian_tui::tools::DisplayModes;
use serde_json::json;

fn approval() -> PendingView<PendingRequest> {
    PendingView {
        request_id: 1,
        origin: "session".into(),
        request: PendingRequest::Approval(ApprovalRequest::new(
            "perm-1",
            "call-1",
            "write",
            "/ws/out.txt",
            "/ws/out.txt",
            json!({}),
            None,
        )),
    }
}

/// A transcript at width 30. The thin user rows (ADR 0035: no padding rows) are one
/// line each and an ordinary block, so the container puts one blank line around them: 0 the
/// first user row, 1 the gap, 2 the assistant, 3 the gap, 4 the second user row.
const USER1: u16 = 0;
const ASSISTANT: u16 = 2;

fn app() -> App {
    let mut app = App::new("m", "s");
    app.push_message(&Message::user("hello there"));
    app.push_message(&Message::assistant("aaaa bbbb cccc dddd"));
    app.push_message(&Message::user("second message"));
    app.set_viewport(5, 5);
    app.set_transcript_width(30);
    app
}

fn press(app: &mut App, row: u16, col: u16) -> UpdateResult {
    app.update(Input::MouseDown { row, col })
}

fn drag(app: &mut App, row: u16, col: u16) -> UpdateResult {
    app.update(Input::MouseDrag { row, col })
}

fn release(app: &mut App, row: u16, col: u16) -> UpdateResult {
    app.update(Input::MouseUp { row, col })
}

#[test]
fn press_drag_release_produces_the_copy_effect() {
    let mut app = app();
    // Width 30 wraps nothing: selecting from the first user line through the assistant's
    // end copies both entries, joined by one newline.
    assert!(
        !press(&mut app, USER1, 2).dirty,
        "a press that starts nothing draws nothing"
    );
    let drag = drag(&mut app, ASSISTANT, 25);
    assert!(drag.dirty);
    assert!(app.session.selection.is_some(), "the drag is live");
    let release = release(&mut app, ASSISTANT, 25);
    assert!(release.dirty);
    assert_eq!(
        release.effects,
        vec![Effect::Copy {
            text: "hello there\naaaa bbbb cccc dddd".into()
        }],
        "one newline between entries"
    );
    // The selection stays visible until the next press.
    assert!(app.session.selection.is_some());
}

#[test]
fn a_copy_across_a_soft_wrap_inserts_no_newline() {
    // Width 12 → the assistant's body wraps "aaaa bbbb cccc dddd" into "aaaa bbbb" /
    // "cccc dddd" (lines 1 and 2). Selecting line 1 through line 2 copies the one-line text.
    let mut app = app();
    app.set_transcript_width(12);
    // Width 12: the first user row wraps to "hello ther" / "e" (rows 0-1), the gap is row 2,
    // and the assistant "aaaa bbbb" is row 3 / "cccc dddd" row 4. Selecting its two lines
    // copies the one-line text.
    press(&mut app, 3, 2);
    drag(&mut app, 4, 200);
    let release = release(&mut app, 4, 200);
    assert_eq!(
        release.effects,
        vec![Effect::Copy {
            text: "aaaa bbbb cccc dddd".into()
        }],
        "no newline at the wrap"
    );
}

#[test]
fn a_backwards_drag_normalizes_to_the_same_copy() {
    let mut app = app();
    press(&mut app, ASSISTANT, 25);
    drag(&mut app, USER1, 2);
    let release = release(&mut app, USER1, 2);
    assert_eq!(
        release.effects,
        vec![Effect::Copy {
            text: "hello there\naaaa bbbb cccc dddd".into()
        }]
    );
}

#[test]
fn a_click_clears_the_selection() {
    let mut app = app();
    press(&mut app, 0, 2);
    drag(&mut app, 2, 25);
    assert!(app.session.selection.is_some());
    // A second press on a different cell clears immediately (the same cell inside the
    // double-click window would be a word select, not a clear).
    assert!(press(&mut app, 2, 4).dirty);
    assert!(app.session.selection.is_none());
    // ...and its release (the click) copies nothing.
    let release = release(&mut app, 2, 4);
    assert!(release.effects.is_empty());
    assert!(app.session.selection.is_none());
}

#[test]
fn a_press_outside_the_transcript_never_starts_one() {
    let mut app = app();
    // Rows 5.. are the input box and the status bar (viewport height 5).
    assert!(!press(&mut app, 5, 0).dirty);
    assert!(app.session.selection.is_none());
    drag(&mut app, 6, 3);
    release(&mut app, 6, 3);
    assert!(app.session.selection.is_none(), "no gesture, no selection");
}

#[test]
fn a_press_on_a_modal_never_starts_one() {
    let mut app = app();
    app.update(Input::Pending(Some(Box::new(approval())), 1));
    // With the approval modal up the selection is off entirely: even a press inside the
    // shrunk transcript viewport (rows 0..2 are still transcript on screen) starts nothing —
    // the modal owns the input until it is answered.
    app.set_viewport(2, 5);
    app.session.scroll = 0;
    app.session.follow = false;
    assert!(!press(&mut app, 0, 5).dirty);
    assert!(app.session.selection.is_none());
    drag(&mut app, 0, 9);
    release(&mut app, 0, 9);
    assert!(
        app.session.selection.is_none(),
        "the drag would have selected real text"
    );
    // The modal's own rows are outside the viewport, doubly so.
    assert!(!press(&mut app, 4, 5).dirty);
    assert!(app.session.selection.is_none());
}

#[test]
fn a_press_on_the_picker_overlay_never_starts_one() {
    let mut app = app();
    let rows = vec![radian_host::sessions::SessionRow {
        session_id: "alpha".into(),
        created_ts: None,
        last_ts: None,
        message_count: 1,
        summary: "first".into(),
    }];
    app.push_overlay(radian_tui::app::Overlay::Picker(
        radian_tui::picker::Picker::new(rows, None, "/w"),
    ));
    assert!(!press(&mut app, 0, 0).dirty);
    assert!(app.session.selection.is_none());
}

#[test]
fn with_mouse_off_the_whole_feature_is_gone() {
    let mut config = radian_host::config::Config::default();
    config.tui.mouse = Some(false);
    let mut app = app();
    app.apply_config(&config, false).unwrap();
    assert!(
        app.clipboard == radian_tui::ClipboardMode::Osc52,
        "unrelated config still applies"
    );
    // Even a stray mouse event (which `tui.mouse = false` prevents upstream) does nothing.
    press(&mut app, 0, 2);
    drag(&mut app, 1, 7);
    let release = release(&mut app, 1, 7);
    assert!(app.session.selection.is_none());
    assert!(release.effects.is_empty());
}

/// The display modes most tests lay out with: both folds expanded, so a test's geometry does
/// not depend on the fold defaults.
#[allow(dead_code)]
const EXPANDED: DisplayModes = DisplayModes {
    reasoning: ReasoningMode::Expanded,
    tools: radian_tui::tools::ToolDisplay {
        fold: radian_tui::tools::ToolFold::Expanded,
        preview_lines: 5,
    },
    mermaid: true,
    highlight: true,
    glyphs: radian_tui::footer::Glyphs::Unicode,
};

#[test]
fn clipboard_config_selects_the_transport_and_pbcopy_falls_back_off_macos() {
    let mut app = app();
    let mut config = radian_host::config::Config::default();
    config.tui.clipboard = Some("off".into());
    app.apply_config(&config, false).unwrap();
    assert_eq!(app.clipboard, radian_tui::ClipboardMode::Off);
    // A release still ends the gesture — it just copies nothing.
    press(&mut app, 0, 2);
    drag(&mut app, 1, 7);
    let release = release(&mut app, 1, 7);
    assert!(release.effects.is_empty(), "`off` produces no copy effect");

    let mut config = radian_host::config::Config::default();
    config.tui.clipboard = Some("pbcopy".into());
    let warnings = app.apply_config(&config, false).unwrap();
    if cfg!(target_os = "macos") {
        assert_eq!(app.clipboard, radian_tui::ClipboardMode::PbCopy);
        assert!(warnings.is_empty(), "{warnings:?}");
    } else {
        assert_eq!(app.clipboard, radian_tui::ClipboardMode::Osc52);
        assert!(
            warnings.iter().any(|w| w.contains("pbcopy")),
            "{warnings:?}"
        );
    }
}

#[test]
fn an_oversized_selection_is_refused_with_the_reason() {
    let mut app = app();
    // One entry of a char-run over the cap: it wraps mid-run (the wrap breaks anywhere), so
    // the drag lands at a row **past the transcript**, which clamps to the last entry's end.
    app.session.entries.clear();
    app.push_message(&Message::assistant("z".repeat(MAX_CLIPBOARD_BYTES + 1)));
    app.set_transcript_width(30);
    let (total, _) =
        radian_tui::render::visible_lines(&mut app.session.entries, EXPANDED, 30, 0, 0, &app.theme);
    app.set_viewport(1, total);
    app.session.scroll = 0;
    app.session.follow = false;
    press(&mut app, 0, 2);
    drag(&mut app, u16::MAX, 79);
    let release = release(&mut app, u16::MAX, 79);
    assert!(release.effects.is_empty(), "nothing is copied");
    assert!(
        app.session.selection.is_none(),
        "the refused selection is dropped"
    );
    let notice = app.session.entries.last().expect("the reason is a notice");
    assert!(notice.text.contains("too large"), "{:?}", notice.text);
    assert!(
        notice.text.contains(&MAX_CLIPBOARD_BYTES.to_string()),
        "{:?}",
        notice.text
    );
}

#[test]
fn the_clipboard_result_is_an_honest_status_note() {
    let mut app = app();
    app.update(Input::ClipboardDone("copied 12 chars (via OSC 52)".into()));
    assert_eq!(
        app.session.note.as_deref(),
        Some("copied 12 chars (via OSC 52)")
    );
}

#[test]
fn the_drag_autoscrolls_at_the_edges_one_tick_at_a_time() {
    // A transcript much taller than the viewport, pinned to the bottom.
    let mut app = App::new("m", "s");
    for index in 0..40 {
        app.push_message(&Message::user(format!("line {index}")));
    }
    app.set_viewport(5, 40);
    app.scroll_to_bottom();
    app.set_transcript_width(80);
    // A drag whose pointer sits on the bottom edge (row 5 = past the viewport of 5 rows).
    // The view starts 10 lines above the bottom, so there is room to scroll down.
    app.scroll_by(-10);
    press(&mut app, 4, 0);
    drag(&mut app, 4, 0);
    assert!(!app.mouse_autoscroll(), "the pointer is not at an edge yet");
    drag(&mut app, 5, 0);
    assert!(app.is_selecting());
    assert!(
        app.mouse_autoscroll(),
        "held past the bottom, the view moves"
    );
    assert_eq!(
        app.session.scroll,
        app.session.max_scroll - 9,
        "one line per tick"
    );
    // Held at the top edge it scrolls back up.
    drag(&mut app, 0, 0);
    let mut ticks = 0;
    while app.session.scroll > 0 {
        assert!(
            app.mouse_autoscroll(),
            "held at the top, the view keeps moving"
        );
        ticks += 1;
        assert!(ticks < 100, "the ticks converge");
    }
    assert!(!app.mouse_autoscroll(), "at the very top nothing moves");
    // The head followed the pointer: the selection extends over the scrolled content.
    assert!(app.session.selection.is_some());
    let release = release(&mut app, 0, 0);
    assert!(
        matches!(release.effects.as_slice(), [Effect::Copy { text }] if !text.is_empty()),
        "the auto-scrolled selection still copies"
    );
    assert!(!app.is_selecting(), "the gesture ended");
}

#[test]
fn auto_scroll_only_ticks_while_a_drag_is_live() {
    let mut app = app();
    assert!(!app.mouse_autoscroll());
    app.update(Input::Pending(Some(Box::new(approval())), 1));
    assert!(
        !app.mouse_autoscroll(),
        "no drag, no tick — the loop stays in blocking recv"
    );
}

#[test]
fn the_tick_constant_is_bounded() {
    // 50 ms: ~20 lines/s, and only while a drag is live.
    assert_eq!(AUTO_SCROLL_TICK.as_millis(), 50);
}

#[test]
fn the_char_under_the_release_is_inside_the_selection() {
    let mut app = app();
    // "hello there": dragging from the 'h' (col 2) onto the 'e' at body column 8 (col 10)
    // copies "hello the" — the char under the pointer included.
    press(&mut app, USER1, 2);
    drag(&mut app, USER1, 10);
    let result = release(&mut app, USER1, 10);
    assert_eq!(
        result.effects,
        vec![Effect::Copy {
            text: "hello the".into()
        }]
    );
    // Backwards: dragging from the 'e' back onto the 'h' selects "hello the" — both
    // endpoint chars are inside, either direction.
    press(&mut app, USER1, 10);
    drag(&mut app, USER1, 2);
    let result = release(&mut app, USER1, 2);
    assert_eq!(
        result.effects,
        vec![Effect::Copy {
            text: "hello the".into()
        }]
    );
    // A release past the line's end stays a plain boundary (nothing is under the pointer):
    // "hello there" in full.
    press(&mut app, USER1, 2);
    drag(&mut app, USER1, 200);
    let result = release(&mut app, USER1, 200);
    assert_eq!(
        result.effects,
        vec![Effect::Copy {
            text: "hello there".into()
        }]
    );
    // Pressing on the `> ` marker selects from the first char (the drag to the margin
    // reaches through it).
    press(&mut app, USER1, 0);
    drag(&mut app, USER1, 1);
    let result = release(&mut app, USER1, 0);
    assert_eq!(result.effects, vec![Effect::Copy { text: "h".into() }]);
}

#[test]
fn a_drag_that_returns_to_its_press_cell_is_still_a_selection() {
    // The pointer left the press cell, so this is not a click: it comes back onto the same
    // char, and that one char is the selection.
    let mut app = app();
    press(&mut app, USER1, 2);
    drag(&mut app, USER1, 3);
    drag(&mut app, USER1, 2);
    let result = release(&mut app, USER1, 2);
    assert_eq!(result.effects, vec![Effect::Copy { text: "h".into() }]);
}

#[test]
fn a_double_click_selects_a_word_and_its_release_copies_it() {
    let mut app = app();
    // "hello there": double-click the 'h' of "there" (body col 6 → col 8).
    press(&mut app, USER1, 8);
    press(&mut app, USER1, 8);
    let selection = app.session.selection.expect("the word is selected");
    let (start, end) = selection.ordered();
    assert_eq!(
        (start.entry, start.offset, end.entry, end.offset),
        (0, 6, 0, 11)
    );
    let result = release(&mut app, USER1, 8);
    assert_eq!(
        result.effects,
        vec![Effect::Copy {
            text: "there".into()
        }]
    );
}

#[test]
fn a_triple_click_selects_the_whole_entry() {
    let mut app = app();
    for _ in 0..3 {
        press(&mut app, ASSISTANT, 5);
    }
    let result = release(&mut app, ASSISTANT, 5);
    assert_eq!(
        result.effects,
        vec![Effect::Copy {
            text: "aaaa bbbb cccc dddd".into()
        }],
        "the whole message, prefixes excluded"
    );
}

#[test]
fn a_click_sequence_on_different_cells_is_three_single_clicks() {
    let mut app = app();
    press(&mut app, USER1, 8);
    press(&mut app, USER1, 9);
    press(&mut app, USER1, 10);
    assert!(
        app.session.selection.is_none(),
        "no word got selected: not one cell"
    );
}

#[test]
fn a_drag_after_a_double_click_extends_from_the_word() {
    let mut app = app();
    press(&mut app, USER1, 8);
    press(&mut app, USER1, 8);
    drag(&mut app, ASSISTANT, 200);
    let result = release(&mut app, ASSISTANT, 200);
    assert_eq!(
        result.effects,
        vec![Effect::Copy {
            text: "there\naaaa bbbb cccc dddd".into()
        }],
        "the drag extends the double-clicked word"
    );
}

#[test]
fn a_double_click_on_whitespace_selects_the_whitespace_run() {
    let mut app = app();
    press(&mut app, USER1, 7);
    press(&mut app, USER1, 7);
    let result = release(&mut app, USER1, 7);
    assert_eq!(
        result.effects,
        vec![Effect::Copy { text: " ".into() }],
        "the single space between the words"
    );
}

#[test]
fn a_selection_from_the_marker_copies_the_text_without_it() {
    // `❯ ` is drawn in the marker column, never body text (ADR 0035):
    // a drag that starts on the marker cell copies the users' words from the first char.
    let mut app = app();
    press(&mut app, USER1, 0); // the `❯ ` marker cell
    drag(&mut app, ASSISTANT, 25);
    let result = release(&mut app, ASSISTANT, 25);
    assert_eq!(
        result.effects,
        vec![Effect::Copy {
            text: "hello there\naaaa bbbb cccc dddd".into()
        }]
    );
}

/// The thin user row has no padding rows (ADR 0035), and the `❯ ` marker is drawn
/// in the marker column, never body text: a selection from the marker cell through a CJK body
/// copies the words alone and never indexes into a multi-byte offset.
#[test]
fn a_marker_selection_copies_no_marker_characters_with_cjk() {
    let mut app = App::new("m", "s");
    app.push_message(&Message::user("中文消息"));
    app.set_viewport(1, 1);
    app.set_transcript_width(30);
    // Row 0 is the whole row: the marker cells then the text from column 3.
    press(&mut app, 0, 0);
    drag(&mut app, 0, 20);
    let result = release(&mut app, 0, 20);
    assert_eq!(
        result.effects,
        vec![Effect::Copy {
            text: "中文消息".into()
        }],
        "the marker contributes no character"
    );
}

#[test]
fn a_selection_over_a_tool_card_head_copies_its_text() {
    use agent_core::{ContentPart, Message, ToolCall, ToolResult};
    let mut app = app();
    app.session.entries.clear();
    app.push_message(&Message::Assistant {
        content: vec![],
        reasoning: vec![],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"command": "make test"}),
        }],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    });
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text("ok")],
            structured: Some(serde_json::json!({"command": "make test", "exit_code": 0})),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    app.set_viewport(6, 6);
    app.set_transcript_width(60);
    // The assistant message is empty, so it contributes no line (its blank line is stripped).
    // The container's gap line is line 0, the card's top border line 1, and the head status
    // line line 2 (ADR 0028). The frame's left border sits in the marker column,
    // so the head's first character is at column 2.
    // The head carries the status glyph: ✔ once the result is in.
    let head = format!("{} shell: make test", app.footer_glyphs().tool_status()[1]);
    let end_col = 2 + head.chars().count() as u16 - 1;
    press(&mut app, 2, 2);
    drag(&mut app, 2, end_col);
    let result = release(&mut app, 2, end_col);
    assert_eq!(
        result.effects,
        vec![Effect::Copy { text: head.clone() }],
        "the whole head, character for character"
    );
}

#[test]
fn the_double_click_window_is_half_a_second() {
    assert_eq!(radian_tui::app::DOUBLE_CLICK_WINDOW.as_millis(), 500);
}
