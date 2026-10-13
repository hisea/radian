//! Host plumbing that `agent-cli` and `radian-tui` both need, kept in one place so the two
//! hosts cannot drift: `--header` parsing (with `{session_id}` expansion), session-id
//! generation and validation, the `--mcp-config` file, and the "built-in tools + MCP servers"
//! provider.
//!
//! Nothing here knows about a terminal, a prompt loop or a session directory; it is the part
//! of a host that is the same whether the user is at a line prompt or a full-screen UI. The
//! two binaries still decide their own flags, startup text and error wording.

mod catalog;
mod describe;
mod diagnostics;
mod errors;
mod headers;
mod home;
mod mcp;
pub mod memory;
mod model;
mod session_id;
pub mod sessions;
mod tools;
mod workspace;

pub use catalog::{
    CACHE_TTL, CATALOG_URL, Catalog, FETCH_TIMEOUT, ModelLimit, OUTPUT_RESERVATION, Window,
    WindowCheck, WindowQuery, WindowSource, cache_path, host_of, input_budget,
    output_ceiling_warning, output_reservation, provider_for_host, resolve,
};
pub use describe::{CLI_WORDING, TUI_WORDING, Wording, describe};
pub use diagnostics::{
    CleanReport, MAX_TOTAL_BYTES, RETENTION, append, cache_dir, clean, clean_startup, clean_with,
    diagnostics_dir, is_diagnostic_file, now_millis, panic_log_name,
};
pub use errors::{CONTEXT_OVERFLOW_HELP, CONTEXT_OVERFLOW_PREFIX, is_context_overflow};
pub use headers::{SESSION_PLACEHOLDER, parse_headers};
pub use home::{
    RADIAN_HOME_ENV, cache_dir_for, catalog_cache, catalog_cache_for, config_dir, config_dir_for,
    diagnostics_dir_for, ensure_private_dir, legacy_sessions_dirs_for, legacy_sessions_hint,
    legacy_sessions_hint_for, memory_dir, memory_dir_for, radian_home, radian_home_for,
    sessions_dir, sessions_dir_for,
};
pub use mcp::load_mcp_config;
pub use model::{
    DEFAULT_MAX_TOKENS, HostModel, MIN_THINKING_BUDGET, ModelArgs, Provider, Thinking, build_model,
    check_provider_flags, redact_base_url,
};
pub use session_id::{generate_session_id, validate_session_id};
pub use sessions::{
    ExportSettings, Format, SEARCH_LIMIT, SUMMARY_MAX_CHARS, SearchRow, SessionRow,
};
pub use tools::{HostSession, HostTools, merge_tool_specs};
pub use workspace::resolve_workspace;
