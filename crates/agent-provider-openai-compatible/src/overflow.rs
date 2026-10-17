//! Recognizing a provider's "the input is over the context window" error (ADR 0018's
//! fallback).
//!
//! The classification is deliberately narrow. Only a `400`/`413` whose body carries the
//! provider's own structured overflow code or a message that explicitly names the context
//! length/window/size is an overflow. A rate limit (`429`) or a server error (`5xx`) is
//! **never** one, even when the body mentions tokens: a body like "too many tokens" is as
//! likely to be a per-minute quota as a context overflow, so guessing there would misclassify
//! it. Generic `too many tokens` / `token limit exceeded` / `payload too large` wording is
//! excluded for the same reason.
//!
//! The message patterns are a subset of pi-ai's `OVERFLOW_PATTERNS`
//! (`@oh-my-pi/pi-ai/src/error/flags.ts`, read 2026-10-16): the entries that are both
//! OpenAI-compatible-service shaped and explicitly about the context window. There is no
//! `regex` dependency in this workspace, so the alternations are written out as substrings.

use serde_json::Value;

/// The provider's structured overflow code. OpenAI's Chat Completions error uses
/// `error.code = "context_length_exceeded"` for this.
fn is_overflow_code(code: &str) -> bool {
    code == "context_length_exceeded"
}

/// Whether `message` (lowercased) explicitly names a context overflow. Each pattern notes its
/// source line in pi-ai's `OVERFLOW_PATTERNS`.
fn is_overflow_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        // pi: `/maximum context length is \d+ tokens/i` (OpenRouter, all backends).
        "maximum context length",
        // pi: `/exceeds the context window/i` (OpenAI Completions & Responses API).
        "exceeds the context window",
        // pi: `/exceeds the available context size/i` (llama.cpp server).
        "exceeds the available context size",
        // pi: `/greater than the context length/i` (LM Studio).
        "greater than the context length",
        // pi: `/reduce the length of the messages/i` (Groq).
        "reduce the length of the messages",
    ];
    if PATTERNS.iter().any(|pattern| message.contains(pattern)) {
        return true;
    }
    // Not in pi's table: opencode-go's gateway (seen 2026-10-17 on deepseek-v4.1-flash at
    // ~287k input tokens) answers 400 with "This text request is too large for the model's
    // configured input limit". It names the *input* limit, so it is not the output-limit
    // "max_tokens is too large" that stays excluded.
    if message.contains("too large") && message.contains("input limit") {
        return true;
    }
    // pi: `/context[_ ]length[_ ]exceeded/i` (generic fallback). The `[_ ]` alternation is
    // written out as its two spellings.
    if message.contains("context length exceeded") || message.contains("context_length_exceeded") {
        return true;
    }
    // pi: `/requested tokens?.*exceed.*context (window|length|size)/i` (llama.cpp and other
    // OpenAI-compatible local servers). Without a regex engine the same intent is the
    // conjunction of the three tokens.
    message.contains("requested")
        && message.contains("exceed")
        && ["context window", "context length", "context size"]
            .iter()
            .any(|context| message.contains(context))
}

/// Whether `status` + `body` is a recognizable context-overflow error.
pub(crate) fn classify_overflow(status: u16, body: &str) -> bool {
    if !matches!(status, 400 | 413) {
        return false;
    }
    let value: Option<Value> = serde_json::from_str(body).ok();
    let code = value
        .as_ref()
        .and_then(|value| value.get("error"))
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str);
    if code.is_some_and(is_overflow_code) {
        return true;
    }
    // The `error.message` when the body is JSON; the whole body when it is not.
    let message = value
        .as_ref()
        .and_then(|value| value.get("error"))
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(body);
    is_overflow_message(message)
}

/// The same recognition for a mid-stream `error` object. A stream error carries no HTTP
/// status, so only the provider's structured code and message are available and the status
/// gate cannot apply; this is the `{"error": {...}}` shape OpenAI-compatible servers send
/// inside the SSE stream.
pub(crate) fn classify_stream_overflow(error: &Value) -> bool {
    if error
        .get("code")
        .and_then(Value::as_str)
        .is_some_and(is_overflow_code)
    {
        return true;
    }
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("");
    is_overflow_message(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A body in the OpenAI error envelope.
    fn status(code: u16, error: Value) -> bool {
        classify_overflow(code, &json!({ "error": error }).to_string())
    }

    #[test]
    fn the_structured_code_is_an_overflow_on_400_and_413() {
        assert!(status(400, json!({"code": "context_length_exceeded"})));
        assert!(status(413, json!({"code": "context_length_exceeded"})));
    }

    #[test]
    fn openais_own_message_is_an_overflow() {
        // OpenAI's real 400 body shape (Completions/Responses API): the code plus a message.
        assert!(status(
            400,
            json!({
                "message": "This model's maximum context length is 16385 tokens. However, your \
                            messages resulted in 20000 tokens.",
                "type": "invalid_request_error",
                "code": "context_length_exceeded"
            })
        ));
        // Without the code, the message alone must still be recognized.
        assert!(classify_overflow(
            400,
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#
        ));
    }

    #[test]
    fn the_narrowed_message_table_matches() {
        for message in [
            "This model's maximum context length is 128000 tokens",
            "the request exceeds the context window",
            "exceeds the available context size",
            "prompt is greater than the context length",
            "please reduce the length of the messages",
            "requested 30000 tokens exceed the context window",
            "Context length exceeded",
            "Context_Length_Exceeded",
        ] {
            assert!(is_overflow_message(message), "{message}");
        }
    }

    #[test]
    fn opencode_gos_input_limit_400_is_an_overflow() {
        // The body opencode-go returned on 2026-10-17, verbatim.
        assert!(classify_overflow(
            400,
            r#"{"error":{"type":"server_error","message":"Upstream request failed: [unsupported_request] This text request is too large for the model's configured input limit."}}"#
        ));
        assert!(classify_stream_overflow(&json!({
            "message": "This text request is too large for the model's configured input limit."
        })));
    }

    #[test]
    fn the_generic_phrasings_are_not_overflow() {
        for message in [
            "too many tokens",
            "token limit exceeded",
            "payload too large",
            "entity too large",
            "invalid model",
            // An output-limit error is not an input overflow (ADR 0018's "recognizable" rule).
            "max_tokens is too large",
            // "input limit" alone (a quota or rate wording) is not enough without "too large".
            "input limit reached for this minute",
        ] {
            assert!(!is_overflow_message(message), "{message}");
        }
    }

    #[test]
    fn only_400_and_413_are_considered() {
        // A rate limit is never an overflow, even when the body carries a matching code or
        // message: that is exactly the "too many tokens" misclassification to avoid.
        assert!(!status(429, json!({"code": "context_length_exceeded"})));
        assert!(!status(
            429,
            json!({"message": "maximum context length is 100 tokens"})
        ));
        // Nor is a server error, an auth error, or any other status.
        assert!(!status(500, json!({"code": "context_length_exceeded"})));
        assert!(!status(
            500,
            json!({"message": "exceeds the context window"})
        ));
        assert!(!status(401, json!({"code": "context_length_exceeded"})));
        assert!(!status(404, json!({"message": "context length exceeded"})));
    }

    #[test]
    fn a_non_json_body_is_matched_as_text() {
        assert!(classify_overflow(
            400,
            "maximum context length is 100 tokens"
        ));
        assert!(!classify_overflow(400, "bad request"));
    }

    #[test]
    fn a_stream_error_uses_code_and_message_without_a_status() {
        assert!(classify_stream_overflow(
            &json!({"code": "context_length_exceeded"})
        ));
        assert!(classify_stream_overflow(
            &json!({"message": "exceeds the context window"})
        ));
        assert!(!classify_stream_overflow(&json!({"message": "overloaded"})));
        assert!(!classify_stream_overflow(
            &json!({"message": "too many tokens"})
        ));
    }
}
