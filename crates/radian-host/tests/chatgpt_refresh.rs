//! ADR 0026 slice 3: `ChatGptModel::complete` refreshes a near-expiry token before
//! sending the request (slice ①'s `ensure_fresh`), and a refresh failure is a clear model
//! error rather than a silent request with the stale token. One `#[tokio::test]` per file —
//! see `tests/chatgpt_headers.rs`'s header comment for why.

use std::sync::OnceLock;

use radian_core::{CancellationToken, Message, Model, ModelContext, ModelRequest, NoopSink};
use agent_provider_openai_responses::testing::{
    MockServer, Reply, completed, message_item, output_item_done,
};
use agent_provider_openai_responses::{OpenAiResponsesConfig, build_client};
use radian_host::oauth_chatgpt::Endpoints;
use radian_host::{AuthStore, ChatGptModel, OAuthEntry};
use serde_json::Value;

fn isolated_home() -> std::path::PathBuf {
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "radian-chatgpt-refresh-home-{}",
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

fn model(base_url: &str, endpoints: Endpoints) -> ChatGptModel {
    ChatGptModel {
        base_url: base_url.to_owned(),
        model: "gpt-5.4".to_owned(),
        static_headers: vec![("originator".to_owned(), "radian".to_owned())],
        reasoning_effort: None,
        client: build_client(OpenAiResponsesConfig::new("", "").connect_timeout).unwrap(),
        endpoints,
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

async fn complete(
    model: &ChatGptModel,
) -> Result<radian_core::ModelResponse, radian_core::ModelError> {
    model
        .complete(
            request(),
            ModelContext {
                cancellation: CancellationToken::new(),
                events: &NoopSink,
            },
        )
        .await
}

/// A minimal fake OAuth token endpoint: one scripted (status, body) reply per request,
/// recording how many times it was hit.
struct FakeTokenServer {
    addr: std::net::SocketAddr,
    hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl FakeTokenServer {
    fn start(reply_body: String) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_task = std::sync::Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                use std::io::{Read, Write};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                hits_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply_body.len(),
                    reply_body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self { addr, hits }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn hits(&self) -> usize {
        self.hits.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[tokio::test]
async fn a_near_expiry_token_is_refreshed_before_the_request_and_saved() {
    let home = isolated_home();
    let auth_path = home.join("credentials").join("auth.json");
    let fresh_access = fake_access_token("acct-refreshed");
    let token_server = FakeTokenServer::start(
        serde_json::json!({
            "access_token": fresh_access,
            "refresh_token": "refresh-2",
            "expires_in": 3600,
        })
        .to_string(),
    );
    {
        let mut store = AuthStore::open_at(auth_path.clone()).unwrap();
        store
            .upsert_oauth(
                "chatgpt",
                &OAuthEntry {
                    access: fake_access_token("acct-stale"),
                    refresh: "refresh-1".into(),
                    // 1 second out: inside REFRESH_MARGIN (120s), so a refresh must fire.
                    expires_at_millis: now_millis() + 1_000,
                },
            )
            .unwrap();
    }
    // Two replies: one for each `complete()` call below (the refreshed-token request, then
    // the no-refresh-needed request).
    let responses_server = MockServer::start(vec![
        Reply::sse(&[
            output_item_done(0, message_item("msg_1", "ok")),
            completed(None),
        ]),
        Reply::sse(&[
            output_item_done(0, message_item("msg_2", "ok again")),
            completed(None),
        ]),
    ])
    .await;
    let endpoints = Endpoints {
        token_url: token_server.url(),
        ..Endpoints::default()
    };
    let model = model(&responses_server.base_url, endpoints);
    complete(&model).await.expect("the request succeeds");

    assert_eq!(token_server.hits(), 1, "exactly one refresh call");
    let recorded = &responses_server.requests()[0];
    assert_eq!(
        recorded.header("authorization"),
        Some(format!("Bearer {fresh_access}")).as_deref(),
        "the request must carry the refreshed token, not the stale one"
    );
    let saved = AuthStore::open_at(auth_path.clone())
        .unwrap()
        .oauth_entry("chatgpt")
        .expect("the refreshed tokens were saved");
    assert_eq!(saved.access, fresh_access);
    assert_eq!(saved.refresh, "refresh-2");

    // A second completion, still within the margin we just set (expires_in 3600 means ~1h
    // out now), must NOT refresh again.
    complete(&model).await.expect("the second request succeeds");
    assert_eq!(
        token_server.hits(),
        1,
        "no second refresh within the margin"
    );
}

#[tokio::test]
async fn a_refresh_failure_is_a_clear_model_error_and_the_stale_entry_is_untouched() {
    let home = {
        let dir = std::env::temp_dir().join(format!(
            "radian-chatgpt-refresh-fail-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    };
    // Unlike the test above, this one never touches `RADIAN_HOME` (no `std::env::set_var`
    // at all): `AuthStore::open_at` and `oauth_ensure_fresh_in` both take an explicit path/
    // store, so it cannot race the other `#[tokio::test]` in this file over that shared,
    // process-wide env var.
    let auth_path = home.join("credentials").join("auth.json");
    let mut store = AuthStore::open_at(auth_path).unwrap();
    store
        .upsert_oauth(
            "chatgpt",
            &OAuthEntry {
                access: fake_access_token("acct-stale"),
                refresh: "refresh-1".into(),
                expires_at_millis: now_millis() + 1_000,
            },
        )
        .unwrap();
    let token_server =
        FakeTokenServer::start(serde_json::json!({"error": "invalid_grant"}).to_string());
    // A 200 with no access_token is "malformed", which `ensure_fresh_in` also reports as a
    // refresh failure (any shape of failure is the same clear "log in again" outcome).
    let endpoints = Endpoints {
        token_url: token_server.url(),
        ..Endpoints::default()
    };
    let entry = store.oauth_entry("chatgpt").unwrap();
    let error = radian_host::oauth_ensure_fresh_in(&endpoints, &mut store)
        .await
        .expect_err("the refresh fails");
    let message = error.to_string();
    assert!(
        message.contains("log in again") || message.contains("chatgpt"),
        "{message}"
    );
    let unchanged = store.oauth_entry("chatgpt").unwrap();
    assert_eq!(
        unchanged.access, entry.access,
        "the stale entry is untouched"
    );
}
