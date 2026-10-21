//! The terminal surface (`LineHub`, `Ui`): one stdin reader shared by the prompt
//! loop and the approval prompt, and the [`EventSink`] that streams model text to
//! stdout and everything else to stderr.
//!
//! Split from `lib.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::AsyncBufReadExt;
use tokio::sync::{Mutex, Notify, mpsc};

use radian_core::{AgentEvent, EventSink, RunStatus};
use tokio::sync::mpsc::UnboundedSender;

use super::jsonl::live_line;

/// Reads stdin on one dedicated task and hands each line to whoever needs it next: the
/// interactive prompt loop between turns, tool approval during one. A blocking read cannot
/// be cancelled, so approval must never read stdin directly — otherwise a cancelled turn
/// would leave a read behind that swallows the user's next line.
#[derive(Clone)]
pub(crate) struct LineHub {
    rx: Arc<Mutex<mpsc::Receiver<String>>>,
}

impl LineHub {
    pub(crate) fn spawn() -> Self {
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
            let mut line = String::new();
            loop {
                line.clear();
                match stdin.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\r', '\n']).to_owned();
                        if tx.send(trimmed).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            rx: Arc::new(Mutex::new(rx)),
        }
    }

    pub(crate) async fn next(&self) -> Option<String> {
        self.rx.lock().await.recv().await
    }
}

/// Streams model text to stdout; everything else — status, reasoning, tool lines — goes to
/// stderr. stdout stays free of status output so one-shot mode can be piped.
///
/// Reasoning is rendered on stderr with a `[reasoning]` prefix, on its own line, so it is
/// never confused with the visible answer (ADR 0014: reasoning is a separate channel).
#[derive(Default)]
pub(crate) struct Ui {
    streamed: AtomicBool,
    reasoning_open: AtomicBool,
    busy: AtomicBool,
    /// Resolves when a turn starts.
    pub(crate) started: Notify,
    /// Resolves when a turn ends.
    pub(crate) finished: Notify,
    /// True while the approval prompt may read stdin (so the prompt loop must not).
    approval_may_read: AtomicBool,
    last_status: StdMutex<Option<RunStatus>>,
    /// In `--output jsonl` mode the model text and reasoning deltas become live JSON lines on
    /// a channel instead of raw stdout text and `[reasoning]` stderr lines. `None` is the
    /// default mode, which is unchanged byte for byte.
    jsonl: Option<UnboundedSender<Vec<u8>>>,
    /// The resolved per-turn model call limit (the CLI flag or its default), so a
    /// `BudgetExceeded` turn names the number it ran under.
    max_model_calls: u32,
    /// Runs synchronously on every `TurnStarted`, before the turn can end. `exec -p`
    /// sets this to count started-but-unresolved turns (see
    /// `CommandHooks::note_turn_started`); `None` elsewhere.
    on_turn_started: StdMutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Ui {
    /// One UI over a run whose per-turn model call limit is `max_model_calls`. `jsonl` carries
    /// the live-line channel for `--output jsonl`, or `None` for the default mode.
    pub(crate) fn new(max_model_calls: u32, jsonl: Option<UnboundedSender<Vec<u8>>>) -> Self {
        Self {
            max_model_calls,
            jsonl,
            ..Self::default()
        }
    }

    /// True while a turn is in flight.
    pub(crate) fn busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    /// Sets the turn-start hook (see the field). Overwrites any previous one.
    pub(crate) fn set_turn_started_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.on_turn_started.lock().unwrap() = Some(hook);
    }

    pub(crate) fn set_approval_may_read(&self, yes: bool) {
        self.approval_may_read.store(yes, Ordering::Relaxed);
    }

    pub(crate) fn approval_may_read(&self) -> bool {
        self.approval_may_read.load(Ordering::Relaxed)
    }

    pub(crate) fn last_status(&self) -> Option<RunStatus> {
        self.last_status.lock().unwrap().clone()
    }

    /// Ends the streamed line, if anything was streamed this turn.
    pub(crate) fn end_of_turn(&self) {
        if self.reasoning_open.swap(false, Ordering::Relaxed) {
            eprintln!();
        }
        if self.streamed.swap(false, Ordering::Relaxed) {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }
    }
}

impl EventSink for Ui {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::TurnStarted => {
                self.busy.store(true, Ordering::Relaxed);
                self.started.notify_one();
                if let Some(hook) = self.on_turn_started.lock().unwrap().as_ref() {
                    hook();
                }
            }
            AgentEvent::ModelReasoningDelta { text } => {
                if let Some(tx) = &self.jsonl {
                    let _ = tx.send(live_line("ModelReasoningDelta", &text));
                    return;
                }
                if !self.reasoning_open.swap(true, Ordering::Relaxed) {
                    eprint!("[reasoning] ");
                }
                eprint!("{text}");
                let _ = std::io::stderr().flush();
            }
            AgentEvent::ModelTextDelta { text } => {
                if let Some(tx) = &self.jsonl {
                    let _ = tx.send(live_line("ModelTextDelta", &text));
                    return;
                }
                if self.reasoning_open.swap(false, Ordering::Relaxed) {
                    eprintln!();
                }
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(text.as_bytes());
                let _ = out.flush();
                self.streamed.store(true, Ordering::Relaxed);
            }
            AgentEvent::ToolStarted { index, name, .. } => {
                eprintln!("[tool {index}] {name}: started");
            }
            AgentEvent::ModelRetrying {
                attempt,
                max,
                delay,
                reason,
            } => {
                // ADR 0017: the same line the TUI shows, on stderr, before the wait.
                // The failed attempt's streamed text went to stdout and cannot be unwritten, so
                // this only closes its line before the retry notice and the next attempt.
                self.end_of_turn();
                eprintln!("retrying {attempt}/{max} in {}s: {reason}", delay.as_secs());
            }
            AgentEvent::ToolFinished {
                index, is_error, ..
            } => {
                if is_error {
                    eprintln!("[tool {index}] finished with an error");
                } else {
                    eprintln!("[tool {index}] finished");
                }
            }
            AgentEvent::CompactionStarted {
                tokens_before,
                keep_target,
                ..
            } => {
                eprintln!(
                    "[compaction] {tokens_before} tokens over the input budget; summarizing \
                     (keep target {keep_target})"
                );
            }
            AgentEvent::CompactionFinished {
                tokens_before,
                tokens_after,
                below_target,
                deterministic,
                dropped,
                ..
            } => {
                // ADR 0018: a deterministic fallback says so and names
                // the messages it dropped; a model summary keeps the token line.
                if deterministic {
                    eprintln!("[compaction] compacted without a summary ({dropped} dropped)");
                } else {
                    let note = if below_target {
                        " (below target; no further compaction this turn)"
                    } else {
                        ""
                    };
                    eprintln!("[compaction] {tokens_before} tokens → {tokens_after} tokens{note}");
                }
            }
            AgentEvent::CompactionFailed { reason } => {
                eprintln!("[compaction] failed: {reason}");
            }
            AgentEvent::CompactionSkipped { reason } => {
                eprintln!("[compaction] skipped: {reason}");
            }
            AgentEvent::RunFinished { status } => {
                if self.reasoning_open.swap(false, Ordering::Relaxed) {
                    eprintln!();
                }
                self.busy.store(false, Ordering::Relaxed);
                // End this turn's streamed line (and close a reasoning line) so the next
                // turn's output starts on its own line.
                self.end_of_turn();
                *self.last_status.lock().unwrap() = Some(status.clone());
                match &status {
                    RunStatus::Completed => {}
                    RunStatus::Cancelled(_) => {
                        // Carries both the lowercase hint and the debug status: the tests
                        // wait for either.
                        eprintln!("radian: cancelled ({status:?}); type another line or /exit")
                    }
                    // ADR 0015: the per-turn **call-count** budget (not the context window).
                    // The one line names the number of calls the turn used and the flag that
                    // raises it; `one_shot` does not repeat it.
                    RunStatus::BudgetExceeded => eprintln!(
                        "radian: {}",
                        radian_host::budget_exceeded_line(self.max_model_calls)
                    ),
                    other => eprintln!("[turn] ended as {other:?}"),
                }
                self.finished.notify_one();
            }
            _ => {}
        }
    }
}
