//! A model adapter for the OpenAI **Responses** API, implementing `agent_core::Model` with
//! streaming.
//!
//! This is the protocol ChatGPT-subscription (Codex) turns use
//! (`https://chatgpt.com/backend-api/codex/responses`), not Chat Completions. Since ADR 0026
//! revision 2 it is also the protocol of a **built-in descriptor's** `@ai-sdk/openai` model
//! (e.g. `opencode-go`'s `muse-spark-1.3-contributor`), where the caller passes a plain
//! `Authorization: Bearer <key>` and none of the Codex-only headers (decision 4; the field-by-
//! field comparison is `docs/providers/openai-responses-wire-notes.md` §8). The adapter knows
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
mod usage_limit;

#[cfg(feature = "testing")]
pub mod testing;

pub use config::{ConfigError, OpenAiResponsesConfig, redact_url};
pub use model::OpenAiResponses;
