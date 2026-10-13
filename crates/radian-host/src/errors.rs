//! Host wording for a failed turn (ADR 0017) that was a context overflow (ADR 0018).
//!
//! A host receives a failed turn as text over `RuntimeEvent::TurnFailed.message`, not as an
//! `agent_core::ModelError`, so it recognizes the overflow case by the prefix
//! [`agent_core::ModelError::ContextOverflow`]'s `Display` writes. `agent-core` pins that
//! prefix with its own test; this module is the only host-side reader of it.

/// The prefix `agent_core::ModelError::ContextOverflow` prints.
pub const CONTEXT_OVERFLOW_PREFIX: &str = agent_core::CONTEXT_OVERFLOW_PREFIX;

/// The one-line help a host adds when a turn failed because the input was over the model's
/// context window. Compaction (ADR 0018) normally keeps the request inside the window —
/// including the provider-overflow fallback — so a failure here means it could not: a manual
/// `/compact`, a larger `--context-window`, or a new session is the remedy.
pub const CONTEXT_OVERFLOW_HELP: &str = "the input is over the model's context window and \
    compaction could not keep the request within it; run /compact, raise --context-window, or \
    start a new session to continue";

/// Whether a `TurnFailed.message` is a context overflow.
pub fn is_context_overflow(message: &str) -> bool {
    message.starts_with(CONTEXT_OVERFLOW_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_overflow_prefix_is_recognized_and_nothing_else_is() {
        assert!(is_context_overflow(
            "model context overflow: HTTP 400: prompt is too long"
        ));
        assert!(!is_context_overflow(
            "model protocol error: HTTP 400: bad request"
        ));
        assert!(!is_context_overflow(
            "model transport error: HTTP 500: oops"
        ));
        assert!(!is_context_overflow(""));
    }
}
