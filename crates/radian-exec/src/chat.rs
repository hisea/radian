//! The chat session (`chat`, ADR 0023 U5): establish the session through
//! `radian_host::open_session`, print the startup lines, and drive the prompt loop.
//!
//! Split from `lib.rs` by pure move (ADR 0043 decision 5): no behaviour change. The
//! session-opening half lives in [`super::host`] so the ACP surface reuses it (ADR 0049).

use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

use radian_core::{EventSink, ModelError};
use radian_permissions::Approver;
use tokio::sync::Notify;

use super::approval::TerminalApprover;
use super::events::spawn_signals;
use super::jsonl;
use super::redact::redact;
use super::turn::{LoopDone, Run, interactive_loop, one_shot};
use super::ui::{LineHub, Ui};
use super::{Cli, Setup, SetupError};

/// The exec host's runtime-event sink. It prints a failed turn's masked message as before and
/// keeps the structured `ModelError` of the latest turn that started, for the `Result` record
/// (ADR 0044 §2b). A `TurnStarted` clears it, so only the run's final turn can contribute;
/// a hook error reports no model error and leaves the field out.
pub(crate) struct ExecRuntimeEvents {
    secrets: Arc<Vec<String>>,
    last_model_error: Arc<Mutex<Option<ModelError>>>,
}

impl ExecRuntimeEvents {
    /// The sink plus the slot the `Result` writer reads (ADR 0044 §2b): the runtime hands the
    /// structured error to [`radian_runtime::RuntimeEvents::on_model_error`], and
    /// [`jsonl::result_line`] serializes the last one.
    pub(crate) fn new(secrets: Arc<Vec<String>>) -> (Self, Arc<Mutex<Option<ModelError>>>) {
        let last_model_error = Arc::new(Mutex::new(None));
        (
            Self {
                secrets,
                last_model_error: Arc::clone(&last_model_error),
            },
            last_model_error,
        )
    }
}

impl radian_runtime::RuntimeEvents for ExecRuntimeEvents {
    fn on_runtime_event(&self, event: &radian_session::RuntimeEvent) {
        match event {
            radian_session::RuntimeEvent::TurnFailed { message, .. } => {
                let message = redact(message, &self.secrets);
                eprintln!("radian: {message}");
                // ADR 0018's fallback: a recognizable context overflow gets one extra line.
                if radian_host::is_context_overflow(&message) {
                    eprintln!("radian: {}", radian_host::CONTEXT_OVERFLOW_HELP);
                }
            }
            radian_session::RuntimeEvent::TurnStarted { .. } => {
                *self.last_model_error.lock().unwrap() = None;
            }
            _ => {}
        }
    }

    fn on_model_error(&self, error: &ModelError) {
        *self.last_model_error.lock().unwrap() = Some(error.clone());
    }
}

pub(crate) async fn chat(setup: Setup, mut cli: Cli) -> Result<i32, SetupError> {
    let interactive = cli.prompt.is_none();
    // `-p -` reads the prompt from stdin to EOF before anything else starts: the line hub
    // below takes stdin as soon as it spawns, so reading here first is the only way the
    // prompt gets the whole pipe.
    if cli.prompt.as_deref() == Some("-") {
        let prompt = tokio::task::spawn_blocking(|| jsonl::read_prompt(std::io::stdin()))
            .await
            .map_err(|error| SetupError(format!("cannot read the prompt from stdin: {error}")))?
            .map_err(|error| SetupError(format!("cannot read the prompt from stdin: {error}")))?;
        cli.prompt = Some(prompt);
    }
    let jsonl_mode = cli.output == Some(super::jsonl::OutputMode::Jsonl);
    // The `--output jsonl` stdout pipeline: durable journal bytes and live delta lines meet
    // on one channel and one task owns stdout, so a blocked pipe never stalls a journal
    // write. Created before the session so the UI can hold the live sender from the start.
    let (jsonl_tx, jsonl_rx) = if jsonl_mode {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let lines = LineHub::spawn();
    // One-shot mode with no terminal on stdin has nobody to ask; interactive mode reads its
    // answers from the same line hub as the prompt loop.
    let can_ask = interactive || std::io::stdin().is_terminal();

    // The terminal approver and the UI sinks exist before the session, so the shared opener
    // can take them as bindings.
    let approver: Arc<dyn Approver> = Arc::new(TerminalApprover {
        lines: lines.clone(),
        can_ask,
    });
    let ui = Arc::new(Ui::new(setup.max_model_calls, jsonl_tx.clone()));
    let signals = spawn_signals();
    let driver_ended = Arc::new(Notify::new());
    // The last turn's structured model error, for the `Result` record (ADR 0044 §2b).
    let (runtime_events_inner, last_model_error): (
        Arc<dyn radian_runtime::RuntimeEvents>,
        Arc<Mutex<Option<ModelError>>>,
    ) = {
        let (sink, slot) = ExecRuntimeEvents::new(Arc::new(setup.secrets.clone()));
        (Arc::new(sink), slot)
    };

    // ADR 0043: the process control port. Bound before the session so its fan-out can
    // wrap the host's own sinks; a bind failure is a warning, never a startup refusal, so a
    // session always opens. The startup line names the path (a client finds it by pid too).
    let control = match radian_host::ControlServer::bind_if(
        setup.control_enabled,
        radian_host::radian_home().as_deref(),
    ) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("[control] disabled: {error}");
            None
        }
    };
    if let Some(server) = &control {
        eprintln!("[control] {}", server.path().display());
    }
    // Held for the whole session: the socket is removed on the normal return and on a panic
    // unwind, even while the driver task still holds a reference to the server (ADR 0043).
    let _control_guard = control.as_ref().map(|server| server.guard());
    let events: Arc<dyn EventSink> = match &control {
        Some(server) => server.events(Arc::clone(&ui) as Arc<dyn EventSink>),
        None => Arc::clone(&ui) as Arc<dyn EventSink>,
    };
    let runtime_events: Arc<dyn radian_runtime::RuntimeEvents> = match &control {
        Some(server) => server.runtime_events(runtime_events_inner),
        None => runtime_events_inner,
    };

    // The trust question requires a real terminal. Piped stdin is **not** an interactive
    // user: its first line could be a message (or even a stray `y`) and must never be read as
    // the answer to a trust question. `-p` and piped input are both non-interactive here, so
    // they take the untrusted default and the one-line hint.
    let trust_interactive = cli.prompt.is_none() && std::io::stdin().is_terminal();
    let opened = super::host::open_host_session(
        setup,
        &cli,
        super::host::HostWiring {
            approvals: radian_host::ApprovalRouting::Direct(approver),
            ask_user: false,
            events,
            runtime_events,
            driver_stopped: {
                let ended = Arc::clone(&driver_ended);
                Arc::new(move |_| ended.notify_one())
            },
            extra_mcp: radian_host::McpLoad::default(),
            trust_interactive,
            trust_lines: Some(lines.clone()),
        },
    )
    .await?;
    let super::host::OpenedSession {
        session,
        audit_task,
        secrets,
        images,
    } = opened;

    // ADR 0043: start accepting control requests for this session.
    if let Some(server) = &control {
        server.serve(
            &tokio::runtime::Handle::current(),
            radian_host::ControlTarget::of(&session),
        );
    }

    // The approval prompt and the prompt loop share stdin. While approval may read, the loop
    // must not compete for a line; with `--yes` (or nobody to ask) it may.
    ui.set_approval_may_read(can_ask && !cli.yes);
    // `--output jsonl`, installed once the journal exists: the tee returns the prefix length,
    // the prefix is read from the file (the lines written before the tee, e.g.
    // `SessionCreated`), and the printer owns stdout from here on. Because install and every
    // write take the same lock, no line is missed or printed twice.
    let (jsonl_printer, jsonl_prefix_len) = if jsonl_mode {
        let tx = jsonl_tx
            .clone()
            .expect("the jsonl channel exists in jsonl mode");
        let prefix_len = session.journal().set_tee(tx) as usize;
        let bytes = std::fs::read(session.journal_path()).map_err(|error| {
            SetupError(format!(
                "cannot read the session file {}: {error}",
                session.journal_path().display()
            ))
        })?;
        let end = prefix_len.min(bytes.len());
        let prefix = bytes[..end]
            .split_inclusive(|byte: &u8| *byte == b'\n')
            .map(|line| line.to_vec())
            .collect::<Vec<_>>();
        (
            Some(jsonl::spawn_printer(
                prefix,
                jsonl_rx.expect("the jsonl channel exists in jsonl mode"),
            )),
            prefix_len as u64,
        )
    } else {
        (None, 0)
    };
    // What the `Result` record reports. Read before the drops below: the projection lives
    // with the session.
    let jsonl_session_id = session.id().as_str().to_owned();
    let jsonl_journal_path = session.journal_path().to_path_buf();
    let jsonl_cost_estimated = session.projection().read().cost_estimated();
    let mut run = Run {
        inbox: Arc::clone(session.inbox()),
        hooks: Arc::clone(session.hooks()),
        ui: Arc::clone(&ui),
        driver_ended,
        signals,
        session: Arc::clone(&session),
        secrets: secrets.clone(),
    };
    // Count started-but-unresolved turns for `exec -p`'s exit decision (see
    // `CommandHooks::note_turn_started`): every turn start raises it synchronously,
    // `on_turn_end` resolves it, so the run never decides in the gap between
    // `RunFinished` emission and the Stop verdict.
    let command_hooks = run.session.command_hooks();
    run.ui.set_turn_started_hook(Arc::new(move || {
        command_hooks.note_turn_started();
    }));
    let done: LoopDone = if let Some(prompt) = cli.prompt {
        one_shot(&mut run, prompt, images).await
    } else {
        interactive_loop(&mut run, &lines).await
    };
    ui.end_of_turn();
    // Flush the audit: dropping the session drops the agent and the hooks, and dropping the
    // last sender closes the channel; the task writes what is left before it ends. The UI is
    // dropped here too: in `--output jsonl` mode it holds a live-line sender, and the
    // printer below only ends once every sender is gone.
    drop(run);
    drop(ui);
    drop(session);
    let _ = audit_task.await;
    // The `Result` line goes after the last durable line: every tee sender is dropped by now
    // (the UI with its run, the journal with its session and the flushed audit task), so
    // awaiting the printer drains the whole stream before the summary is folded.
    if let Some(printer) = jsonl_printer {
        drop(jsonl_tx);
        let _ = printer.await;
        let summary = jsonl::summarize_journal(&jsonl_journal_path);
        let run_summary = jsonl::summarize_journal_from(&jsonl_journal_path, jsonl_prefix_len);
        let line = jsonl::result_line(
            &jsonl_session_id,
            &done.end,
            &summary,
            &run_summary,
            jsonl_cost_estimated,
            last_model_error.lock().unwrap().as_ref(),
        );
        let _ = std::io::Write::write_all(&mut std::io::stdout().lock(), &line);
    }
    Ok(done.code)
}
