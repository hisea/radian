//! Host plumbing that `agent-cli` and `radian-tui` both need, kept in one place so the two
//! hosts cannot drift: `--header` parsing (with `{session_id}` expansion), session-id
//! generation and validation, the `--mcp-config` file, and the "built-in tools + MCP servers"
//! provider.
//!
//! Nothing here knows about a terminal, a prompt loop or a session directory; it is the part
//! of a host that is the same whether the user is at a line prompt or a full-screen UI. The
//! two binaries still decide their own flags, startup text and error wording.

pub mod approval;
pub mod auth;
mod catalog;
pub mod config;
mod describe;
mod diagnostics;
mod errors;
mod headers;
mod home;
mod mcp;
pub mod memory;
mod model;
mod output;
pub mod pending;
pub mod projection;
pub mod provider;
pub mod session;
mod session_id;
pub mod sessions;
mod tools;
mod workspace;

pub use approval::{StartupApproval, resolve_startup, restored_approval};
pub use auth::{
    AUTH_FILE_NAME, AuthEntry, AuthError, AuthStore, RADIAN_BASE_URL, ResolvedKey, auth_path,
    list as list_auth, remove as remove_auth, resolve_key, resolve_note,
    upsert_api_key as upsert_auth_api_key,
};
pub use catalog::{
    CACHE_TTL, CATALOG_URL, Catalog, FETCH_TIMEOUT, ModelEntry, ModelLimit, OUTPUT_RESERVATION_CAP,
    ProviderInfo, Window, WindowCheck, WindowQuery, WindowSource, cache_path, host_of,
    input_budget, output_ceiling_warning, output_reservation, provider_for_host, resolve,
};
pub use config::migrate_pi::{PiDraft, PiMigrateError, import_pi, write_draft as write_pi_draft};
pub use config::{
    CONFIG_FILE_NAME, CompactionConfig, Config, ConfigError, ModelConfig, PermissionsConfig,
    RADIAN_CONFIG_ENV, SUPPORTED_SCHEMA_VERSION, TuiConfig, load as load_config,
    parse as parse_config, resolve_path as resolve_config_path,
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
    credentials_dir, credentials_dir_for, diagnostics_dir_for, ensure_private_dir,
    legacy_sessions_dirs_for, legacy_sessions_hint, legacy_sessions_hint_for, memory_dir,
    memory_dir_for, radian_home, radian_home_for, sessions_dir, sessions_dir_for, themes_dir,
    themes_dir_for,
};
pub use mcp::load_mcp_config;
pub use model::{
    DEFAULT_MAX_TOKENS, HostModel, MIN_THINKING_BUDGET, ModelArgs, Provider, Thinking, build_model,
    check_provider_flags, redact_base_url,
};
pub use output::{StdoutError, write_line, write_stdout};
pub use pending::{
    Abandoned, Answered, PendingAnswer, PendingQueue, PendingRequest, PendingResponse, PendingView,
    QueuedApprover, QueuedPrompter,
};
pub use projection::{
    LastToolCall, ProjectionRead, SessionProjection, StreamKind, StreamSnapshot, TurnState,
};
pub use provider::{
    DESCRIPTORS, DescriptorModel, GENERIC_ID, LoginMethod, Protocol, ProviderDescriptor, Route,
    descriptor, descriptors, resolve_route,
};
pub use session::{
    ApprovalRouting, CommandRefused, DriverEnd, HostBindings, LocalSession, SessionCapability,
    SessionCommand, SessionError, SessionFacade, SessionFacts, SessionId, SessionQueue,
    SessionSetup, open_session,
};
pub use session_id::{generate_session_id, validate_session_id};
pub use sessions::{
    ExportSettings, Format, SEARCH_LIMIT, SUMMARY_MAX_CHARS, SearchRow, SessionRow,
};
pub use tools::{HostSession, HostTools, merge_tool_specs};
pub use workspace::resolve_workspace;
