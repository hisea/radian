//! A model adapter for the Anthropic Messages API, implementing `agent_core::Model` with
//! streaming, using **API key** authentication only.
//!
//! The wire format follows Anthropic's official developer documentation; every event, field
//! and `stop_reason` value the adapter relies on is recorded with its URL and access date in
//! `docs/providers/anthropic-wire-notes.md`. The adapter does **not** implement subscription
//! OAuth (Anthropic's policy forbids routing consumer subscriptions through third-party
//! tools). Extended thinking is implemented behind an explicit config; server-side tool
//! blocks remain unimplemented.
//!
//! The mapping is deliberately honest rather than clever: text and tool calls round-trip
//! exactly, usage keeps `total: None` (Anthropic never reports one), and content kinds the
//! Messages API cannot carry become short text placeholders.

mod config;
mod model;
mod request;
mod sse;
mod stream;

#[cfg(feature = "testing")]
pub mod testing;

pub use config::{AnthropicConfig, ApiKey, ConfigError, ThinkingConfig};
pub use model::Anthropic;
