//! Recognizing Anthropic's "the input is over the context window" error (ADR 0018's
//! fallback).
//!
//! Anthropic reports this two ways: a `400 invalid_request_error` whose message says the
//! prompt is too long, and a `413 request_too_large` for a body over the size limit. Both the
//! status and the error `type` are required: the message alone is not enough, and a `429`
//! (rate limit) or `5xx` is never an overflow even when the text mentions tokens.
//!
//! Source: Anthropic's Messages API error documentation (`invalid_request_error` for an
//! over-length prompt) and pi-ai's `OVERFLOW_PATTERNS`
//! (`@oh-my-pi/pi-ai/src/error/flags.ts`, read 2026-10-16: `/prompt is too long/i` marked
//! "Anthropic", `/request_too_large/i` marked "Anthropic 413").

use serde_json::Value;

/// Whether `(status, error.type, error.message)` is Anthropic's own overflow shape. The
/// status/type pairing is deliberate: a `400` must be `invalid_request_error` with the
/// "prompt is too long" text, and a `413` must be `request_too_large`.
fn is_overflow_signature(status: u16, error_type: &str, message: &str) -> bool {
    match status {
        400 => {
            error_type == "invalid_request_error"
                && message.to_ascii_lowercase().contains("prompt is too long")
        }
        413 => error_type == "request_too_large",
        _ => false,
    }
}

/// Whether `status` + `body` is a recognizable context-overflow error.
pub(crate) fn classify_overflow(status: u16, body: &str) -> bool {
    if !matches!(status, 400 | 413) {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    let error = &value["error"];
    is_overflow_signature(
        status,
        error["type"].as_str().unwrap_or(""),
        error["message"].as_str().unwrap_or(""),
    )
}

/// The same recognition for a mid-stream `error` event. Anthropic's stream error carries no
/// HTTP status, so the `error.type`/`error.message` pair is what identifies it: either the
/// 400 shape or the 413 shape.
pub(crate) fn classify_stream_overflow(error: &Value) -> bool {
    let error_type = error["type"].as_str().unwrap_or("");
    let message = error["message"].as_str().unwrap_or("");
    is_overflow_signature(400, error_type, message)
        || is_overflow_signature(413, error_type, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_400_prompt_too_long_shape_is_an_overflow() {
        assert!(classify_overflow(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#
        ));
        // Case-insensitive on the message.
        assert!(classify_overflow(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"Prompt is too long"}}"#
        ));
    }

    #[test]
    fn the_413_request_too_large_shape_is_an_overflow() {
        assert!(classify_overflow(
            413,
            r#"{"type":"error","error":{"type":"request_too_large","message":"Request size exceeds the maximum"}}"#
        ));
    }

    #[test]
    fn the_error_type_matters() {
        // The right message with the wrong type is not Anthropic's overflow.
        assert!(!classify_overflow(
            400,
            r#"{"error":{"type":"overloaded_error","message":"prompt is too long"}}"#
        ));
        // A missing type is not enough either.
        assert!(!classify_overflow(
            400,
            r#"{"error":{"message":"prompt is too long"}}"#
        ));
        // A 400 with the 413 type is not the 400 shape.
        assert!(!classify_overflow(
            400,
            r#"{"error":{"type":"request_too_large"}}"#
        ));
    }

    #[test]
    fn the_message_is_required_for_a_400() {
        assert!(!classify_overflow(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"invalid model"}}"#
        ));
        // An output-limit error is not an input overflow (ADR 0018's "recognizable" rule).
        assert!(!classify_overflow(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"max_tokens is too large"}}"#
        ));
    }

    #[test]
    fn other_statuses_are_not_overflows() {
        // A rate limit is never an overflow, even with the exact 400 body.
        assert!(!classify_overflow(
            429,
            r#"{"error":{"type":"invalid_request_error","message":"prompt is too long"}}"#
        ));
        assert!(!classify_overflow(
            500,
            r#"{"error":{"type":"invalid_request_error","message":"prompt is too long"}}"#
        ));
        assert!(!classify_overflow(
            500,
            r#"{"error":{"type":"request_too_large"}}"#
        ));
        assert!(!classify_overflow(
            401,
            r#"{"error":{"type":"invalid_request_error","message":"prompt is too long"}}"#
        ));
    }

    #[test]
    fn a_non_json_body_is_not_an_overflow() {
        // Anthropic's error bodies are JSON; without one there is no `type` to check.
        assert!(!classify_overflow(400, "prompt is too long"));
    }

    #[test]
    fn a_stream_error_uses_type_and_message_without_a_status() {
        assert!(classify_stream_overflow(&json!({
            "type": "invalid_request_error",
            "message": "prompt is too long: 210000 tokens > 200000 maximum"
        })));
        assert!(classify_stream_overflow(&json!({
            "type": "request_too_large",
            "message": "Request size exceeds the maximum"
        })));
        assert!(!classify_stream_overflow(&json!({
            "type": "overloaded_error",
            "message": "Overloaded"
        })));
        assert!(!classify_stream_overflow(&json!({
            "message": "prompt is too long"
        })));
    }
}
