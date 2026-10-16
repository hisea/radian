//! ADR 0026: `ChatGptUsage::fetch` resolves a (fresh) access token and
//! account id the same way `ChatGptModel` does, then maps a fake `/wham/usage` body into the
//! footer's display text. One `#[tokio::test]` per file — see `tests/chatgpt_headers.rs`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::OnceLock;
use std::time::Duration;

use radian_host::contrib::StatusContributor as _;
use radian_host::oauth_chatgpt::Endpoints;
use radian_host::{AuthStore, ChatGptUsage, OAuthEntry};

fn isolated_home() -> std::path::PathBuf {
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("radian-chatgpt-usage-home-{}", std::process::id()));
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

/// The recorded request's headers, lower-cased.
type RecordedHeaders = std::sync::Arc<std::sync::Mutex<Option<Vec<(String, String)>>>>;

/// A one-shot usage mock that records the request's headers.
struct UsageMock {
    url: String,
    recorded: RecordedHeaders,
}

impl UsageMock {
    fn start(body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        let recorded_task = std::sync::Arc::clone(&recorded);
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut scratch = [0u8; 4096];
            loop {
                let n = stream.read(&mut scratch).unwrap();
                buf.extend_from_slice(&scratch[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || n == 0 {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf).into_owned();
            let headers: Vec<(String, String)> = head
                .split("\r\n")
                .skip(1)
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
                .collect();
            *recorded_task.lock().unwrap() = Some(headers);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        });
        Self {
            url: format!("http://{addr}/wham/usage"),
            recorded,
        }
    }

    fn header(&self, name: &str) -> Option<String> {
        self.recorded
            .lock()
            .unwrap()
            .as_ref()?
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    }
}

#[tokio::test]
async fn fetch_resolves_the_token_and_maps_the_body() {
    let home = isolated_home();
    let access = fake_access_token("acct-usage-test");
    {
        let mut store = AuthStore::open_at(home.join("credentials").join("auth.json")).unwrap();
        store
            .upsert_oauth(
                "chatgpt",
                &OAuthEntry {
                    access: access.clone(),
                    refresh: "refresh-unused".into(),
                    expires_at_millis: now_millis() + 3_600_000,
                },
            )
            .unwrap();
    }
    let usage_body = serde_json::json!({
        "plan_type": "pro",
        "rate_limit": {
            "primary_window": {"used_percent": 5, "limit_window_seconds": 18000, "reset_after_seconds": 3600},
        }
    })
    .to_string();
    let mock = UsageMock::start(usage_body);

    let contributor = ChatGptUsage::with_url(
        mock.url.clone(),
        "sess-usage-test",
        Duration::from_secs(60),
        Endpoints::default(),
    );
    let contributions = contributor.refresh().await.expect("a refresh");
    assert_eq!(contributions[0].item, "chatgpt.usage");
    let text: String = contributions[0]
        .segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect();
    assert!(text.contains("ChatGPT usage (remaining):"), "{text}");
    assert!(text.contains("5h: 95%"), "{text}");
    assert!(text.contains("pro"), "{text}");
    assert!(
        !text.contains(&access),
        "the token must never appear: {text}"
    );

    assert_eq!(
        mock.header("authorization"),
        Some(format!("Bearer {access}"))
    );
    assert_eq!(
        mock.header("chatgpt-account-id"),
        Some("acct-usage-test".to_owned())
    );
    assert_eq!(mock.header("originator"), Some("radian".to_owned()));
    assert_eq!(
        mock.header("session_id"),
        Some("sess-usage-test".to_owned())
    );
}
