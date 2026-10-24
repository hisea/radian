//! `radian arc` with no subcommand: the arc TUI (ADR 0048 §1, §13).
//!
//! This loop enters the terminal, draws one frame, reads one key at a time, re-reads the store
//! every [`ARC_REFRESH`] seconds, and runs git (the remote main and a task's diff), the merge
//! page's data, a command's log and the owner's actions on worker threads so a frame never waits
//! on any of them. The TUI holds no `runner.lock` and runs no pass: when it opens and when `R`
//! is pressed, [`LoopStarter`] starts `radian arc run --watch` as a detached process if no loop
//! holds the lock, and quitting never stops it. `q` (or Ctrl-C) leaves through the same
//! clean-exit path as `attach`.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use radian_tui::arc::loop_log::LoopLogTail;
use radian_tui::arc::looping::{self, LoopStarter};
use radian_tui::arc::{ArcApp, ArcKey, actions, render};

use super::*;

/// How often the view re-reads the store and the open task's journal.
const ARC_REFRESH: Duration = Duration::from_secs(2);
/// The input poll: a key, a resize or an idle tick, at the same granularity as the other
/// full-screen views.
const ARC_POLL: Duration = Duration::from_millis(100);
/// How often an action in flight redraws its spinner (ADR 0027: a rotating frame every 250 ms).
const ARC_SPINNER: Duration = Duration::from_millis(250);

/// One worker-thread reply.
enum GitReply {
    /// The remote main's short sha (`None` when the ref does not exist).
    Main(Option<String>),
    /// A task's diff.
    Diff {
        id: String,
        attempt: u32,
        result: Result<radian_arc::read::TaskDiff, String>,
    },
    /// The merge page's data.
    Merge {
        id: String,
        result: Box<Result<radian_arc::read::MergeView, String>>,
    },
    /// An owner action's line.
    Action(actions::Outcome),
    /// A command's log, read on a worker thread.
    Log {
        request: radian_tui::arc::LogRequest,
        result: Result<String, String>,
    },
}

/// `radian arc`: open the arc TUI. Without setup the refusal is one line on the normal screen
/// (exit `3`); with it, the alternate screen is entered and restored on every exit path.
pub(super) fn run_arc(config: Option<&Path>) -> ExitCode {
    let dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("radian arc: cannot read the current directory: {error}");
            return ExitCode::from(1);
        }
    };
    let store = match radian_arc::store::Store::open(&dir) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("radian arc: {}", error.message());
            return ExitCode::from(error.exit().code());
        }
    };
    let (mut guard, mut terminal) = match enter_terminal(false) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("radian: {error}");
            return ExitCode::from(2);
        }
    };
    // The theme is the radian TUI's theme: the same `config.toml` (the same precedence, through
    // `radian_host::config::resolved_path` with the top-level `--config`), the same `theme::load`
    // call and the same `NO_COLOR` and truecolor checks, read once when the TUI opens (brief
    // TK0087). A theme changed while the TUI runs shows the next time it opens. The load's
    // warnings, and a config that cannot be read, are one footer line; the TUI still opens with
    // the theme the load fell back to.
    let config_path = radian_host::config::resolved_path(config);
    let open_theme = radian_tui::arc::open_theme(
        config_path.as_deref(),
        radian_tui::theme::Theme::no_color_env(),
        radian_tui::theme::truecolor_env(),
        radian_host::themes_dir().as_deref(),
    );
    let mut app = ArcApp::open_with_theme(store, open_theme);
    // The loop's log baseline is the file as it is when the TUI opens: a line the loop writes
    // afterwards is news for the ticker, whether the TUI starts that loop or finds it running.
    let mut tail = LoopLogTail::new(&app.store);
    app.apply_loop_log(tail.read());
    // The scheduling loop (ADR 0048 §13): the TUI never holds `runner.lock`. Opening it starts
    // `radian arc run --watch` as a detached process when no loop runs, and quitting leaves
    // that process alone.
    let mut starter = LoopStarter::new(app.store.clone());
    if let Err(text) = starter.start() {
        // A loop that already holds the lock makes this fail before the first frame: the
        // open-time theme warning stays, with the start's line after it (brief TK0087).
        app.add_result(text, false);
    }
    looping::install_stop_handler();
    app.set_loop_status(starter.status());
    let (tx, rx) = mpsc::channel();
    let mut main_pending = false;
    let mut main_at = None;
    let mut diff_pending: Option<(String, u32)> = None;
    let mut merge_pending: Option<String> = None;
    let mut spinner_at = Instant::now();
    let mut refresh_at = Instant::now() + ARC_REFRESH;
    let mut dirty = true;
    loop {
        // SIGTERM closes the TUI, and nothing else: the loop it started keeps running in its
        // own session.
        if looping::signal_received() {
            break;
        }
        // Replies from the git workers: a stale diff (another task or attempt) is dropped by
        // the app.
        while let Ok(reply) = rx.try_recv() {
            match reply {
                GitReply::Main(sha) => {
                    app.apply_main(sha);
                    main_pending = false;
                    main_at = Some(Instant::now());
                    dirty = true;
                }
                GitReply::Diff {
                    id,
                    attempt,
                    result,
                } => {
                    if diff_pending.as_ref() == Some(&(id.clone(), attempt)) {
                        diff_pending = None;
                    }
                    if app.apply_diff(&id, attempt, result) {
                        dirty = true;
                    }
                }
                GitReply::Merge { id, result } => {
                    if merge_pending.as_deref() == Some(id.as_str()) {
                        merge_pending = None;
                    }
                    if app.apply_merge(&id, *result) {
                        dirty = true;
                    }
                }
                GitReply::Action(outcome) => {
                    app.finish_action(outcome);
                    dirty = true;
                }
                GitReply::Log { request, result } => {
                    if app.apply_log(&request, result) {
                        dirty = true;
                    }
                }
            }
        }
        app.apply_pending_scroll();
        if dirty {
            if terminal
                .draw(|frame| render::draw(frame, &mut app))
                .is_err()
            {
                break;
            }
            dirty = false;
        }
        // An owner action runs on a worker thread; the footer's spinner advances until its
        // reply, so the view keeps drawing while a merge or a `start` takes its time.
        if app.pending.is_some() && spinner_at.elapsed() >= ARC_SPINNER {
            spinner_at = Instant::now();
            dirty = true;
        }
        if let Some(action) = app.take_action() {
            spinner_at = Instant::now();
            let store = app.store.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let outcome = actions::run(&action, &store, &actions::Library);
                let _ = tx.send(GitReply::Action(outcome));
            });
            dirty = true;
        }
        // A command's log is read on a worker thread, from its end and bounded: a big log
        // never blocks the frame or loads whole into memory.
        if let Some(request) = app.take_log_request() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let result = radian_tui::arc::read_log_tail(&request.path);
                let _ = tx.send(GitReply::Log { request, result });
            });
            dirty = true;
        }
        // The header's remote main: once at start, then every refresh. Never on the draw
        // path.
        if !main_pending && main_at.is_none_or(|at: Instant| at.elapsed() >= ARC_REFRESH) {
            main_pending = true;
            let store = app.store.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let sha = radian_arc::read::remote_main_short(&store).ok().flatten();
                let _ = tx.send(GitReply::Main(sha));
            });
        }
        // The diff: the task page's selected attempt, or the merge page's. A request per
        // attempt, off the UI thread. A switch while one is in flight starts the new
        // attempt's request at once; the old reply is dropped by the app.
        if let Some((id, attempt)) = app.diff_request()
            && diff_pending.as_ref() != Some(&(id.clone(), attempt))
        {
            app.mark_diff_loading((id.clone(), attempt));
            diff_pending = Some((id.clone(), attempt));
            let store = app.store.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let result =
                    radian_arc::read::task_diff(&store, &id, (attempt > 0).then_some(attempt))
                        .map_err(|error| error.message());
                let _ = tx.send(GitReply::Diff {
                    id,
                    attempt,
                    result,
                });
            });
            dirty = true;
        }
        // The merge page's stage rows, review and commits. The gate lock tells the view which
        // stage is running; the probe never starts or stops anything.
        if let Some(id) = app.merge_request()
            && merge_pending.as_deref() != Some(id.as_str())
        {
            app.mark_merge_loading();
            merge_pending = Some(id.clone());
            let store = app.store.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let running = radian_arc::gate::background::running(&store, &id);
                let result = radian_arc::read::merge_view(&store, &id, running)
                    .map_err(|error| error.message());
                let _ = tx.send(GitReply::Merge {
                    id,
                    result: Box::new(result),
                });
            });
            dirty = true;
        }
        let wait = ARC_POLL.min(refresh_at.saturating_duration_since(Instant::now()));
        match read_input(wait) {
            Input::Gone => break,
            Input::Idle => {}
            Input::Event(Event::Key(key)) => match app.on_key(&key) {
                ArcKey::Quit => break,
                ArcKey::StartLoop => {
                    if let Err(text) = starter.start() {
                        app.set_result(text, false);
                    }
                    app.set_loop_status(starter.status());
                    dirty = true;
                }
                ArcKey::Redraw => dirty = true,
                ArcKey::Ignored => {}
            },
            Input::Event(Event::Resize(..)) => dirty = true,
            Input::Event(_) => {}
        }
        if Instant::now() >= refresh_at {
            // A start in flight shows `starting` until a probe sees its lock; a start that
            // failed is one footer line.
            if let Some(line) = starter.poll() {
                app.set_result(line, false);
            }
            app.set_loop_status(starter.status());
            app.apply_loop_log(tail.read());
            app.refresh();
            refresh_at = Instant::now() + ARC_REFRESH;
            dirty = true;
        }
    }
    // Quitting closes this window and nothing else: no signal to a loop, whether this TUI
    // started it or not (ADR 0048 §13).
    drop(starter);
    guard.restore();
    ExitCode::SUCCESS
}
