//! Recognizing opencode-go's Go plan limit in an error response.
//!
//! The Go gateway answers with `429` and a body of this shape once a workspace's plan limit
//! (monthly, or another window) is spent:
//!
//! ```json
//! {"type":"error",
//!  "error":{"type":"GoUsageLimitError","message":"Go usage limit exceeded"},
//!  "metadata":{"workspace":"wrk_…","limitName":"monthly"}}
//! ```
//!
//! That is not the provider being busy: the window resets later, so retrying the same request
//! cannot help and the retries of ADR 0017 only delay the same failure. The adapter reports it
//! as [`ModelError::UsageLimit`](radian_core::ModelError::UsageLimit) — the core retries only a
//! *transient* error (`radian_core::ModelError::is_transient`), so the turn fails at once, and a
//! host reports `Result.model_error` with kind `usage_limit` (ADR 0044 §2b).
//!
//! The recognition is narrow on purpose: status `429` plus `error.type == "GoUsageLimitError"`,
//! nothing else. Every other `429` stays a retryable [`ModelError::Unavailable`] in `model.rs`.
//! The message is the body's own `error.message` plus `metadata.limitName` when present; the
//! body's `metadata.workspace` is an id and is never read.

use std::time::Duration;

use radian_core::ModelError;
use serde_json::Value;

/// The body's `error.type` that marks the Go plan limit.
const GO_USAGE_LIMIT: &str = "GoUsageLimitError";

/// The never-retried [`ModelError::UsageLimit`] for a spent Go plan window, or `None` when
/// `status` + `body` is not that error.
///
/// `retry_after` is the response's own `Retry-After`, read by the caller before the body. The
/// body seen carries no reset time; an optional `error.resets_in_seconds` countdown is honoured
/// too, and the header wins when both are present. With neither, `resets_in` is `None`.
pub(crate) fn usage_limit(
    status: u16,
    body: &str,
    retry_after: Option<Duration>,
) -> Option<ModelError> {
    if status != 429 {
        return None;
    }
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    if error.get("type").and_then(Value::as_str) != Some(GO_USAGE_LIMIT) {
        return None;
    }
    // The provider's own sentence. The body seen says "Go usage limit exceeded"; the fallback
    // keeps a body that omits the message recognizable without inventing provider text.
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Go usage limit exceeded");
    // The window (`monthly`, …), when the body names one. Never `metadata.workspace`: that is
    // an account id and has no place in a user-facing error.
    let message = match value
        .get("metadata")
        .and_then(|metadata| metadata.get("limitName"))
        .and_then(Value::as_str)
    {
        Some(limit) => format!("{message} ({limit})"),
        None => message.to_owned(),
    };
    let body_resets = error
        .get("resets_in_seconds")
        .and_then(Value::as_u64)
        .map(Duration::from_secs);
    Some(ModelError::UsageLimit {
        message,
        resets_in: retry_after.or(body_resets),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body opencode-go returned on 2026-10-27, verbatim.
    fn go_body() -> String {
        r#"{"type":"error","error":{"type":"GoUsageLimitError","message":"Go usage limit exceeded"},"metadata":{"workspace":"wrk_secret123","limitName":"monthly"}}"#
            .to_owned()
    }

    #[test]
    fn the_go_usage_limit_body_names_the_message_and_the_window() {
        let error = usage_limit(429, &go_body(), None).expect("recognized");
        let ModelError::UsageLimit { message, resets_in } = &error else {
            panic!("expected a usage-limit error: {error}");
        };
        assert_eq!(message, "Go usage limit exceeded (monthly)");
        // The workspace id is not part of the message.
        assert!(!message.contains("wrk_"), "{message}");
        assert_eq!(*resets_in, None, "this body carries no reset time");
        assert!(!error.is_transient(), "a usage limit is never retried");
    }

    #[test]
    fn a_retry_after_header_is_the_reset_wait_and_wins_over_the_body() {
        let error = usage_limit(429, &go_body(), Some(Duration::from_secs(3600))).expect("ok");
        let ModelError::UsageLimit { resets_in, .. } = error else {
            panic!("expected a usage-limit error");
        };
        assert_eq!(resets_in, Some(Duration::from_secs(3600)));

        let body = r#"{"error":{"type":"GoUsageLimitError","resets_in_seconds":45}}"#;
        let error = usage_limit(429, body, None).expect("ok");
        let ModelError::UsageLimit { message, resets_in } = error else {
            panic!("expected a usage-limit error");
        };
        assert_eq!(
            message, "Go usage limit exceeded",
            "no limit name in the body"
        );
        assert_eq!(resets_in, Some(Duration::from_secs(45)));
    }

    #[test]
    fn another_status_type_or_body_is_not_the_go_usage_limit() {
        let other_type = r#"{"error":{"type":"rate_limit_exceeded","message":"slow down"}}"#;
        assert!(usage_limit(429, other_type, None).is_none());
        assert!(usage_limit(429, "slow down", None).is_none());
        // The ChatGPT body belongs to the other adapter.
        let chatgpt = r#"{"error":{"type":"usage_limit_reached"}}"#;
        assert!(usage_limit(429, chatgpt, None).is_none());
        // A 400 that happens to carry the type is not a 429.
        assert!(usage_limit(400, &go_body(), None).is_none());
    }
}
