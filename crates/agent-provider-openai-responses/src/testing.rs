//! A mock Responses server for tests (feature `testing`).
//!
//! It speaks just enough HTTP/1.1 to stream a chunked response with headers, hang, or cut the
//! connection, and it records every request it receives. Replies are used in order, one per
//! request. The event builders below produce the standard Responses event shapes recorded in
//! `crates/agent-provider-openai-responses/WIRE.md`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

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

impl Reply {
    /// An event stream with one `data:` line per event.
    pub fn sse(events: &[Value]) -> Self {
        Reply::Sse {
            pieces: sse_pieces(events),
            pause: Duration::ZERO,
            headers: Vec::new(),
        }
    }

    /// Like [`Reply::sse`], with extra response headers (e.g. the `x-codex-*` ones).
    pub fn sse_with_headers(events: &[Value], headers: Vec<(String, String)>) -> Self {
        Reply::Sse {
            pieces: sse_pieces(events),
            pause: Duration::ZERO,
            headers,
        }
    }

    /// A complete response with this status and body.
    pub fn status(code: u16, body: &str) -> Self {
        Reply::Status {
            code,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// A complete response with this status and extra headers.
    pub fn status_with_headers(code: u16, body: &str, headers: Vec<(String, String)>) -> Self {
        Reply::Status {
            code,
            headers,
            body: body.into(),
        }
    }

    /// The same bytes as [`Reply::sse`], but cut into pieces of `size` bytes, so events and even
    /// characters are split across network chunks.
    pub fn sse_split(events: &[Value], size: usize) -> Self {
        let bytes = sse_pieces(events).concat();
        Reply::Sse {
            pieces: bytes.chunks(size).map(<[u8]>::to_vec).collect(),
            pause: Duration::ZERO,
            headers: Vec::new(),
        }
    }
}

fn sse_pieces(events: &[Value]) -> Vec<Vec<u8>> {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n").into_bytes())
        .collect()
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

// ---------- the server ----------

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    /// Header names are lower-case.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("the request body is JSON")
    }
}

pub struct MockServer {
    /// `http://127.0.0.1:<port>` (the adapter appends `/responses`).
    pub base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    pub async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("address").port();
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task = {
            let (replies, requests) = (Arc::clone(&replies), Arc::clone(&requests));
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(handle(stream, Arc::clone(&replies), Arc::clone(&requests)));
                }
            })
        };
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
            task,
        }
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle(
    mut stream: TcpStream,
    replies: Arc<Mutex<VecDeque<Reply>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    // ADR 0026 D0: the host refreshes a provider's model list with `GET <base>/models`. Answer
    // it here, outside the scripted queue (a real endpoint serves it too), so the background
    // refresh cannot consume a reply a test scripted for a chat call. It is not recorded:
    // `requests()` stays the model calls a test asserts on.
    if request.method == "GET" && request.path.trim_end_matches('/').ends_with("/models") {
        let _ = write_plain(&mut stream, 200, &[], r#"{"object":"list","data":[]}"#).await;
        return;
    }
    requests.lock().unwrap().push(request);
    let reply = replies.lock().unwrap().pop_front();
    let _ = respond(&mut stream, reply).await;
}

async fn respond(stream: &mut TcpStream, reply: Option<Reply>) -> std::io::Result<()> {
    match reply {
        None => write_plain(stream, 500, &[], "no reply was scripted").await,
        Some(Reply::Status {
            code,
            headers,
            body,
        }) => write_plain(stream, code, &headers, &body).await,
        Some(Reply::Sse {
            pieces,
            pause,
            headers,
        }) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n{}\
                 transfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                response_headers(&headers)
            );
            stream.write_all(head.as_bytes()).await?;
            for piece in pieces {
                write_chunk(stream, &piece).await?;
                if !pause.is_zero() {
                    tokio::time::sleep(pause).await;
                }
            }
            stream.write_all(b"0\r\n\r\n").await?;
            stream.shutdown().await
        }
        Some(Reply::SseThenHang { pieces }) => {
            stream.write_all(sse_head(&[]).as_bytes()).await?;
            for piece in pieces {
                write_chunk(stream, &piece).await?;
            }
            std::future::pending().await
        }
        Some(Reply::SseThenCut { pieces }) => {
            stream.write_all(sse_head(&[]).as_bytes()).await?;
            for piece in pieces {
                write_chunk(stream, &piece).await?;
            }
            Ok(()) // dropping the stream cuts it without the closing chunk
        }
    }
}

fn sse_head(headers: &[(String, String)]) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n{}\
         transfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        response_headers(headers)
    )
}

fn response_headers(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .map(|(n, v)| format!("{n}: {v}\r\n"))
        .collect()
}

async fn write_chunk(stream: &mut TcpStream, piece: &[u8]) -> std::io::Result<()> {
    stream
        .write_all(format!("{:x}\r\n", piece.len()).as_bytes())
        .await?;
    stream.write_all(piece).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await
}

async fn write_plain(
    stream: &mut TcpStream,
    code: u16,
    headers: &[(String, String)],
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {code} Status\r\ncontent-type: application/json\r\n{}\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        response_headers(headers),
        body.len(),
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut scratch = [0u8; 8192];
    let head_end = loop {
        if let Some(pos) = find(&buffer, b"\r\n\r\n") {
            break pos;
        }
        if buffer.len() > MAX_REQUEST_BYTES {
            return None;
        }
        let n = stream.read(&mut scratch).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&scratch[..n]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?.to_owned(), first.next()?.to_owned());
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let body_start = head_end + 4;
    while buffer.len() < body_start + length {
        if buffer.len() > MAX_REQUEST_BYTES {
            return None;
        }
        let n = stream.read(&mut scratch).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&scratch[..n]);
    }
    let body = String::from_utf8_lossy(&buffer[body_start..body_start + length]).into_owned();
    Some(RecordedRequest {
        method,
        path,
        headers,
        body,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
