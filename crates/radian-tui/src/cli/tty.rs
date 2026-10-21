//! The tty's own layer: tty readiness (`poll(2)`) and the bounded input poll, the input
//! thread with its resize coalescing, the frame draw and the clipboard writes, the spinner tick,
//! the `--exit-trace`/`--block-input` test hooks, and the `Dirty` handling. The loop in `cli.rs`
//! drives these; they read the app only through [`App`]'s own API.

use super::*;

/// Test hook (`--block-input`): a path whose creation marks "the read is now blocked forever".
/// Set once in `run`; `read_input` writes it and parks.
pub(super) static BLOCK_INPUT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Test hook (`--panic-in-driver`): the driver task panics, so the exit path can be checked.
pub(super) fn panic_in_driver() {
    panic!("radian: driver task panic test");
}

/// The `--exit-trace` file (test hook). One `OnceLock` so every call site reads the same
/// path; unset in production, where [`exit_trace`] is a no-op.
pub(super) static EXIT_TRACE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// [`exit_trace`] at most once per site (the input thread's poll loop runs every 50 ms; the
/// trace must show its *state*, not a flood). Test-only, like the trace itself.
pub(super) fn exit_trace_once(marked: &std::sync::atomic::AtomicBool, point: &str) {
    // Zero cost when the test hook is off (production): no atomic, no write, no output.
    if EXIT_TRACE.get().is_none() {
        return;
    }
    if !marked.swap(true, std::sync::atomic::Ordering::SeqCst) {
        exit_trace(point);
    }
}

/// Appends one step of the terminal-gone exit path, when the test hook is on. A plain
/// unbuffered append per point, so a hard failure still leaves the earlier points on disk.
pub(super) fn exit_trace(point: &str) {
    let Some(path) = EXIT_TRACE.get() else {
        return;
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{point} {nanos}");
    }
}

/// Parks forever when the `--block-input` test hook is on, writing its marker once.
fn block_input_if_asked() {
    let Some(path) = BLOCK_INPUT.get() else {
        return;
    };
    static MARKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !MARKED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let _ = std::fs::write(path, "blocked\n");
    }
    loop {
        std::thread::park();
    }
}

/// What one input poll found.
pub(super) enum Input {
    Event(Event),
    Idle,
    /// The terminal is gone: the pty master closed, the ssh session dropped, ...
    Gone,
}

/// The tty's readiness as `poll(2)` sees it.
#[cfg(unix)]
enum Tty {
    Readable,
    Idle,
    /// The master end closed (`POLLHUP`) or the fd is unusable.
    Gone,
}

#[cfg(unix)]
fn tty_readiness(timeout: Duration) -> Tty {
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: `fd` is a valid `pollfd` for the duration of the call.
    let ready = unsafe { libc::poll(&mut fd, 1, millis) };
    if ready < 0 {
        // EINTR is a signal (SIGWINCH, SIGTERM/SIGHUP): report Idle and let the loop check
        // `signals::received` on its next turn.
        return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            Tty::Idle
        } else {
            Tty::Gone
        };
    }
    if ready == 0 {
        return Tty::Idle;
    }
    if fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return Tty::Gone;
    }
    if fd.revents & libc::POLLIN != 0 {
        Tty::Readable
    } else {
        Tty::Idle
    }
}

/// Asks crossterm for the next event. Safe only when the tty is readable (a pending byte) or
/// crossterm already holds a parsed event: its reader reads the fd, and a read that returns
/// EOF loops forever (see [`read_input`]).
fn crossterm_event() -> Input {
    exit_trace_once(&TRACED_READ, "crossterm-read");
    match event::read() {
        Ok(event) => Input::Event(event),
        Err(_) => Input::Gone,
    }
}

/// Polls stdin for one input event, bounded by `timeout`.
///
/// `crossterm::event::poll`/`read` cannot drive the loop directly: when the terminal has gone
/// away, the Unix event source keeps reading a tty that now returns EOF (`read` = 0) and never
/// returns to the caller, so the app spins forever (crossterm 0.29.0; 16 orphaned `radian-tui`
/// processes at 15–25% CPU). So the readiness check is `poll(2)`, which reports `POLLHUP` once
/// the pty master is closed, and the caller leaves through the ordinary clean-exit path.
///
/// crossterm reads up to 1024 bytes per call and buffers the extra events, so after a readable
/// tty is drained the remaining events must still be returned. **The buffer check is not a
/// safe guard**: `event::poll(0)` returns a buffered event if the parser has one
/// (`crossterm-0.29.0/src/event/source/unix/mio.rs:69`), but with an empty buffer it polls the
/// fd and, when the fd is ready, reads it (`mio.rs:95`); a hung-up tty is "ready" (macOS
/// returns `POLLIN|POLLHUP`), the read returns 0 (EOF), and the inner `loop` (`mio.rs:94-114`)
/// **never leaves** — the read spins on EOF forever. The `signals` task is not a backstop for
/// this either: a pty master close sends no SIGHUP (review, 2026-10-14; the closed-master
/// pty test reproduced a hang whose stack was 424/424 samples inside `event::poll` →
/// `try_read` → `read`).
///
/// Two guards keep it out (both needed; each one's mutation is in the pty test):
/// 1. **Narrowing**: the idle branch calls `event::poll(0)` only when a previous
///    [`crossterm_event`] read may have buffered something ([`MAY_HAVE_BUFFERED`]), and
///    re-checks `tty_readiness(0)` immediately before, so a Close raced since the readiness
///    poll above is reported as [`Input::Gone`] instead of entering crossterm.
/// 2. **Fallback**: the main loop calls [`terminal_is_gone`] (a non-consuming `poll(2)`) on a
///    periodic tick, so even a thread stuck inside crossterm ends the process through the
///    ordinary [`UiEvent::TerminalGone`] path.
///
/// When guard 1 applies (a previous read may have buffered events) the readiness wait is
/// **zero**: the buffered events are already in hand, so there is nothing to wait for on the
/// fd. Waiting the full `timeout` there added one poll period (~50 ms) per buffered event; an
/// IME commits a whole word at once, so its characters appeared one at a time, ~50 ms apart.
/// The zero-timeout poll still reports a hang-up, so the safety semantics do not change.
#[cfg(unix)]
pub(super) fn read_input(timeout: Duration) -> Input {
    block_input_if_asked();
    let timeout = if MAY_HAVE_BUFFERED.load(std::sync::atomic::Ordering::SeqCst) {
        Duration::ZERO
    } else {
        timeout
    };
    match tty_readiness(timeout) {
        Tty::Gone => Input::Gone,
        Tty::Readable => {
            exit_trace_once(&TRACED_READABLE, "poll-readable");
            let input = crossterm_event();
            if matches!(input, Input::Event(_)) {
                MAY_HAVE_BUFFERED.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            input
        }
        Tty::Idle => {
            exit_trace_once(&TRACED_IDLE, "poll-idle");
            // A window resize reaches crossterm only while it is reading stdin, which an idle
            // app never does: its SIGWINCH pipe is drained inside `event::poll`/`read`. So an
            // idle app stayed at the old width until the next key or wheel ("scroll once and
            // it re-lays out"). Ask the kernel directly instead: `TIOCGWINSZ` does not read
            // stdin, so it cannot hit the EOF spin guard 1 exists for.
            if let Some((rows, cols)) = size_changed() {
                // crossterm's own order is `(cols, rows)`; the synthetic event must match it so
                // the input thread's single `Event::Resize` match is correct for both sources.
                return Input::Event(Event::Resize(cols, rows));
            }
            // Guard 1: do not enter crossterm unless a previous read may have buffered
            // events. Its `poll(0)` reads the fd on an empty buffer and loops on a hung-up
            // tty (see the function docs); a plain Idle is safe.
            if !MAY_HAVE_BUFFERED.load(std::sync::atomic::Ordering::SeqCst) {
                return Input::Idle;
            }
            // The master may have closed since the readiness poll above. The zero-timeout
            // poll is non-consuming, so a Gone here is safe to report.
            if matches!(tty_readiness(Duration::ZERO), Tty::Gone) {
                return Input::Gone;
            }
            match event::poll(Duration::ZERO) {
                Ok(true) => {
                    exit_trace_once(&TRACED_CROSSTERM_ZERO, "poll-crossterm-zero");
                    let input = crossterm_event();
                    if !matches!(input, Input::Event(_)) {
                        MAY_HAVE_BUFFERED.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                    input
                }
                Ok(false) => {
                    MAY_HAVE_BUFFERED.store(false, std::sync::atomic::Ordering::SeqCst);
                    Input::Idle
                }
                Err(_) => Input::Gone,
            }
        }
    }
}

/// Whether a previous [`crossterm_event`] read may have buffered more events (crossterm reads
/// up to 1024 bytes and parses one event). Only then is it safe to ask crossterm's `poll(0)`;
/// see [`read_input`]'s docs.
static MAY_HAVE_BUFFERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The last terminal size this thread reported, as `rows << 16 | cols` (`0` = never). A window
/// resize is detected by comparing it against a fresh `TIOCGWINSZ`, because crossterm only
/// drains its SIGWINCH pipe while it is reading stdin (see [`read_input`]).
static LAST_SIZE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The current terminal size when it differs from the last one reported, else `None`. `TIOCGWINSZ`
/// only; never a stdin read. The first call records the size without reporting a change.
fn size_changed() -> Option<(u16, u16)> {
    let (cols, rows) = terminal::size().ok()?;
    let packed = (u32::from(rows) << 16) | u32::from(cols);
    let previous = LAST_SIZE.swap(packed, std::sync::atomic::Ordering::SeqCst);
    (previous != 0 && previous != packed).then_some((rows, cols))
}

/// One-per-site markers for the input thread's trace (test hook only).
static TRACED_READABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TRACED_IDLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TRACED_CROSSTERM_ZERO: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static TRACED_READ: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(not(unix))]
pub(super) fn read_input(timeout: Duration) -> Input {
    block_input_if_asked();
    match event::poll(timeout) {
        Ok(true) => match event::read() {
            Ok(event) => Input::Event(event),
            Err(_) => Input::Gone,
        },
        Ok(false) => Input::Idle,
        Err(_) => Input::Gone,
    }
}

/// Whether an I/O error means the terminal went away (`EIO`/`EPIPE`) rather than a real
/// failure.
fn terminal_gone(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        if error.raw_os_error() == Some(libc::EIO) {
            return true;
        }
    }
    false
}

/// Advances the spinner's clock and returns the new frame when a spinner is visible and the
/// frame changed since `last` (ADR 0027/13). The idle tick calls this, so an idle
/// app still makes no frame and a visible spinner redraws on its interval — the breath's step
/// (about 125 ms) while it interpolates, the two-colour switch or the rotating frames otherwise.
pub(super) fn spinner_tick(
    app: &mut App,
    last: &mut Option<radian_tui::spinner::Frame>,
) -> Option<radian_tui::spinner::Frame> {
    spinner_frame_at(app, last, std::time::Instant::now())
}

/// [`spinner_tick`] with the wake's instant as an argument: the loop passes the clock, a test
/// walks it.
pub(super) fn spinner_frame_at(
    app: &mut App,
    last: &mut Option<radian_tui::spinner::Frame>,
    now: std::time::Instant,
) -> Option<radian_tui::spinner::Frame> {
    if !app.spinner_visible() {
        return None;
    }
    app.set_now(now);
    let frame = app.spinner_frame();
    if *last == Some(frame) {
        return None;
    }
    *last = Some(frame);
    Some(frame)
}

/// How long the loop may wait for an event: the hang-up check's own tick; only while a
/// spinner is visible, that spinner's frame interval, so the breath is sampled at the step it
/// turns at; only while background jobs run and the footer shows the `jobs` item, the sooner
/// of the hang-up tick and the wait until the displayed age text changes — every second
/// under a minute, the next minute boundary after (ADR 0023 U2). Nothing else wakes the loop
/// early; the loop redraws only when the age text changed.
pub(super) fn wait_for_an_event(app: &App) -> Duration {
    if app.spinner_visible() {
        TTY_CHECK_TICK.min(app.spinner_interval())
    } else if let Some(jobs) = crate::footer::tick_in(app, app.now) {
        TTY_CHECK_TICK.min(jobs)
    } else {
        TTY_CHECK_TICK
    }
}

/// The `jobs` age tick (ADR 0023 U2): redraws the idle loop when the displayed age text
/// changes, and never otherwise. The loop stamps the clock before calling (its own stamp,
/// like `spinner_tick`'s), so this reads `app.now` and stays wall-clock-free for tests.
pub(super) fn jobs_tick(app: &App, last: &mut Option<String>) -> Option<String> {
    let text = crate::footer::age_text(app, app.now)?;
    if *last == Some(text.clone()) {
        return None;
    }
    *last = Some(text.clone());
    Some(text)
}

/// Whether the tty is already hung up, as `poll(2)` sees it right now. A draw failure carries
/// crossterm's own "failed to write the buffered data" (no errno), so the tty's own state is
/// the reliable test.
pub(super) fn terminal_is_gone() -> bool {
    #[cfg(unix)]
    {
        matches!(tty_readiness(Duration::ZERO), Tty::Gone)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Test hook (`--freeze-draw`): once the marker exists, `draw_frame` stops drawing. The app
/// still writes ~30 bytes per frame when idle (the cursor position), so a *content*-static
/// frame is not enough to keep a closed pty off the draw path; this is.
pub(super) static FREEZE_DRAW: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Test hook (`--slow-draw <MILLIS>`): make every `draw_frame` sleep, so the main loop is
/// genuinely stuck and cannot reach `confirm_exit` before the signal watchdog fires.
pub(super) static SLOW_DRAW: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Draws one frame. A draw that fails because the terminal is gone is the same event as
/// [`Input::Gone`]: report it so the caller leaves cleanly instead of erroring out. `Ok(false)`
/// means "the terminal is gone".
pub(super) fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<bool, String> {
    if let Some(millis) = SLOW_DRAW.get() {
        std::thread::sleep(Duration::from_millis(*millis));
    }
    // ADR 0027: hand the background `command` items the footer's live
    // facts. One pure state read and one lock write, before the frame; a command reads the latest
    // snapshot when it next runs, so the UI never waits for one.
    app.publish_footer_context();
    if FREEZE_DRAW.get().is_some_and(|path| path.exists()) {
        return Ok(true);
    }
    // `draw/end` is the end of the render closure; the gap to `draw/terminal-done` is
    // ratatui's buffer diff plus the write and flush to the terminal (resume-perf-c: the
    // owner's 1.2 s first-frame gap had no mark here).
    radian_session::trace::mark("draw/terminal-start");
    let drawn = terminal.draw(|frame| radian_tui::render::draw(frame, app));
    radian_session::trace::mark("draw/terminal-done");
    match drawn {
        Ok(_) => {
            if let Some(path) = FREEZE_DRAW.get() {
                let _ = std::fs::write(path, "drawn\n");
            }
            Ok(true)
        }
        Err(error) if terminal_gone(&error) || terminal_is_gone() => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

/// Writes the OSC 52 clipboard sequence for `text` to stdout. Fire-and-forget: the terminal
/// never answers, so the caller's notice says "via OSC 52", never that the clipboard holds
/// the text.
pub(super) fn write_osc52(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    out.write_all(osc52(text).as_bytes())?;
    out.flush()
}

/// Rings the terminal bell: one `\x07` on stdout (`tui.bell`). Fire-and-forget like
/// OSC 52 — a terminal that cannot ring ignores it — so a failed write is swallowed, never
/// a session failure.
pub(super) fn write_bel() {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
}

/// Feeds `text` to macOS's `pbcopy` and reports whether it accepted it. Only reachable when
/// `tui.clipboard = "pbcopy"` on macOS (anywhere else the config warns and falls back to
/// OSC 52 before the loop ever sees the mode). The pipe write happens on a thread so a large
/// selection cannot deadlock against `pbcopy`'s own buffer.
pub(super) fn run_pbcopy(text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut child = std::process::Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let text = text.to_owned();
    let writer = std::thread::spawn(move || {
        stdin
            .write_all(text.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|error| error.to_string())
    });
    let status = child.wait().map_err(|error| error.to_string())?;
    writer
        .join()
        .map_err(|_| "the pbcopy writer panicked".to_string())??;
    if status.success() {
        Ok(())
    } else {
        Err(match status.code() {
            Some(code) => format!("exited with status {code}"),
            None => "was terminated by a signal".to_string(),
        })
    }
}

pub(super) fn spawn_input_thread(tx: Sender<UiEvent>) {
    std::thread::spawn(move || {
        exit_trace("input-thread-start");
        // Record the size the app is about to draw at, so a resize that lands before the
        // first idle poll is still detected (the owner's bare-resize bug).
        if let Ok((cols, rows)) = terminal::size() {
            let packed = (u32::from(rows) << 16) | u32::from(cols);
            LAST_SIZE.store(packed, std::sync::atomic::Ordering::SeqCst);
        }
        // An event read while a resize burst coalesces: it is handled on the next turn. It
        // cannot stay in crossterm's buffer — `read_input` already consumed it.
        let mut stashed: Option<Event> = None;
        loop {
            let event = match stashed.take() {
                Some(event) => event,
                None => match read_input(INPUT_POLL) {
                    Input::Gone => {
                        exit_trace("pollhup");
                        let _ = tx.send(UiEvent::TerminalGone);
                        return;
                    }
                    Input::Event(event) => event,
                    Input::Idle => continue,
                },
            };
            // A resize burst coalesces to the last one; everything else goes through the one
            // pure event map (which drops clicks, drags and moves).
            let event = match event {
                // crossterm's order is `(cols, rows)`; `coalesce_resize` takes rows first.
                Event::Resize(cols, rows) => coalesce_resize(rows, cols, &mut stashed),
                other => match radian_tui::events::input_event(other) {
                    Some(event) => event,
                    None => continue,
                },
            };
            if tx.send(UiEvent::Input(event)).is_err() {
                return;
            }
        }
    });
}

/// A burst of resizes (a window drag) collapses to the last one: every size in between would
/// cost a full re-wrap for a frame nobody sees. A non-resize event ends the collapse; it was
/// already read, so it is stashed for the next turn rather than dropped (the coalescer used to
/// consume one typed key whenever a resize and a keystroke arrived together).
fn coalesce_resize(rows: u16, cols: u16, stashed: &mut Option<Event>) -> InputEvent {
    let (mut rows, mut cols) = (rows, cols);
    loop {
        match read_input(Duration::ZERO) {
            Input::Event(Event::Resize(next_cols, next_rows)) => {
                rows = next_rows;
                cols = next_cols;
            }
            Input::Event(other) => {
                *stashed = Some(other);
                break;
            }
            Input::Idle | Input::Gone => break,
        }
    }
    // A drag may have moved the window again since; the last size is the one that matters, so
    // report it rather than an intermediate one.
    if let Some((next_rows, next_cols)) = size_changed() {
        rows = next_rows;
        cols = next_cols;
    }
    InputEvent::Resize { rows, cols }
}

/// The loop's side of the world (ADR 0023 U2/U3): everything handling an event or running an
/// effect needs. The app stays pure; this is where the outside lives.
/// What a `Dirty` notification means (ADR 0023 U2): clear the flag **first**, then read the
/// projection, and always draw a frame when the read succeeded. The frame reads the projection
/// for every other fact (turn, usage, budget, tier, MCP), so a fact-only update with an empty
/// streaming buffer still changes what is on screen. `false` means another `Dirty` is already
/// in flight, so this one is coalesced away.
pub(super) fn apply_dirty(app: &mut App) -> bool {
    match app.session.projection.take_snapshot() {
        Some(snapshot) => {
            app.update(AppInput::Stream(snapshot));
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0027 + ADR 0023 U2: the breath redraws **once per step**, and only while a
    /// spinner is visible. The test walks the injected clock in 50 ms wakes — no wall clock — and
    /// counts the frames the loop's own tick hands out over two breath periods: 32 for an RGB
    /// theme (16 steps each), 8 for a theme that can only switch (one every 500 ms), none idle.
    #[test]
    fn the_breath_redraws_once_per_step_and_only_when_a_spinner_is_visible() {
        /// The frames `spinner_tick` hands the loop while the clock walks two periods in 50 ms
        /// wakes — the same function the loop calls, with the wake's instant as the argument.
        fn frames(app: &mut App) -> usize {
            let base = app.session.spinner_origin;
            let mut last = None;
            let mut count = 0;
            let mut t = Duration::ZERO;
            while t < radian_tui::spinner::PULSE_PERIOD * 2 {
                if spinner_frame_at(app, &mut last, base + t).is_some() {
                    count += 1;
                }
                t += Duration::from_millis(50);
            }
            count
        }

        let mut app = App::new("m", "s");
        app.session
            .projection
            .set_turn(radian_tui::TurnState::Running);
        assert!(app.spinner_visible(), "a running turn shows a spinner");
        app.theme = radian_tui::theme::Theme::dark();
        assert_eq!(
            frames(&mut app),
            usize::from(radian_tui::spinner::PULSE_STEPS) * 2,
            "one frame per step, 16 steps in every 2.0 s period"
        );
        assert_eq!(
            wait_for_an_event(&app),
            radian_tui::spinner::pulse_step(),
            "the loop wakes at the breath's step while it is visible"
        );
        // The 250 ms hang-up tick is still the floor: a slower frame never shortens it further.
        app.theme = radian_tui::theme::Theme::mono();
        assert_eq!(
            frames(&mut app),
            8,
            "a theme that cannot interpolate switches every 500 ms (two colours over 4.0 s)"
        );
        assert_eq!(wait_for_an_event(&app), TTY_CHECK_TICK);
        assert_eq!(
            frames(&mut app),
            8,
            "the walk is a pure function of the clock: the same count again, no wall clock"
        );

        // Idle: the same walk hands out nothing, so an idle screen is never redrawn.
        let mut idle = App::new("m", "s");
        idle.theme = radian_tui::theme::Theme::dark();
        assert_eq!(frames(&mut idle), 0, "no spinner visible, no frame");
        assert_eq!(wait_for_an_event(&idle), TTY_CHECK_TICK);
    }

    /// ADR 0023 U2's time-driven `jobs` age: with an injected clock (`App::now`/`set_now`)
    /// the loop redraws when the displayed age text changes (1 s, near the minute boundary,
    /// the new minute) and never on an unchanged tick; the wait never passes the hang-up
    /// tick, shortening below it only for the next change. With no job — or a footer without
    /// the item — the wait is the plain `TTY_CHECK_TICK` and no redraw. The loop only ever
    /// waits and redraws on the text the `jobs` item itself shows.
    #[test]
    fn the_idle_loop_ticks_the_jobs_age_and_nothing_else() {
        fn footer_with(ids: &[&str]) -> radian_host::config::TuiFooterConfig {
            radian_host::config::TuiFooterConfig {
                rows: vec![radian_host::config::TuiFooterRowConfig {
                    left: ids
                        .iter()
                        .map(|id| radian_host::config::TuiFooterItemConfig {
                            id: id.to_string(),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            }
        }
        fn start_job(app: &mut App, id: &str, started: std::time::Instant) {
            app.session.running_jobs.push(crate::app::RunningJob {
                id: id.to_owned(),
                started,
            });
        }
        let mut app = App::new("mock", "s");
        app.apply_footer(&footer_with(&["jobs"]));
        // No job: the plain tick, no redraw.
        assert_eq!(wait_for_an_event(&app), TTY_CHECK_TICK);
        let mut last = None;
        assert_eq!(jobs_tick(&app, &mut last), None);

        let base = std::time::Instant::now();
        start_job(&mut app, "job-1", base);
        // The oldest job is 1 s old: the wait stays within the hang-up tick and the
        // redraw shows the new age.
        app.set_now(base + Duration::from_secs(1));
        assert_eq!(
            wait_for_an_event(&app),
            TTY_CHECK_TICK,
            "the jobs wait never passes the hang-up tick"
        );
        assert_eq!(jobs_tick(&app, &mut last), Some("1s".to_owned()));
        assert_eq!(
            jobs_tick(&app, &mut last),
            None,
            "the same age redraws nothing"
        );
        // Near the minute boundary the wait shortens below the hang-up tick.
        app.set_now(base + Duration::from_millis(59_950));
        assert_eq!(
            wait_for_an_event(&app),
            Duration::from_millis(50),
            "the loop wakes for the age-text change"
        );
        assert_eq!(jobs_tick(&app, &mut last), Some("59s".to_owned()));
        assert_eq!(
            jobs_tick(&app, &mut last),
            None,
            "the same age redraws nothing"
        );
        // Past one minute the wait is capped again, and the redraw shows the new minute.
        app.set_now(base + Duration::from_secs(61));
        assert_eq!(wait_for_an_event(&app), TTY_CHECK_TICK);
        assert_eq!(jobs_tick(&app, &mut last), Some("1m".to_owned()));
        assert_eq!(
            jobs_tick(&app, &mut last),
            None,
            "the same age redraws nothing"
        );
        // The job settled: back to the plain tick, and the last text is forgotten so a new
        // job redraws at once.
        app.session.running_jobs.clear();
        assert_eq!(wait_for_an_event(&app), TTY_CHECK_TICK);
        assert_eq!(jobs_tick(&app, &mut last), None);
        start_job(&mut app, "job-2", base);
        app.set_now(base + Duration::from_secs(2));
        assert_eq!(jobs_tick(&app, &mut last), Some("2s".to_owned()));

        // A footer without the item: no timer at all, even with a job running.
        let mut bare = App::new("mock", "s");
        bare.apply_footer(&footer_with(&["session"]));
        start_job(&mut bare, "job-1", base);
        bare.set_now(base + Duration::from_secs(30));
        assert_eq!(wait_for_an_event(&bare), TTY_CHECK_TICK);
        let mut last = None;
        assert_eq!(jobs_tick(&bare, &mut last), None);
    }

    /// ADR 0023 U5: a fact-only update (usage, budget, tier, MCP) wakes the UI with a `Dirty`
    /// and the buffer is empty. The loop must still draw a frame, or the status bar would stay
    /// stale until some unrelated event arrived.
    #[test]
    fn a_dirty_notification_with_an_empty_stream_still_draws() {
        let projection = Arc::new(radian_host::SessionProjection::new());
        let mut app = App::new("m", "s");
        app.set_projection(Arc::clone(&projection));
        // The fact-only update: no delta, so the stream snapshot is empty.
        assert!(projection.set_context_window(Some(1000)));
        assert!(
            apply_dirty(&mut app),
            "a fact update must draw a frame even with an empty stream"
        );
        // And a second `Dirty` that finds the flag already cleared is coalesced away.
        assert!(!apply_dirty(&mut app));
    }
}
