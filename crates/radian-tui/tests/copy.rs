//! `/copy [code]`: the last committed assistant message, or its last fenced block.
//!
//! Pure `update` tests (ADR 0023 U3): `/copy` becomes one `Effect::Copy` (the loop owns the
//! clipboard write and its notice, exactly like a mouse selection); a streaming entry never
//! counts, and an empty transcript, a missing block, an oversized message or a disabled
//! backend is a notice.

use radian_core::Message;
use radian_tui::app::{App, Effect, Entry, EntryKind};
use radian_tui::clip::ClipboardMode;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Submits a `/copy` line like the loop (no `@` expansion is involved on this path).
fn submit(app: &mut App, line: &str) -> Vec<Effect> {
    app.input = line.into();
    app.on_key(key(KeyCode::Enter)).effects
}

fn notices(app: &App) -> Vec<String> {
    app.session
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

fn committed(app: &mut App, text: &str) {
    app.push_message(&Message::assistant(text));
}

fn live(app: &mut App, text: &str) {
    let mut entry = Entry::new(EntryKind::Assistant, text);
    entry.live = true;
    app.session.entries.push(entry);
}

/// `/copy` copies the last committed assistant message's markdown source, whole.
#[test]
fn copy_takes_the_last_committed_assistant_message() {
    let mut app = App::new("m", "s");
    app.push_message(&Message::user("q1"));
    committed(&mut app, "first *answer*");
    app.push_message(&Message::user("q2"));
    committed(&mut app, "second `answer`");
    assert_eq!(
        submit(&mut app, "/copy"),
        vec![Effect::Copy {
            text: "second `answer`".into()
        }]
    );
}

/// While a turn streams `/copy` takes the previous committed message, never the in-flight
/// text; with only a live entry there is nothing to copy.
#[test]
fn copy_ignores_the_in_flight_streaming_text() {
    let mut app = App::new("m", "s");
    committed(&mut app, "done answer");
    live(&mut app, "in-flight partial");
    assert_eq!(
        submit(&mut app, "/copy"),
        vec![Effect::Copy {
            text: "done answer".into()
        }],
        "the streaming text must not be copied"
    );

    let mut app = App::new("m", "s");
    live(&mut app, "in-flight partial");
    assert!(submit(&mut app, "/copy").is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("nothing to copy")),
        "{:?}",
        notices(&app)
    );

    // No assistant message at all is the same notice.
    let mut app = App::new("m", "s");
    app.push_message(&Message::user("only a question"));
    assert!(submit(&mut app, "/copy").is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("nothing to copy")),
        "{:?}",
        notices(&app)
    );
}

/// `/copy code` copies the last fenced block's content, without the fence lines.
#[test]
fn copy_code_takes_the_last_block_without_its_fences() {
    let mut app = App::new("m", "s");
    committed(
        &mut app,
        "first:\n```rust\nlet a = 1;\n```\nsecond:\n```python\nprint(2)\n```",
    );
    assert_eq!(
        submit(&mut app, "/copy code"),
        vec![Effect::Copy {
            text: "print(2)".into()
        }],
        "the fences must not be copied"
    );
}

/// `/copy code` with no block in the message is a notice saying so.
#[test]
fn copy_code_without_a_block_is_a_notice() {
    let mut app = App::new("m", "s");
    committed(&mut app, "plain prose, no fences");
    assert!(submit(&mut app, "/copy code").is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("no code block")),
        "{:?}",
        notices(&app)
    );
}

/// A disabled backend answers instead of copying silently: nothing is copied, and why is a
/// notice.
#[test]
fn copy_with_the_clipboard_off_is_a_notice() {
    let mut app = App::new("m", "s");
    app.clipboard = ClipboardMode::Off;
    committed(&mut app, "an answer");
    assert!(submit(&mut app, "/copy").is_empty());
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("nothing was copied")),
        "{:?}",
        notices(&app)
    );
}

/// `/copy` completes its one selector: `/copy ` offers `code`, and nothing past it.
#[test]
fn copy_completes_its_code_selector() {
    use radian_tui::command::{Level, complete_in, tree};
    let line = "/copy ";
    let completion = complete_in(tree(), line, line.chars().count()).expect("a completion");
    match completion.level_result {
        Level::Ready(candidates) => {
            assert_eq!(candidates.len(), 1, "{candidates:?}");
            assert_eq!(candidates[0].insert, "code");
        }
        other => panic!("expected the code selector, got {other:?}"),
    }
    // Past the selector there is nothing to complete, so Enter submits.
    let line = "/copy code";
    assert!(
        complete_in(tree(), line, line.chars().count()).is_none(),
        "an exact selector must not arm the menu"
    );
}
