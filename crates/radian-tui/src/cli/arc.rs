//! `radian arc` with no subcommand: the arc TUI (ADR 0048 §1, §13).
//!
//! This loop enters the terminal, draws one frame, reads one key at a time, re-reads the store
//! every [`ARC_REFRESH`] seconds, and runs git (the remote main and a task's diff), the merge
//! page's data, a command's log and the owner's actions on worker threads so a frame never waits
//! on any of them. The TUI holds no `runner.lock` and runs no pass: when it opens and when `R`
//! is pressed, [`LoopStarter`] starts `radian arc serve` as a detached process if no loop
//! holds the lock, and quitting never stops it. `q` (or Ctrl-C) leaves through the same
//! clean-exit path as `attach`.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use radian_tui::arc::loop_log::LoopLogTail;
use radian_tui::arc::looping::{self, LoopStarter};
use radian_tui::arc::{ArcApp, ArcKey, actions, render};
use ratatui::layout::Rect;

use super::*;

/// How often the view re-reads the store and the open task's journal.
const ARC_REFRESH: Duration = Duration::from_secs(2);
/// The input poll: a key, a resize or an idle tick, at the same granularity as the other
/// full-screen views.
const ARC_POLL: Duration = Duration::from_millis(100);
/// How often an action in flight redraws its spinner (ADR 0027: a rotating frame every 250 ms).
const ARC_SPINNER: Duration = Duration::from_millis(250);

/// One worker thread's reply: what its body produced, and whether a panic was caught before
/// it could.
///
/// `panicked` makes the loop clear the screen before the next frame: a caught panic must not
/// leave anything it printed on the alternate screen.
struct WorkerReply {
    reply: GitReply,
    panicked: bool,
}

/// Runs one worker thread's body with its panic caught, and always sends a reply.
///
/// The loop learns that a worker finished only from the reply on `tx`: an owner action's
/// `pending`, a log's `log_pending`, a diff's or merge's request and the header's
/// `main_pending` are cleared by that reply and by nothing else. A thread that panicked before
/// its last statement would leave one of them set for the rest of the session — with `pending`
/// set every page key is ignored and the spinner never stops — so all five spawns run through
/// here.
///
/// `on_panic` builds the reply that thread would otherwise have sent: the failure value in the
/// reply's own shape, carrying the panic message.
fn spawn_reply<F, P>(tx: mpsc::Sender<WorkerReply>, body: F, on_panic: P)
where
    F: FnOnce() -> GitReply + Send + 'static,
    P: FnOnce(&str) -> GitReply + Send + 'static,
{
    std::thread::spawn(move || {
        let reply = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            Ok(reply) => WorkerReply {
                reply,
                panicked: false,
            },
            Err(payload) => WorkerReply {
                reply: on_panic(&panic_message(payload.as_ref())),
                panicked: true,
            },
        };
        // The loop may have left (a signal, a closed terminal): the reply is then dropped.
        let _ = tx.send(reply);
    });
}

/// The text of a caught panic payload. `panic!` produces a `&str` or a `String`; anything else
/// is named rather than guessed.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "a panic with no message".to_owned()
    }
}

/// One worker-thread reply's value.
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
pub(super) fn run_arc() -> ExitCode {
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
    let mut app = ArcApp::open(store);
    // The loop's log baseline is the file as it is when the TUI opens: a line the loop writes
    // afterwards is news for the ticker, whether the TUI starts that loop or finds it running.
    let mut tail = LoopLogTail::new(&app.store);
    app.apply_loop_log(tail.read());
    // The scheduling loop (ADR 0048 §13): the TUI never holds `runner.lock`. Opening it starts
    // `radian arc serve` as a detached process when no loop runs, and quitting leaves
    // that process alone.
    let mut starter = LoopStarter::new(app.store.clone());
    if let Err(text) = starter.start() {
        app.set_result(text, false);
    }
    looping::install_stop_handler();
    app.set_loop_status(starter.status());
    // Test seam (like `--panic-in-driver`): with `RADIAN_ARC_PANIC_ACTION` set, every owner
    // action's worker panics instead of running its library call, so the pty suite can check
    // that a panicked worker still replies and leaves the TUI responsive. Test-only: the
    // manual does not document it and radian never sets it itself.
    let panic_action = std::env::var_os("RADIAN_ARC_PANIC_ACTION").is_some();
    let (tx, rx) = mpsc::channel();
    let mut main_pending = false;
    let mut main_at = None;
    let mut diff_pending: Option<(String, u32)> = None;
    let mut merge_pending: Option<String> = None;
    let mut spinner_at = Instant::now();
    let mut refresh_at = Instant::now() + ARC_REFRESH;
    let mut dirty = true;
    // The next frame must repaint every cell: a worker panicked and may have printed over the
    // alternate screen.
    let mut full_redraw = false;
    loop {
        // SIGTERM closes the TUI, and nothing else: the loop it started keeps running in its
        // own session.
        if looping::signal_received() {
            break;
        }
        // Replies from the git workers: a stale diff (another task or attempt) is dropped by
        // the app.
        while let Ok(WorkerReply { reply, panicked }) = rx.try_recv() {
            if panicked {
                // The caught panic's reply is a normal failure value; the screen is cleared
                // below so nothing the panic printed stays behind.
                full_redraw = true;
                dirty = true;
            }
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
            if full_redraw {
                // Clear the screen and reset the frame buffer (`resize` with the area the
                // terminal already has), so the next draw writes every cell instead of only
                // the ones that changed since the previous frame.
                if let Ok(size) = terminal.size() {
                    let _ = terminal.resize(Rect::new(0, 0, size.width, size.height));
                }
                full_redraw = false;
            }
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
            let label = action.label();
            spawn_reply(
                tx.clone(),
                move || {
                    if panic_action {
                        panic!("arc action worker panic test");
                    }
                    GitReply::Action(actions::run(&action, &store, &actions::Library))
                },
                move |message| {
                    GitReply::Action(actions::Outcome {
                        ok: false,
                        text: format!("{label} failed: it panicked ({message})"),
                    })
                },
            );
            dirty = true;
        }
        // A command's log is read on a worker thread, from its end and bounded: a big log
        // never blocks the frame or loads whole into memory.
        if let Some(request) = app.take_log_request() {
            let reply_request = request.clone();
            spawn_reply(
                tx.clone(),
                move || {
                    let result = radian_tui::arc::read_log_tail(&request.path);
                    GitReply::Log { request, result }
                },
                move |message| GitReply::Log {
                    request: reply_request,
                    result: Err(format!("it panicked: {message}")),
                },
            );
            dirty = true;
        }
        // The header's remote main: once at start, then every refresh. Never on the draw
        // path.
        if !main_pending && main_at.is_none_or(|at: Instant| at.elapsed() >= ARC_REFRESH) {
            main_pending = true;
            let store = app.store.clone();
            spawn_reply(
                tx.clone(),
                move || {
                    let sha = radian_arc::read::remote_main_short(&store).ok().flatten();
                    GitReply::Main(sha)
                },
                |_| GitReply::Main(None),
            );
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
            let reply_id = id.clone();
            spawn_reply(
                tx.clone(),
                move || {
                    let result =
                        radian_arc::read::task_diff(&store, &id, (attempt > 0).then_some(attempt))
                            .map_err(|error| error.message());
                    GitReply::Diff {
                        id,
                        attempt,
                        result,
                    }
                },
                move |message| GitReply::Diff {
                    id: reply_id,
                    attempt,
                    result: Err(format!("it panicked: {message}")),
                },
            );
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
            let reply_id = id.clone();
            spawn_reply(
                tx.clone(),
                move || {
                    let running = radian_arc::gate::background::running(&store, &id);
                    let result = radian_arc::read::merge_view(&store, &id, running)
                        .map_err(|error| error.message());
                    GitReply::Merge {
                        id,
                        result: Box::new(result),
                    }
                },
                move |message| GitReply::Merge {
                    id: reply_id,
                    result: Box::new(Err(format!("it panicked: {message}"))),
                },
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use radian_tui::arc::Page;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A fresh arc store in a temp directory that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "radian-arc-worker-{label}-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(".radian/arc")).expect("the scratch dir is created");
            std::fs::write(
                dir.join(".radian/arc/config.toml"),
                radian_arc::store::config::default_text("main"),
            )
            .expect("arc's config is written");
            Self(dir)
        }

        fn store(&self) -> radian_arc::store::Store {
            radian_arc::store::Store::at(&self.0)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The helper is what makes the reply unconditional: a body that panics still sends the
    /// failure value its caller built, marked as a panic.
    ///
    /// The panic is real and the default hook prints its one line; the test harness captures it.
    #[test]
    fn a_panicking_worker_still_sends_its_failure_reply() {
        let (tx, rx) = mpsc::channel();
        spawn_reply(
            tx,
            || panic!("worker panic"),
            |message| {
                GitReply::Action(actions::Outcome {
                    ok: false,
                    text: format!("merge failed: it panicked ({message})"),
                })
            },
        );
        let reply = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("a panicked worker still replies");
        assert!(reply.panicked);
        let GitReply::Action(outcome) = reply.reply else {
            panic!("the failure value is the action's Outcome");
        };
        assert!(!outcome.ok);
        assert_eq!(outcome.text, "merge failed: it panicked (worker panic)");
    }

    /// A panicked owner action must not leave `pending` set: the reply is a failed `Outcome`,
    /// `finish_action` stops the spinner, and the very next page key is handled (with `pending`
    /// set `on_key` returns `Ignored`).
    ///
    /// The panic is real and the default hook prints its one line; the test harness captures it.
    #[test]
    fn a_panicked_action_clears_pending_and_the_next_page_key_is_handled() {
        let scratch = Scratch::new("action-panic");
        let mut app = ArcApp::with_session_dir(scratch.store(), None);
        app.queue_action(actions::Action::Confirm("PL0001-x".to_owned()));
        let action = app.take_action().expect("the action is queued");
        assert!(
            app.pending.is_some(),
            "the spinner runs while it is in flight"
        );

        let label = action.label();
        let (tx, rx) = mpsc::channel();
        spawn_reply(
            tx,
            move || {
                // The action is moved in and never run: the panic is the point.
                drop(action);
                panic!("the action runner panicked");
            },
            move |message| {
                GitReply::Action(actions::Outcome {
                    ok: false,
                    text: format!("{label} failed: it panicked ({message})"),
                })
            },
        );
        let reply = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("a panicked action still replies");
        assert!(reply.panicked);
        let GitReply::Action(outcome) = reply.reply else {
            panic!("the failure value is the action's Outcome");
        };
        app.finish_action(outcome);
        assert!(app.pending.is_none(), "the spinner must stop");
        let result = app.result.as_ref().expect("the failure line is kept");
        assert!(result.failed);
        assert!(
            result
                .text
                .contains("confirm failed: it panicked (the action runner panicked)"),
            "{}",
            result.text
        );

        assert_eq!(app.on_key(&key(KeyCode::Char('b'))), ArcKey::Redraw);
        assert_eq!(app.page, Page::Board, "the page key was handled");
    }
}
