//! `/login`'s shared pieces (ADR 0026 D3): the pre-save verification request, and the
//! redaction rule for anything an error might carry.
//!
//! The verification is a **minimal** request — `GET <base>/models` — sent before a key is
//! written to `auth.json`. A failure is shown to the user, who may still save. This is a
//! new outbound item in ADR 0013's inventory: one request, only when the user runs
//! `/login` or `radian login`, to the endpoint they named, with the key they typed.
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
    }
}

/// The auth header the wire protocol expects, as `(name, value)` pairs.
fn auth_headers(protocol: Protocol, key: &str) -> Vec<(String, String)> {
    match protocol {
        Protocol::OpenAiCompatible => {
            vec![("authorization".to_owned(), format!("Bearer {key}"))]
        }
        Protocol::Anthropic => vec![
            ("x-api-key".to_owned(), key.to_owned()),
            ("anthropic-version".to_owned(), "2023-06-01".to_owned()),
        ],
    }
}

/// Trims one trailing slash so `https://x/v1/` and `https://x/v1` behave the same.
fn join_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    format!("{base}/{path}")
}

/// Sends the verification request: `GET <base>/models` with the protocol's auth header.
///
/// - a `2xx` status is success;
/// - `401`/`403` name the key (without showing it);
/// - any other status or a transport error names the redacted URL, never the key and never
///   URL credentials (the URL is redacted before it can reach a message).
///
/// The error strings are for a modal / a terminal line; they contain no secret by
/// construction (status + redacted URL only).
pub async fn verify(protocol: Protocol, base_url: &str, key: &str) -> Result<(), String> {
    let url = join_url(base_url, models_path(protocol));
    let shown_url = crate::redact_base_url(&url);
    let client = reqwest::Client::builder()
        .timeout(VERIFY_TIMEOUT)
        .build()
        .map_err(|_| format!("could not create an HTTP client for {shown_url}"))?;
    let mut request = client.get(&url);
    for (name, value) in auth_headers(protocol, key) {
        request = request.header(&name, value);
    }
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
                Err(format!(
                    "{shown_url} answered HTTP {}; the endpoint may not serve `/models`",
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
        verify(Protocol::OpenAiCompatible, &base, "sk-test")
            .await
            .expect("a 200 verifies");

        let base = one_shot_server("401 Unauthorized");
        let error = verify(Protocol::OpenAiCompatible, &base, "sk-SUPER-SECRET")
            .await
            .expect_err("401 fails");
        assert!(error.contains("rejected the key"), "{error}");
        assert!(!error.contains("sk-SUPER-SECRET"), "{error}");

        // Any other status: the endpoint may not serve `/models`; still no key, no URL
        // credentials.
        let base = one_shot_server("500 Internal Server Error");
        let error = verify(Protocol::OpenAiCompatible, &base, "sk-SUPER-SECRET")
            .await
            .expect_err("500 fails");
        assert!(error.contains("HTTP 500"), "{error}");
        assert!(!error.contains("sk-SUPER-SECRET"), "{error}");
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_errors_without_the_key_or_credentials() {
        let error = verify(
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
