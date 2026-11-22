//! A mock Anthropic Messages server for tests (feature `testing`): the shared mock HTTP server
//! of `agent-provider-common` plus the official event shapes recorded in
//! `crates/agent-provider-anthropic/WIRE.md` §4.

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

// ---------- official event shapes (wire notes §4) ----------

/// `message_start` carrying an empty `content` and the initial usage, if any.
pub fn message_start(usage: Option<Value>) -> Value {
    let mut message = json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "content": [],
        "model": "claude-test",
        "stop_reason": null,
        "stop_sequence": null
    });
    if let Some(usage) = usage {
        message["usage"] = usage;
    }
    json!({"type": "message_start", "message": message})
}

pub fn text_start(index: usize) -> Value {
    json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}})
}

pub fn text_delta(index: usize, text: &str) -> Value {
    json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": text}})
}

pub fn block_stop(index: usize) -> Value {
    json!({"type": "content_block_stop", "index": index})
}

/// A `tool_use` block start. `id` is omitted when `None`, which is how the contract scripts a
/// provider that forgot the id.
pub fn tool_start(index: usize, id: Option<&str>, name: &str, input: Value) -> Value {
    let mut block = json!({"type": "tool_use", "name": name, "input": input});
    if let Some(id) = id {
        block["id"] = json!(id);
    }
    json!({"type": "content_block_start", "index": index, "content_block": block})
}

pub fn input_json_delta(index: usize, partial: &str) -> Value {
    json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": partial}})
}

pub fn message_delta(stop_reason: &str, usage: Option<Value>) -> Value {
    let mut event = json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": null}
    });
    if let Some(usage) = usage {
        event["usage"] = usage;
    }
    event
}

pub fn message_stop() -> Value {
    json!({"type": "message_stop"})
}

pub fn ping() -> Value {
    json!({"type": "ping"})
}

pub fn error(message: &str) -> Value {
    json!({"type": "error", "error": {"type": "overloaded_error", "message": message}})
}

/// The mock server; see `agent_provider_common::testing::MockServer`.
pub struct MockServer {
    /// `http://127.0.0.1:<port>` (the adapter appends `/v1/messages`).
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
