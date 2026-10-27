//! A model adapter for OpenAI-compatible Chat Completions APIs (OpenAI, and the many servers
//! that copy it), implementing `radian_core::Model` with streaming.
//!
//! The adapter keeps the mapping honest rather than clever: text and tool calls round-trip
//! exactly; images and audio go out where the API takes them; resource links, embedded
//! resources and opaque blocks become short text placeholders. Reasoning text some servers
//! stream (`reasoning_content`) is ignored. See [`OpenAiCompatibleConfig`] for the knobs.

mod config;
mod model;
mod overflow;
mod request;
mod sse;
mod stream;

#[cfg(feature = "testing")]
pub mod testing;

pub use config::{ApiKey, ConfigError, OpenAiCompatibleConfig, catalog_max_tokens, redact_url};
pub use model::OpenAiCompatible;
pub use request::reserved_body_keys;
