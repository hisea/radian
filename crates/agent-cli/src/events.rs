//! Turn-end plumbing (ADR 0011): what the loops wait for, graceful shutdown, forced
//! exit, and the SIGINT listener.
//!
//! Split from `lib.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use std::sync::Arc;
use std::time::Duration;

use radian_runtime::{CancelCause, Inbox};
use tokio::sync::{Notify, mpsc};

use super::redact::redact;
use super::ui::{LineHub, Ui};

/// What the interactive loop is waiting for.
pub(crate) enum Next {
    Line(String),
    /// A turn ended, or a cancellation was requested: re-evaluate and keep going.
    Again,
    /// Leave (EOF, `/exit`, or Ctrl-C at the prompt).
    Leave,
    /// A second Ctrl-C during a wind-down.
    Forced,
    /// SIGTERM: the process was asked to stop (ADR 0043 decision 2, fix round 1 B). Leave
    /// through the ordinary path — cancel what runs, then the caller closes the session.
    Terminated,
    /// The driver stopped on its own (an error).
    DriverEnded,
}

/// SIGINT: what a terminal's Ctrl-C sends.
pub(crate) const SIGINT: i32 = 2;
/// SIGTERM: the signal a parent that wants this process to stop sends. ADR 0043 decision 2's
/// `subagent` parent sends it before it falls back to SIGKILL, so `radian exec` must handle it:
/// without a handler the kernel's default action kills the process at once, and the session-end
/// cleanup (which kills every shell command and background job this session started) never runs.
pub(crate) const SIGTERM: i32 = 15;

/// Waits for the next thing that can happen: a line, a turn ending, the driver ending, or a
/// signal. While a turn is in flight and the approval prompt may read stdin, the loop does
/// **not** read — the approval owns stdin then, exactly as before this slice.
pub(crate) async fn next_event(
    lines: &LineHub,
    ui: &Ui,
    inbox: &Inbox,
    driver_ended: &Arc<Notify>,
    signals: &mut mpsc::UnboundedReceiver<i32>,
    cancelling: &mut bool,
) -> Next {
    let read_lines = !ui.busy() || !ui.approval_may_read();
    // `biased` with the signal first: a pending Ctrl-C must beat a ready EOF or a turn that
    // just ended, or a forced exit could be skipped in favour of a graceful one.
    tokio::select! {
        biased;
        Some(signal) = signals.recv() => on_signal(ui, inbox, cancelling, signal),
        _ = driver_ended.notified() => Next::DriverEnded,
        // Not guarded by `busy`: `RunFinished` clears `busy` before it notifies, so a guard
        // here would disable the branch exactly when the turn end arrives and the loop would
        // never see it (a lost wakeup). A stale permit costs one extra loop turn, nothing more.
        _ = ui.finished.notified() => Next::Again,
        line = lines.next(), if read_lines => match line {
            Some(line) => Next::Line(line),
            None => Next::Leave,
        },
    }
}

pub(crate) fn on_signal(ui: &Ui, inbox: &Inbox, cancelling: &mut bool, signal: i32) -> Next {
    // SIGTERM is not a Ctrl-C: the process was asked to stop, so there is nothing to keep
    // running for. `terminate` (the caller) cancels what is running and joins the driver.
    if signal == SIGTERM {
        return Next::Terminated;
    }
    // The second Ctrl-C of an open wind-down always forces, even if the turn ended in the
    // meantime: the user asked twice for it.
    if *cancelling {
        return Next::Forced;
    }
    if !ui.busy() {
        // At the prompt, Ctrl-C means leave, like EOF.
        return Next::Leave;
    }
    *cancelling = true;
    inbox.cancel(CancelCause::Requested);
    eprintln!("radian: cancelling (Ctrl-C again to quit)");
    Next::Again
}

/// Stops the driver gracefully: it finishes the turns already queued, closes the session
/// (so MCP servers shut down), and returns.
pub(crate) async fn shutdown(
    inbox: &Arc<Inbox>,
    ui: &Arc<Ui>,
    session: &Arc<radian_host::LocalSession>,
) -> Result<(), String> {
    // Let queued inputs run: closing now would abandon them.
    while ui.busy() || !inbox.is_empty().await {
        ui.finished.notified().await;
    }
    inbox.close();
    match session.join().await {
        radian_host::DriverEnd::Ok => Ok(()),
        radian_host::DriverEnd::Failed(error) => Err(error),
        radian_host::DriverEnd::Panicked(message) => {
            Err(format!("the driver task ended unexpectedly: {message}"))
        }
    }
}

/// Closes the inbox at once and joins the driver, without running queued input.
///
/// Unlike [`shutdown`], this does not let queued inputs run first: `-p` only reaches a
/// close once nothing queued can open a turn (a wake entry would have kept it waiting).
/// What is left then is dead `inject` context no turn will ever absorb, and waiting for
/// it to drain would hang. A claimed-but-unstarted turn still runs to completion; the
/// join waits for it.
pub(crate) async fn close_and_join(
    inbox: &Arc<Inbox>,
    session: &Arc<radian_host::LocalSession>,
) -> Result<(), String> {
    inbox.close();
    match session.join().await {
        radian_host::DriverEnd::Ok => Ok(()),
        radian_host::DriverEnd::Failed(error) => Err(error),
        radian_host::DriverEnd::Panicked(message) => {
            Err(format!("the driver task ended unexpectedly: {message}"))
        }
    }
}

/// Aborts the driver: the session is dropped, which kills the tool processes at once. Used
/// only for the forced second Ctrl-C.
pub(crate) async fn force_exit(session: &Arc<radian_host::LocalSession>) -> i32 {
    session.abort();
    let _ = session.join().await;
    130
}

/// Awaits the driver task once and reports its error, if any. Returns the exit code and
/// whether the driver ended cleanly, for the `Result` record.
pub(crate) async fn report_task(
    session: &Arc<radian_host::LocalSession>,
    secrets: &[String],
) -> (i32, bool) {
    match session.join().await {
        radian_host::DriverEnd::Ok => (0, true),
        radian_host::DriverEnd::Failed(error) => {
            eprintln!("radian: {}", redact(&error, secrets));
            (1, false)
        }
        radian_host::DriverEnd::Panicked(message) => {
            eprintln!("radian: the driver task ended unexpectedly: {message}");
            (1, false)
        }
    }
}

pub(crate) fn report_error(result: &Result<(), String>, secrets: &[String]) -> Option<i32> {
    match result {
        Ok(()) => None,
        Err(error) => {
            eprintln!("radian: {}", redact(error, secrets));
            Some(1)
        }
    }
}

/// Waits for the turn in flight to end; the first Ctrl-C cancels it, the second forces exit.
pub(crate) async fn wait_for_turn(
    ui: &Ui,
    inbox: &Inbox,
    driver_ended: &Arc<Notify>,
    signals: &mut mpsc::UnboundedReceiver<i32>,
    cancelling: &mut bool,
) -> TurnEnd {
    loop {
        tokio::select! {
            biased;
            Some(signal) = signals.recv() => {
                if signal == SIGTERM {
                    // The process was asked to stop: no more turns, and `terminate` closes the
                    // session once this one has ended.
                    return TurnEnd::Terminated;
                }
                if *cancelling {
                    return TurnEnd::Forced;
                }
                if !ui.busy() {
                    return TurnEnd::Done;
                }
                *cancelling = true;
                inbox.cancel(CancelCause::Requested);
                eprintln!("radian: cancelling (Ctrl-C again to quit)");
            }
            _ = driver_ended.notified() => return TurnEnd::DriverEnded,
            _ = ui.finished.notified() => return TurnEnd::Done,
        }
    }
}

/// The signal-driven stop (`SIGTERM`, ADR 0043 decision 2 fix round 1 B): cancel what is
/// running, close the inbox at once — the process was asked to stop, so a queued input is not
/// run — and join the driver. The driver finishes the cancelled turn and closes the session,
/// which kills the whole session of every background job (ADR 0037 decision 6) and shuts the
/// MCP servers down; a foreground `shell` command was already killed by its cancellation. The
/// exit code is the shell's `128 + signal`.
pub(crate) async fn terminate(
    inbox: &Arc<Inbox>,
    session: &Arc<radian_host::LocalSession>,
    secrets: &[String],
    signal: i32,
) -> i32 {
    inbox.cancel(CancelCause::Requested);
    inbox.close();
    match session.join().await {
        radian_host::DriverEnd::Ok => {}
        radian_host::DriverEnd::Failed(error) => {
            eprintln!("radian: {}", redact(&error, secrets));
        }
        radian_host::DriverEnd::Panicked(message) => {
            eprintln!("radian: the driver task ended unexpectedly: {message}");
        }
    }
    128 + signal
}

pub(crate) enum TurnEnd {
    Done,
    /// A second Ctrl-C arrived while the first cancellation was still settling.
    Forced,
    /// SIGTERM: the process was asked to stop (ADR 0043 decision 2, fix round 1 B).
    Terminated,
    /// The driver stopped before the turn finished (an error).
    DriverEnded,
}

/// The signal listeners: one task for SIGINT, one for SIGTERM, both forwarding into one channel.
///
/// `tokio::signal::ctrl_c()` does **not** buffer: every call creates a fresh watch receiver, so a
/// signal that arrives while no `ctrl_c()` future is registered is lost. Awaiting a fresh
/// `ctrl_c()` on each loop iteration (as this CLI first did) therefore dropped the second Ctrl-C
/// of a wind-down now and then, and the CLI left with code 0 instead of 130. One task loops on
/// `ctrl_c()` and forwards into a channel, so no signal can fall between two calls. SIGTERM has
/// its own listener because `radian exec` must handle it at all: its default action would kill
/// the process before any session-end cleanup (ADR 0043 decision 2 fix round 1 B).
pub(crate) fn spawn_signals() -> mpsc::UnboundedReceiver<i32> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn({
        let tx = tx.clone();
        async move {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    // The handler could not be installed (it can be retried); do not drop the
                    // channel, or the loop would spin on a closed receiver.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                if tx.send(SIGINT).is_err() {
                    break;
                }
            }
        }
    });
    #[cfg(unix)]
    tokio::spawn({
        let tx = tx.clone();
        async move {
            use tokio::signal::unix::{SignalKind, signal};
            let Ok(mut term) = signal(SignalKind::terminate()) else {
                // No listener: a SIGTERM then takes the kernel's default action, exactly as it
                // did before this listener existed.
                return;
            };
            while term.recv().await.is_some() {
                if tx.send(SIGTERM).is_err() {
                    break;
                }
            }
        }
    });
    rx
}
