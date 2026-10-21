//! `/login`'s shared pieces (ADR 0026 D3): the pre-save verification request, and the
//! redaction rule for anything an error might carry.
//!
//! The verification is a **minimal** request — `GET <base>/models`, or the descriptor's own
//! verification URL when it names one (`opencode-go` verifies against its usage endpoint,
//! whose `/models` path does not exist) — sent before a key is written to `auth.json`. A
//! failure is shown to the user, who may still save. This is a new outbound item in ADR
//! 0013's inventory: one request, only when the user runs `/login` or `radian login`, to the
//! endpoint the descriptor names, with the key they typed.
//!
//! The key never appears in an error: responses are matched by status, and transport errors
//! (which can embed the URL) are rebuilt from a redacted URL (ADR 0013's `redact` rule).

use crate::provider::Protocol;

/// How long the verification request may take. Shorter than a model call: this is a
/// reachability check, not a completion, and the user is waiting on a modal.
pub const VERIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The path appended to the base URL for the verification request, per protocol.
fn models_path(protocol: Protocol) -> &'static str {
    match protocol {
        // The OpenAI-compatible `/v1` is conventionally part of the base URL; the Anthropic
        // base URL never carries it (the adapter appends `/v1/messages`).
        Protocol::OpenAiCompatible => "models",
        Protocol::Anthropic => "v1/models",
        // Unreachable in practice: `chatgpt` is the only `Responses` descriptor and its login
        // is OAuth (ADR 0026), never routed through this API-key
        // verification. Kept total (no panic) rather than relying on that staying true.
        Protocol::Responses => "models",
    }
}

/// The auth header the wire protocol expects, as `(name, value)` pairs.
fn auth_headers(protocol: Protocol, key: &str) -> Vec<(String, String)> {
    match protocol {
        Protocol::OpenAiCompatible | Protocol::Responses => {
            vec![("authorization".to_owned(), format!("Bearer {key}"))]
        }
        Protocol::Anthropic => vec![
            ("x-api-key".to_owned(), key.to_owned()),
            ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
        ],
    }
}

/// The URL one verification `GET` goes to (ADR 0026 D3): the descriptor's own verification
/// URL when it names one, else `<base>/models` for the protocol. Pure, so both hosts check
/// — and print — the endpoint they actually verify against.
pub fn verification_url(provider_id: &str, protocol: Protocol, base_url: &str) -> String {
    if let Some(descriptor) = crate::descriptor(provider_id)
        && let Some(url) = descriptor.verification_url
    {
        return url.to_owned();
    }
    join_url(base_url, models_path(protocol))
}

/// Trims one trailing slash so `https://x/v1/` and `https://x/v1` behave the same.
fn join_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    format!("{base}/{path}")
}

/// Sends the verification request: `GET <base>/models` with the protocol's auth header, or
/// the descriptor's own verification request when it names one (`GET` of its URL with
/// `Authorization: Bearer <key>`, the usage footer's own auth — see `opencode_go.rs`).
///
/// - a `2xx` status is success;
/// - `401`/`403` name the key (without showing it);
/// - any other status or a transport error names the redacted URL, never the key and never
///   URL credentials (the URL is redacted before it can reach a message).
///
/// The error strings are for a modal / a terminal line; they contain no secret by
/// construction (status + redacted URL only).
pub async fn verify(
    provider_id: &str,
    protocol: Protocol,
    base_url: &str,
    key: &str,
) -> Result<(), String> {
    let named = crate::descriptor(provider_id).and_then(|d| d.verification_url);
    let url = named
        .map(str::to_owned)
        .unwrap_or_else(|| join_url(base_url, models_path(protocol)));
    verify_url(&url, named.is_some(), protocol, key).await
}

/// Sends one verification `GET` to `url`: with the usage footer's Bearer auth when `named`
/// (the descriptor named its own verification request), else with the protocol's auth header.
/// A mock server stands in for the endpoint in tests, so both shapes are covered without
/// network. `protocol` is unused when `named`.
async fn verify_url(url: &str, named: bool, protocol: Protocol, key: &str) -> Result<(), String> {
    let shown_url = crate::redact_base_url(url);
    let client = reqwest::Client::builder()
        .timeout(VERIFY_TIMEOUT)
        .build()
        .map_err(|_| format!("could not create an HTTP client for {shown_url}"))?;
    let mut request = client.get(url);
    // The descriptor's own request carries the usage footer's auth, not the protocol's.
    request = match named {
        true => request.bearer_auth(key),
        false => {
            let mut request = request;
            for (name, value) in auth_headers(protocol, key) {
                request = request.header(&name, value);
            }
            request
        }
    };
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                Ok(())
            } else if status.as_u16() == 401 || status.as_u16() == 403 {
                Err(format!(
                    "{shown_url} rejected the key (HTTP {}); fix it or cancel",
                    status.as_u16()
                ))
            } else {
                let detail = match named {
                    true => "the endpoint did not verify the key",
                    false => "the endpoint may not serve `/models`",
                };
                Err(format!(
                    "{shown_url} answered HTTP {}; {detail}",
                    status.as_u16()
                ))
            }
        }
        Err(error) => {
            // `reqwest` errors interpolate the URL, which can carry credentials; rebuild the
            // message from the redacted URL instead (ADR 0013).
            let reason = if error.is_timeout() {
                "timed out".to_owned()
            } else if error.is_connect() {
                "could not connect".to_owned()
            } else {
                "request failed".to_owned()
            };
            Err(format!("{shown_url}: {reason}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-shot HTTP server answering one request with `status` (the adapters' mock only
    /// serves `/chat/completions`, and the verification must not pretend it does more).
    fn one_shot_server(status_line: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            // Drain the request headers first: writing before reading can close the
            // connection under the client's feet.
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let body = "{}";
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_200_verifies_and_401_names_the_key_problem_without_the_key() {
        let base = one_shot_server("200 OK");
        verify("openai", Protocol::OpenAiCompatible, &base, "sk-test")
            .await
            .expect("a 200 verifies");

        let base = one_shot_server("401 Unauthorized");
        let error = verify(
            "openai",
            Protocol::OpenAiCompatible,
            &base,
            "sk-SUPER-SECRET",
        )
        .await
        .expect_err("401 fails");
        assert!(error.contains("rejected the key"), "{error}");
        assert!(!error.contains("sk-SUPER-SECRET"), "{error}");

        // Any other status: the endpoint may not serve `/models`; still no key, no URL
        // credentials.
        let base = one_shot_server("500 Internal Server Error");
        let error = verify(
            "openai",
            Protocol::OpenAiCompatible,
            &base,
            "sk-SUPER-SECRET",
        )
        .await
        .expect_err("500 fails");
        assert!(error.contains("HTTP 500"), "{error}");
        assert!(error.contains("/models"), "{error}");
        assert!(!error.contains("sk-SUPER-SECRET"), "{error}");
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_errors_without_the_key_or_credentials() {
        let error = verify(
            "openai",
            Protocol::OpenAiCompatible,
            "https://user:secret@no-such-host.example/v1",
            "sk-SUPER-SECRET-KEY",
        )
        .await
        .expect_err("nothing answers there");
        assert!(!error.contains("sk-SUPER-SECRET-KEY"), "{error}");
        assert!(!error.contains("user:secret"), "{error}");
        assert!(!error.contains("secret"), "{error}");
        assert!(
            error.contains("https://no-such-host.example/v1/models"),
            "{error}"
        );
    }

    /// `opencode-go` verifies against its usage endpoint, not `<base>/models` (that path answers 404);
    /// every other provider keeps `GET <base>/models`.
    /// Pointing `opencode-go` back at `<base>/models` fails this test.
    #[test]
    fn opencode_go_verification_uses_the_usage_endpoint_not_models() {
        assert_eq!(
            verification_url(
                "opencode-go",
                Protocol::OpenAiCompatible,
                "https://opencode.ai/inference/go/openai/v1"
            ),
            crate::opencode_go::USAGE_URL,
        );
        // An unrelated provider still uses `/models`.
        assert_eq!(
            verification_url(
                "openai",
                Protocol::OpenAiCompatible,
                "https://api.openai.com/v1"
            ),
            "https://api.openai.com/v1/models",
        );
        assert_eq!(
            verification_url(
                "anthropic",
                Protocol::Anthropic,
                "https://api.anthropic.com"
            ),
            "https://api.anthropic.com/v1/models",
        );
        // An unknown (custom) provider verifies against what it was given.
        assert_eq!(
            verification_url(
                "corp",
                Protocol::OpenAiCompatible,
                "https://corp.example/v1/"
            ),
            "https://corp.example/v1/models",
        );
    }

    /// A one-shot server that records the request head, then answers `status_line`: the mock
    /// stands in for a descriptor-named verification endpoint (no network in tests).
    fn capturing_server(status_line: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
            let body = "{}";
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{addr}/usage"), rx)
    }

    /// The descriptor-named request is a `GET` of the usage path with the footer's Bearer auth
    /// (the same `Authorization: Bearer <key>` `opencode_go.rs` sends); a 200 verifies. The
    /// failure messages never carry the request, so the key cannot reach test output.
    #[tokio::test]
    async fn a_named_verification_gets_the_usage_path_with_bearer_auth() {
        let (url, request) = capturing_server("200 OK");
        verify_url(&url, true, Protocol::OpenAiCompatible, "sk-test")
            .await
            .expect("a 200 verifies");
        let head = request
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the request");
        assert!(head.starts_with("GET /usage "), "the usage path is hit");
        assert!(
            head.to_lowercase()
                .contains("authorization: bearer sk-test"),
            "the footer's Bearer auth is sent"
        );
    }

    /// A refused key on the named endpoint says so without the key; any other status says the
    /// endpoint did not verify the key — never `/models`, which is not this request's path.
    #[tokio::test]
    async fn a_named_verification_names_a_refused_key_but_never_models() {
        let (url, _) = capturing_server("401 Unauthorized");
        let error = verify_url(&url, true, Protocol::OpenAiCompatible, "sk-SUPER-SECRET")
            .await
            .expect_err("401 fails");
        assert!(error.contains("rejected the key"), "{error}");
        assert!(!error.contains("sk-SUPER-SECRET"), "{error}");

        let (url, _) = capturing_server("500 Internal Server Error");
        let error = verify_url(&url, true, Protocol::OpenAiCompatible, "sk-SUPER-SECRET")
            .await
            .expect_err("500 fails");
        assert!(error.contains("HTTP 500"), "{error}");
        assert!(error.contains("did not verify the key"), "{error}");
        assert!(!error.contains("/models"), "{error}");
        assert!(!error.contains("sk-SUPER-SECRET"), "{error}");
    }

    #[test]
    fn the_url_joins_and_headers_follow_the_protocol() {
        assert_eq!(
            join_url(
                "https://x.example/v1/",
                models_path(Protocol::OpenAiCompatible)
            ),
            "https://x.example/v1/models"
        );
        assert_eq!(
            join_url("https://x.example", models_path(Protocol::Anthropic)),
            "https://x.example/v1/models"
        );
        assert_eq!(
            auth_headers(Protocol::OpenAiCompatible, "k")[0].0,
            "authorization"
        );
        assert_eq!(auth_headers(Protocol::Anthropic, "k")[0].0, "x-api-key");
    }
}
