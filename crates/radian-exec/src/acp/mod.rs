//! `radian acp`: the Agent Client Protocol server (ADR 0049).
//!
//! One process, one session, JSON-RPC over stdio. The session itself is opened by
//! [`super::host::open_host_session`], the same function `radian exec` uses; this module is the
//! protocol surface: the read loop,
//! `initialize`/`session/new`/`session/load`/`session/prompt`/`session/cancel`, the live-update
//! sink, the `session/load` replay, and the pending-queue bridge that turns approvals and
//! questions into `session/request_permission`.

mod bridge;
mod events;
mod protocol;
mod replay;
mod wire;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use radian_core::{EventSink, InputSource, RunStatus};
use radian_runtime::CancelCause;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Notify, mpsc, oneshot};

use self::events::{AcpEvents, AcpRuntimeEvents};
use self::wire::{Incoming, Outbox, PermissionOutcome, RpcError};

use super::host::{self, HostWiring, OpenedSession};
use super::{Cli, CommonArgs};

/// The shared protocol and bridge state. One instance per process.
pub(crate) struct State {
    out: Outbox,
    next_id: AtomicI64,
    /// Outgoing request id -> the waiting end. Only `session/request_permission` uses it.
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>,
    session: Mutex<Option<Arc<radian_host::LocalSession>>>,
    session_id: Mutex<Option<String>>,
    events: Mutex<Option<Arc<AcpEvents>>>,
    runtime_events: Mutex<Option<Arc<AcpRuntimeEvents>>>,
    /// True from the moment a prompt is accepted until its turn ends.
    prompt_active: AtomicBool,
    /// Wakes an in-flight `request_permission` wait when the turn is cancelled.
    cancel: Notify,
    /// Wakes the prompt driver when the driver task ends.
    driver_ended: Notify,
    driver_end: Mutex<Option<radian_host::DriverEnd>>,
    audit_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    bridge_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    prompt_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Why the bridge gave up waiting for a `session/request_permission` answer.
pub(crate) enum RequestError {
    Rpc(RpcError),
    Cancelled,
}

impl State {
    fn new(out: Outbox) -> Self {
        Self {
            out,
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            session: Mutex::new(None),
            session_id: Mutex::new(None),
            events: Mutex::new(None),
            runtime_events: Mutex::new(None),
            prompt_active: AtomicBool::new(false),
            cancel: Notify::new(),
            driver_ended: Notify::new(),
            driver_end: Mutex::new(None),
            audit_task: Mutex::new(None),
            bridge_task: Mutex::new(None),
            prompt_task: Mutex::new(None),
        }
    }

    pub(crate) fn out(&self) -> &Outbox {
        &self.out
    }

    fn session(&self) -> Option<Arc<radian_host::LocalSession>> {
        self.session.lock().unwrap().clone()
    }

    fn session_id(&self) -> Option<String> {
        self.session_id.lock().unwrap().clone()
    }

    fn events(&self) -> Option<Arc<AcpEvents>> {
        self.events.lock().unwrap().clone()
    }

    fn runtime_events(&self) -> Option<Arc<AcpRuntimeEvents>> {
        self.runtime_events.lock().unwrap().clone()
    }

    fn driver_end(&self) -> Option<radian_host::DriverEnd> {
        self.driver_end.lock().unwrap().clone()
    }

    fn route_response(&self, id: &Value, result: Result<Value, RpcError>) -> bool {
        let Some(id) = id.as_i64() else {
            return false;
        };
        let sender = self.pending.lock().unwrap().remove(&id);
        match sender {
            Some(sender) => {
                let _ = sender.send(result);
                true
            }
            None => false,
        }
    }

    /// Sends one request to the client and waits for its answer, unless the turn is
    /// cancelled first. A cancelled wait removes its pending entry; a later answer is ignored.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, RequestError> {
        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, sender);
        self.out.request(id, method, params);
        tokio::select! {
            biased;
            answer = receiver => match answer {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(RequestError::Rpc(error)),
                Err(_) => Err(RequestError::Cancelled),
            },
            _ = self.cancel.notified() => {
                self.pending.lock().unwrap().remove(&id);
                Err(RequestError::Cancelled)
            }
        }
    }

    /// Cancels the running turn and wakes a pending permission wait.
    fn cancel_turn(&self) {
        self.cancel.notify_waiters();
        if let Some(session) = self.session() {
            session.inbox().cancel(CancelCause::Requested);
        }
    }

    /// `session/cancel` on an idle session is a no-op; the protocol allows it at any time.
    fn cancel_if_running(&self) {
        let running = self.prompt_active.load(Ordering::Acquire)
            || self.events().is_some_and(|events| events.busy())
            || self.session().is_some_and(|session| session.turn_running());
        if running {
            self.cancel_turn();
        }
    }
}

/// Runs `radian acp` until the client closes stdin or the process is told to stop.
pub(crate) async fn run(mut common: CommonArgs, child_headers: Result<Vec<String>, String>) -> i32 {
    let (out, writer) = wire::spawn_writer();
    let state = Arc::new(State::new(out));
    let mut child_headers = Some(child_headers);
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut sigterm = spawn_sigterm();
    let mut code = 0;
    loop {
        tokio::select! {
            line = stdin.next_line() => match line {
                Ok(Some(line)) => {
                    if !handle_line(&state, &mut common, &mut child_headers, &line).await {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            },
            _ = async {
                match sigterm.as_mut() {
                    Some(receiver) => {
                        receiver.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            } => {
                code = 143;
                break;
            }
        }
    }
    shutdown(&state).await;
    drop(stdin);
    drop(state);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), writer).await;
    code
}

/// One parsed line: a request, a notification, or a response to one of our requests.
async fn handle_line(
    state: &Arc<State>,
    common: &mut CommonArgs,
    child_headers: &mut Option<Result<Vec<String>, String>>,
    line: &str,
) -> bool {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            state.out().respond_error(
                &Value::Null,
                protocol::INVALID_PARAMS,
                &format!("invalid JSON: {error}"),
            );
            return true;
        }
    };
    match wire::classify(value) {
        Incoming::Request { id, method, params } => match method.as_str() {
            "initialize" => state.out().respond(&id, protocol::initialize_result()),
            // There is no ACP authentication method: the key comes from `auth.json`.
            "authenticate" => state.out().respond(&id, json!({})),
            "session/new" => {
                open_session_request(state, common, child_headers, &id, params, None).await;
            }
            "session/load" => {
                let Some(session_id) = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                else {
                    state.out().respond_error(
                        &id,
                        protocol::INVALID_PARAMS,
                        "session/load needs the id of an existing session",
                    );
                    return true;
                };
                open_session_request(state, common, child_headers, &id, params, Some(session_id))
                    .await;
            }
            "session/prompt" => prompt_request(state, &id, params),
            other => state.out().respond_error(
                &id,
                protocol::METHOD_NOT_FOUND,
                &format!("method `{other}` is not supported"),
            ),
        },
        Incoming::Notification { method, params } => {
            if method == "session/cancel" {
                let requested = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| state.session_id().as_deref() == Some(id));
                if requested {
                    state.cancel_if_running();
                }
            }
        }
        Incoming::Response { id, result } => {
            let cancelled = result
                .as_ref()
                .map(|value| {
                    matches!(
                        wire::permission_outcome(value),
                        PermissionOutcome::Cancelled
                    )
                })
                .unwrap_or(false);
            state.route_response(&id, result);
            // A `cancelled` outcome to a permission question interrupts the turn (ADR 0049).
            if cancelled {
                state.cancel_turn();
            }
        }
        Incoming::Invalid(message) => {
            state
                .out()
                .respond_error(&Value::Null, protocol::INVALID_PARAMS, &message);
        }
    }
    true
}

/// `session/new` and `session/load`: opens this process's one session, or refuses because it
/// already holds one. `load` is the id a `session/load` named; it opens the session through
/// `--resume`, the same lookup `radian exec` uses (ADR 0006, ADR 0022: the journal continues, it
/// is never forked).
async fn open_session_request(
    state: &Arc<State>,
    common: &mut CommonArgs,
    child_headers: &mut Option<Result<Vec<String>, String>>,
    id: &Value,
    params: Value,
    load: Option<String>,
) {
    let method = if load.is_some() {
        "session/load"
    } else {
        "session/new"
    };
    if let Some(session_id) = state.session_id() {
        state.out().respond_error(
            id,
            protocol::INVALID_PARAMS,
            &format!(
                "one process holds one session; this process already opened session {session_id}"
            ),
        );
        return;
    }
    // `--resume` already names the session this process was started for; a `session/load` of a
    // different id would be a second session.
    if let Some(session_id) = &load
        && let Some(explicit) = &common.resume
        && explicit != session_id
    {
        state.out().respond_error(
            id,
            protocol::INVALID_PARAMS,
            &format!(
                "one process holds one session; session/load names {session_id}, but this process \
                 was started to resume {explicit}"
            ),
        );
        return;
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct SessionRequest {
        cwd: PathBuf,
        #[serde(default)]
        mcp_servers: Option<Vec<Value>>,
    }
    let request: SessionRequest = match serde_json::from_value(params) {
        Ok(request) => request,
        Err(error) => {
            state
                .out()
                .respond_error(id, protocol::INVALID_PARAMS, &format!("{method}: {error}"));
            return;
        }
    };
    if !request.cwd.is_absolute() {
        state.out().respond_error(
            id,
            protocol::INVALID_PARAMS,
            &format!("{method} cwd {} must be absolute", request.cwd.display()),
        );
        return;
    }
    let requested = match radian_host::resolve_workspace(Some(&request.cwd)) {
        Ok(workspace) => workspace,
        Err(error) => {
            state.out().respond_error(
                id,
                protocol::INVALID_PARAMS,
                &format!("{method} cwd: {error}"),
            );
            return;
        }
    };
    let workspace = match &common.workspace {
        Some(flag) => {
            let flag = match radian_host::resolve_workspace(Some(Path::new(flag))) {
                Ok(workspace) => workspace,
                Err(error) => {
                    state.out().respond_error(
                        id,
                        protocol::INVALID_PARAMS,
                        &format!("--workspace: {error}"),
                    );
                    return;
                }
            };
            if flag != requested {
                state.out().respond_error(
                    id,
                    protocol::INVALID_PARAMS,
                    &format!(
                        "one process holds one session; {method} cwd {} is not --workspace {}",
                        requested.display(),
                        flag.display()
                    ),
                );
                return;
            }
            flag
        }
        None => requested,
    };
    let workspace_key = workspace.to_string_lossy().into_owned();
    // ADR 0049 decision 11: a session is loaded only in the workspace it was created in, the
    // rule `--export` applies (`SessionCreated.workspace`). Both are canonical paths
    // (`resolve_workspace`), so a workspace reached through a symlink matches. The check reads
    // the journal before `setup`, so a refusal writes nothing and leaves the process able to
    // load another session; a file the reader cannot open or that has no `SessionCreated` is
    // left to the opener, whose error names the id.
    if let Some(session_id) = &load
        && let Ok(dir) = radian_host::session_dir(common.session_dir.as_deref().map(Path::new))
        && let Some(recorded) =
            radian_host::sessions::other_workspace(&dir, session_id, &workspace_key)
    {
        state.out().respond_error(
            id,
            protocol::INVALID_PARAMS,
            &format!(
                "session/load: session {session_id:?} is not in this workspace; it was created \
                 in {recorded:?} and is only loaded there"
            ),
        );
        return;
    }
    // The session's copy of the arguments is built here and written back only once the
    // session is open. A failed `setup` therefore leaves the process's arguments as they were:
    // a retried `session/new` may name another cwd, and a failed `session/load` does not leave
    // its `--resume` behind for the retry.
    let mut session_common = common.clone();
    session_common.workspace = Some(workspace_key);
    // ADR 0049: `session/load` opens the named session exactly as `radian exec --resume` does.
    // An unknown id is an error from the opener that names it.
    if let Some(session_id) = &load {
        session_common.resume = Some(session_id.clone());
    }
    let mcp = match protocol::parse_mcp_servers(&request.mcp_servers.unwrap_or_default()) {
        Ok(mcp) => mcp,
        Err(error) => {
            state
                .out()
                .respond_error(id, protocol::INVALID_PARAMS, &error);
            return;
        }
    };
    // The headers are only consumed once the session is open: a `setup` that fails (a refused
    // config, a missing key) leaves them in place, so the client can fix the cause and retry
    // `session/new` in the same process.
    let Some(headers) = child_headers.as_ref() else {
        state.out().respond_error(
            id,
            protocol::INTERNAL_ERROR,
            "the internal child headers were already consumed",
        );
        return;
    };
    let cli = Cli {
        common: session_common.clone(),
        ..Default::default()
    };
    let setup = match super::setup::setup(&cli, headers.clone()).await {
        Ok(setup) => setup,
        Err(error) => {
            state
                .out()
                .respond_error(id, protocol::INTERNAL_ERROR, &error.0);
            return;
        }
    };
    let session_id = setup.session_id.clone();
    let (events, runtime_events) = events::events(
        state.out().clone(),
        session_id.clone(),
        workspace.clone(),
        setup.window.context,
        setup.secrets.clone(),
    );
    *state.events.lock().unwrap() = Some(Arc::clone(&events));
    *state.runtime_events.lock().unwrap() = Some(Arc::clone(&runtime_events));

    let state_for_stop = Arc::clone(state);
    let opened = host::open_host_session(
        setup,
        &cli,
        HostWiring {
            approvals: radian_host::ApprovalRouting::Queued,
            ask_user: true,
            events: Arc::clone(&events) as Arc<dyn EventSink>,
            runtime_events: Arc::clone(&runtime_events) as Arc<dyn radian_runtime::RuntimeEvents>,
            driver_stopped: Arc::new(move |end| {
                *state_for_stop.driver_end.lock().unwrap() = Some(end);
                state_for_stop.driver_ended.notify_one();
            }),
            extra_mcp: mcp,
            trust_interactive: false,
            trust_lines: None,
        },
    )
    .await;
    let OpenedSession {
        session,
        audit_task,
        ..
    } = match opened {
        Ok(opened) => opened,
        Err(error) => {
            *state.events.lock().unwrap() = None;
            *state.runtime_events.lock().unwrap() = None;
            state
                .out()
                .respond_error(id, protocol::INTERNAL_ERROR, &error.0);
            return;
        }
    };
    *state.audit_task.lock().unwrap() = Some(audit_task);
    *state.session.lock().unwrap() = Some(Arc::clone(&session));
    *state.session_id.lock().unwrap() = Some(session_id.clone());
    // The session is open, so the one-session rule now answers every later `session/new`; the
    // headers and the session arguments have done their one job.
    *child_headers = None;
    *common = session_common;
    let task = bridge::spawn(
        Arc::clone(state),
        Arc::clone(&session),
        session_id.clone(),
        workspace,
    );
    *state.bridge_task.lock().unwrap() = Some(task);
    // ADR 0049: a loaded session's transcript is replayed before the response, so the client
    // sees the same messages and tool calls a live session streamed. The transcript is the
    // journal's (`LocalSession::history`), so the replay and the model's next request agree.
    if load.is_some() {
        for update in replay::updates(&session.history, session.workspace()) {
            state.out().notify(
                "session/update",
                json!({ "sessionId": session_id, "update": update }),
            );
        }
        // `LoadSessionResponse` has no required field.
        state.out().respond(id, json!({}));
    } else {
        state.out().respond(id, json!({ "sessionId": session_id }));
    }
}

/// Accepts `session/prompt` when the session is idle; a second prompt during a turn is
/// refused. The turn itself runs in its own task so the read loop keeps handling
/// `session/cancel` and permission answers.
fn prompt_request(state: &Arc<State>, id: &Value, params: Value) {
    let Some(session) = state.session() else {
        state.out().respond_error(
            id,
            protocol::INVALID_PARAMS,
            "session/prompt before session/new or session/load",
        );
        return;
    };
    let busy = state.prompt_active.load(Ordering::Acquire)
        || session.turn_running()
        || state.events().is_some_and(|events| events.busy());
    if busy {
        state.out().respond_error(
            id,
            protocol::INVALID_PARAMS,
            "a second session/prompt during a turn is refused",
        );
        return;
    }
    state.prompt_active.store(true, Ordering::Release);
    let task_state = Arc::clone(state);
    let id = id.clone();
    let task = tokio::spawn(async move {
        let result = prompt_turn(&task_state, params).await;
        match result {
            Ok(result) => task_state.out().respond(&id, result),
            Err((code, message)) => task_state.out().respond_error(&id, code, &message),
        }
        task_state.prompt_active.store(false, Ordering::Release);
    });
    *state.prompt_task.lock().unwrap() = Some(task);
}

/// Runs one prompt turn and builds its `session/prompt` result.
async fn prompt_turn(state: &Arc<State>, params: Value) -> Result<Value, (i64, String)> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Prompt {
        session_id: String,
        prompt: Vec<Value>,
    }
    let request: Prompt = serde_json::from_value(params)
        .map_err(|error| (protocol::INVALID_PARAMS, format!("session/prompt: {error}")))?;
    let session = state
        .session()
        .ok_or((protocol::INVALID_PARAMS, "no session".to_owned()))?;
    let session_id = state
        .session_id()
        .ok_or((protocol::INVALID_PARAMS, "no session".to_owned()))?;
    if request.session_id != session_id {
        return Err((
            protocol::INVALID_PARAMS,
            format!("unknown session {}", request.session_id),
        ));
    }
    let text = protocol::prompt_text(&request.prompt)
        .map_err(|error| (protocol::INVALID_PARAMS, error))?;
    // ADR 0011: the prompt is a user message, and the turn's rights are a human's, so an
    // `ask` may reach the permission bridge (ADR 0012).
    let hooks = session.hooks();
    hooks.set_turn_source(Some(InputSource::User));
    hooks.observe_input_source(InputSource::User);
    let runtime_events = state
        .runtime_events()
        .ok_or((protocol::INTERNAL_ERROR, "no runtime event sink".to_owned()))?;
    let ledger = Arc::clone(runtime_events.ledger());
    let message_id = session
        .inbox()
        .followup(radian_core::Message::user(text), InputSource::User)
        .await
        .map_err(|error| {
            (
                protocol::INTERNAL_ERROR,
                format!("cannot enqueue the prompt: {error}"),
            )
        })?;
    // ADR 0049: the answer waits for the driver's **journaled** turn end, not only the core's
    // live `RunFinished`, and it waits for the end of the turn that claimed **this** message
    // (`InputClaimed`) — never for the next turn end to happen. A background job from an
    // earlier prompt can settle and open a system wake turn (ADR 0037) after the prompt was
    // enqueued; that turn's end answers nothing, and the prompt keeps waiting for its own.
    // When the prompt is merged into a turn with other queued input, that turn is its turn.
    let turn = loop {
        let changed = ledger.changed().notified();
        if let Some(turn) = ledger
            .turn_for(&message_id)
            .filter(|turn| ledger.is_journaled(turn))
        {
            break turn;
        }
        if let Some(end) = state.driver_end() {
            return Err((protocol::INTERNAL_ERROR, driver_message(&end)));
        }
        tokio::select! {
            biased;
            _ = changed => {}
            _ = state.driver_ended.notified() => {
                let end = state.driver_end().unwrap_or(radian_host::DriverEnd::Ok);
                return Err((protocol::INTERNAL_ERROR, driver_message(&end)));
            }
        }
    };
    // The status of **that** turn's live end: another turn's `RunFinished` must not decide this
    // answer's `stopReason`.
    let status = ledger.status(&turn);
    // The fee and text are that turn's own (ADR 0049): folding the journal now is safe because
    // the fold takes only the records between this turn's `TurnStarted` and `TurnFinished`, so
    // a turn that starts after it cannot leak in. `estimated` is false: every call of a turn
    // this process ran was priced as it arrived (ADR 0027).
    let summary = super::jsonl::summarize_turn(session.journal_path(), &turn);
    let cost = summary.fee_micros.map(|micros| {
        json!({
            "micros": micros,
            "incomplete": summary.fee_incomplete,
            "estimated": false,
        })
    });
    let mut radian = json!({
        "cost": cost,
        "text": summary.last_text,
    });
    if let Some(error) = ledger.error(&turn) {
        radian["error"] = Value::String(error);
    }
    Ok(json!({
        "stopReason": stop_reason(status.as_ref()),
        "_meta": { "radian": radian },
    }))
}

/// The ACP stop reason for a core turn status.
fn stop_reason(status: Option<&RunStatus>) -> &'static str {
    match status {
        Some(RunStatus::Completed) => "end_turn",
        Some(RunStatus::Cancelled(_)) => "cancelled",
        Some(RunStatus::Incomplete(radian_core::IncompleteReason::MaxTokens)) => "max_tokens",
        Some(RunStatus::BudgetExceeded) => "max_turn_requests",
        Some(_) => "refusal",
        None => "refusal",
    }
}

fn driver_message(end: &radian_host::DriverEnd) -> String {
    match end {
        radian_host::DriverEnd::Ok => "the driver ended".to_owned(),
        radian_host::DriverEnd::Failed(error) => error.clone(),
        radian_host::DriverEnd::Panicked(message) => {
            format!("the driver task ended unexpectedly: {message}")
        }
    }
}

/// Stops the session the way `radian exec` does: cancel what runs, close the inbox, join the
/// driver, and flush the audit.
async fn shutdown(state: &Arc<State>) {
    state.cancel.notify_waiters();
    let session = state.session();
    if let Some(session) = &session {
        session.inbox().cancel(CancelCause::Requested);
        session.inbox().close();
        let _ = session.join().await;
    }
    if let Some(task) = state.prompt_task.lock().unwrap().take() {
        task.abort();
    }
    if let Some(task) = state.bridge_task.lock().unwrap().take() {
        task.abort();
    }
    // Drop the session (and its journal) before waiting for the audit drain, which ends when
    // the journal's audit sender is gone.
    *state.session.lock().unwrap() = None;
    drop(session);
    let audit = state.audit_task.lock().unwrap().take();
    if let Some(task) = audit {
        let _ = task.await;
    }
}

/// A SIGTERM listener, so a wrapper that stops the process cancels the turn and closes the
/// session instead of leaving a half-written turn. `None` on a platform without the signal.
#[cfg(unix)]
fn spawn_sigterm() -> Option<mpsc::UnboundedReceiver<()>> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            return;
        };
        while term.recv().await.is_some() {
            if tx.send(()).is_err() {
                break;
            }
        }
    });
    Some(rx)
}

#[cfg(not(unix))]
fn spawn_sigterm() -> Option<mpsc::UnboundedReceiver<()>> {
    None
}
