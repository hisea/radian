//! A mock Responses server for tests (feature `testing`): the shared mock HTTP server of
//! `agent-provider-common` plus the standard Responses event shapes recorded in
//! `crates/agent-provider-openai-responses/WIRE.md`.

use std::time::Duration;

use agent_provider_common::testing as common;
pub use agent_provider_common::testing::RecordedRequest;
use serde_json::{Value, json};

/// What the mock does with one request.
#[derive(Debug, Clone)]
pub enum Reply {
    /// `200` with an event stream, one network chunk per piece, `pause` between pieces, and
    /// these extra response headers.
    Sse {
        pieces: Vec<Vec<u8>>,
        pause: Duration,
        headers: Vec<(String, String)>,
    },
    /// The pieces, then silence: the connection stays open and nothing more is sent.
    SseThenHang { pieces: Vec<Vec<u8>> },
    /// The pieces, then the connection is cut without the end of the chunked body.
    SseThenCut { pieces: Vec<Vec<u8>> },
    /// A complete response with this status, extra headers and body.
    Status {
        code: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
}

impl From<Reply> for common::Reply {
    fn from(reply: Reply) -> Self {
        match reply {
            Reply::Sse {
                pieces,
                pause,
                headers,
            } => common::Reply::Sse {
                pieces,
                pause,
                headers,
            },
            Reply::SseThenHang { pieces } => common::Reply::SseThenHang { pieces },
            Reply::SseThenCut { pieces } => common::Reply::SseThenCut { pieces },
            Reply::Status {
                code,
                headers,
                body,
            } => common::Reply::Status {
                code,
                headers,
                body,
            },
        }
    }
}

impl Reply {
    /// An event stream with one `data:` line per event.
    pub fn sse(events: &[Value]) -> Self {
        Self::sse_with_headers(events, Vec::new())
    }

    /// Like [`Reply::sse`], with extra response headers.
    pub fn sse_with_headers(events: &[Value], headers: Vec<(String, String)>) -> Self {
        let pieces = common::sse_pieces(events);
        Reply::Sse {
            pieces,
            pause: Duration::ZERO,
            headers,
        }
    }

    /// A complete response with this status and body.
    pub fn status(code: u16, body: &str) -> Self {
        Self::status_with_headers(code, body, Vec::new())
    }

    /// A complete response with this status, body and extra headers.
    pub fn status_with_headers(code: u16, body: &str, headers: Vec<(String, String)>) -> Self {
        Reply::Status {
            code,
            headers,
            body: body.into(),
        }
    }

    /// The same bytes as [`Reply::sse`], but cut into pieces of `size` bytes, so events and
    /// even characters are split across network chunks.
    pub fn sse_split(events: &[Value], size: usize) -> Self {
        let bytes = common::sse_pieces(events).concat();
        Reply::Sse {
            pieces: bytes.chunks(size).map(<[u8]>::to_vec).collect(),
            pause: Duration::ZERO,
            headers: Vec::new(),
        }
    }
}

// ---------- event shapes ----------

pub fn created(usage: Option<Value>) -> Value {
    let mut response = json!({"id": "resp_test", "status": "in_progress", "usage": null});
    if let Some(usage) = usage {
        response["usage"] = usage;
    }
    json!({"type": "response.created", "response": response})
}

pub fn output_item_added(output_index: usize, item: Value) -> Value {
    json!({"type": "response.output_item.added", "output_index": output_index, "item": item})
}

pub fn output_item_done(output_index: usize, item: Value) -> Value {
    json!({"type": "response.output_item.done", "output_index": output_index, "item": item})
}

/// A completed assistant `message` item with one `output_text` part.
pub fn message_item(id: &str, text: &str) -> Value {
    json!({"type": "message", "id": id, "role": "assistant", "status": "completed",
           "content": [{"type": "output_text", "text": text}]})
}

/// A completed assistant `message` item with a refusal part.
pub fn refusal_item(id: &str, text: &str) -> Value {
    json!({"type": "message", "id": id, "role": "assistant", "status": "completed",
           "content": [{"type": "refusal", "refusal": text}]})
}

pub fn text_delta(output_index: usize, text: &str) -> Value {
    json!({"type": "response.output_text.delta", "output_index": output_index, "delta": text})
}

pub fn reasoning_summary_delta(output_index: usize, text: &str) -> Value {
    json!({"type": "response.reasoning_summary_text.delta", "output_index": output_index,
           "delta": text})
}

pub fn reasoning_item(id: &str, summary: &str, encrypted_content: &str) -> Value {
    json!({
        "type": "reasoning",
        "id": id,
        "summary": [{"type": "summary_text", "text": summary}],
        "encrypted_content": encrypted_content
    })
}

pub fn function_call_added(output_index: usize, item_id: &str, call_id: &str, name: &str) -> Value {
    output_item_added(
        output_index,
        json!({"type": "function_call", "id": item_id, "call_id": call_id, "name": name,
               "arguments": ""}),
    )
}

pub fn function_call_arguments_delta(output_index: usize, item_id: &str, delta: &str) -> Value {
    json!({"type": "response.function_call_arguments.delta", "output_index": output_index,
           "item_id": item_id, "delta": delta})
}

pub fn function_call_arguments_done(output_index: usize, item_id: &str, arguments: &str) -> Value {
    json!({"type": "response.function_call_arguments.done", "output_index": output_index,
           "item_id": item_id, "arguments": arguments})
}

/// A terminal `response.completed`. `end_turn: false` pauses the turn (codex).
pub fn completed(usage: Option<Value>) -> Value {
    terminal("response.completed", "completed", usage)
}

pub fn completed_pausing(usage: Option<Value>) -> Value {
    let mut event = terminal("response.completed", "completed", usage);
    event["response"]["end_turn"] = json!(false);
    event
}

pub fn incomplete(reason: &str, usage: Option<Value>) -> Value {
    let mut event = terminal("response.incomplete", "incomplete", usage);
    event["response"]["incomplete_details"] = json!({"reason": reason});
    event
}

fn terminal(kind: &str, status: &str, usage: Option<Value>) -> Value {
    let mut response = json!({"id": "resp_test", "status": status, "usage": null});
    if let Some(usage) = usage {
        response["usage"] = usage;
    }
    json!({"type": kind, "response": response})
}

pub fn failed(message: &str) -> Value {
    json!({"type": "response.failed",
           "response": {"id": "resp_test", "status": "failed",
                        "error": {"code": "server_error", "message": message}}})
}

pub fn error(message: &str) -> Value {
    json!({"type": "error", "error": {"code": "server_error", "message": message}})
}

/// The Responses usage object from the neutral fields the tests care about.
pub fn usage(input: u64, output: u64, total: u64, cached: u64) -> Value {
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": total
    })
}

/// The mock server; see `agent_provider_common::testing::MockServer`.
pub struct MockServer {
    /// `http://127.0.0.1:<port>` (the adapter appends `/responses`).
    pub base_url: String,
    inner: common::MockServer,
}

impl MockServer {
    pub async fn start(replies: Vec<Reply>) -> Self {
        let inner =
            common::MockServer::start(replies.into_iter().map(Into::into).collect(), "").await;
        Self {
            base_url: inner.base_url.clone(),
            inner,
        }
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.inner.requests()
    }
}
