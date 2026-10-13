//! Host plumbing that `agent-cli` and `radian-tui` both need, kept in one place so the two
//! hosts cannot drift: `--header` parsing (with `{session_id}` expansion), session-id
//! generation and validation, the `--mcp-config` file, and the "built-in tools + MCP servers"
//! provider.
//!
//! Nothing here knows about a terminal, a prompt loop or a session directory; it is the part
//! of a host that is the same whether the user is at a line prompt or a full-screen UI. The
//! two binaries still decide their own flags, startup text and error wording.

mod describe;
mod headers;
mod mcp;
mod model;
mod session_id;
mod tools;
mod workspace;

pub use describe::{CLI_WORDING, TUI_WORDING, Wording, describe};
pub use headers::{SESSION_PLACEHOLDER, parse_headers};
pub use mcp::load_mcp_config;
pub use model::{DEFAULT_MAX_TOKENS, HostModel, ModelArgs, Provider, Thinking, build_model};
pub use session_id::{generate_session_id, validate_session_id};
pub use tools::{HostSession, HostTools, merge_tool_specs};
pub use workspace::resolve_workspace;
