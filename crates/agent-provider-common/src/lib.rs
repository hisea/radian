//! Code the three provider adapters (`agent-provider-openai-compatible`,
//! `agent-provider-openai-responses`, `agent-provider-anthropic`) share, kept once here
//! (ADR 0026 D12): the server-sent-events reader, the `Retry-After` parser, the usage-limit
//! body recognizers, and, behind the `testing` feature, the mock HTTP server their `testing`
//! modules wrap. Wire-specific code stays in each adapter.

mod retry_after;
mod sse;
pub mod usage_limit;

#[cfg(feature = "testing")]
pub mod testing;

pub use retry_after::retry_after;
pub use sse::SseParser;
