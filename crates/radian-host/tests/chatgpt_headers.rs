//! ADR 0026 slice 3: `ChatGptModel` builds the right request — the descriptor's
//! static headers plus a fresh `Authorization`/`chatgpt-account-id` per call — against a
//! local mock (never `chatgpt.com`). One `#[tokio::test]` per file touching the process-wide
//! `auth.json` (`AuthStore::open()`/`ensure_fresh` read `RADIAN_HOME`, a process-global; a
//! second test in the same binary would race it under the default parallel test runner).

use std::sync::OnceLock;

use agent_core::{
    CancellationToken, Message, Model, ModelContext, ModelRequest, NoopSink, StopReason,
};
use agent_provider_openai_responses::testing::{
    MockServer, Reply, completed, message_item, output_item_done,
};
use radian_host::oauth_chatgpt::Endpoints;
use radian_host::{AuthStore, ChatGptModel, OAuthEntry};
use serde_json::Value;

fn isolated_home() -> std::path::PathBuf {
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "radian-chatgpt-headers-home-{}",
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

/// An unsigned JWT carrying the ChatGPT workspace claim (matches
/// `oauth_chatgpt::decode_account_id`'s documented "no signature check" contract).
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

#[tokio::test]
async fn headers_carry_originator_radian_and_the_account_id_from_the_token() {
    let home = isolated_home();
    let access = fake_access_token("acct-headers-test");
    {
        let mut store = AuthStore::open_at(home.join("credentials").join("auth.json")).unwrap();
        store
            .upsert_oauth(
                "chatgpt",
                &OAuthEntry {
                    access: access.clone(),
                    refresh: "refresh-unused".into(),
                    // An hour out: comfortably outside REFRESH_MARGIN, so no refresh call
                    // fires (there is no refresh mock here on purpose).
                    expires_at_millis: now_millis() + 3_600_000,
                },
            )
            .unwrap();
    }

    let server = MockServer::start(vec![Reply::sse(&[
        output_item_done(0, message_item("msg_1", "ok")),
        completed(None),
    ])])
    .await;

    let model = ChatGptModel {
        base_url: server.base_url.clone(),
        model: "gpt-5.4".to_owned(),
        static_headers: vec![
            (
                "OpenAI-Beta".to_owned(),
                "responses=experimental".to_owned(),
            ),
            ("originator".to_owned(), "radian".to_owned()),
            ("session_id".to_owned(), "sess-headers-test".to_owned()),
        ],
        reasoning_effort: None,
        endpoints: Endpoints::default(),
    };

    let request = ModelRequest {
        system_prompt: None,
        messages: vec![Message::user("hello")],
        tools: Vec::new(),
        settings: Value::Null,
    };
    let response = model
        .complete(
            request,
            ModelContext {
                cancellation: CancellationToken::new(),
                events: &NoopSink,
            },
        )
        .await
        .expect("the mock answers");
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let recorded = &requests[0];
    assert_eq!(
        recorded.header("authorization"),
        Some(format!("Bearer {access}")).as_deref()
    );
    assert_eq!(
        recorded.header("chatgpt-account-id"),
        Some("acct-headers-test")
    );
    assert_eq!(recorded.header("originator"), Some("radian"));
    assert_eq!(
        recorded.header("openai-beta"),
        Some("responses=experimental")
    );
    assert_eq!(recorded.header("session_id"), Some("sess-headers-test"));
}
