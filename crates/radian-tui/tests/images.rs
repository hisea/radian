//! Image inputs in the TUI (backlog `images.md` Q3/Q6/Q7): `@path` attaches an image, the
//! transcript shows the placeholder line (also after a journal round trip), a dropped path
//! becomes an attachment and Backspace removes it, and an over-limit image is unsendable.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use radian_core::ContentPart;
use radian_tui::app::{App, Effect, EntryKind, Input};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-tui-images-{}-{label}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.0.join(name), bytes).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes
}

fn app_with(workspace: &Path) -> App {
    let mut app = App::new("m", "s");
    app.set_workspace(workspace.to_path_buf());
    app
}

fn user_entry(app: &App) -> String {
    app.session
        .entries
        .iter()
        .rev()
        .find(|entry| entry.kind == EntryKind::User)
        .map(|entry| entry.text.clone())
        .expect("a user entry")
}

fn submit(app: &mut App, text: &str) -> Vec<Effect> {
    app.input = text.into();
    app.on_key(key(KeyCode::Enter)).effects
}

#[test]
fn an_at_image_is_sent_as_a_part_and_shown_as_the_placeholder() {
    let dir = TempDir::new("at");
    dir.write("shot.png", &png(1200, 800));
    let mut app = app_with(dir.path());

    let effects = submit(&mut app, "look at @shot.png");
    let text = match &effects[0] {
        Effect::ExpandMentions { text } => text.clone(),
        other => panic!("expected an @ expansion, got {other:?}"),
    };
    let expansion = radian_tui::mention::expand(&text, dir.path()).unwrap();
    let effects = app.update(Input::MentionsExpanded(Ok(expansion))).effects;
    let message = match &effects[0] {
        Effect::SendFollowup(message) => message.clone(),
        other => panic!("expected a followup, got {other:?}"),
    };
    let radian_core::Message::User { content, .. } = &message else {
        panic!("a user message");
    };
    assert!(
        matches!(content[1], ContentPart::Image { .. }),
        "{content:?}"
    );

    app.push_message(&message);
    assert_eq!(
        user_entry(&app),
        "look at @shot.png\n\n[image: shot.png, 1200×800, 1 KB]"
    );

    // Journal replay: the same message after a JSON round trip shows the same line.
    let stored = serde_json::to_string(&message).unwrap();
    let read_back: radian_core::Message = serde_json::from_str(&stored).unwrap();
    let mut resumed = App::new("m", "s");
    resumed.push_message(&read_back);
    assert_eq!(user_entry(&resumed), user_entry(&app));
}

#[test]
fn an_image_over_the_limit_is_unsendable_and_keeps_the_input() {
    let dir = TempDir::new("too-big");
    let mut huge = png(10, 10);
    huge.resize(radian_coding::MAX_IMAGE_BYTES as usize + 1, 0);
    dir.write("huge.png", &huge);
    let mut app = app_with(dir.path());

    let effects = submit(&mut app, "see @huge.png");
    let text = match &effects[0] {
        Effect::ExpandMentions { text } => text.clone(),
        other => panic!("expected an @ expansion, got {other:?}"),
    };
    let error = radian_tui::mention::expand(&text, dir.path()).unwrap_err();
    let effects = app.update(Input::MentionsExpanded(Err(error))).effects;
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.input.text(), "see @huge.png", "the input comes back");
    assert!(
        app.session
            .entries
            .iter()
            .any(|entry| entry.text.contains("not sent:") && entry.text.contains("5 MiB")),
        "{:?}",
        app.session.entries
    );
}

#[test]
fn a_dropped_path_becomes_an_attachment_and_backspace_removes_it() {
    let workspace = TempDir::new("drop-ws");
    let outside = TempDir::new("drop-outside");
    outside.write("shot one.png", &png(10, 10));
    let path = outside.path().join("shot one.png");
    let escaped = path.display().to_string().replace(' ', "\\ ");
    let mut app = app_with(workspace.path());

    let pasted = format!("see {escaped} ");
    let effects = app.update(Input::Paste(pasted)).effects;
    let (text, path, range) = match &effects[0] {
        Effect::LoadDropped { text, path, range } => (text.clone(), path.clone(), range.clone()),
        other => panic!("expected a dropped-path read, got {other:?}"),
    };
    let outcome = radian_tui::mention::load_dropped(&path);
    app.update(Input::DroppedLoaded {
        text,
        range,
        outcome,
    });
    assert_eq!(app.composer_images.len(), 1);
    assert!(
        !app.input.text().contains("shot one.png"),
        "the path left the text: {:?}",
        app.input.text()
    );
    assert_eq!(
        app.composer_images[0].placeholder(),
        "[image: shot one.png, 10×10, 1 KB]"
    );

    // Backspace on an empty input removes the attachment.
    app.input.clear();
    app.on_key(key(KeyCode::Backspace));
    assert!(app.composer_images.is_empty());
}

#[test]
fn an_image_only_message_is_sendable() {
    let workspace = TempDir::new("image-only");
    let outside = TempDir::new("image-only-outside");
    outside.write("shot.png", &png(10, 10));
    let mut app = app_with(workspace.path());
    app.composer_images
        .push(radian_coding::image::load("shot.png", &png(10, 10)).expect("a valid PNG"));

    let effects = app.on_key(key(KeyCode::Enter)).effects;
    let text = match &effects[0] {
        Effect::ExpandMentions { text } => text.clone(),
        other => panic!("expected an @ expansion, got {other:?}"),
    };
    let expansion = radian_tui::mention::expand(&text, workspace.path()).unwrap();
    let effects = app.update(Input::MentionsExpanded(Ok(expansion))).effects;
    let message = match &effects[0] {
        Effect::SendFollowup(message) => message.clone(),
        other => panic!("expected a followup, got {other:?}"),
    };
    let radian_core::Message::User { content, .. } = &message else {
        panic!("a user message");
    };
    assert_eq!(content.len(), 1, "no empty text part: {content:?}");
    assert!(matches!(content[0], ContentPart::Image { .. }));
}
