//! The arc TUI starts the scheduling loop and never holds it (ADR 0048 decision 13).
//!
//! The TUI holds no `runner.lock` and runs no pass. When it opens, and when `R` is pressed, it
//! starts `radian arc serve` as a detached process if the probe says no loop runs, and it
//! never signals that process. These tests use the real `radian` binary as that process, in a
//! scratch repository, so the lock and the process are real; the fake executable covers the
//! failure a start can report.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use radian_arc::store::{Store, config};
use radian_tui::arc::loop_log::{LoopLogTail, MAX_LINES};
use radian_tui::arc::looping::{LoopStarter, LoopStatus};
use radian_tui::arc::{ArcApp, ArcKey, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-arc-start-{label}-{}-{n}",
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

/// A git repository with arc's `config.toml`: what `radian arc run` opens.
fn store_at(dir: &Path) -> Store {
    fs::create_dir_all(dir).expect("the repository directory");
    let output = Command::new("git")
        .current_dir(dir)
        .args(["init", "-q"])
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git init: {output:?}");
    let root = dir.join(".radian/arc");
    fs::create_dir_all(&root).expect("the arc directory");
    fs::write(root.join("config.toml"), config::default_text("main")).expect("config.toml");
    Store::at(dir)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn screen(app: &mut ArcApp, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
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
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `pid` exists (a `kill -0` probe).
fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

/// A loop a test started, stopped on drop if the test fails first.
struct KillOnDrop(Option<u32>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }
}

/// Waits until `predicate` holds, failing with `what` after `limit`.
fn wait_until(limit: Duration, what: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen in {limit:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Starts the real `radian arc serve` in `store`'s repository and waits for its lock.
/// Returns the starter and the holder's pid.
fn start_real_loop(store: &Store) -> (LoopStarter, u32) {
    let radian = PathBuf::from(env!("CARGO_BIN_EXE_radian"));
    let mut starter = LoopStarter::with_program(store.clone(), radian);
    assert_eq!(
        starter.status(),
        LoopStatus::Stopped,
        "the fixture's runner.lock is free"
    );
    starter.start().expect("the loop starts");
    assert_eq!(starter.status(), LoopStatus::Starting);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        starter.poll();
        if let LoopStatus::Elsewhere(Some(pid)) = starter.status() {
            return (starter, pid);
        }
        assert!(
            Instant::now() < deadline,
            "the started loop never took runner.lock"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_detached_start_runs_arc_serve() {
    let scratch = Scratch::new("args");
    let store = store_at(&scratch.path("repo"));
    let args_file = scratch.path("args");
    let script = scratch.path("fake-loop");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 0\n",
            args_file.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut starter = LoopStarter::with_program_and_timeout(store, script, Duration::from_secs(5));
    starter.start().expect("the process is spawned");
    wait_until(Duration::from_secs(5), "the fake loop's arguments", || {
        args_file.exists()
    });
    let args = fs::read_to_string(&args_file).expect("the fake loop wrote its arguments");
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        ["arc", "serve"],
        "the TUI starts `radian arc serve`"
    );
}

#[test]
fn the_tui_starts_one_loop_and_leaves_it_running() {
    let scratch = Scratch::new("starts");
    let store = store_at(&scratch.path("repo"));
    let (starter, pid) = start_real_loop(&store);
    assert_ne!(pid, std::process::id(), "the loop is another process");
    assert!(
        store.try_runner_lock().unwrap().is_none(),
        "the started loop holds runner.lock"
    );
    let guard = KillOnDrop(Some(pid));

    // The TUI quits by dropping its starter: no wait, no signal, and the loop goes on.
    drop(starter);
    assert!(alive(pid), "quitting the TUI killed the loop");
    assert!(
        store.try_runner_lock().unwrap().is_none(),
        "quitting the TUI released the loop's lock"
    );

    let sent = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(sent.success(), "SIGTERM reaches the loop");
    wait_until(Duration::from_secs(10), "the loop's lock release", || {
        store.try_runner_lock().is_ok_and(|lock| lock.is_some())
    });
    drop(guard);
}

/// `setsid` puts the loop in a session and process group of its own, so the terminal's SIGHUP
/// and the TUI's process group never reach it (ADR 0048 decision 13).
#[test]
fn the_started_loop_has_its_own_session_and_process_group() {
    let scratch = Scratch::new("session");
    let store = store_at(&scratch.path("repo"));
    let (starter, pid) = start_real_loop(&store);
    let guard = KillOnDrop(Some(pid));

    #[cfg(unix)]
    {
        // The syscall, not `ps`: the session and the process group are the child's own.
        let session = unsafe { libc::getsid(pid as libc::pid_t) };
        let group = unsafe { libc::getpgid(pid as libc::pid_t) };
        assert_eq!(
            session, pid as libc::pid_t,
            "the loop does not lead its session"
        );
        assert_eq!(
            group, pid as libc::pid_t,
            "the loop does not lead its process group"
        );
    }

    drop(starter);
    assert!(alive(pid));
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    wait_until(Duration::from_secs(10), "the loop's lock release", || {
        store.try_runner_lock().is_ok_and(|lock| lock.is_some())
    });
    drop(guard);
}

#[test]
fn quitting_while_a_start_is_in_flight_leaves_the_loop_running() {
    let scratch = Scratch::new("start-in-flight");
    let store = store_at(&scratch.path("repo"));
    let radian = PathBuf::from(env!("CARGO_BIN_EXE_radian"));
    let mut starter = LoopStarter::with_program(store.clone(), radian);
    starter.start().expect("the loop starts");
    // The TUI quits before its next probe: nothing waits on the child, and nothing signals it.
    drop(starter);
    // The loop writes its pid after it locks; a probe that takes the lock here would race
    // the starting loop's own attempt and could make it exit `4`.
    let deadline = Instant::now() + Duration::from_secs(20);
    let pid = loop {
        if let Some(pid) = store.runner_lock_pid() {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "the loop never took runner.lock after the TUI quit"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        store.try_runner_lock().unwrap().is_none(),
        "the loop holds runner.lock"
    );
    let guard = KillOnDrop(Some(pid));
    assert!(alive(pid), "quitting the TUI killed the loop it started");
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    wait_until(Duration::from_secs(10), "the loop's lock release", || {
        store.try_runner_lock().is_ok_and(|lock| lock.is_some())
    });
    drop(guard);
}

#[test]
fn a_held_lock_starts_nothing_and_the_header_names_the_holder() {
    let scratch = Scratch::new("held");
    let store = store_at(&scratch.path("repo"));
    let held = store
        .try_runner_lock()
        .unwrap()
        .expect("the test takes the lock first");
    // A program that would leave a marker had the TUI started it.
    let marker = scratch.path("spawned");
    let script = scratch.path("fake-loop");
    fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\nsleep 30\n", marker.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut starter = LoopStarter::with_program(store.clone(), script);
    assert_eq!(
        starter.status(),
        LoopStatus::Elsewhere(Some(std::process::id())),
        "the probe names the holder the lock recorded"
    );
    let error = starter.start().expect_err("a held lock starts nothing");
    assert!(error.contains("already running"), "{error}");
    assert_eq!(
        starter.status(),
        LoopStatus::Elsewhere(Some(std::process::id()))
    );
    assert!(!marker.exists(), "the TUI spawned a loop beside the holder");

    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    app.set_loop_status(starter.status());
    let text = screen(&mut app, 120, 30);
    assert!(
        text.contains(&format!("loop ● running (pid {})", std::process::id())),
        "the header does not name the holder:\n{text}"
    );
    drop(held);
}

#[test]
fn r_starts_a_loop_only_when_none_runs() {
    let scratch = Scratch::new("r-key");
    let store = store_at(&scratch.path("repo"));
    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));

    app.set_loop_status(LoopStatus::Stopped);
    assert_eq!(app.on_key(&key(KeyCode::Char('R'))), ArcKey::StartLoop);
    assert!(app.result.is_none());

    app.set_loop_status(LoopStatus::Starting);
    assert_eq!(app.on_key(&key(KeyCode::Char('R'))), ArcKey::Redraw);
    assert!(
        app.result
            .as_ref()
            .is_some_and(|line| line.text.contains("a loop is starting")),
        "the footer does not say why R did nothing: {:?}",
        app.result
    );

    app.set_loop_status(LoopStatus::Elsewhere(Some(4242)));
    assert_eq!(app.on_key(&key(KeyCode::Char('R'))), ArcKey::Redraw);
    assert!(
        app.result
            .as_ref()
            .is_some_and(|line| line.text.contains("already running (pid 4242)")),
        "the footer does not name the running loop: {:?}",
        app.result
    );
}

#[test]
fn a_start_that_exits_at_once_is_stopped_with_its_exit_code() {
    let scratch = Scratch::new("exit-four");
    let store = store_at(&scratch.path("repo"));
    let script = scratch.path("fake-loop");
    fs::write(
        &script,
        "#!/bin/sh\n\
         echo '2026-10-24T00:00:00.000Z radian arc: another loop holds runner.lock' \
         >> .radian/arc/loop.log\n\
         exit 4\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut starter = LoopStarter::with_program_and_timeout(store, script, Duration::from_secs(10));
    starter.start().expect("the process is spawned");
    assert_eq!(starter.status(), LoopStatus::Starting);
    let deadline = Instant::now() + Duration::from_secs(5);
    let line = loop {
        if let Some(line) = starter.poll() {
            break line;
        }
        assert!(
            Instant::now() < deadline,
            "the failed start was never reported"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(starter.status(), LoopStatus::Stopped);
    assert!(
        line.contains("exited 4"),
        "the exit code is missing: {line}"
    );
    assert!(
        line.contains("radian arc: another loop holds runner.lock"),
        "the last loop.log error line is missing: {line}"
    );
}

#[test]
fn the_ticker_shows_a_line_loop_log_gained_after_open() {
    let scratch = Scratch::new("ticker");
    let store = store_at(&scratch.path("repo"));
    let mut app = ArcApp::with_session_dir(store.clone(), Some(scratch.path("sessions")));
    let mut tail = LoopLogTail::new(&store);
    // The file at open is the baseline: its lines are not news.
    assert!(tail.read().is_none(), "no loop.log yet");
    app.apply_loop_log(None);
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("loop ○ stopped"), "{text}");
    app.on_key(&key(KeyCode::Char('L')));
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("no loop log yet"), "{text}");
    app.on_key(&key(KeyCode::Char('L')));

    fs::write(
        scratch.path("repo/.radian/arc/loop.log"),
        "2026-10-24T10:00:00.000Z TK0001-work-1 started -> doing.running\n",
    )
    .unwrap();
    let read = tail.read().expect("the log exists now");
    assert_eq!(
        read.ticker.as_deref(),
        Some("TK0001-work-1 started -> doing.running"),
        "the ticker takes the new transition line"
    );
    app.apply_loop_log(Some(read));
    assert_eq!(
        app.ticker.as_deref(),
        Some("TK0001-work-1 started -> doing.running")
    );
    let text = screen(&mut app, 120, 30);
    assert!(
        text.contains("TK0001-work-1 started -> doing.running"),
        "the ticker is drawn:\n{text}"
    );

    // `L` opens the log view over the page.
    assert_eq!(app.on_key(&key(KeyCode::Char('L'))), ArcKey::Redraw);
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("╭─ log"), "the log view is drawn:\n{text}");
    assert!(
        text.contains("TK0001-work-1 started -> doing.running"),
        "the log view shows the file:\n{text}"
    );
    assert_eq!(app.on_key(&key(KeyCode::Char('L'))), ArcKey::Redraw);
    assert!(!app.log_open);
}

#[test]
fn the_log_view_keeps_the_last_500_lines_and_follows_the_end() {
    let scratch = Scratch::new("log-view");
    let store = store_at(&scratch.path("repo"));
    let mut app = ArcApp::with_session_dir(store.clone(), Some(scratch.path("sessions")));
    let mut tail = LoopLogTail::new(&store);
    let path = scratch.path("repo/.radian/arc/loop.log");
    let mut text = String::new();
    for n in 0..600 {
        text.push_str(&format!("2026-10-24T10:00:00.000Z line {n}\n"));
    }
    fs::write(&path, &text).unwrap();
    app.apply_loop_log(tail.read());
    assert_eq!(
        app.log.len(),
        MAX_LINES,
        "the view keeps the last 500 lines"
    );
    assert!(
        app.log[0].contains("line 100"),
        "the oldest kept line is 100: {}",
        app.log[0]
    );
    assert_eq!(app.log_scroll, MAX_LINES - 1, "a new view follows the end");

    // Scrolled up, a new line does not drag the view back.
    app.on_key(&key(KeyCode::Char('L')));
    app.on_key(&key(KeyCode::Char('k')));
    app.on_key(&key(KeyCode::Char('k')));
    let scrolled = app.log_scroll;
    text.push_str("2026-10-24T10:00:01.000Z line 600\n");
    fs::write(&path, &text).unwrap();
    app.apply_loop_log(tail.read());
    assert_eq!(app.log_scroll, scrolled, "a scrolled view stays put");

    // At the end again, it follows.
    app.on_key(&key(KeyCode::Char('f')));
    text.push_str("2026-10-24T10:00:02.000Z line 601\n");
    fs::write(&path, &text).unwrap();
    app.apply_loop_log(tail.read());
    assert_eq!(app.log_scroll, MAX_LINES - 1, "the end follows a new line");
    assert!(
        app.log.last().is_some_and(|line| line.contains("line 601")),
        "the newest line is the last one: {:?}",
        app.log.last()
    );
}

/// The `L` view covers the whole body area below the header and fills it: opened at the end it
/// shows the last lines that fit, the newest at the bottom, scrolling up shows earlier lines,
/// and scrolling back to the bottom follows new lines again. The page under it never shows
/// through a short log.
#[test]
fn the_log_view_fills_the_body_with_the_last_lines_and_follows_its_end() {
    let scratch = Scratch::new("log-fill");
    let store = store_at(&scratch.path("repo"));
    // A plan under the overlay: a row of it must not show through the log's clear body.
    store
        .add_plan("under", "the page under the log", "a request\n")
        .unwrap();
    let mut app = ArcApp::with_session_dir(store.clone(), Some(scratch.path("sessions")));
    let mut tail = LoopLogTail::new(&store);
    let path = scratch.path("repo/.radian/arc/loop.log");
    let mut log = String::new();
    for n in 0..120 {
        log.push_str(&format!("2026-10-24T10:00:00.000Z line {n}\n"));
    }
    fs::write(&path, &log).unwrap();
    app.apply_loop_log(tail.read());

    app.on_key(&key(KeyCode::Char('L')));
    let text = screen(&mut app, 80, 40);
    let rows: Vec<&str> = text.lines().collect();
    let top = rows
        .iter()
        .position(|row| row.contains("╭─ log"))
        .expect("the log frame is drawn");
    let bottom = rows
        .iter()
        .rposition(|row| row.contains("╰──"))
        .expect("the log frame ends");
    // A 40-row terminal: the header takes 2 rows, the footer 2, and the frame's own top and
    // bottom one each.
    assert_eq!(bottom - top - 1, 34, "the frame fills the body:\n{text}");
    assert!(
        rows[top + 1].contains("line 86"),
        "the first row is the first of the last page:\n{text}"
    );
    assert!(
        rows[bottom - 1].contains("line 119"),
        "the newest line is the last one:\n{text}"
    );
    for row in &rows[top..=bottom] {
        assert!(
            !row.contains("LANS")
                && !row.contains("priority")
                && !row.contains("retro")
                && !row.contains("the page under"),
            "the page under the log shows through: {row:?}\n{text}"
        );
    }

    // `k` scrolls one line up from the last page, `j` back down to its end.
    app.on_key(&key(KeyCode::Char('k')));
    let text = screen(&mut app, 80, 40);
    let rows: Vec<&str> = text.lines().collect();
    let top = rows.iter().position(|row| row.contains("╭─ log")).unwrap();
    let bottom = rows.iter().rposition(|row| row.contains("╰──")).unwrap();
    assert!(
        rows[top + 1].contains("line 85") && rows[bottom - 1].contains("line 118"),
        "scrolling up shows earlier lines:\n{text}"
    );
    app.on_key(&key(KeyCode::Char('j')));
    // At the bottom again, a new line is followed.
    log.push_str("2026-10-24T10:00:01.000Z line 120\n");
    fs::write(&path, &log).unwrap();
    app.apply_loop_log(tail.read());
    let text = screen(&mut app, 80, 40);
    let rows: Vec<&str> = text.lines().collect();
    let bottom = rows.iter().rposition(|row| row.contains("╰──")).unwrap();
    assert!(
        rows[bottom - 1].contains("line 120"),
        "the end follows a new line:\n{text}"
    );
}

/// A two-line log draws its two lines and leaves the rest of the body cleared (never the page
/// under it).
#[test]
fn a_short_log_leaves_the_rest_of_its_body_cleared() {
    let scratch = Scratch::new("log-short");
    let store = store_at(&scratch.path("repo"));
    // A plan under the overlay: a row of it must not show through the log's clear body.
    store
        .add_plan("under", "the page under the log", "a request\n")
        .unwrap();
    let mut app = ArcApp::with_session_dir(store.clone(), Some(scratch.path("sessions")));
    let mut tail = LoopLogTail::new(&store);
    let path = scratch.path("repo/.radian/arc/loop.log");
    fs::write(
        &path,
        "2026-10-24T10:00:00.000Z line 0\n2026-10-24T10:00:01.000Z line 1\n",
    )
    .unwrap();
    app.apply_loop_log(tail.read());
    app.on_key(&key(KeyCode::Char('L')));
    let text = screen(&mut app, 80, 40);
    let rows: Vec<&str> = text.lines().collect();
    let top = rows.iter().position(|row| row.contains("╭─ log")).unwrap();
    let bottom = rows.iter().rposition(|row| row.contains("╰──")).unwrap();
    assert!(rows[top + 1].contains("line 0"), "{text}");
    assert!(rows[top + 2].contains("line 1"), "{text}");
    for row in &rows[top + 3..bottom] {
        assert!(!row.contains("line"), "a short log draws no line: {row:?}");
    }
    for row in &rows[top..=bottom] {
        assert!(
            !row.contains("LANS")
                && !row.contains("priority")
                && !row.contains("retro")
                && !row.contains("the page under"),
            "the page under the log shows through: {row:?}\n{text}"
        );
    }
}

/// The shortest body `draw` accepts is one row: a 5-row terminal with no ticker leaves
/// `height - 2 header rows - 2 footer rows`. That row cannot hold the frame's top line, a log
/// row and its tail at once, so the view keeps the tail (the way the task and merge pages keep
/// theirs): it never promises a bottom frame it clipped away.
#[test]
fn a_one_row_body_keeps_the_log_views_frame_tail() {
    let scratch = Scratch::new("log-one-row");
    let store = store_at(&scratch.path("repo"));
    // A plan under the overlay: a row of it must not show through the log's clear body.
    store
        .add_plan("under", "the page under the log", "a request\n")
        .unwrap();
    let mut app = ArcApp::with_session_dir(store.clone(), Some(scratch.path("sessions")));
    let mut tail = LoopLogTail::new(&store);
    let path = scratch.path("repo/.radian/arc/loop.log");
    fs::write(&path, "2026-10-24T10:00:00.000Z line 0\n").unwrap();
    app.apply_loop_log(tail.read());
    assert!(app.ticker.is_none(), "the fixture adds no ticker row");
    app.on_key(&key(KeyCode::Char('L')));

    let text = screen(&mut app, 80, 5);
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(rows.len(), 5, "{text}");
    // Header, blank, the body's one row, the footer's rule and its key hints.
    assert!(rows[1].is_empty(), "the body is one row:\n{text}");
    assert_eq!(
        rows[2], "  ╰──",
        "the one-row body is the frame's tail, not a clipped top:\n{text}"
    );
    assert!(
        !text.contains("╭─ log") && !text.contains("PLANS"),
        "the log view is drawn as its tail alone, over a cleared body:\n{text}"
    );

    // One row more gives the body its top line back, still with the tail directly under it.
    let text = screen(&mut app, 80, 6);
    let rows: Vec<&str> = text.lines().collect();
    assert!(rows[2].contains("╭─ log"), "{text}");
    assert_eq!(rows[3], "  ╰──", "the two frame lines meet:\n{text}");

    // A terminal shorter than the header, the body and the footer draws nothing at all.
    let text = screen(&mut app, 80, 4);
    assert!(text.trim().is_empty(), "nothing fits:\n{text}");
}
