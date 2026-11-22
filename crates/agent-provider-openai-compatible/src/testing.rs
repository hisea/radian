//! A mock OpenAI-compatible server for tests (feature `testing`): the shared mock HTTP server
//! of `agent-provider-common` plus the Chat Completions chunk builder.

use std::time::Duration;

use agent_provider_common::testing as common;
pub use agent_provider_common::testing::Barrier;
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
    /// A barrier: this request waits until every `Reply::Barrier` sharing the same [`Barrier`]
    /// has arrived (or the barrier's own timeout expires), then gets `reply`. Queue one of
    /// these per expected request; a test asserts [`Barrier::peak`] to prove they overlapped.
    Barrier { barrier: Barrier, reply: Box<Reply> },
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
            Reply::Barrier { barrier, reply } => common::Reply::Barrier {
                barrier,
                reply: Box::new((*reply).into()),
            },
        }
    }
}

impl Reply {
    /// An event stream with one `data:` line per event, ending with `[DONE]`.
    pub fn sse(chunks: &[Value]) -> Self {
        Self::sse_with_headers(chunks, Vec::new())
    }

    /// Like [`Reply::sse`], with extra response headers.
    pub fn sse_with_headers(chunks: &[Value], headers: Vec<(String, String)>) -> Self {
        let mut pieces = common::sse_pieces(chunks);
        pieces.push(b"data: [DONE]\n\n".to_vec());
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
    pub fn sse_split(chunks: &[Value], size: usize) -> Self {
        let mut bytes = common::sse_pieces(chunks).concat();
        bytes.extend_from_slice(b"data: [DONE]\n\n");
        Reply::Sse {
            pieces: bytes.chunks(size).map(<[u8]>::to_vec).collect(),
            pause: Duration::ZERO,
            headers: Vec::new(),
        }
    }
}

/// A stream chunk with one choice.
pub fn chunk(delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]
    })
}

/// The mock server; see `agent_provider_common::testing::MockServer`.
pub struct MockServer {
    /// `http://127.0.0.1:<port>/v1`
    pub base_url: String,
    inner: common::MockServer,
}

impl MockServer {
    pub async fn start(replies: Vec<Reply>) -> Self {
        let inner =
            common::MockServer::start(replies.into_iter().map(Into::into).collect(), "/v1").await;
        Self {
            base_url: inner.base_url.clone(),
            inner,
        }
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.inner.requests()
    }
}
