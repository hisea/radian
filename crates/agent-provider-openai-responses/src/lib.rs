//! A model adapter for the OpenAI **Responses** API, implementing `agent_core::Model` with
//! streaming.
//!
//! This is the protocol ChatGPT-subscription (Codex) turns use
//! (`https://chatgpt.com/backend-api/codex/responses`), not Chat Completions. The adapter knows
//! nothing about OAuth: the caller supplies the whole header set (`Authorization`,
//! `chatgpt-account-id`, `OpenAI-Beta`, `originator`) as configuration, so the login/token
//! machinery stays in the host (ADR 0026).
//!
//! The mapping is deliberately literal: text and tool calls round-trip exactly; a completed
//! `reasoning` output item (id, summary, `encrypted_content`) is kept verbatim in the
//! assistant message's `provider_payload` and replayed verbatim on the next request (ADR 0014
//! R10/revision 1); a payload written by another provider (e.g. Anthropic thinking blocks) is
//! ignored rather than misinterpreted. Every wire field the adapter relies on is recorded with
//! its source line in `docs/providers/openai-responses-wire-notes.md`.
//!
//! Rate-limit headers (`x-codex-primary-*`, `x-codex-secondary-*`) are kept raw on
//! [`agent_core::ModelResponse::limits`] without being parsed (ADR 0026;
//! ADR 0027 slice ④ parses them later).

mod config;
mod model;
mod overflow;
mod request;
mod sse;
mod stream;

#[cfg(feature = "testing")]
pub mod testing;

pub use config::{ConfigError, OpenAiResponsesConfig, redact_url};
pub use model::OpenAiResponses;
