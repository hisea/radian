//! `radian attach <session> [--follow]` (ADR 0043 decision 8): the read-only view of another
//! session's committed facts. It reads the journal file and nothing else - no socket, no token,
//! no write - with the poll interval that serves both the input and the tail.

use super::*;

/// How often [`run_attach`] reads: one poll serves the input **and**, with `--follow`, the
/// journal, so a key press and a newly committed line are both seen inside the brief's 250 ms
/// ceiling. Draws are not on this tick: a still frame is not redrawn at all.
const ATTACH_POLL: Duration = Duration::from_millis(50);

/// `radian attach <session> [--follow]` (ADR 0043 slice ①, decision D14): the read-only view
/// of another session's committed facts. It reads the journal file and nothing else — no
/// socket, no token, no write — and the observed session is never told.
///
/// Exit codes follow ADR 0032 A3: `2` for a refusal (an unknown id, no terminal), `0` for a
/// clean quit. The refusal is decided **before** the terminal is entered, so stderr keeps its
/// one line and nothing on disk is touched.
pub(super) fn run_attach(dir_flag: Option<&Path>, id: &str, follow: bool) -> ExitCode {
    // The id becomes a file name, so it goes through the same rule `--resume` uses
    // (`validate_session_id`): a path-like id must not name a file outside the session
    // directory. The refusal comes before the directory is even resolved, so nothing is
    // looked up, created or read.
    if let Err(error) = radian_host::validate_session_id(id) {
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
    let path = dir.join(format!("{id}.jsonl"));
    if !path.exists() {
        eprintln!("radian: no session {id:?} in {}", dir.display());
        return ExitCode::from(2);
    }
    let mut view = match radian_tui::attach::AttachView::open(&path, id) {
        Ok(view) => view,
        Err(error) => {
            eprintln!("radian: {error}");
            return ExitCode::from(2);
        }
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
    let mut dirty = true;
    let mut failure: Option<String> = None;
    loop {
        if dirty {
            if let Err(error) = terminal.draw(|frame| view.render(frame)) {
                // The terminal is gone (the pty master closed): leave the ordinary way.
                failure = Some(error.to_string());
                break;
            }
            dirty = false;
        }
        match read_input(ATTACH_POLL) {
            Input::Gone => break,
            Input::Idle => {
                if follow {
                    match view.poll() {
                        // Durable facts only: the frame changes when a message was committed,
                        // a replacement landed or the turn's boundary moved.
                        Ok(changed) => dirty |= changed,
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                }
            }
            Input::Event(Event::Key(key)) => match view.on_key(&key) {
                radian_tui::attach::AttachKey::Quit => break,
                radian_tui::attach::AttachKey::Redraw => dirty = true,
                // Swallowed: no input event, no command, no write (ADR 0043 D14).
                radian_tui::attach::AttachKey::Ignored => {}
            },
            // A resize re-lays the transcript out on the next draw.
            Input::Event(Event::Resize(..)) => dirty = true,
            Input::Event(_) => {}
        }
    }
    guard.restore();
    match failure {
        None => ExitCode::SUCCESS,
        Some(error) => {
            eprintln!("radian: {error}");
            ExitCode::from(1)
        }
    }
}
