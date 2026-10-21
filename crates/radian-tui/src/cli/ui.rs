//! The event loop's frame-facing half: entering the terminal, the `--pick` phase and its
//! handler, the effect worklist (`RoundStep`, `run_effect_rounds`) with its cap, the wake
//! interval and `ui_loop` itself, which reads the resize/wake facts and drives
//! `App::update`. The `/usage` gate helpers sit with it.

use super::herdr::HerdrReporter;
use super::session::SessionHost;
use super::*;

#[allow(clippy::too_many_arguments)]
/// Enters the terminal (raw mode + the alternate screen, and mouse reporting when `mouse`):
/// the guard restores it on every exit path, and the `Terminal` owns the frame buffer. Split
/// out because `--pick` enters it before the session and a plain run enters it after.
pub(super) fn enter_terminal(
    mouse: bool,
) -> Result<
    (
        radian_tui::terminal::TtyGuard,
        Terminal<CrosstermBackend<std::io::Stdout>>,
    ),
    String,
> {
    let guard =
        radian_tui::terminal::TtyGuard::enter(mouse).map_err(|e| format!("terminal: {e}"))?;
    let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))
        .map_err(|e| format!("terminal: {e}"))?;
    Ok((guard, terminal))
}

/// What one [`ui_loop`] run is: the pre-session `--pick` phase (only the picker overlay is
/// live) or an ordinary session (ADR 0023 U4 / plan B). One loop implementation, one
/// terminal and one input thread; the phase is the only difference.
pub(super) enum Phase<'a> {
    /// No session yet. Agent/runtime/approval/dirty events have no producer, so only terminal
    /// input and the two ways to leave are handled.
    Picking,
    /// A session is open; the loop runs effects through the facade.
    Session {
        workspace: &'a Path,
        runtime: &'a tokio::runtime::Runtime,
        session: &'a Arc<dyn radian_host::SessionFacade>,
        host: &'a SessionHost,
        memory: &'a Option<MemoryService>,
    },
}

/// Handles one event in the pre-session `--pick` phase. Returns whether the frame is dirty.
fn handle_picking(event: UiEvent, app: &mut App, signal: &mut Option<i32>) -> bool {
    match event {
        UiEvent::Input(InputEvent::Key(key)) => app.on_key(key).dirty,
        UiEvent::Input(InputEvent::Paste(text)) => app.update(AppInput::Paste(text)).dirty,
        UiEvent::Input(InputEvent::Resize { rows, cols }) => {
            app.update(AppInput::Resize { rows, cols }).dirty
        }
        UiEvent::Input(InputEvent::MouseScroll(direction)) => app.mouse_scroll(direction).dirty,
        // Mouse presses during `--pick` reach the same gated path (the picker overlay blocks
        // them); there is no transcript yet, so they are no-ops at worst.
        UiEvent::Input(InputEvent::MouseDown { row, col }) => {
            app.update(AppInput::MouseDown { row, col }).dirty
        }
        UiEvent::Input(InputEvent::MouseDrag { row, col }) => {
            app.update(AppInput::MouseDrag { row, col }).dirty
        }
        UiEvent::Input(InputEvent::MouseUp { row, col }) => {
            app.update(AppInput::MouseUp { row, col }).dirty
        }
        UiEvent::Signal(value) => {
            // Remembered so the caller can exit like the session phase (128 + signal and the
            // reason line); nothing has been created.
            *signal = Some(value);
            app.quit = true;
            false
        }
        UiEvent::TerminalGone => {
            // Leave through the ordinary clean-exit path; nothing has been created.
            app.quit = true;
            false
        }
        // No session exists: these have no producer and are ignored if one somehow arrives.
        _ => false,
    }
}

/// The opencode-go usage refresh period (ADR 0027 C4): `None` (off) for `0`, the default for
/// an unset value, and the clamped configured value otherwise.
pub(super) fn usage_period(configured: Option<u64>) -> Option<std::time::Duration> {
    match configured {
        Some(0) => None,
        Some(seconds) => Some(radian_host::clamp_opencode_go_period(seconds)),
        None => Some(radian_host::OPENCODE_GO_DEFAULT_PERIOD),
    }
}

/// Whether the `chatgpt` usage gate (ADR 0026) should be active for
/// `provider`: only while the current route's provider is exactly `chatgpt`, mirroring
/// opencode-go's own (inline) `provider == "opencode-go"` gate. Pulled into its own function
/// — unlike opencode-go's — so a mutation to it (polling while routed elsewhere) has a direct
/// unit test rather than needing a full TUI run to catch.
pub(super) fn chatgpt_usage_active(provider: &str) -> bool {
    provider == radian_host::CHATGPT_PROVIDER_ID
}

#[cfg(test)]
mod chatgpt_usage_gate_tests {
    use super::chatgpt_usage_active;

    #[test]
    fn the_gate_is_active_only_for_chatgpt() {
        assert!(chatgpt_usage_active("chatgpt"));
        assert!(!chatgpt_usage_active("opencode-go"));
        assert!(!chatgpt_usage_active("anthropic"));
        assert!(!chatgpt_usage_active("openai"));
    }
}

/// How `ui_loop` left: the ordinary quit (with the signal that asked, if any), or an
/// accepted `/new` — end this session and open a fresh one in the same terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LoopEnd {
    Quit(Option<i32>),
    NewSession,
}

/// One step of [`run_effect_rounds`]: run one effect, or take the result events the last
/// batch produced.
enum RoundStep<'a, T> {
    Execute(T),
    Drain(&'a mut Vec<T>, usize),
}

/// Runs at most [`MAX_EFFECT_ROUNDS`] effect rounds (ADR 0023 U2's draw fairness). `step` runs
/// one effect or takes the result events the last batch produced (returning whether it handled
/// any). Whatever is left in `effects` stays there for the next turn — never dropped, order
/// kept; the loop draws a frame and continues. A genuine infinite chain keeps running and warns
/// once (see [`MAX_CARRIED_FRAMES`]), it never exits. Returns the rounds run.
fn run_effect_rounds<T>(
    effects: &mut Vec<T>,
    mut step: impl FnMut(RoundStep<'_, T>) -> Result<bool, String>,
) -> Result<usize, String> {
    let mut rounds = 0;
    while !effects.is_empty() && rounds < MAX_EFFECT_ROUNDS {
        rounds += 1;
        let batch = std::mem::take(effects);
        let produced = batch.len();
        for effect in batch {
            step(RoundStep::Execute(effect))?;
        }
        if !step(RoundStep::Drain(effects, produced))? {
            break;
        }
    }
    Ok(rounds)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn ui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    rx: &Receiver<UiEvent>,
    tx: &Sender<UiEvent>,
    phase: Phase<'_>,
    startup_route: (String, String),
    route_price: RoutePrice,
    config_providers: &std::collections::BTreeMap<String, radian_host::ProviderDecl>,
    // The configured theme name (`tui.theme`), for `/theme`'s current-value marker.
    current_theme: Option<String>,
    thinking_max_tokens: u32,
    bash_sandbox: Option<radian_sandbox::Sandbox>,
    // The opencode-go usage gate (ADR 0027), flipped on a `/model` switch.
    usage_gate: Option<tokio::sync::watch::Sender<bool>>,
    // The chatgpt usage gate (ADR 0026), same mechanism.
    chatgpt_usage_gate: Option<tokio::sync::watch::Sender<bool>>,
    // The herdr reporter (ADR 0034), when running inside a herdr pane.
    mut herdr: Option<HerdrReporter>,
) -> Result<LoopEnd, String> {
    let mut picking_signal: Option<i32> = None;
    let mut ctx = match phase {
        Phase::Picking => None,
        Phase::Session {
            workspace,
            runtime,
            session,
            host,
            memory,
        } => Some(LoopContext {
            startup_route: startup_route.clone(),
            route_price: Arc::clone(&route_price),
            config_providers: config_providers.clone(),
            current_thinking: app.session.thinking_level.clone().unwrap_or_default(),
            current_theme,
            thinking_max_tokens,
            workspace,
            runtime,
            session,
            memory,
            host,
            tx,
            queue: session.pending(),
            signal: None,
            clipboard: app.clipboard,
            bell_rang: false,
            bash_sandbox: bash_sandbox.clone(),
            bash_cancel: None,
            side_cancel: None,
            usage_gate,
            chatgpt_usage_gate,
            herdr: herdr.take(),
            // ADR 0008 §2: the first `/model` in a session that started without one
            // writes back to the config file; later switches never do.
            model_writeback_done: false,
        }),
    };
    // The first frame: the startup notices and the resumed history are already in the app, and
    // zero-redraw means nothing else would draw one.
    mark("first-frame-start");
    let drawn = draw_frame(terminal, app)?;
    mark("first-frame-done");
    if !drawn {
        return Ok(LoopEnd::Quit(None));
    }
    radian_tui::terminal::set_cursor_visible(app.input_editable());
    // The last spinner frame drawn (ADR 0027): the idle tick redraws only when the
    // frame changes, so the pulse (500 ms) does not draw on every 250 ms wake.
    let mut last_spinner: Option<radian_tui::spinner::Frame> = None;
    // The last `jobs` age text drawn (ADR 0023 U2): the idle tick redraws only when the text
    // changes, so a running job does not draw on every wake.
    let mut last_jobs: Option<String> = None;
    // An effect batch the draw-fairness cap did not finish: declared outside the loop so the
    // rest survives to the next turn (never dropped, order kept).
    let mut effects: Vec<Effect> = Vec::new();
    let mut carried_frames: u64 = 0;
    // The pre-session phase ends when the picker resolves; otherwise only `quit` leaves.
    while !app.quit && !app.picking_resolved() {
        let mut dirty = false;
        // A new frame: bell-class events in it merge into one ring.
        if let Some(ctx) = ctx.as_mut() {
            ctx.bell_rang = false;
        }
        // A carried batch runs first, without a blocking wait: the effects must not wait for a
        // new event. Otherwise block until there is something to do. No display in this build
        // changes with time (the
        // status bar, the notes, the find box and the budget line are static text), so there is
        // no `recv_timeout` here: an idle app is not redrawn at all (ADR 0023 U2). A clock-like
        // display would take the timeout branch at this line. The one exception is a live
        // selection drag held at the transcript's top or bottom edge: the auto-scroll is
        // time-driven, so the loop takes a bounded timeout **only while the drag is live**,
        // and goes straight back to blocking `recv` when it ends. The armed Ctrl-C quit
        // window is the other time-driven display (ADR 0023 U2): while it is armed the loop
        // wakes at its deadline to drop the hint.
        if effects.is_empty() {
            let now = std::time::Instant::now();
            let quit_remaining = app
                .quit_pending
                .map(|deadline| deadline.saturating_duration_since(now));
            // A `/fork` waiting for the turn boundary has its own deadline (FORK_COMMIT_WAIT).
            let fork_remaining = app
                .session
                .pending_fork
                .map(|deadline| deadline.saturating_duration_since(now));
            // A `/loop --every` wait ends at the next iteration's due time (the turn-end
            // decision still re-checks; the deadline only wakes an idle loop).
            let loop_remaining = app
                .loop_deadline()
                .map(|deadline| deadline.saturating_duration_since(now));
            let wait_remaining = [quit_remaining, fork_remaining, loop_remaining]
                .into_iter()
                .flatten()
                .min();
            let first = if app.is_selecting() || wait_remaining.is_some() {
                // The nearer of the deadlines; `AUTO_SCROLL_TICK` is the drag's own tick.
                let timeout = match (app.is_selecting(), wait_remaining) {
                    (true, Some(wait)) => wait.min(AUTO_SCROLL_TICK),
                    (true, None) => AUTO_SCROLL_TICK,
                    (false, Some(wait)) => wait.max(std::time::Duration::ZERO),
                    (false, None) => unreachable!("the branch requires selection or a deadline"),
                }
                // Also wake for the hang-up check, so a drag or a deadline never postpones it
                // for longer than one tick.
                .min(TTY_CHECK_TICK);
                match rx.recv_timeout(timeout) {
                    Ok(event) => Some(event),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // Guard 2: the input thread may be stuck inside crossterm's reader; a
                        // non-consuming `poll(2)` on stdin still reports the hang-up.
                        if terminal_is_gone() {
                            app.quit = true;
                            break;
                        }
                        // A `/fork` waiting for the journaled turn boundary: give up at its
                        // deadline with a notice, never an empty child session.
                        if app
                            .session
                            .pending_fork
                            .is_some_and(|d| std::time::Instant::now() >= d)
                            && app.update(AppInput::ForkDeadline).dirty
                            && !draw_frame(terminal, app)?
                        {
                            app.quit = true;
                            break;
                        }
                        // A `/loop --every` wait ended while idle: send the next iteration
                        // (a running turn ignores the deadline; its end decides instead).
                        if app
                            .loop_deadline()
                            .is_some_and(|d| std::time::Instant::now() >= d)
                            && app.update(AppInput::LoopDeadline).dirty
                            && !draw_frame(terminal, app)?
                        {
                            app.quit = true;
                            break;
                        }
                        // The quit window closed: disarm and redraw, so the hint disappears. A
                        // drag at an edge (or both) still ticks its scroll on the same wake.
                        if app
                            .quit_pending
                            .is_some_and(|d| std::time::Instant::now() >= d)
                            && app.update(AppInput::QuitDeadline).dirty
                            && !draw_frame(terminal, app)?
                        {
                            app.quit = true;
                            break;
                        }
                        if app.is_selecting()
                            && app.mouse_autoscroll()
                            && !draw_frame(terminal, app)?
                        {
                            app.quit = true;
                            break;
                        }
                        if spinner_tick(app, &mut last_spinner).is_some()
                            && !draw_frame(terminal, app)?
                        {
                            app.quit = true;
                            break;
                        }
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
                }
            } else {
                // A bounded wait, not a blocking one: guard 2's hang-up check runs every tick even
                // while the app is idle. A timeout with the tty alive loops without drawing. The
                // spinner shortens it to its own frame interval while one is visible (ADR 0027
                // revision 13), which is what makes the breath turn at its step; running
                // background jobs shown in the footer shorten it toward the next age-text
                // change, never past the hang-up tick (ADR 0023 U2), so the `jobs` age ticks
                // while idle.
                app.set_now(std::time::Instant::now());
                match rx.recv_timeout(wait_for_an_event(app)) {
                    Ok(event) => Some(event),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if terminal_is_gone() {
                            app.quit = true;
                            break;
                        }
                        app.set_now(std::time::Instant::now());
                        if spinner_tick(app, &mut last_spinner).is_some()
                            && !draw_frame(terminal, app)?
                        {
                            app.quit = true;
                            break;
                        }
                        if jobs_tick(app, &mut last_jobs).is_some() && !draw_frame(terminal, app)? {
                            app.quit = true;
                            break;
                        }
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
                }
            };
            let Some(first) = first else {
                // Every sender is gone: nothing left to drive the UI.
                break;
            };
            let mut facts = 1;
            dirty |= match ctx.as_mut() {
                Some(ctx) => ctx.handle(first, app, &mut effects),
                None => handle_picking(first, app, &mut picking_signal),
            };
            // At most `MAX_FACTS_PER_TURN` facts before a frame (draw fairness).
            while facts < MAX_FACTS_PER_TURN {
                match rx.try_recv() {
                    Ok(event) => {
                        dirty |= match ctx.as_mut() {
                            Some(ctx) => ctx.handle(event, app, &mut effects),
                            None => handle_picking(event, app, &mut picking_signal),
                        };
                        facts += 1;
                    }
                    Err(_) => break,
                }
            }
        } else {
            // A carried batch draws a frame before the rest runs: the cap is about drawing
            // fairly, not about failing.
            dirty = true;
        }
        // Run the effects (session phase only: the picker produces none). Their results are
        // events, so they are handled by the same code; a result can ask for one more effect
        // (`@` expansion → send), so this repeats until the cap hands the rest to the next
        // turn.
        if let Some(ctx) = ctx.as_mut() {
            run_effect_rounds(&mut effects, |step| match step {
                RoundStep::Execute(effect) => ctx.execute(effect).map(|()| true),
                RoundStep::Drain(effects, produced) => {
                    // Each effect produces at most one result event; take that many (plus
                    // whatever else was already queued behind them).
                    for _ in 0..produced {
                        match rx.try_recv() {
                            Ok(event) => dirty |= ctx.handle(event, app, effects),
                            Err(_) => return Ok(false),
                        }
                    }
                    Ok(true)
                }
            })?;
            if !effects.is_empty() {
                // The cap was hit: the rest carries to the next turn (draw fairness). A chain
                // that never clears is a bug, but the TUI keeps running and says so once.
                carried_frames += 1;
                mark("effect-carry");
                if carried_frames == MAX_CARRIED_FRAMES {
                    app.push_notice("the effect worklist is not settling; continuing anyway");
                }
                dirty = true;
            } else {
                carried_frames = 0;
            }
        }
        if let Some(ctx) = ctx.as_mut()
            && let Some(herdr) = ctx.herdr.as_mut()
        {
            herdr.maybe_report(app);
        }
        if dirty {
            if !draw_frame(terminal, app)? {
                // The terminal went away between the input poll and the frame: leave cleanly.
                app.quit = true;
                break;
            }
            last_spinner = Some(app.spinner_frame());
            // The cursor is visible only while the input box is editable.
            radian_tui::terminal::set_cursor_visible(app.input_editable());
        }
    }
    // ADR 0034 H5: a normal exit releases the pane (500 ms). A panic or a kill does not: herdr
    // clears the pane itself once the shell prompt returns.
    if let Some(ctx) = ctx.as_ref()
        && let Some(herdr) = ctx.herdr.as_ref()
    {
        herdr.release();
    }
    Ok(match app.new_session {
        // `/new`: the host ends this session and opens a fresh one; the terminal stays.
        true => LoopEnd::NewSession,
        false => LoopEnd::Quit(ctx.and_then(|ctx| ctx.signal).or(picking_signal)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0023 U2's draw fairness: a chain longer than `MAX_EFFECT_ROUNDS` carries the rest to
    /// the next turn — never dropped, order kept — instead of failing the whole TUI. This is
    /// the `the effect worklist did not settle` bug: under load the loop's drain consumes
    /// unrelated events whose handlers produce more effects, so a real chain can exceed the cap.
    #[test]
    fn effect_rounds_carry_the_rest_in_order() {
        let mut pending: Vec<u32> = vec![19];
        let mut executed: Vec<u32> = Vec::new();
        let mut next: Vec<u32> = Vec::new();
        let mut calls = 0;
        while !pending.is_empty() {
            calls += 1;
            assert!(calls < 100, "the carry must terminate");
            let rounds = run_effect_rounds(&mut pending, |step| match step {
                RoundStep::Execute(n) => {
                    executed.push(n);
                    if n > 0 {
                        next.push(n - 1);
                    }
                    Ok(true)
                }
                RoundStep::Drain(pending, produced) => {
                    // Each executed effect produces one result event; handling it produces the
                    // next effect.
                    for _ in 0..produced {
                        if next.is_empty() {
                            return Ok(false);
                        }
                        pending.push(next.remove(0));
                    }
                    Ok(true)
                }
            })
            .unwrap();
            assert!(rounds <= MAX_EFFECT_ROUNDS, "each call is capped");
        }
        assert_eq!(
            executed,
            (0..20).rev().collect::<Vec<_>>(),
            "every effect ran, in order"
        );
    }
}
