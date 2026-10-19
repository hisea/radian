//! `radian attach [session] [--follow]` (ADR 0043 decision 8): the read-only view of another
//! session's committed facts. It reads the journal file and nothing else - no socket, no token,
//! no write - with the poll interval that serves both the input and the tail. Without a
//! `session` it first shows this workspace's picker (the `--pick` rows, without the "new
//! session" row) and attaches to the choice, in the same terminal.

use super::*;

/// How often [`run_attach`] reads: one poll serves the input **and**, with `--follow`, the
/// journal, so a key press and a newly committed line are both seen inside the brief's 250 ms
/// ceiling. Draws are not on this tick: a still frame is not redrawn at all.
const ATTACH_POLL: Duration = Duration::from_millis(50);

/// Which session the view reads: the id named on the command line (already checked to exist,
/// before the terminal is entered), or this workspace's picker, whose choice is only known once
/// it is up.
enum Start {
    Id(String),
    Pick(Box<radian_tui::picker::Picker>),
}

/// `radian attach [<session>] [--follow]` (ADR 0043): the read-only view
/// of another session's committed facts. It reads the journal file and nothing else — no
/// socket, no token, no write — and the observed session is never told.
///
/// Exit codes follow ADR 0032 A3: `2` for a refusal (an unknown id, no terminal), `0` for a
/// clean quit. With an id the refusal is decided **before** the terminal is entered, so stderr
/// keeps its one line and nothing on disk is touched. Without one, this workspace's picker is
/// the way in; `Enter` attaches to the choice, `Esc`/`q` leaves without attaching (exit 0), and
/// no session file, socket or model is ever created on this path.
pub(super) fn run_attach(dir_flag: Option<&Path>, id: Option<&str>, follow: bool) -> ExitCode {
    // The id becomes a file name, so it goes through the same rule `--resume` uses
    // (`validate_session_id`): a path-like id must not name a file outside the session
    // directory. The refusal comes before the directory is even resolved, so nothing is
    // looked up, created or read.
    if let Some(id) = id
        && let Err(error) = radian_host::validate_session_id(id)
    {
        eprintln!("radian: {error}");
        return ExitCode::from(2);
    }
    // The same resolution as `--resume`: `--session-dir`, then `RADIAN_SESSION_DIR`, then the
    // data directory (ADR 0020).
    let dir = match radian_host::session_dir(dir_flag) {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("radian: {error}");
            return ExitCode::from(2);
        }
    };
    let start = match id {
        Some(id) => {
            if !dir.join(format!("{id}.jsonl")).exists() {
                eprintln!("radian: no session {id:?} in {}", dir.display());
                return ExitCode::from(2);
            }
            Start::Id(id.to_owned())
        }
        // The picker is loaded here, **before** the terminal is entered: a workspace with
        // nothing to attach is one line on the normal screen, never an alternate screen that
        // flashes and leaves. It reads the session index (a disposable cache, ADR 0006 §10)
        // and nothing else; no session file is created.
        None => match workspace_picker(&dir) {
            Ok(Some(picker)) => Start::Pick(Box::new(picker)),
            Ok(None) => {
                eprintln!("radian: no sessions for this workspace; nothing to attach");
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("radian: {error}");
                return ExitCode::from(2);
            }
        },
    };
    // No mouse reporting: this view has no mouse features, so the terminal keeps its native
    // selection and copy.
    let (mut guard, mut terminal) = match enter_terminal(false) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("radian: {error}");
            return ExitCode::from(2);
        }
    };
    // The picker and the view share this one terminal (as `--pick` and its session share
    // theirs): Enter opens the view without the alternate screen closing in between.
    let id = match start {
        Start::Id(id) => id,
        Start::Pick(mut picker) => match run_picker(&mut terminal, picker.as_mut()) {
            Some(id) => id,
            // `Esc`/`q`, or the terminal going away: nothing was attached to and nothing was
            // created. Leave through the ordinary clean-exit path.
            None => {
                guard.restore();
                return ExitCode::SUCCESS;
            }
        },
    };
    let path = dir.join(format!("{id}.jsonl"));
    let mut view = match radian_tui::attach::AttachView::open(&path, &id) {
        Ok(view) => view,
        Err(error) => {
            // A file that was there a moment ago (the picker listed it, or the caller named
            // it) and is not any more: the same one-line refusal as an unknown id.
            guard.restore();
            eprintln!("radian: {error}");
            return ExitCode::from(2);
        }
    };
    let failure = run_view(&mut terminal, &mut view, follow);
    guard.restore();
    match failure {
        None => ExitCode::SUCCESS,
        Some(error) => {
            eprintln!("radian: {error}");
            ExitCode::from(1)
        }
    }
}

/// The picker an `attach` without an id shows: this workspace's sessions, **without** the "new
/// session" row (`without_new` — attach creates nothing). `Ok(None)` means the workspace has no
/// sessions. The rows and the workspace are the ones `--pick` resolves (`prepare_picker`), so
/// an attach picker and a `--pick` screen agree by construction; the workspace is the current
/// directory through the TUI's coding root, exactly as `--list` filters it.
fn workspace_picker(dir: &Path) -> Result<Option<radian_tui::picker::Picker>, String> {
    let workspace = radian_host::session_workspace(Some(&coding_root(None)?.path))?;
    Ok(prepare_picker(dir, &workspace)?.map(radian_tui::picker::Picker::without_new))
}

/// The picker's loop over the entered terminal: the same `Picker` and the same keys the
/// `--pick` screen runs (`Up`/`Down`, type to filter, `Tab` full-text search), minus the "new
/// session" row. `Some(id)` is the chosen session; `None` means the user left (`Esc`/`Ctrl-C`)
/// or the terminal went away — either way nothing is attached to.
fn run_picker(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    picker: &mut radian_tui::picker::Picker,
) -> Option<String> {
    // No config is read on this path (exactly as with an id), so the picker draws with the
    // theme and glyph set an unattached `App` starts with.
    let theme = radian_tui::theme::Theme::default();
    let glyphs = radian_tui::footer::Glyphs::default();
    let mut dirty = true;
    loop {
        if dirty {
            let drawn = terminal.draw(|frame| {
                let area = frame.area();
                radian_tui::picker::draw(frame, area, picker, &theme, glyphs);
            });
            if drawn.is_err() {
                // The terminal is gone (the pty master closed): leave the ordinary way.
                return None;
            }
            dirty = false;
        }
        match read_input(ATTACH_POLL) {
            Input::Gone => return None,
            Input::Idle => {}
            Input::Event(Event::Key(key)) => match picker.on_key(key) {
                Some(radian_tui::picker::Outcome::Resume(id)) => return Some(id),
                // `without_new` is what makes `New` unreachable here; `Quit` is `Esc`/`Ctrl-C`.
                // Both mean "attach to nothing" (as does a terminal that went away).
                Some(_) => return None,
                // A key the picker consumed itself (a filter character, a move): the frame
                // may have changed.
                None => dirty = true,
            },
            // A resize re-lays the rows out on the next draw.
            Input::Event(Event::Resize(..)) => dirty = true,
            Input::Event(_) => {}
        }
    }
}

/// The read-only view's loop over an entered terminal: draw the frame, poll the input and,
/// with `--follow`, the journal. Returns the one-line failure when the terminal or the journal
/// broke (`None` is a clean quit).
fn run_view(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    view: &mut radian_tui::attach::AttachView,
    follow: bool,
) -> Option<String> {
    let mut dirty = true;
    loop {
        if dirty {
            if let Err(error) = terminal.draw(|frame| view.render(frame)) {
                // The terminal is gone (the pty master closed): leave the ordinary way.
                return Some(error.to_string());
            }
            dirty = false;
        }
        match read_input(ATTACH_POLL) {
            Input::Gone => return None,
            Input::Idle => {
                if follow {
                    match view.poll() {
                        // Durable facts only: the frame changes when a message was committed,
                        // a replacement landed or the turn's boundary moved.
                        Ok(changed) => dirty |= changed,
                        Err(error) => return Some(error),
                    }
                }
            }
            Input::Event(Event::Key(key)) => match view.on_key(&key) {
                radian_tui::attach::AttachKey::Quit => return None,
                radian_tui::attach::AttachKey::Redraw => dirty = true,
                // Swallowed: no input event, no command, no write (ADR 0043 D14).
                radian_tui::attach::AttachKey::Ignored => {}
            },
            // A resize re-lays the transcript out on the next draw.
            Input::Event(Event::Resize(..)) => dirty = true,
            Input::Event(_) => {}
        }
    }
}
