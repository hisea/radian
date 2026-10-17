//! Integration tests for ADR 0026 slice ① against a local fake authorization /
//! device server — **never** `auth.openai.com` or `chatgpt.com` (AGENTS.md).

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener as StdTcpListener, TcpStream as StdTcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use radian_host::oauth_chatgpt::{
    CallbackListener, DevicePoll, Endpoints, OAuthError, device_poll_once, device_start,
    exchange_code, poll_device_until_done, refresh_token,
};
use radian_host::{AuthStore, OAuthEntry};

const PATH: &str = "/auth/callback";

/// Per-path queued (status, body) replies.
type ScriptedReplies = Arc<Mutex<HashMap<String, VecDeque<(u16, String)>>>>;
/// Every (path, body) this server has seen, in order.
type SeenRequests = Arc<Mutex<Vec<(String, String)>>>;

/// A queued-response fake HTTP server (one thread, blocking `std::net`): good enough for the
/// token/device endpoints, which this module calls one at a time and never concurrently.
struct FakeServer {
    addr: SocketAddr,
    responses: ScriptedReplies,
    requests: SeenRequests,
}

impl FakeServer {
    fn start() -> Self {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind a local listener");
        let addr = listener.local_addr().expect("local addr");
        let responses: ScriptedReplies = Arc::new(Mutex::new(HashMap::new()));
        let requests: SeenRequests = Arc::new(Mutex::new(Vec::new()));
        let (responses_task, requests_task) = (Arc::clone(&responses), Arc::clone(&requests));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let Some((path, body)) = read_request(&mut stream) else {
                    continue;
                };
                requests_task.lock().unwrap().push((path.clone(), body));
                let reply = responses_task
                    .lock()
                    .unwrap()
                    .get_mut(&path)
                    .and_then(VecDeque::pop_front);
                let (status, body) =
                    reply.unwrap_or((500, "no reply scripted for this path".to_owned()));
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: \
                     {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            addr,
            responses,
            requests,
        }
    }

    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn push(&self, path: &str, status: u16, body: &str) {
        self.responses
            .lock()
            .unwrap()
            .entry(path.to_owned())
            .or_default()
            .push_back((status, body.to_owned()));
    }

    fn request_count(&self, path: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .count()
    }

    fn endpoints(&self) -> Endpoints {
        Endpoints {
            authorize_url: format!("{}/oauth/authorize", self.base()),
            token_url: format!("{}/oauth/token", self.base()),
            device_usercode_url: format!("{}/deviceauth/usercode", self.base()),
            device_token_url: format!("{}/deviceauth/token", self.base()),
        }
    }
}

fn read_request(stream: &mut StdTcpStream) -> Option<(String, String)> {
    let mut buffer = Vec::new();
    let mut scratch = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = find(&buffer, b"\r\n\r\n") {
            break pos;
        }
        if buffer.len() > 1024 * 1024 {
            return None;
        }
        let n = stream.read(&mut scratch).ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&scratch[..n]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    let _method = parts.next()?;
    let target = parts.next()?;
    let path = target.split('?').next().unwrap_or(target).to_owned();
    let length: usize = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);
    let body_start = head_end + 4;
    while buffer.len() < body_start + length {
        let n = stream.read(&mut scratch).ok()?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&scratch[..n]);
    }
    let end = (body_start + length).min(buffer.len());
    let body = String::from_utf8_lossy(&buffer[body_start..end]).into_owned();
    Some((path, body))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// An unsigned (`alg: none`) JWT carrying the ChatGPT workspace claim — all this module reads
/// out of it, by design (see `oauth_chatgpt::decode_account_id`'s doc comment on why no
/// signature check is needed).
fn fake_access_token(account_id: &str) -> String {
    let header = base64url(br#"{"alg":"none"}"#);
    let payload = base64url(
        format!(r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#)
            .as_bytes(),
    );
    format!("{header}.{payload}.sig")
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

fn token_body(access: &str, refresh: &str, expires_in: u64) -> String {
    serde_json::json!({
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": expires_in,
    })
    .to_string()
}

/// A scratch `auth.json` under its own temp `RADIAN_HOME`-shaped root.
struct Scratch {
    root: std::path::PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "radian-oauth-chatgpt-test-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn path(&self) -> std::path::PathBuf {
        self.root.join("credentials").join("auth.json")
    }

    fn open(&self) -> AuthStore {
        AuthStore::open_at(self.path()).expect("open")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Connects to `addr` and sends `GET {path}?{query} HTTP/1.1`, reading (and discarding) the
/// response so the connection closes cleanly on both ends.
fn send_callback(addr: SocketAddr, path: &str, query: &str) {
    let mut stream = StdTcpStream::connect(addr).expect("connect to the callback listener");
    let request =
        format!("GET {path}?{query} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
}

// Both callback-listener tests below block synchronously (`std::net`) on the *same* thread
// that must also drive the spawned `listener.wait(...)` future, simulating the external
// browser making a real TCP connection rather than calling internals directly; the default
// single-threaded `#[tokio::test]` runtime would deadlock (the blocking send starves the
// task that has to `accept()` and answer it), so these two need a second worker thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_matching_callback_completes_and_the_port_then_refuses_a_second_try() {
    let listener = CallbackListener::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let waiting = tokio::spawn(listener.wait(PATH, "the-state", Duration::from_secs(5)));
    // Give the spawned task a moment to reach `accept()` before connecting (bounded by the
    // 5s timeout above if this is somehow too eager).
    tokio::time::sleep(Duration::from_millis(20)).await;
    send_callback(addr, PATH, "code=abc123&state=the-state");
    let code = waiting.await.unwrap().expect("the first callback succeeds");
    assert_eq!(code, "abc123");

    // One-shot: a second connection attempt to the same address is refused outright — the
    // listener (and the OS socket backing it) is gone.
    let second = StdTcpStream::connect(addr);
    assert!(
        second.is_err(),
        "a second connection must be refused after the one-shot listener is done"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_state_mismatch_is_refused_and_never_reaches_the_token_endpoint() {
    let server = FakeServer::start();
    let listener = CallbackListener::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let waiting = tokio::spawn(listener.wait(PATH, "expected-state", Duration::from_secs(5)));
    tokio::time::sleep(Duration::from_millis(20)).await;
    send_callback(addr, PATH, "code=abc123&state=WRONG-STATE");
    let error = waiting
        .await
        .unwrap()
        .expect_err("a mismatched state is refused");
    assert!(matches!(error, OAuthError::StateMismatch), "{error}");
    assert_eq!(
        server.request_count("/oauth/token"),
        0,
        "the token endpoint must never see a request from a refused callback"
    );
}

#[tokio::test]
async fn port_busy_is_a_clear_error_naming_the_device_code_as_the_alternative() {
    // Hold the production port ourselves, then ask for it again the same way `BrowserLogin`
    // would: the error must name the real problem, not time out or panic.
    let hold = StdTcpListener::bind((
        std::net::Ipv4Addr::LOCALHOST,
        radian_host::oauth_chatgpt::CALLBACK_PORT,
    ));
    let Ok(hold) = hold else {
        // Some sandboxed CI runners cannot bind privileged-adjacent ports or already run
        // something there; skip rather than false-fail on an environment this test does not
        // control.
        eprintln!(
            "skipping: could not bind {} to set up the test",
            radian_host::oauth_chatgpt::CALLBACK_PORT
        );
        return;
    };
    let endpoints = Endpoints::default();
    let error = radian_host::BrowserLogin::start(&endpoints)
        .await
        .expect_err("the port is already held");
    assert!(
        matches!(error, OAuthError::PortBusy { port } if port == radian_host::oauth_chatgpt::CALLBACK_PORT)
    );
    let message = error.to_string();
    assert!(message.contains("device"), "{message}");
    drop(hold);
}

#[tokio::test]
async fn exchange_stores_an_oauth_entry_at_0600() {
    let server = FakeServer::start();
    let access = fake_access_token("acct-123");
    server.push(
        "/oauth/token",
        200,
        &token_body(&access, "refresh-abc", 3600),
    );
    let token = exchange_code(
        &server.endpoints(),
        "the-code",
        "the-verifier",
        "http://localhost:1455/auth/callback",
    )
    .await
    .expect("exchange succeeds");
    assert_eq!(token.access, access);
    assert_eq!(token.refresh, "refresh-abc");
    assert_eq!(token.account_id, "acct-123");

    let scratch = Scratch::new("exchange-store");
    let mut store = scratch.open();
    radian_host::oauth_store_token(&mut store, &token).unwrap();
    let reopened = scratch.open();
    let entry = reopened
        .oauth_entry("chatgpt")
        .expect("the entry was written");
    assert_eq!(entry.access, access);
    assert_eq!(entry.refresh, "refresh-abc");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(scratch.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{mode:04o}");
    }
}

#[tokio::test]
async fn refresh_runs_only_within_the_margin_and_writes_back() {
    let server = FakeServer::start();
    let scratch = Scratch::new("refresh-margin");
    let mut store = scratch.open();

    // Far from expiry: `ensure_fresh_in` must not call the network at all.
    store
        .upsert_oauth(
            "chatgpt",
            &OAuthEntry {
                access: "still-good".into(),
                refresh: "r0".into(),
                expires_at_millis: now_millis() + Duration::from_secs(3600).as_millis() as u64,
            },
        )
        .unwrap();
    let access = radian_host::oauth_ensure_fresh_in(&server.endpoints(), &mut store)
        .await
        .unwrap();
    assert_eq!(access, "still-good");
    assert_eq!(server.request_count("/oauth/token"), 0, "no refresh yet");

    // Within the margin: a refresh fires and the new tokens land in the store.
    store
        .upsert_oauth(
            "chatgpt",
            &OAuthEntry {
                access: "stale".into(),
                refresh: "r0".into(),
                expires_at_millis: now_millis() + 1_000, // 1s: inside REFRESH_MARGIN (120s)
            },
        )
        .unwrap();
    let fresh_access = fake_access_token("acct-refreshed");
    server.push("/oauth/token", 200, &token_body(&fresh_access, "r1", 3600));
    let access = radian_host::oauth_ensure_fresh_in(&server.endpoints(), &mut store)
        .await
        .unwrap();
    assert_eq!(access, fresh_access);
    assert_eq!(server.request_count("/oauth/token"), 1);
    let entry = store.oauth_entry("chatgpt").unwrap();
    assert_eq!(entry.access, fresh_access);
    assert_eq!(entry.refresh, "r1");
}

/// ADR 0026: a token obtained **mid-session** must be redactable the
/// moment it is written. A redaction list read from `resolved_keys()` at startup cannot carry
/// it, so `store_token` registers what it writes. The registry is process-global, so every
/// value here is unique to this test and it never asserts the registry is empty.
#[tokio::test]
async fn a_refresh_registers_the_new_tokens_for_redaction() {
    let server = FakeServer::start();
    let scratch = Scratch::new("refresh-registers");
    let mut store = scratch.open();

    // A still-valid token first: `ensure_fresh_in` returns it without writing anything, so
    // neither it nor its refresh token is registered — only `store_token` registers, never a
    // direct `upsert_oauth`.
    let untouched_access = fake_access_token("acct-refresh-registers-untouched");
    let untouched_refresh = "refresh-registers-untouched-1";
    store
        .upsert_oauth(
            "chatgpt",
            &OAuthEntry {
                access: untouched_access.clone(),
                refresh: untouched_refresh.into(),
                expires_at_millis: now_millis() + Duration::from_secs(3600).as_millis() as u64,
            },
        )
        .unwrap();
    let access = radian_host::oauth_ensure_fresh_in(&server.endpoints(), &mut store)
        .await
        .unwrap();
    assert_eq!(access, untouched_access);
    assert_eq!(server.request_count("/oauth/token"), 0, "no refresh yet");
    let registered = radian_host::registered_secrets();
    assert!(
        !registered.contains(&untouched_access),
        "a token that was not refreshed must not be registered: {registered:?}"
    );
    assert!(
        !registered.contains(&untouched_refresh.to_owned()),
        "neither must its refresh token: {registered:?}"
    );

    // Within the margin: the refresh writes a new pair, and both values are already in the
    // list a redactor reads *now*.
    store
        .upsert_oauth(
            "chatgpt",
            &OAuthEntry {
                access: "stale".into(),
                refresh: "r0".into(),
                expires_at_millis: now_millis() + 1_000, // 1s: inside REFRESH_MARGIN (120s)
            },
        )
        .unwrap();
    let fresh_access = fake_access_token("acct-refresh-registers-fresh");
    let fresh_refresh = "refresh-registers-fresh-1";
    server.push(
        "/oauth/token",
        200,
        &token_body(&fresh_access, fresh_refresh, 3600),
    );
    let access = radian_host::oauth_ensure_fresh_in(&server.endpoints(), &mut store)
        .await
        .unwrap();
    assert_eq!(access, fresh_access);
    let registered = radian_host::registered_secrets();
    assert!(
        registered.contains(&fresh_access),
        "the refreshed access token must be registered for redaction: {registered:?}"
    );
    assert!(
        registered.contains(&fresh_refresh.to_owned()),
        "the rotated refresh token must be registered too: {registered:?}"
    );
}

#[tokio::test]
async fn a_refresh_failure_tells_the_user_to_log_in_again() {
    let server = FakeServer::start();
    server.push(
        "/oauth/token",
        400,
        r#"{"error":"invalid_grant","error_description":"refresh token revoked"}"#,
    );
    let scratch = Scratch::new("refresh-fail");
    let mut store = scratch.open();
    store
        .upsert_oauth(
            "chatgpt",
            &OAuthEntry {
                access: "stale".into(),
                refresh: "revoked".into(),
                expires_at_millis: now_millis(),
            },
        )
        .unwrap();
    let error = radian_host::oauth_ensure_fresh_in(&server.endpoints(), &mut store)
        .await
        .expect_err("the refresh fails");
    let message = error.to_string();
    assert!(
        message.contains("log in again") || message.contains("/login chatgpt"),
        "{message}"
    );
    assert!(
        !message.contains("revoked") || message.contains("invalid_grant"),
        "{message}"
    );
    // The stale entry is untouched by a failed refresh.
    let entry = store.oauth_entry("chatgpt").unwrap();
    assert_eq!(entry.access, "stale");
}

/// Refresh without a rotated refresh token keeps the one that was used (OpenAI does not
/// always rotate it).
#[tokio::test]
async fn refresh_keeps_the_old_refresh_token_when_none_is_returned() {
    let server = FakeServer::start();
    let access = fake_access_token("acct-1");
    server.push(
        "/oauth/token",
        200,
        &serde_json::json!({"access_token": access, "expires_in": 3600}).to_string(),
    );
    let token = refresh_token(&server.endpoints(), "the-old-refresh")
        .await
        .expect("refresh succeeds");
    assert_eq!(token.refresh, "the-old-refresh");
}

#[tokio::test]
async fn device_code_polling_goes_pending_then_succeeds() {
    let server = FakeServer::start();
    server.push(
        "/deviceauth/usercode",
        200,
        r#"{"device_auth_id":"dev-1","user_code":"ABCD-1234","interval":0}"#,
    );
    let start = device_start(&server.endpoints())
        .await
        .expect("device start");
    assert_eq!(start.user_code, "ABCD-1234");

    // Two pending polls (403, then 404 — both OpenAI's own "not yet" signals), then success.
    server.push("/deviceauth/token", 403, "");
    server.push("/deviceauth/token", 404, "");
    let access = fake_access_token("acct-device");
    server.push(
        "/deviceauth/token",
        200,
        r#"{"authorization_code":"auth-code-1","code_verifier":"verifier-1"}"#,
    );
    server.push(
        "/oauth/token",
        200,
        &token_body(&access, "refresh-device", 3600),
    );

    let token = poll_device_until_done(&server.endpoints(), &start, Duration::from_millis(1), 10)
        .await
        .expect("eventually succeeds");
    assert_eq!(token.access, access);
    assert_eq!(server.request_count("/deviceauth/token"), 3);
}

#[tokio::test]
async fn device_code_polling_reports_expired() {
    let server = FakeServer::start();
    server.push(
        "/deviceauth/usercode",
        200,
        r#"{"device_auth_id":"dev-2","user_code":"WXYZ-9999","interval":0}"#,
    );
    let start = device_start(&server.endpoints()).await.unwrap();
    server.push(
        "/deviceauth/token",
        400,
        r#"{"error":"expired_token","error_description":"the code expired"}"#,
    );
    let error = device_poll_once(&server.endpoints(), &start.device_auth_id, &start.user_code)
        .await
        .expect_err("expired is an error, not Pending");
    assert!(matches!(error, OAuthError::DeviceExpired), "{error}");
}

#[tokio::test]
async fn a_pending_poll_does_not_touch_the_token_endpoint() {
    let server = FakeServer::start();
    server.push("/deviceauth/token", 403, "");
    let outcome = device_poll_once(&server.endpoints(), "dev-x", "code-x")
        .await
        .expect("403 is Pending, not an error");
    assert!(matches!(outcome, DevicePoll::Pending));
    assert_eq!(server.request_count("/oauth/token"), 0);
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
