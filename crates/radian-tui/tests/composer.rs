//! The input composer in `App`: history, Esc, arrow keys and the editing keys (ADR 0009
//! Phase 4). Pure app state, so no process is started.

use radian_tui::app::{App, Effect};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn shift(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

#[test]
fn up_browses_history_and_down_returns_to_the_draft() {
    let mut app = App::new("m", "s");
    for line in ["first", "second"] {
        app.input.set_text(line);
        assert!(!app.on_key(key(KeyCode::Enter)).effects.is_empty());
    }
    app.input.set_text("draft");

    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input, "second", "the newest history entry is first");
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input, "first");
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input, "first", "already at the oldest");

    app.on_key(key(KeyCode::Down));
    assert_eq!(app.input, "second");
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.input, "draft", "past the newest entry is the draft");
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.input, "draft", "already at the draft");
}

#[test]
fn editing_a_browsed_entry_does_not_change_the_history() {
    let mut app = App::new("m", "s");
    app.input.set_text("hello");
    app.on_key(key(KeyCode::Enter));
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input, "hello");
    app.input.insert_char('!');
    assert_eq!(app.input, "hello!");
    // Back to the draft and up again: the stored entry is unchanged.
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.input, "");
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input, "hello");
}

#[test]
fn up_moves_inside_a_multiline_input_instead_of_the_history() {
    let mut app = App::new("m", "s");
    app.input.set_text("hello");
    app.on_key(key(KeyCode::Enter)); // one history entry
    app.input.set_text("ab\ncd");
    // The cursor is at the end (line 2); Up moves to the previous line, not the history.
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input, "ab\ncd", "the input is not replaced by history");
    assert_eq!(app.input.cursor(), 2);
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.input.cursor(), 5);
    // At the first line Up now browses history.
    app.on_key(key(KeyCode::Up));
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input, "hello");
}

#[test]
fn shift_up_scrolls_the_transcript() {
    let mut app = App::new("m", "s");
    app.set_viewport(4, 40);
    app.scroll_to_bottom();
    let bottom = app.scroll;
    app.on_key(shift(KeyCode::Up));
    assert!(
        app.scroll < bottom,
        "shift-up scrolls the transcript by a line"
    );
    assert!(!app.is_following());
    // A plain Up with an empty input browses history (a no-op here) and never scrolls.
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.scroll, bottom - 1);
}

#[test]
fn esc_clears_a_non_empty_input_and_quits_only_when_empty() {
    let mut app = App::new("m", "s");
    app.input.set_text("unsent");
    assert!(
        app.on_key(key(KeyCode::Esc)).effects.is_empty(),
        "Esc clears, it does not quit"
    );
    assert!(app.input.is_empty());
    assert!(!app.quit);
    // The cleared line is kept in the history, so Up finds it again.
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input, "unsent");

    app.input.clear();
    assert_eq!(app.on_key(key(KeyCode::Esc)).effects, vec![Effect::Quit]);
    assert!(app.quit);
}

#[test]
fn enter_clears_the_input_and_records_every_submitted_line() {
    let mut app = App::new("m", "s");
    app.input.set_text("hello");
    assert_eq!(
        app.on_key(key(KeyCode::Enter)).effects,
        vec![Effect::ExpandMentions {
            text: "hello".into()
        }]
    );
    assert!(app.input.is_empty());
    // A slash command is recorded too, and is not sent to the model.
    app.input.set_text("/help");
    assert!(app.on_key(key(KeyCode::Enter)).effects.is_empty());
    assert!(app.input.is_empty());
    assert_eq!(app.history, vec!["hello".to_string(), "/help".to_string()]);
}

#[test]
fn editing_keys_edit_at_the_cursor() {
    let mut app = App::new("m", "s");
    app.input.set_text("ac");
    app.on_key(key(KeyCode::Left));
    app.on_key(key(KeyCode::Char('b')));
    assert_eq!(app.input, "abc");
    assert_eq!(app.input.cursor(), 2);
    // Ctrl-A / Ctrl-E are the line's start / end.
    app.on_key(ctrl(KeyCode::Char('a')));
    assert_eq!(app.input.cursor(), 0);
    app.on_key(ctrl(KeyCode::Char('e')));
    assert_eq!(app.input.cursor(), 3);
    // Ctrl-K kills to the end of the line.
    app.on_key(ctrl(KeyCode::Char('a')));
    app.on_key(ctrl(KeyCode::Char('k')));
    assert_eq!(app.input, "");
    // Ctrl-W deletes the previous word.
    app.input.set_text("git status");
    app.on_key(ctrl(KeyCode::Char('w')));
    assert_eq!(app.input, "git ");
    // Delete removes under the cursor.
    app.input.set_text("abc");
    app.on_key(key(KeyCode::Left));
    app.on_key(key(KeyCode::Delete));
    assert_eq!(app.input, "ab");
}

#[test]
fn resume_prefills_the_history_from_user_messages_only() {
    let mut app = App::new("m", "s");
    let user = agent_core::Message::user("from the user");
    let assistant = agent_core::Message::assistant("from the model");
    app.prefill_history(&[user, assistant]);
    assert_eq!(app.history, vec!["from the user".to_string()]);
}
