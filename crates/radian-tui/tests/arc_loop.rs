//! The arc TUI's scheduling loop (ADR 0048 decision 13): the lock, the background pass, the
//! ticker and the log, and the quit float.
//!
//! The pass is the seam: these tests hand in a scripted [`Pass`] instead of running real
//! workers (the pty suite runs the real one end to end). The lock, the thread and the store are
//! real: `runner.lock` in a scratch store is the same flock `radian arc run` takes.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use radian_arc::run::Report;
use radian_arc::store::model::TaskState;
use radian_arc::store::{Error, Store, config};
use radian_tui::arc::looping::{ArcLoop, LoopEvent, LoopStatus, Pass};
use radian_tui::arc::{ArcApp, ArcKey, Float, render};
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
            "radian-arc-loop-{label}-{}-{n}",
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

/// A store in `dir`: arc's `config.toml` and the layout it expects.
fn store(dir: &Path) -> Store {
    let root = dir.join(".radian/arc");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("config.toml"), config::default_text("main")).unwrap();
    Store::at(dir)
}

/// A plan with one `doing.running` task: the shape the quit float asks about.
fn running_task(store: &Store) -> (String, String) {
    let plan = store
        .add_plan("loop", "the loop plan", "a request\n")
        .unwrap();
    let task = store
        .add_task(
            &plan,
            "work",
            "the work task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .unwrap();
    store
        .change_plan(&plan, |plan| {
            plan.status = "confirmed".to_owned();
            plan.auto = "run".to_owned();
            Ok(())
        })
        .unwrap();
    store
        .change_task(&task, |task| {
            task.state = TaskState::Doing;
            task.phase = "running".to_owned();
            task.attempt = 1;
            task.attempt_id = Some(format!("{}-1", task.id));
            Ok(())
        })
        .unwrap();
    (plan, task)
}

/// A scripted pass: it reports the thread it ran on, how often it ran, and writes the lines a
/// scripted pass gives it. An optional failure exercises the error line.
struct FakePass {
    calls: Arc<AtomicUsize>,
    started: Mutex<Option<mpsc::Sender<ThreadId>>>,
    lines: Vec<String>,
    delay: Duration,
    fail: bool,
}

impl FakePass {
    fn new(
        lines: &[&str],
        delay: Duration,
        fail: bool,
    ) -> (Self, Arc<AtomicUsize>, Receiver<ThreadId>) {
        let (tx, rx) = mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                calls: Arc::clone(&calls),
                started: Mutex::new(Some(tx)),
                lines: lines.iter().map(|line| (*line).to_owned()).collect(),
                delay,
                fail,
            },
            calls,
            rx,
        )
    }
}

impl Pass for FakePass {
    fn run(
        &self,
        _store: &Store,
        emit: &mut dyn FnMut(&str),
        _stop: &std::sync::atomic::AtomicBool,
    ) -> Result<Report, Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut guard) = self.started.lock()
            && let Some(sender) = guard.take()
        {
            let _ = sender.send(std::thread::current().id());
        }
        std::thread::sleep(self.delay);
        for line in &self.lines {
            emit(&format!("{line}\n"));
        }
        if self.fail {
            return Err(Error::Failed("the store did not settle".to_owned()));
        }
        Ok(Report {
            transitions: self.lines.clone(),
            waiting: Vec::new(),
            notes: Vec::new(),
        })
    }
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

/// Drains the loop until `predicate` holds, collecting every event into `app`.
fn drain_until(
    app: &mut ArcApp,
    arc_loop: &mut ArcLoop,
    what: &str,
    predicate: impl Fn(&ArcApp) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        for event in arc_loop.drain() {
            app.push_loop_event(event);
        }
        if predicate(app) {
            return;
        }
        assert!(Instant::now() < deadline, "{what} did not happen");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_tui_takes_the_lock_and_runs_the_pass_on_a_background_thread() {
    let scratch = Scratch::new("takes-lock");
    let store = store(&scratch.path("repo"));
    let (fake, calls, started) = FakePass::new(
        &["TK0001-work-1 started -> doing.running"],
        Duration::from_millis(10),
        false,
    );
    let mut arc_loop =
        ArcLoop::with_pass_and_interval(store.clone(), Box::new(fake), Duration::from_secs(30));
    assert_eq!(
        arc_loop.status(),
        LoopStatus::ThisWindow,
        "a free lock makes this window the loop"
    );
    let thread = started
        .recv_timeout(Duration::from_secs(5))
        .expect("the pass started");
    assert_ne!(
        thread,
        std::thread::current().id(),
        "the pass must run off the UI thread"
    );
    assert!(
        store.try_runner_lock().unwrap().is_none(),
        "the loop holds runner.lock for its life"
    );
    assert!(calls.load(Ordering::SeqCst) >= 1);

    arc_loop.stop();
    assert_eq!(arc_loop.status(), LoopStatus::Stopped);
    assert!(
        store.try_runner_lock().unwrap().is_some(),
        "quitting releases runner.lock"
    );
    // A stopped loop never takes it again.
    arc_loop.refresh();
    assert_eq!(arc_loop.status(), LoopStatus::Stopped);
    assert!(store.try_runner_lock().unwrap().is_some());
}

#[test]
fn a_viewer_reports_the_holders_pid_and_takes_the_lock_after_it_exits() {
    let scratch = Scratch::new("viewer");
    let store = store(&scratch.path("repo"));
    let held = store
        .try_runner_lock()
        .unwrap()
        .expect("the test takes the lock first");
    let (fake, calls, started) = FakePass::new(&[], Duration::from_millis(10), false);
    let mut arc_loop =
        ArcLoop::with_pass_and_interval(store.clone(), Box::new(fake), Duration::from_secs(30));
    assert_eq!(
        arc_loop.status(),
        LoopStatus::Elsewhere(Some(std::process::id())),
        "the header names the pid the holder recorded"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a viewer runs no pass");
    arc_loop.refresh();
    assert_eq!(
        arc_loop.status(),
        LoopStatus::Elsewhere(Some(std::process::id()))
    );
    assert!(
        started.recv_timeout(Duration::from_millis(100)).is_err(),
        "a viewer starts no pass thread"
    );

    drop(held);
    arc_loop.refresh();
    assert_eq!(
        arc_loop.status(),
        LoopStatus::ThisWindow,
        "the next refresh after the holder exits takes the lock"
    );
    started
        .recv_timeout(Duration::from_secs(5))
        .expect("the pass started once this window is the loop");
    arc_loop.stop();
    assert!(store.try_runner_lock().unwrap().is_some());
}

#[test]
fn transitions_reach_the_ticker_and_the_log_and_a_nudge_runs_the_pass_at_once() {
    let scratch = Scratch::new("ticker");
    let store = store(&scratch.path("repo"));
    let (fake, calls, _started) = FakePass::new(
        &[
            "TK0001-work-1 started -> doing.running",
            "TK0001-work-1 verify started (pid 42)",
        ],
        Duration::from_millis(10),
        false,
    );
    let mut arc_loop =
        ArcLoop::with_pass_and_interval(store.clone(), Box::new(fake), Duration::from_secs(3600));
    let mut app = ArcApp::with_session_dir(store.clone(), Some(scratch.path("sessions")));
    app.set_loop_status(arc_loop.status());
    drain_until(&mut app, &mut arc_loop, "both transition lines", |app| {
        app.log.len() >= 3
    });
    assert_eq!(
        app.ticker.as_deref(),
        Some("TK0001-work-1 verify started (pid 42)"),
        "the ticker holds the last transition"
    );
    assert!(
        app.log
            .iter()
            .any(|line| line.contains("started -> doing.running")),
        "the log keeps every transition: {:?}",
        app.log
    );
    // The footer draws the ticker, and `L` opens the log view.
    let text = screen(&mut app, 120, 30);
    assert!(
        text.contains("TK0001-work-1 verify started (pid 42)"),
        "the ticker is drawn:\n{text}"
    );
    assert_eq!(app.on_key(&key(KeyCode::Char('L'))), ArcKey::Redraw);
    assert!(app.log_open);
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("╭─ log"), "the log view is drawn:\n{text}");
    assert!(
        text.contains("TK0001-work-1 started -> doing.running"),
        "the log view shows the transitions:\n{text}"
    );
    assert_eq!(app.on_key(&key(KeyCode::Char('L'))), ArcKey::Redraw);
    assert!(!app.log_open, "L closes the log again");

    // A quiet pass would wait an hour; a nudge runs the next one now.
    let before = calls.load(Ordering::SeqCst);
    arc_loop.nudge();
    let deadline = Instant::now() + Duration::from_secs(5);
    while calls.load(Ordering::SeqCst) <= before {
        assert!(
            Instant::now() < deadline,
            "the nudge did not run the pass at once"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    arc_loop.stop();
}

#[test]
fn a_pass_error_is_a_ticker_line_and_the_loop_stays_alive() {
    let scratch = Scratch::new("pass-error");
    let store = store(&scratch.path("repo"));
    let (fake, calls, started) = FakePass::new(&[], Duration::from_millis(10), true);
    let mut arc_loop =
        ArcLoop::with_pass_and_interval(store.clone(), Box::new(fake), Duration::from_secs(3600));
    started
        .recv_timeout(Duration::from_secs(5))
        .expect("the pass ran");
    let mut app = ArcApp::with_session_dir(store.clone(), Some(scratch.path("sessions")));
    drain_until(&mut app, &mut arc_loop, "the error line", |app| {
        app.ticker.is_some()
    });
    assert_eq!(
        app.ticker.as_deref(),
        Some("radian arc: the store did not settle"),
        "the error is the line `--watch` prints"
    );
    assert_eq!(
        arc_loop.status(),
        LoopStatus::ThisWindow,
        "an error does not end the loop"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the next pass waits the interval"
    );
    arc_loop.stop();
    assert!(store.try_runner_lock().unwrap().is_some());
}

#[test]
fn quit_asks_while_an_attempt_runs_and_only_this_window_holds_the_loop() {
    let scratch = Scratch::new("quit-float");
    let store = store(&scratch.path("repo"));
    running_task(&store);
    let mut app = ArcApp::with_session_dir(store.clone(), Some(scratch.path("sessions")));
    app.set_loop_status(LoopStatus::ThisWindow);
    assert_eq!(app.running_attempts(), 1);

    // `q` asks first; `n` stays.
    assert_eq!(app.on_key(&key(KeyCode::Char('q'))), ArcKey::Redraw);
    assert_eq!(app.float, Some(Float::Quit { running: 1 }));
    assert_eq!(app.on_key(&key(KeyCode::Char('n'))), ArcKey::Redraw);
    assert_eq!(app.float, None);
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("PLANS"), "the view stayed:\n{text}");

    // The float's own text says what happens to the attempt.
    assert_eq!(app.on_key(&key(KeyCode::Char('q'))), ArcKey::Redraw);
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("the loop stops"), "{text}");
    assert!(
        text.contains("the next loop reconciles it"),
        "the float says the attempt is reconciled by the next loop:\n{text}"
    );
    assert_eq!(app.on_key(&key(KeyCode::Char('y'))), ArcKey::Quit);

    // A viewer does not ask: it runs nothing.
    app.set_loop_status(LoopStatus::Elsewhere(Some(7)));
    assert_eq!(app.on_key(&key(KeyCode::Char('q'))), ArcKey::Quit);
    assert_eq!(app.float, None);

    // With this window the loop but nothing running, `q` leaves at once.
    store
        .change_task(&store.task_ids().unwrap()[0], |task| {
            task.state = TaskState::Todo;
            task.phase = "ready".to_owned();
            Ok(())
        })
        .unwrap();
    app.refresh();
    app.set_loop_status(LoopStatus::ThisWindow);
    assert_eq!(app.running_attempts(), 0);
    assert_eq!(app.on_key(&key(KeyCode::Char('q'))), ArcKey::Quit);
    assert_eq!(app.float, None);
}

/// The log view takes the scroll keys and leaves the page when `L` closes it.
#[test]
fn the_log_view_scrolls() {
    let scratch = Scratch::new("log-scroll");
    let store = store(&scratch.path("repo"));
    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    for n in 0..40 {
        app.push_loop_event(LoopEvent::Transition(format!("line {n}")));
    }
    app.on_key(&key(KeyCode::Char('L')));
    assert_eq!(app.on_key(&key(KeyCode::Char('j'))), ArcKey::Redraw);
    assert_eq!(app.log_scroll, 1);
    assert_eq!(app.on_key(&key(KeyCode::Char('k'))), ArcKey::Redraw);
    assert_eq!(app.log_scroll, 0);
    assert_eq!(app.on_key(&key(KeyCode::Char('f'))), ArcKey::Redraw);
    assert_eq!(app.log_scroll, 39);
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert!(!app.log_open);
}
