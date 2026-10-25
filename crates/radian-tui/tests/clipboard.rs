//! Ctrl-V pastes an image from the system clipboard (docs/backlog/images.md Q2/Q7).
//!
//! The terminal handles `Cmd-V`/`Ctrl-V` itself and sends nothing when the clipboard holds an
//! image, so the `paste_image` action reads the clipboard in the loop and turns RGBA pixels
//! into the same image part `@` and a dropped path make. Every test here injects a reader: the
//! real clipboard is never touched. Each test names the mutation it catches where the slice's
//! acceptance list requires one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use radian_core::ContentPart;
use radian_tui::app::{App, Effect, Input};
use radian_tui::clipboard::{Content, Outcome, Reader};
use radian_tui::keys::Keymap;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A scratch workspace, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-tui-paste-{}-{label}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An injected clipboard. `read` is the only thing the app can do with it.
struct Fake(Content);

impl Reader for Fake {
    fn read(&self) -> Content {
        self.0.clone()
    }
}

/// A fixed RGBA image, every pixel the same colour.
fn rgba(width: u32, height: u32) -> Content {
    let pixels = [17u8, 34, 51, 255]
        .iter()
        .copied()
        .cycle()
        .take(width as usize * height as usize * 4)
        .collect();
    Content::Rgba {
        width,
        height,
        pixels,
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl_v() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)
}

fn app_with(workspace: &Path, content: Content) -> App {
    let mut app = App::new("m", "s");
    app.set_workspace(workspace.to_path_buf());
    app.clipboard_reader = Arc::new(Fake(content));
    app
}

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, app))
        .unwrap();
    terminal.backend().to_string()
}

/// The loop's half of the effect: read the injected clipboard through the shared pipeline.
fn read(app: &App) -> Outcome {
    radian_tui::clipboard::read(app.clipboard_reader.as_ref())
}

/// The names in a directory, sorted (the scratch workspace is empty before and after).
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|read| {
            read.flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The marker that tells the re-run test it is the isolated child: the parent writes it into
/// the child's `TMPDIR`, so it exists only there.
const CHILD_MARKER: &str = ".radian-paste-child";

/// The complete contents of `dir`, recursively, as sorted relative paths. This is not a name
/// filter: a regression that writes any temp file at all, under any name, changes the list.
fn tree(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            out.push(
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned(),
            );
            if path.is_dir() {
                walk(&path, root, out);
            }
        }
    }
    let mut entries = Vec::new();
    walk(dir, dir, &mut entries);
    entries.sort();
    entries
}

#[test]
fn a_clipboard_image_becomes_a_png_part_and_the_composer_shows_the_placeholder() {
    let workspace = TempDir::new("part");
    let mut app = app_with(workspace.path(), rgba(3, 2));

    // Ctrl-V produces the clipboard read effect (the host does the I/O).
    let effects = app.on_key(ctrl_v()).effects;
    assert!(
        matches!(effects.as_slice(), [Effect::ReadClipboard]),
        "{effects:?}"
    );

    let outcome = read(&app);
    let Outcome::Image(image) = &outcome else {
        panic!("an image part: {outcome:?}");
    };
    assert_eq!(image.name, "clipboard");
    assert_eq!(image.mime, "image/png");
    assert_eq!((image.width, image.height), (3, 2));
    assert!(
        image.data.starts_with("iVBORw0KGgo"),
        "the payload is a base64 PNG: {}",
        image.data
    );

    app.update(Input::ClipboardRead(outcome));
    assert_eq!(app.composer_images.len(), 1);
    // The composer draws the same placeholder line a dropped path draws.
    let shown = screen(&mut app, 60, 12);
    assert!(
        shown.contains("[image: clipboard, 3×2, 1 KB]"),
        "the composer shows the placeholder:\n{shown}"
    );

    // Sending it puts the PNG image part on the user message, and no path.
    let effects = app.on_key(key(KeyCode::Enter)).effects;
    let text = match &effects[0] {
        Effect::ExpandMentions { text } => text.clone(),
        other => panic!("expected an @ expansion, got {other:?}"),
    };
    assert!(text.trim().is_empty(), "no text and no path: {text:?}");
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
    let ContentPart::Image {
        mime_type,
        annotations: Some(annotations),
        ..
    } = &content[0]
    else {
        panic!("an image part: {content:?}");
    };
    assert_eq!(mime_type, "image/png");
    assert_eq!(annotations["name"], "clipboard");
    assert_eq!(annotations["width"].as_u64(), Some(3));
    assert_eq!(annotations["height"].as_u64(), Some(2));
}

#[test]
fn backspace_removes_the_clipboard_attachment() {
    let workspace = TempDir::new("backspace");
    let mut app = app_with(workspace.path(), rgba(4, 4));
    app.update(Input::ClipboardRead(read(&app)));
    assert_eq!(app.composer_images.len(), 1);

    // Backspace on an empty input removes the newest image, like a dropped path.
    app.on_key(key(KeyCode::Backspace));
    assert!(app.composer_images.is_empty());
}

#[test]
fn a_clipboard_with_no_image_attaches_nothing_and_the_key_stays_inert() {
    let workspace = TempDir::new("text");
    let mut app = app_with(workspace.path(), Content::NoImage);
    app.input = "keep typing".into();

    let effects = app.on_key(ctrl_v()).effects;
    assert!(
        matches!(effects.as_slice(), [Effect::ReadClipboard]),
        "{effects:?}"
    );
    let outcome = read(&app);
    assert_eq!(outcome, Outcome::NoImage);
    app.update(Input::ClipboardRead(outcome));
    assert!(app.composer_images.is_empty());
    assert_eq!(app.input.text(), "keep typing");
    assert!(
        app.session
            .entries
            .iter()
            .all(|entry| entry.kind != radian_tui::app::EntryKind::Notice),
        "no notice for a text-only clipboard: {:?}",
        app.session.entries
    );
}

#[test]
fn an_over_limit_clipboard_image_is_unusable() {
    let workspace = TempDir::new("too-wide");
    let width = radian_coding::MAX_IMAGE_SIDE + 1;
    let mut app = app_with(workspace.path(), rgba(width, 1));
    let effects = app.on_key(ctrl_v()).effects;
    assert!(
        matches!(effects.as_slice(), [Effect::ReadClipboard]),
        "{effects:?}"
    );

    let outcome = read(&app);
    let Outcome::Unusable(reason) = &outcome else {
        panic!("an unusable image: {outcome:?}");
    };
    assert!(reason.contains("8000"), "{reason}");
    app.update(Input::ClipboardRead(outcome));
    assert!(app.composer_images.is_empty());
    // The same unusable-attachment line `@` reports, with the clipboard as the source.
    assert!(
        app.session.entries.iter().any(|entry| {
            entry.text.contains("unusable attachment(s): clipboard:") && entry.text.contains("8000")
        }),
        "{:?}",
        app.session.entries
    );
}

#[test]
fn the_rebound_key_pastes_and_the_old_one_no_longer_does() {
    let workspace = TempDir::new("rebind");
    let mut app = app_with(workspace.path(), rgba(2, 2));
    app.keymap = Keymap::from_config(&[("paste_image".into(), vec!["ctrl-y".into()])]).unwrap();

    // The configured key reads the clipboard.
    let rebind = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL);
    let effects = app.on_key(rebind).effects;
    assert!(
        matches!(effects.as_slice(), [Effect::ReadClipboard]),
        "the rebound key pastes: {effects:?}"
    );
    // The default key replaces nothing: Ctrl-V is no longer a clipboard read.
    let effects = app.on_key(ctrl_v()).effects;
    assert!(
        effects.is_empty(),
        "Ctrl-V must not paste after the rebind: {effects:?}"
    );
}

#[test]
fn the_clipboard_image_never_touches_the_disk() {
    let temp = std::env::temp_dir();
    if !temp.join(CHILD_MARKER).exists() {
        // This test compares the **whole** temp dir, so it must be its only user: it re-runs
        // itself in a child process whose TMPDIR is a private empty directory. Setting TMPDIR
        // here would be process-wide and would race every other test in this binary (and every
        // other crate's tests in the workspace run).
        let root = TempDir::new("isolated");
        let child_temp = root.path().join("tmp");
        std::fs::create_dir_all(&child_temp).unwrap();
        std::fs::write(child_temp.join(CHILD_MARKER), b"").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("the_clipboard_image_never_touches_the_disk")
            .env("TMPDIR", &child_temp)
            .output()
            .expect("the test binary re-runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "the isolated child failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // A renamed test would make libtest match nothing and still exit 0.
        assert!(
            stdout.contains("1 passed"),
            "the child must run the test itself:\n{stdout}"
        );
        return;
    }

    // Child: its TMPDIR holds only the marker and this workspace, so the whole-directory
    // comparison is stable even under a parallel run.
    let workspace = temp.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut app = app_with(&workspace, rgba(40, 30));
    let temp_before = tree(&temp);
    let workspace_before = listing(&workspace);

    let effects = app.on_key(ctrl_v()).effects;
    assert!(
        matches!(effects.as_slice(), [Effect::ReadClipboard]),
        "{effects:?}"
    );
    app.update(Input::ClipboardRead(read(&app)));
    assert_eq!(app.composer_images.len(), 1);

    // Send it too: the whole path from key to message stays in memory.
    let effects = app.on_key(key(KeyCode::Enter)).effects;
    let text = match &effects[0] {
        Effect::ExpandMentions { text } => text.clone(),
        other => panic!("expected an @ expansion, got {other:?}"),
    };
    let expansion = radian_tui::mention::expand(&text, &workspace).unwrap();
    let effects = app.update(Input::MentionsExpanded(Ok(expansion))).effects;
    assert!(
        matches!(effects.first(), Some(Effect::SendFollowup(_))),
        "{effects:?}"
    );

    // Every entry under the temp dir, not just the clipboard-named ones: the workspace is one of
    // them, so this covers both directories the slice names.
    assert_eq!(
        tree(&temp),
        temp_before,
        "nothing is written under the system temp dir"
    );
    assert_eq!(
        listing(&workspace),
        workspace_before,
        "nothing is written into the workspace"
    );
}
