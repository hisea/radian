//! MCP stdio servers as `agent-core` tools.
//!
//! Each `AgentSession` gets its own server processes, started by [`McpToolProvider`] and
//! kept for the whole conversation. Tools are exposed under stable aliases
//! (see [`tool_alias`]); timeouts, cancellation and crashes leave a server unavailable for the
//! rest of the session rather than restarting it.

mod alias;
mod client;
mod config;
mod process;
mod session;

pub use alias::{MAX_ALIAS_LEN, tool_alias};
pub use config::McpServerConfig;
pub use session::{McpSession, McpStatus, McpToolProvider};
