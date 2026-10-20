//! The input loops (`one_shot`, `interactive_loop`): route terminal lines into the
//! inbox, run manual `/compact`, and shut the driver down (ADR 0011, ADR 0012 decision 5,
//! ADR 0018).
//!
//! Split from `lib.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use std::io::Write;
use std::sync::Arc;

use agent_core::{InputSource, RunStatus};
use radian_permissions::PermissionHooks;
use radian_runtime::{CancelCause, Inbox, InboxError};
use tokio::sync::{Notify, mpsc};

use super::approval::TERMINAL_SOURCE;
use super::events::{
    Next, SIGTERM, TurnEnd, close_and_join, force_exit, next_event, report_error, report_task,
    shutdown, terminate, wait_for_turn,
};
use super::jsonl::{self, ExecEnd};
use super::redact::redact;
use super::ui::{LineHub, Ui};

/// What an input loop finished with: the process exit code and how the run ended, for the
/// `--output jsonl` `Result` record.
pub(crate) struct LoopDone {
    pub(crate) code: i32,
    pub(crate) end: ExecEnd,
}

/// The single place a turn's source is decided.
///
/// Both the permission layer's current-turn source and the inbox enqueue use the same value,
/// so they cannot disagree. `reset` is true for the input that starts a turn: it resets the
/// effective source, and then every accepted input reports itself. The effective source only
/// ever **lowers** (ADR 0012 decision 5), so a mid-turn non-human input takes the turn's
/// rights down and a later human input cannot raise them back. All CLI inputs are terminal
/// input today, so this never lowers yet — but the path is wired, and this is the only place
/// to change.
fn set_turn_source(hooks: &PermissionHooks, source: InputSource, reset: bool) {
    if reset {
        hooks.set_turn_source(Some(source));
    }
    hooks.observe_input_source(source);
}

/// The running pieces the two input loops need, bundled so their signatures stay small.
pub(crate) struct Run {
    pub(crate) inbox: Arc<Inbox>,
    pub(crate) hooks: Arc<PermissionHooks>,
    pub(crate) ui: Arc<Ui>,
    pub(crate) driver_ended: Arc<Notify>,
    pub(crate) signals: mpsc::UnboundedReceiver<i32>,
    /// The session facade (ADR 0023 U5): its driver handle, its journal and its projection.
    pub(crate) session: Arc<radian_host::LocalSession>,
    pub(crate) secrets: Vec<String>,
}

/// Enqueues one terminal input and sets the turn source to match. This is the only call site
/// of [`set_turn_source`].
// Carrying the rejected message is the runtime's contract (ADR 0011 decision 1); the
// error is large because the message is, exactly as in `agent_core::SteerError`.
#[allow(clippy::result_large_err)]
async fn enqueue_input(
    inbox: &Arc<Inbox>,
    hooks: &PermissionHooks,
    text: String,
    source: InputSource,
    turn_in_flight: bool,
) -> Result<agent_core::MessageId, InboxError> {
    // The input that starts a turn resets the effective source; a mid-turn one only lowers.
    set_turn_source(hooks, source, !turn_in_flight);
    if turn_in_flight {
        inbox.steer(text, source).await
    } else {
        inbox.followup(text, source).await
    }
}

/// `-p`: send one input, wait for its turn, then wait for the background jobs that turn
/// (or a woken one) started, until no job is running and no woken turn is left.
///
/// Each settle is delivered by the notifier as usual: a woken turn runs, and this waits
/// for it the same way it waited for the first. The exit code and the `Result` record
/// come from the last turn (ADR 0044). With no background job the loop below breaks at
/// once and the run is exactly what it was before.
pub(crate) async fn one_shot(run: &mut Run, prompt: String) -> LoopDone {
    if let Err(error) = enqueue_input(&run.inbox, &run.hooks, prompt, TERMINAL_SOURCE, false).await
    {
        eprintln!("radian: {}", redact(&error.to_string(), &run.secrets));
        let _ = shutdown(&run.inbox, &run.ui, &run.session).await;
        return LoopDone {
            code: 1,
            end: ExecEnd::NotStarted,
        };
    }
    let mut cancelling = false;
    match wait_for_turn(
        &run.ui,
        &run.inbox,
        &run.driver_ended,
        &mut run.signals,
        &mut cancelling,
    )
    .await
    {
        TurnEnd::Terminated => {
            let code = terminate(&run.inbox, &run.session, &run.secrets, SIGTERM).await;
            return LoopDone {
                code,
                end: ExecEnd::Terminated,
            };
        }
        TurnEnd::Forced => {
            let code = force_exit(&run.session).await;
            return LoopDone {
                code,
                end: ExecEnd::Forced,
            };
        }
        TurnEnd::DriverEnded => {
            let (code, ok) = report_task(&run.session, &run.secrets).await;
            return LoopDone {
                code,
                end: ExecEnd::DriverEnded { code, ok },
            };
        }
        TurnEnd::Done => {}
    }
    loop {
        // A degraded notice never opens a turn (the wake gate is closed): end the session
        // instead of waiting for something that cannot happen. Exit 3, the budget family
        // (ADR 0044); the `Result` record names the budget and the unhandled count.
        if let Some(notifier) = run.session.job_notifier()
            && notifier.degraded_pending() > 0
        {
            let unhandled = notifier.degraded_pending();
            eprintln!(
                "radian: the wake budget is used up with {unhandled} job completion \
                 notice(s) unhandled; ending the run"
            );
            if let Err(error) = close_and_join(&run.inbox, &run.session).await {
                eprintln!("radian: {}", redact(&error, &run.secrets));
            }
            return LoopDone {
                code: 3,
                end: ExecEnd::WakeBudget { unhandled },
            };
        }
        if jobs_quiescent(run).await {
            break;
        }
        match wait_for_progress(run, &mut cancelling).await {
            TurnEnd::Terminated => {
                let code = terminate(&run.inbox, &run.session, &run.secrets, SIGTERM).await;
                return LoopDone {
                    code,
                    end: ExecEnd::Terminated,
                };
            }
            TurnEnd::Forced => {
                let code = force_exit(&run.session).await;
                return LoopDone {
                    code,
                    end: ExecEnd::Forced,
                };
            }
            TurnEnd::DriverEnded => {
                let (code, ok) = report_task(&run.session, &run.secrets).await;
                return LoopDone {
                    code,
                    end: ExecEnd::DriverEnded { code, ok },
                };
            }
            TurnEnd::Done => {}
        }
    }
    if let Some(code) = report_error(
        &close_and_join(&run.inbox, &run.session).await,
        &run.secrets,
    ) {
        return LoopDone {
            code,
            end: ExecEnd::Status(run.ui.last_status()),
        };
    }
    // The exit code follows the turn's outcome in every exec mode (ADR 0044); the stderr
    // lines stay exactly what they were.
    match run.ui.last_status() {
        Some(RunStatus::Completed) => LoopDone {
            code: 0,
            end: ExecEnd::Status(Some(RunStatus::Completed)),
        },
        // The `RunFinished` sink already printed the budget line (it names the limit and the
        // flag), so this does not repeat it; the exit code is still a failed turn.
        Some(RunStatus::BudgetExceeded) => LoopDone {
            code: 3,
            end: ExecEnd::Status(Some(RunStatus::BudgetExceeded)),
        },
        Some(status) => {
            eprintln!("radian: the turn ended as {status:?}");
            let code = jsonl::exit_code_for_status(&status);
            LoopDone {
                code,
                end: ExecEnd::Status(Some(status)),
            }
        }
        None => LoopDone {
            code: 1,
            end: ExecEnd::Status(None),
        },
    }
}

/// The quiescence predicate, pure for testing: `-p` may exit only when no job is running,
/// every recorded settle was fully handled, nothing queued could open a turn, and the
/// driver is idle. `handled < settled` means a settle was recorded but its inject/followup
/// has not returned yet: exiting then would miss the turn it wakes.
fn may_exit(running: usize, settled: u64, handled: u64, wake_pending: bool, active: bool) -> bool {
    running == 0 && handled >= settled && !wake_pending && !active
}

/// Whether `-p` may exit: [`may_exit`] over one consistent observation.
///
/// # Why this is race-free
///
/// Three orderings compose. (1) The registry: `settle` flips the cell, bumps its settle
/// total and sends the broadcast under one lock, and `settle_snapshot` reads `(running,
/// settled)` under the same lock — so a settle either precedes the snapshot (the total
/// includes it, and this waits for its delivery via `handled`) or follows it (the job
/// still counts as running, and this waits for it). (2) The notifier bumps `handled`
/// only after the inject/followup returns, so `handled >= settled` means every recorded
/// settle reached the inbox; each bump also resolves `settled_signal`, so a settle that
/// lands after the snapshot wakes the wait below instead of being missed. (3) The inbox
/// version brackets the whole check: the driver's claim pops silently, with no
/// notification, but it bumps the version inside the same critical section — a claim
/// between the two version reads makes them differ and this re-checks instead of
/// exiting. A wake entry the driver has not claimed yet is still queued (`wake_pending`)
/// or the driver is already running (`active`); inject-only leftovers never open a turn
/// (ADR 0011 L5), so they do not block the exit.
///
/// Residual: `Lagged` broadcast losses (the channel holds 16 with at most 4 jobs in
/// flight) would stall `handled` below `settled`; the existing deadline/SIGTERM handling
/// still bounds that wait, and no new timer is added.
async fn jobs_quiescent(run: &Run) -> bool {
    let before = run.inbox.version();
    let (running, settled) = run
        .session
        .jobs_registry()
        .map(|jobs| jobs.settle_snapshot())
        .unwrap_or((0, 0));
    let handled = run
        .session
        .job_notifier()
        .map(|notifier| notifier.settled_handled())
        .unwrap_or(0);
    let wake_pending = run.inbox.snapshot().await.iter().any(|entry| entry.wake);
    let active = run.ui.busy() || run.session.turn_running();
    run.inbox.version() == before && may_exit(running, settled, handled, wake_pending, active)
}

/// After a `-p` turn ends with jobs still outstanding: waits for the next thing that can
/// change the exit condition — a turn ending or starting, a handled settle, a queued wake
/// entry, the driver ending, or a signal. Signal handling matches [`wait_for_turn`]:
/// SIGTERM stops, a second Ctrl-C forces, a first Ctrl-C cancels a running turn and
/// otherwise just re-checks (cancelling with no turn in flight could eat the next woken
/// turn, so an idle Ctrl-C only wakes the loop).
async fn wait_for_progress(run: &mut Run, cancelling: &mut bool) -> TurnEnd {
    let settled = run
        .session
        .job_notifier()
        .map(|notifier| notifier.settled_signal());
    tokio::select! {
        biased;
        Some(signal) = run.signals.recv() => {
            if signal == SIGTERM {
                // The process was asked to stop: no more turns, and `terminate` closes
                // the session once this one has ended.
                return TurnEnd::Terminated;
            }
            if *cancelling {
                return TurnEnd::Forced;
            }
            if !run.ui.busy() && !run.session.turn_running() {
                return TurnEnd::Done;
            }
            *cancelling = true;
            run.inbox.cancel(CancelCause::Requested);
            eprintln!("radian: cancelling (Ctrl-C again to quit)");
            TurnEnd::Done
        }
        _ = run.driver_ended.notified() => TurnEnd::DriverEnded,
        _ = run.ui.finished.notified() => TurnEnd::Done,
        _ = run.ui.started.notified() => TurnEnd::Done,
        _ = async {
            match &settled {
                Some(signal) => signal.notified().await,
                None => std::future::pending().await,
            }
        } => TurnEnd::Done,
        _ = run.inbox.wake_signal() => TurnEnd::Done,
    }
}

/// Interactive: read lines and route them into the inbox.
/// Recognizes a `/compact [instructions]` line (ADR 0018). Only a whole line whose first word
/// is exactly `/compact` counts: `/compactfoo` and `see /compact` are ordinary messages.
fn compact_request(line: &str) -> Option<Option<String>> {
    let rest = line.strip_prefix("/compact")?;
    if rest.is_empty() {
        return Some(None);
    }
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let instructions = rest.trim();
    Some((!instructions.is_empty()).then(|| instructions.to_owned()))
}

/// Runs a manual `/compact` between turns (ADR 0018) and prints the result. Ctrl-C cancels the
/// in-flight summarizer; nothing is committed then. Returns the signal that asked this process
/// to stop, if one arrived while the summarizer ran (SIGTERM, ADR 0043 decision 1a fix round 1 B).
async fn run_compact(run: &mut Run, instructions: Option<String>) -> Option<i32> {
    let mut compact = Box::pin(run.inbox.compact(instructions));
    let mut stop = None;
    let result = loop {
        tokio::select! {
            result = &mut compact => break result,
            Some(signal) = run.signals.recv() => {
                run.inbox.cancel(CancelCause::Requested);
                if signal == SIGTERM {
                    stop = Some(signal);
                }
            }
        }
    };
    match result {
        Ok(agent_core::CompactionOutcome::Compacted {
            tokens_before,
            tokens_after,
            ..
        }) => eprintln!("[compaction] {tokens_before} tokens → {tokens_after} tokens"),
        Ok(agent_core::CompactionOutcome::NothingToCompact) => {
            eprintln!("[compaction] nothing to compact")
        }
        Err(message) => eprintln!("[compaction] failed: {message}"),
    }
    stop
}

pub(crate) async fn interactive_loop(run: &mut Run, lines: &LineHub) -> LoopDone {
    // True once the first Ctrl-C of a wind-down has been seen: the second one forces exit.
    let mut cancelling = false;
    loop {
        if !run.ui.busy() {
            cancelling = false;
            eprint!("> ");
            let _ = std::io::stderr().flush();
        }
        match next_event(
            lines,
            &run.ui,
            &run.inbox,
            &run.driver_ended,
            &mut run.signals,
            &mut cancelling,
        )
        .await
        {
            Next::Leave => break,
            Next::Terminated => {
                let code = terminate(&run.inbox, &run.session, &run.secrets, SIGTERM).await;
                return LoopDone {
                    code,
                    end: ExecEnd::Terminated,
                };
            }
            Next::Forced => {
                let code = force_exit(&run.session).await;
                return LoopDone {
                    code,
                    end: ExecEnd::Forced,
                };
            }
            Next::DriverEnded => {
                let (code, ok) = report_task(&run.session, &run.secrets).await;
                return LoopDone {
                    code,
                    end: ExecEnd::DriverEnded { code, ok },
                };
            }
            Next::Again => continue,
            Next::Line(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if line == "/exit" || line == "/quit" {
                    break;
                }
                // `/compact [instructions]` is a control request, not a message (ADR 0018).
                // Only a whole line whose first word is exactly `/compact` counts; `-p` mode
                // has no such command.
                if let Some(instructions) = compact_request(line) {
                    if let Some(signal) = run_compact(run, instructions).await {
                        let code = terminate(&run.inbox, &run.session, &run.secrets, signal).await;
                        return LoopDone {
                            code,
                            end: ExecEnd::Terminated,
                        };
                    }
                    continue;
                }
                // A turn in flight gets the line as a steer (it changes this turn's
                // direction); an idle driver gets a followup (a new turn). Both are
                // `source = User`.
                let result = enqueue_input(
                    &run.inbox,
                    &run.hooks,
                    line.to_owned(),
                    TERMINAL_SOURCE,
                    run.ui.busy(),
                )
                .await;
                if let Err(error) = result {
                    match &error {
                        InboxError::Full { .. } => eprintln!(
                            "radian: the inbox is full; that input was not queued (try again)"
                        ),
                        other => eprintln!("radian: {}", redact(&other.to_string(), &run.secrets)),
                    }
                    continue;
                }
                if run.ui.approval_may_read() {
                    // Do not read again until the turn has started: an approval answer the
                    // user types now must reach the approval prompt, not become the next
                    // input.
                    tokio::select! {
                        _ = run.ui.started.notified() => {}
                        _ = run.ui.finished.notified() => {}
                    }
                }
            }
        }
    }
    if let Some(code) = report_error(
        &shutdown(&run.inbox, &run.ui, &run.session).await,
        &run.secrets,
    ) {
        return LoopDone {
            code,
            end: ExecEnd::Status(run.ui.last_status()),
        };
    }
    // A clean leave ends the session; the `Result` outcome is the last turn's status, or a
    // plain completion when no turn ran.
    let end = match run.ui.last_status() {
        Some(status) => ExecEnd::Status(Some(status)),
        None => ExecEnd::NoTurns,
    };
    LoopDone { code: 0, end }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exit predicate, one row per reason to stay: a running job, an unhandled
    /// settle, a queued wake entry, or an active driver each blocks the exit on its own.
    /// Mutation: drop any conjunct (in particular the `handled >= settled` check) → the
    /// row that names it fails.
    #[test]
    fn the_exit_needs_no_running_job_no_unhandled_settle_no_wake_and_no_turn() {
        // Fully quiescent: may exit.
        assert!(may_exit(0, 2, 2, false, false));
        // No settle was ever recorded: `0 >= 0` exits (the no-background-job run).
        assert!(may_exit(0, 0, 0, false, false));
        // A job is still running.
        assert!(!may_exit(1, 0, 0, false, false));
        // A settle was recorded but its inject/followup has not returned: the turn it
        // wakes has not started yet, so exiting now would miss it.
        assert!(!may_exit(0, 2, 1, false, false));
        // A wake entry is queued: the driver is about to open a turn for it.
        assert!(!may_exit(0, 2, 2, true, false));
        // The driver is running a turn (or claimed its input and is about to).
        assert!(!may_exit(0, 2, 2, false, true));
        // Over-delivery is not a thing, but the comparison stays total either way.
        assert!(may_exit(0, 2, 3, false, false));
    }
}
