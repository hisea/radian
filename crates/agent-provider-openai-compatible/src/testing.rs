//! A mock OpenAI-compatible server for tests (feature `testing`).
//!
//! It speaks just enough HTTP/1.1 to stream a chunked response, hang, or cut the connection,
//! and it records every request it receives. Replies are used in order, one per request.

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
    /// `200` with an event stream, one network chunk per piece, `pause` between pieces.
    Sse {
        pieces: Vec<Vec<u8>>,
        pause: Duration,
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
    /// An event stream with one piece per event, ending with `[DONE]`.
    pub fn sse(chunks: &[Value]) -> Self {
        let mut pieces = sse_pieces(chunks);
        pieces.push(b"data: [DONE]\n\n".to_vec());
        Reply::Sse {
            pieces,
            pause: Duration::ZERO,
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

    /// The same bytes as [`Reply::sse`], but cut into pieces of `size` bytes, so events and
    /// even characters are split across network chunks.
    pub fn sse_split(chunks: &[Value], size: usize) -> Self {
        let mut bytes = sse_pieces(chunks).concat();
        bytes.extend_from_slice(b"data: [DONE]\n\n");
        Reply::Sse {
            pieces: bytes.chunks(size).map(<[u8]>::to_vec).collect(),
            pause: Duration::ZERO,
        }
    }
}

fn sse_pieces(chunks: &[Value]) -> Vec<Vec<u8>> {
    chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n").into_bytes())
        .collect()
}

/// A stream chunk with one choice.
pub fn chunk(delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]
    })
}

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
    /// `http://127.0.0.1:<port>/v1`
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
            base_url: format!("http://127.0.0.1:{port}/v1"),
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
    requests.lock().unwrap().push(request);
    let reply = replies.lock().unwrap().pop_front();
    let _ = respond(&mut stream, reply).await;
}

async fn respond(stream: &mut TcpStream, reply: Option<Reply>) -> std::io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                transfer-encoding: chunked\r\nconnection: close\r\n\r\n";
    match reply {
        None => write_plain(stream, 500, &[], "no reply was scripted").await,
        Some(Reply::Status {
            code,
            headers,
            body,
        }) => write_plain(stream, code, &headers, &body).await,
        Some(Reply::Sse { pieces, pause }) => {
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
            stream.write_all(head.as_bytes()).await?;
            for piece in pieces {
                write_chunk(stream, &piece).await?;
            }
            std::future::pending().await
        }
        Some(Reply::SseThenCut { pieces }) => {
            stream.write_all(head.as_bytes()).await?;
            for piece in pieces {
                write_chunk(stream, &piece).await?;
            }
            Ok(()) // dropping the stream cuts it without the closing chunk
        }
    }
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
    let extra: String = headers
        .iter()
        .map(|(n, v)| format!("{n}: {v}\r\n"))
        .collect();
    let response = format!(
        "HTTP/1.1 {code} Status\r\ncontent-type: application/json\r\n{extra}\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
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
