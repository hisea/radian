//! `radian acp`: the Agent Client Protocol server (ADR 0049).
//!
//! One process, one session, JSON-RPC over stdio. The session itself is opened by
//! [`super::host::open_host_session`], the same function `radian exec` uses; this module is the
//! protocol surface: the read loop, `initialize`/`session/new`/`session/prompt`/`session/cancel`,
//! the live-update sink, and the pending-queue bridge that turns approvals and questions into
//! `session/request_permission`.

mod bridge;
mod events;
mod protocol;
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
                new_session(state, common, child_headers, &id, params).await;
            }
            "session/load" => state.out().respond_error(
                &id,
                protocol::METHOD_NOT_FOUND,
                "session/load is not supported in this build; loadSession is false",
            ),
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

/// `session/new`: creates the one session, or refuses because this process already holds one.
async fn new_session(
    state: &Arc<State>,
    common: &mut CommonArgs,
    child_headers: &mut Option<Result<Vec<String>, String>>,
    id: &Value,
    params: Value,
) {
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
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct NewSession {
        cwd: PathBuf,
        #[serde(default)]
        mcp_servers: Option<Vec<Value>>,
    }
    let request: NewSession = match serde_json::from_value(params) {
        Ok(request) => request,
        Err(error) => {
            state.out().respond_error(
                id,
                protocol::INVALID_PARAMS,
                &format!("session/new: {error}"),
            );
            return;
        }
    };
    if !request.cwd.is_absolute() {
        state.out().respond_error(
            id,
            protocol::INVALID_PARAMS,
            &format!("session/new cwd {} must be absolute", request.cwd.display()),
        );
        return;
    }
    let requested = match radian_host::resolve_workspace(Some(&request.cwd)) {
        Ok(workspace) => workspace,
        Err(error) => {
            state.out().respond_error(
                id,
                protocol::INVALID_PARAMS,
                &format!("session/new cwd: {error}"),
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
                        "one process holds one session; session/new cwd {} is not --workspace {}",
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
    common.workspace = Some(workspace.to_string_lossy().into_owned());
    let mcp = match protocol::parse_mcp_servers(&request.mcp_servers.unwrap_or_default()) {
        Ok(mcp) => mcp,
        Err(error) => {
            state
                .out()
                .respond_error(id, protocol::INVALID_PARAMS, &error);
            return;
        }
    };
    let Some(headers) = child_headers.take() else {
        state.out().respond_error(
            id,
            protocol::INTERNAL_ERROR,
            "the internal child headers were already consumed",
        );
        return;
    };
    let cli = Cli {
        common: common.clone(),
        ..Default::default()
    };
    let setup = match super::setup::setup(&cli, headers).await {
        Ok(setup) => setup,
        Err(error) => {
            state
                .out()
                .respond_error(id, protocol::INTERNAL_ERROR, &error.0);
            return;
        }
    };
    let session_id = setup.session_id.clone();
    let events = events::events(
        state.out().clone(),
        session_id.clone(),
        workspace.clone(),
        setup.window.context,
    );
    let runtime_events = Arc::new(AcpRuntimeEvents::new(setup.secrets.clone()));
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
    let task = bridge::spawn(
        Arc::clone(state),
        Arc::clone(&session),
        session_id.clone(),
        workspace,
    );
    *state.bridge_task.lock().unwrap() = Some(task);
    state.out().respond(id, json!({ "sessionId": session_id }));
}

/// Accepts `session/prompt` when the session is idle; a second prompt during a turn is
/// refused. The turn itself runs in its own task so the read loop keeps handling
/// `session/cancel` and permission answers.
fn prompt_request(state: &Arc<State>, id: &Value, params: Value) {
    let Some(session) = state.session() else {
        state.out().respond_error(
            id,
            protocol::INVALID_PARAMS,
            "session/prompt before session/new",
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
    let events = state
        .events()
        .ok_or((protocol::INTERNAL_ERROR, "no event sink".to_owned()))?;
    let before = events.finished_generation();
    session
        .inbox()
        .followup(radian_core::Message::user(text), InputSource::User)
        .await
        .map_err(|error| {
            (
                protocol::INTERNAL_ERROR,
                format!("cannot enqueue the prompt: {error}"),
            )
        })?;
    loop {
        let finished = events.finished.notified();
        if events.finished_generation() > before {
            break;
        }
        if let Some(end) = state.driver_end() {
            return Err((protocol::INTERNAL_ERROR, driver_message(&end)));
        }
        tokio::select! {
            biased;
            _ = finished => {}
            _ = state.driver_ended.notified() => {
                let end = state.driver_end().unwrap_or(radian_host::DriverEnd::Ok);
                return Err((protocol::INTERNAL_ERROR, driver_message(&end)));
            }
        }
    }
    let status = events.last_status();
    let summary = super::jsonl::summarize_journal(session.journal_path());
    let cost = summary.fee_micros.map(|micros| {
        json!({
            "micros": micros,
            "incomplete": summary.fee_incomplete,
            "estimated": session.projection().read().cost_estimated(),
        })
    });
    let mut radian = json!({
        "cost": cost,
        "text": summary.last_text,
    });
    if let Some(error) = state
        .runtime_events()
        .and_then(|events| events.last_error())
    {
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
