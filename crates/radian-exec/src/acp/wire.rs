//! The JSON-RPC 2.0 layer over stdio (ADR 0049): one writer task owns stdout, and every
//! message the host sends goes through an unbounded channel, so a turn's event sink never
//! blocks on a pipe.

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// The sending end of the stdout writer.
#[derive(Clone)]
pub(crate) struct Outbox {
    tx: mpsc::UnboundedSender<Value>,
}

impl Outbox {
    /// A notification: no id, no response.
    pub(crate) fn notify(&self, method: &str, params: Value) {
        let _ = self.tx.send(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    /// A successful response to the client's request `id`.
    pub(crate) fn respond(&self, id: &Value, result: Value) {
        let _ = self.tx.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }));
    }

    /// An error response to the client's request `id`.
    pub(crate) fn respond_error(&self, id: &Value, code: i64, message: &str) {
        let _ = self.tx.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }));
    }

    /// A request from the host to the client.
    pub(crate) fn request(&self, id: i64, method: &str, params: Value) {
        let _ = self.tx.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
    }
}

/// Starts the task that serializes every outgoing message, one JSON object per line. A write
/// failure (the client closed stdout) ends the task; later sends are dropped.
pub(crate) fn spawn_writer() -> (Outbox, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(message) = rx.recv().await {
            let Ok(mut bytes) = serde_json::to_vec(&message) else {
                continue;
            };
            bytes.push(b'\n');
            if out.write_all(&bytes).await.is_err() {
                return;
            }
            let _ = out.flush().await;
        }
    });
    (Outbox { tx }, task)
}

/// One incoming JSON-RPC message, classified. A response has an `id` and a `result` or
/// `error` and no `method`; a request has both `id` and `method`; a notification has a
/// `method` and no `id`.
pub(crate) enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        result: Result<Value, RpcError>,
    },
    Invalid(String),
}

/// A JSON-RPC error the client returned for one of the host's requests.
#[derive(Debug, Clone)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub(crate) fn message(&self) -> String {
        format!("{} (code {})", self.message, self.code)
    }
}

/// Classifies one parsed line. Unknown top-level shapes are `Invalid`, never silently dropped.
pub(crate) fn classify(value: Value) -> Incoming {
    let Some(object) = value.as_object() else {
        return Incoming::Invalid("the message is not a JSON object".to_owned());
    };
    let method = object.get("method").and_then(Value::as_str);
    let id = object.get("id");
    match (method, id) {
        (Some(method), Some(id)) => Incoming::Request {
            id: id.clone(),
            method: method.to_owned(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        },
        (Some(method), None) => Incoming::Notification {
            method: method.to_owned(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        },
        (None, Some(id)) => {
            if let Some(error) = object.get("error") {
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32603);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the client returned an error")
                    .to_owned();
                Incoming::Response {
                    id: id.clone(),
                    result: Err(RpcError { code, message }),
                }
            } else {
                Incoming::Response {
                    id: id.clone(),
                    result: Ok(object.get("result").cloned().unwrap_or(Value::Null)),
                }
            }
        }
        (None, None) => Incoming::Invalid("the message has neither a method nor an id".to_owned()),
    }
}

/// The outcome of a `session/request_permission` response.
pub(crate) enum PermissionOutcome {
    Selected(String),
    Cancelled,
    Invalid(String),
}

/// Reads the protocol's nested outcome object:
/// `{"outcome": {"outcome": "selected", "optionId": "…"}}` or `{"outcome": {"outcome":
/// "cancelled"}}`.
pub(crate) fn permission_outcome(value: &Value) -> PermissionOutcome {
    let outcome = value.get("outcome").unwrap_or(value);
    match outcome.get("outcome").and_then(Value::as_str) {
        Some("selected") => match outcome.get("optionId").and_then(Value::as_str) {
            Some(option) => PermissionOutcome::Selected(option.to_owned()),
            None => PermissionOutcome::Invalid("a selected outcome has no optionId".to_owned()),
        },
        Some("cancelled") => PermissionOutcome::Cancelled,
        other => PermissionOutcome::Invalid(format!("unknown permission outcome {other:?}")),
    }
}
