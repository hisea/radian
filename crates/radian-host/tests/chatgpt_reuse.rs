//! `ChatGptModel` reuses one HTTP client: two consecutive `complete()` calls against a
//! local keep-alive mock open **one** TCP connection. One `#[tokio::test]` in this file —
//! see `tests/chatgpt_headers.rs`'s header comment for why (`RADIAN_HOME` is process-wide).

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_core::{CancellationToken, Message, Model, ModelContext, ModelRequest, NoopSink};
use agent_provider_openai_responses::testing::{
    completed, message_item, output_item_done,
};
use agent_provider_openai_responses::{OpenAiResponsesConfig, build_client};
use radian_host::oauth_chatgpt::Endpoints;
use radian_host::{AuthStore, ChatGptModel, OAuthEntry};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn isolated_home() -> std::path::PathBuf {
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "radian-chatgpt-reuse-home-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: `get_or_init` runs this once, before any `AuthStore::open()` in this binary.
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        }
    }
    out
}

fn fake_access_token(account_id: &str) -> String {
    let header = base64url(br#"{"alg":"none"}"#);
    let payload = base64url(
        format!(r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#)
            .as_bytes(),
    );
    format!("{header}.{payload}.sig")
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[derive(Debug, Clone)]
struct Recorded {
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A minimal HTTP/1.1 keep-alive server: every response carries a full `Content-Length` and
/// `Connection: keep-alive`, so a client with a connection pool reuses the one TCP
/// connection. Counts accepted connections on the listener.
struct KeepAliveServer {
    base_url: String,
    connections: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<Recorded>>>,
    task: tokio::task::JoinHandle<()>,
}

impl KeepAliveServer {
    async fn start(bodies: Vec<Vec<u8>>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("address").port();
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(VecDeque::from(bodies)));
        let task = {
            let (connections, requests, replies) =
                (Arc::clone(&connections), Arc::clone(&requests), Arc::clone(&replies));
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    connections.fetch_add(1, Ordering::SeqCst);
                    let (requests, replies) = (Arc::clone(&requests), Arc::clone(&replies));
                    tokio::spawn(async move {
                        serve(stream, requests, replies).await;
                    });
                }
            })
        };
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            connections,
            requests,
            task,
        }
    }

    fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for KeepAliveServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    mut stream: tokio::net::TcpStream,
    requests: Arc<Mutex<Vec<Recorded>>>,
    replies: Arc<Mutex<VecDeque<Vec<u8>>>>,
) {
    loop {
        let Some(recorded) = read_request(&mut stream).await else {
            return;
        };
        requests.lock().unwrap().push(recorded);
        let body = replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| b"no reply was scripted".to_vec());
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
            body.len()
        );
        if stream.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        if stream.write_all(&body).await.is_err() {
            return;
        }
        if stream.flush().await.is_err() {
            return;
        }
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Recorded> {
    let mut buffer = Vec::new();
    let mut scratch = [0u8; 8192];
    let head_end = loop {
        if let Some(pos) = find(&buffer, b"\r\n\r\n") {
            break pos;
        }
        if buffer.len() > 16 * 1024 * 1024 {
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
    lines.next()?;
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
        if buffer.len() > 16 * 1024 * 1024 {
            return None;
        }
        let n = stream.read(&mut scratch).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&scratch[..n]);
    }
    let body = String::from_utf8_lossy(&buffer[body_start..body_start + length]).into_owned();
    Some(Recorded { headers, body })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn sse_body(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n").into_bytes())
        .collect::<Vec<_>>()
        .concat()
}

fn model(base_url: &str) -> ChatGptModel {
    ChatGptModel {
        base_url: base_url.to_owned(),
        model: "gpt-5.4".to_owned(),
        static_headers: vec![("originator".to_owned(), "radian".to_owned())],
        reasoning_effort: None,
        client: build_client(OpenAiResponsesConfig::new("", "").connect_timeout).unwrap(),
        endpoints: Endpoints::default(),
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        system_prompt: None,
        messages: vec![Message::user("hello")],
        tools: Vec::new(),
        settings: Value::Null,
    }
}

#[tokio::test]
async fn two_completes_reuse_one_tcp_connection() {
    let home = isolated_home();
    let access = fake_access_token("acct-reuse-test");
    {
        let mut store = AuthStore::open_at(home.join("credentials").join("auth.json")).unwrap();
        store
            .upsert_oauth(
                "chatgpt",
                &OAuthEntry {
                    access: access.clone(),
                    refresh: "refresh-unused".into(),
                    // Far out: no refresh fires, so only the Responses calls hit the server.
                    expires_at_millis: now_millis() + 3_600_000,
                },
            )
            .unwrap();
    }

    let server = KeepAliveServer::start(vec![
        sse_body(&[
            output_item_done(0, message_item("msg_1", "ok")),
            completed(None),
        ]),
        sse_body(&[
            output_item_done(0, message_item("msg_2", "ok again")),
            completed(None),
        ]),
    ])
    .await;

    let model = model(&server.base_url);
    for _ in 0..2 {
        model
            .complete(
                request(),
                ModelContext {
                    cancellation: CancellationToken::new(),
                    events: &NoopSink,
                },
            )
            .await
            .expect("the mock answers");
    }

    assert_eq!(server.requests().len(), 2, "both requests arrived");
    for recorded in server.requests() {
        assert_eq!(
            recorded.header("authorization"),
            Some(format!("Bearer {access}")).as_deref(),
            "every call still sends a fresh Authorization header"
        );
    }
    assert_eq!(
        server.connection_count(),
        1,
        "two completes over one reused client open one TCP connection"
    );
}
