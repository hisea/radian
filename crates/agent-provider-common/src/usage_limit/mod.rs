//! Recognizing a provider's used-up plan window in a `429` error body (ADR 0017 §2). Each
//! submodule recognizes one provider's body; an adapter calls only its own.

/// The ChatGPT subscription's `usage_limit_reached` body (the Responses adapter).
pub mod chatgpt;
/// opencode-go's `GoUsageLimitError` body (the Chat Completions adapter).
pub mod go;
