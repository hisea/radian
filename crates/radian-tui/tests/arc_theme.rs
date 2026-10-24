//! The arc TUI's theme is the radian TUI's (brief TK0087): the same `config.toml`, the same
//! `theme::load` call, `NO_COLOR` and truecolor detection, read once when the TUI opens.
//!
//! The config is injected as a resolved path, the way the radian TUI's own config tests feed a
//! path to `radian_host::config::parse`, so no test sets a process-wide environment variable.
//! The theme a problem warns about is shown in the footer, so the footer is rendered and read
//! like any other frame.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, config};
use radian_tui::arc::render;
use radian_tui::arc::{ArcApp, open_theme};
use radian_tui::theme::{Theme, ThemeName};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-arc-theme-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the temp dir is creatable");
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A store in `dir`: arc's own `config.toml` and the layout it expects.
fn store(dir: &Path) -> Store {
    let root = dir.join(".radian/arc");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("config.toml"), config::default_text("main")).unwrap();
    Store::at(dir)
}

/// Writes the user's `config.toml` in `dir` with the required version line, and returns its path.
fn write_config(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("config.toml");
    fs::write(&path, format!("schema_version = 1\n{text}")).unwrap();
    path
}

fn render(app: &mut ArcApp, width: u16, height: u16) -> Terminal<TestBackend> {
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
                        .map(|cell| cell.symbol())
                        .unwrap_or(" ")
                        .to_string()
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn screen(app: &mut ArcApp, width: u16, height: u16) -> String {
    rows(&render(app, width, height)).join("\n")
}

/// The foreground of the cell at `(row, column)`.
fn fg(terminal: &Terminal<TestBackend>, row: u16, column: u16) -> Option<Color> {
    terminal
        .backend()
        .buffer()
        .cell((column, row))
        .map(|cell| cell.fg)
}

/// The row and column of the `mark` that sits before the first `needle` in its own row.
fn mark_cell(terminal: &Terminal<TestBackend>, needle: &str, mark: char) -> (u16, u16) {
    let rows = rows(terminal);
    for (row, text) in rows.iter().enumerate() {
        let Some(byte) = text.find(needle) else {
            continue;
        };
        let id_column = text[..byte].chars().count();
        let chars: Vec<char> = text.chars().collect();
        if let Some(column) = chars[..id_column].iter().rposition(|ch| *ch == mark) {
            return (row as u16, column as u16);
        }
    }
    panic!("`{mark}` is not before `{needle}`:\n{}", rows.join("\n"));
}

/// The configured `tui.theme` is the theme the TUI draws with: the same built-in palette the
/// radian TUI resolves for the same truecolor setting (mutation: the arc TUI keeps
/// `Theme::default()`).
#[test]
fn the_configured_theme_is_the_radian_tuis() {
    let scratch = Scratch::new("light");
    let path = write_config(&scratch.dir, "[tui]\ntheme = \"light\"\n");
    let store = store(&scratch.dir);
    for truecolor in [true, false] {
        let open = open_theme(Some(&path), false, truecolor, None);
        assert!(open.warnings.is_empty(), "{:?}", open.warnings);
        let app = ArcApp::open_with_theme(store.clone(), open);
        assert_eq!(
            app.theme,
            Theme::builtin(ThemeName::Light, truecolor),
            "truecolor = {truecolor}"
        );
    }
}

/// `[tui.colors]` overrides are passed to `theme::load`, so `plan_running` reaches the plans
/// page's `●` mark (mutation: the overrides are not passed).
#[test]
fn tui_colors_reaches_the_plans_pages_running_mark() {
    let scratch = Scratch::new("plan-running");
    let path = write_config(&scratch.dir, "[tui.colors]\nplan_running = \"#0a0b0c\"\n");
    let store = store(&scratch.dir);
    let plan = store
        .add_plan("run", "a running plan", "the request\n")
        .unwrap();
    let task = store
        .add_task(
            &plan,
            "work",
            "the work",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .unwrap();
    store
        .change_task(&task, |task| {
            task.state = TaskState::Doing;
            task.phase = "running".to_owned();
            Ok(())
        })
        .unwrap();
    store
        .change_plan(&plan, |plan| {
            plan.status = "confirmed".to_owned();
            Ok(())
        })
        .unwrap();

    let open = open_theme(Some(&path), false, true, None);
    assert!(open.warnings.is_empty(), "{:?}", open.warnings);
    let mut app = ArcApp::open_with_theme(store, open);
    assert_eq!(
        app.theme.plan_running.fg,
        Some(Color::Rgb(0x0a, 0x0b, 0x0c))
    );
    let terminal = render(&mut app, 140, 30);
    let (row, column) = mark_cell(&terminal, &plan, '●');
    assert_eq!(
        fg(&terminal, row, column),
        Some(Color::Rgb(0x0a, 0x0b, 0x0c)),
        "the plan list's running mark takes the configured role"
    );
    assert_ne!(
        Some(Color::Rgb(0x0a, 0x0b, 0x0c)),
        Theme::dark().plan_running.fg,
        "the configured color is not the dark default"
    );
}

/// A name that is not a built-in resolves through `themes_dir`, like the radian TUI's own
/// `~/.radian/themes/<name>.toml`.
#[test]
fn a_theme_file_name_resolves_through_the_themes_dir() {
    let scratch = Scratch::new("theme-file");
    let themes = scratch.path("themes");
    fs::create_dir_all(&themes).unwrap();
    fs::write(
        themes.join("mine.toml"),
        "name = \"mine\"\n[roles]\nplan_running = \"#123456\"\n",
    )
    .unwrap();
    let path = write_config(&scratch.dir, "[tui]\ntheme = \"mine\"\n");

    let open = open_theme(Some(&path), false, true, Some(&themes));
    assert!(open.warnings.is_empty(), "{:?}", open.warnings);
    let app = ArcApp::open_with_theme(store(&scratch.dir), open);
    assert_eq!(
        app.theme.plan_running.fg,
        Some(Color::Rgb(0x12, 0x34, 0x56))
    );
    assert_eq!(app.theme.name, ThemeName::File);
}

/// `NO_COLOR` beats the config's theme, exactly as it does in the radian TUI.
#[test]
fn no_color_forces_mono() {
    let scratch = Scratch::new("no-color");
    let path = write_config(&scratch.dir, "[tui]\ntheme = \"light\"\n");
    let open = open_theme(Some(&path), true, true, None);
    assert!(open.warnings.is_empty(), "{:?}", open.warnings);
    let app = ArcApp::open_with_theme(store(&scratch.dir), open);
    assert_eq!(app.theme, Theme::mono());
}

/// An unknown theme name opens with the fallback theme and the load's warning in the footer
/// (mutation: the load warnings are dropped).
#[test]
fn an_unknown_theme_opens_with_the_fallback_and_warns_in_the_footer() {
    let scratch = Scratch::new("unknown-theme");
    let themes = scratch.path("themes");
    fs::create_dir_all(&themes).unwrap();
    let path = write_config(&scratch.dir, "[tui]\ntheme = \"future\"\n");

    let open = open_theme(Some(&path), false, true, Some(&themes));
    assert_eq!(open.theme.name, ThemeName::Dark);
    let warning = open
        .warnings
        .iter()
        .find(|warning| warning.contains("future"))
        .unwrap_or_else(|| panic!("no warning names the theme: {:?}", open.warnings))
        .clone();

    let mut app = ArcApp::open_with_theme(store(&scratch.dir), open);
    let text = screen(&mut app, 240, 30);
    let needle: String = warning.chars().take(48).collect();
    assert!(text.contains(&needle), "{text}");
    assert!(text.contains("using built-in dark"), "{text}");
}

/// A config that cannot be read falls back to the theme `theme::load` resolves for an empty
/// configuration, with the reader's reason in the footer; it never stops the TUI. The radian
/// TUI's own handling of the error is unchanged: `load` still refuses it, the arc TUI is the
/// one that carries on. `NO_COLOR` and truecolor detection still decide the fallback (ADR
/// 0029 decision 7: `NO_COLOR` wins), as they do for a readable file.
#[test]
fn an_unreadable_config_opens_with_the_loader_theme_and_names_the_reason_in_the_footer() {
    let scratch = Scratch::new("unreadable");
    // A directory where the file should be is a read error on every platform the TUI runs on.
    let path = scratch.path("config-is-a-directory");
    fs::create_dir_all(&path).unwrap();

    let open = open_theme(Some(&path), false, true, None);
    assert_eq!(open.theme, Theme::dark());
    assert!(
        open.warnings
            .iter()
            .any(|warning| warning.contains("cannot read")),
        "{:?}",
        open.warnings
    );
    let mut app = ArcApp::open_with_theme(store(&scratch.dir), open);
    let text = screen(&mut app, 240, 30);
    assert!(text.contains("cannot read"), "{text}");
    assert!(
        text.contains("config-is-a-directory"),
        "the footer names the file it could not read:\n{text}"
    );

    // An unreadable config never bypasses the loader: `NO_COLOR` still wins (ADR 0029), and a
    // terminal without truecolor still gets the loader's 256-color palette rather than
    // hard-coded RGB.
    let mono = open_theme(Some(&path), true, true, None);
    assert_eq!(mono.theme, Theme::mono());
    assert!(
        mono.warnings
            .iter()
            .any(|warning| warning.contains("cannot read")),
        "{:?}",
        mono.warnings
    );
    let no_truecolor = open_theme(Some(&path), false, false, None);
    assert_eq!(no_truecolor.theme, Theme::builtin(ThemeName::Dark, false));
    assert_ne!(
        no_truecolor.theme,
        Theme::dark(),
        "a terminal without truecolor must not open on hard-coded RGB"
    );
}

/// The open-time warning stays when a loop start fails before the first frame: the start's
/// line is added after it, not put in its place (brief TK0087; the footer has one line).
#[test]
fn a_failed_start_before_the_first_frame_keeps_the_open_warning() {
    let scratch = Scratch::new("warning-then-start");
    // pi's reserved name warns without a path, so the two lines fit the footer side by side.
    let path = write_config(&scratch.dir, "[tui]\ntheme = \"system\"\n");

    let open = open_theme(Some(&path), false, true, None);
    let mut app = ArcApp::open_with_theme(store(&scratch.dir), open);
    app.add_result("a loop is already running".to_owned(), false);
    let text = screen(&mut app, 240, 30);
    assert!(
        text.contains("theme `system` is not a usable theme name"),
        "{text}"
    );
    assert!(text.contains("using built-in dark"), "{text}");
    assert!(text.contains("a loop is already running"), "{text}");
}

/// A missing config file is the empty configuration, exactly as it is for the radian TUI, so
/// the arc TUI opens on the built-in dark theme with no footer line.
#[test]
fn a_missing_config_is_the_empty_config_like_the_radian_tui() {
    let scratch = Scratch::new("missing");
    let open = open_theme(Some(&scratch.path("absent.toml")), false, true, None);
    assert_eq!(open.theme, Theme::dark());
    assert!(open.warnings.is_empty(), "{:?}", open.warnings);
}
