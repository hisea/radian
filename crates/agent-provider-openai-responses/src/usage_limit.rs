//! Recognizing the ChatGPT plan's used-up usage window in an error response.
//!
//! A ChatGPT subscription answers with `429` and `{"error":{"type":"usage_limit_reached", …}}`
//! once the plan's usage window (5 hours on Plus) is spent. That is not the provider being
//! busy: the window resets minutes-to-hours later, so retrying the same request cannot help and
//! the three retries of ADR 0017 only delay the same failure. The adapter reports it
//! as a [`ModelError::Protocol`](radian_core::ModelError::Protocol) error instead — the core
//! retries only a *transient* error (`radian_core::ModelError::is_transient`), so the turn fails
//! at once with a message that names the plan, its window, when the window resets, and `/model`
//! as the way to another provider, rather than quoting the raw JSON.
//!
//! omp recognizes the same body by `error.code ?? error.type`
//! (`@oh-my-pi/pi-ai/src/error/flags.ts:487`: `code === "usage_limit_reached"`;
//! `src/providers/openai-codex/response-handler.ts:72`); the ChatGPT backend's own body carries
//! the field as `type` (see `crates/agent-provider-openai-responses/WIRE.md` §7).

use serde_json::Value;

/// The never-retried message for a used-up plan window, or `None` when `status` + `body` is not
/// that error (every other `429` stays a retryable [`ModelError::Unavailable`] in `model.rs`).
///
/// Every clause the message carries is read from the body; a field the body does not carry is
/// left out rather than guessed. The body's own `resets_in_seconds` countdown is used verbatim
/// — the adapter never computes one from `resets_at` and the local clock.
pub(crate) fn usage_limit(status: u16, body: &str) -> Option<String> {
    if status != 429 {
        return None;
    }
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    if error.get("type").and_then(Value::as_str) != Some("usage_limit_reached") {
        return None;
    }
    // The plan (`plus`, `pro`, …), the window it is counting (`limit_window_minutes`), and when
    // that window comes back (`resets_in_seconds`).
    let plan = match error.get("plan_type").and_then(Value::as_str) {
        Some(plan) => format!("the ChatGPT {plan} plan's"),
        None => "the ChatGPT plan's".to_string(),
    };
    let window = match error.get("limit_window_minutes").and_then(Value::as_u64) {
        Some(minutes) => format!("{minutes} min usage window"),
        None => "usage window".to_string(),
    };
    let resets = error
        .get("resets_in_seconds")
        .and_then(Value::as_u64)
        .map(|seconds| format!("; it resets {}", countdown(seconds)))
        .unwrap_or_default();
    Some(format!(
        "HTTP {status}: {plan} {window} is used up{resets}; /model switches to another provider"
    ))
}

/// `in 58 min` from the body's `resets_in_seconds` (whole minutes; seconds only under a
/// minute, so a body with a 45-second countdown does not read "in 0 min").
fn countdown(seconds: u64) -> String {
    let minutes = seconds / 60;
    if minutes > 0 {
        format!("in {minutes} min")
    } else {
        format!("in {seconds} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The body the ChatGPT Plus turn got on 2026-10-17, verbatim.
    fn owner_body() -> String {
        json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached",
                "plan_type": "plus",
                "resets_at": 1792215076,
                "eligible_promo": null,
                "limit_window_minutes": 300,
                "resets_in_seconds": 3516
            }
        })
        .to_string()
    }

    #[test]
    fn the_usage_limit_body_names_the_plan_the_window_and_the_reset() {
        let message = usage_limit(429, &owner_body()).expect("recognized");
        assert!(message.contains("plus"), "{message}");
        assert!(message.contains("300 min usage window"), "{message}");
        assert!(message.contains("in 58 min"), "{message}");
        assert!(
            message.contains("/model switches to another provider"),
            "{message}"
        );
        assert!(message.starts_with("HTTP 429: "), "{message}");
        // The raw JSON is not the message.
        assert!(!message.contains("resets_at"), "{message}");
    }

    #[test]
    fn a_body_without_the_optional_fields_still_names_the_limit() {
        let body = json!({"error": {"type": "usage_limit_reached"}}).to_string();
        let message = usage_limit(429, &body).expect("recognized");
        assert!(
            message.contains("the ChatGPT plan's usage window is used up"),
            "{message}"
        );
        assert!(!message.contains("resets"), "{message}");
    }

    #[test]
    fn a_second_countdown_under_a_minute_is_not_zero_minutes() {
        let body = json!({
            "error": {
                "type": "usage_limit_reached",
                "plan_type": "pro",
                "limit_window_minutes": 60,
                "resets_in_seconds": 45
            }
        })
        .to_string();
        let message = usage_limit(429, &body).expect("recognized");
        assert!(message.contains("the ChatGPT pro plan's"), "{message}");
        assert!(message.contains("in 45 s"), "{message}");
    }

    #[test]
    fn another_types_and_statuses_are_not_the_usage_limit() {
        let other = json!({"error": {"type": "rate_limit_exceeded", "message": "slow down"}});
        assert_eq!(usage_limit(429, &other.to_string()), None);
        assert_eq!(usage_limit(429, "slow down"), None);
        assert_eq!(usage_limit(500, &owner_body()), None);
        // A 400 that happens to carry the type is not a 429.
        assert_eq!(usage_limit(400, &owner_body()), None);
    }
}
