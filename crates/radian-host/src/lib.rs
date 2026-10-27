//! Host plumbing that `radian-exec` and `radian-tui` both need, kept in one place so the two
//! hosts cannot drift: `--header` parsing (with `{session_id}` expansion), session-id
//! generation and validation, the `--mcp-config` file, and the "built-in tools + MCP servers"
//! provider.
//!
//! Nothing here knows about a terminal, a prompt loop or a session directory; it is the part
//! of a host that is the same whether the user is at a line prompt or a full-screen UI. The
//! two binaries still decide their own flags, startup text and error wording.

pub mod access;
pub mod approval;
pub mod atomic_write;
pub mod auth;
pub mod btw;
mod catalog;
pub mod chatgpt_usage;
pub mod command_footer;
pub mod config;
pub mod context;
pub mod context_estimate;
pub mod contrib;
pub mod control;
pub mod cost;
mod describe;
mod diagnostics;
mod errors;
mod escape;
pub mod extra_body;
pub mod goal;
mod headers;
pub mod headers_usage;
pub mod herdr;
mod home;
pub mod hooks;
pub mod intent_stop;
pub mod job_notify;
pub mod key_pool;
pub mod login;
mod mcp;
pub mod memory;
mod model;
pub mod oauth_chatgpt;
pub mod observer;
pub mod opencode_go;
mod output;
pub mod pending;
pub mod project;
pub mod project_hooks;
pub mod projection;
pub mod prompt;
pub mod provider;
pub mod secrets;
pub mod session;
mod session_id;
pub mod sessions;
pub mod skills;
pub mod startup;
pub mod subagent;
#[cfg(test)]
mod test_support;
mod tools;
pub mod trust;
pub mod usage;
pub mod wake_gate;
mod workspace;

pub use approval::{StartupApproval, resolve_startup, restored_approval};
pub use auth::{
    AUTH_FILE_NAME, AuthEntry, AuthError, AuthStore, KeyChoice, NamedKey, OAuthEntry,
    RADIAN_BASE_URL, ResolvedKey, ResolvedKeys, add_api_key as add_auth_api_key, auth_path,
    has_login, ignored_keys_note, key_choices as auth_key_choices, list as list_auth, mask_key,
    remove as remove_auth, remove_key as remove_auth_key, resolve_key, resolve_keys, resolve_note,
    resolved_keys, upsert_api_key as upsert_auth_api_key,
};
pub use goal::{GoalHooks, GoalTools, SharedGoal, fresh_share, goal_block};
pub use key_pool::{DEFAULT_REST, KeyPool, MAX_REST, PickedKey, PoolKey};
/// The machine's hostname (ADR 0027 `host`), read once at startup. `HOSTNAME` is
/// set by most shells (and by BSD's `login`); the `hostname` command is the portable fallback.
/// No network, no new dependency, and never called at draw time.
pub fn hostname() -> Option<String> {
    if let Ok(name) = std::env::var("HOSTNAME") {
        let name = name.trim();
        if !name.is_empty() {
            return Some(name.to_owned());
        }
    }
    let output = std::process::Command::new("hostname").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8(output.stdout).ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

pub use catalog::{
    CACHE_TTL, CATALOG_SNAPSHOT, CATALOG_URL, Catalog, FETCH_TIMEOUT, ModelEntry, ModelLimit,
    ModelPrice, OUTPUT_RESERVATION_CAP, PROVIDER_MODELS_RETRY, PROVIDER_MODELS_TTL, PriceRates,
    PriceTier, ProviderInfo, ProviderModels, RouteOutput, SNAPSHOT_DATE, Window, WindowCheck,
    WindowQuery, WindowSource, bundled_catalog, cache_path, check_context_window_flag,
    fetch_provider_models, host_of, input_budget, models_url_accepts_key, output_ceiling_warning,
    output_reservation, protocol_catalog, provider_for_host, provider_models_cache,
    provider_models_stale, provider_models_url, read_provider_models, resolve, resolve_offline,
    route_output, route_takes_images, write_provider_models,
};
pub use chatgpt_usage::{
    ChatGptUsage, USAGE_BASE as CHATGPT_USAGE_BASE, USAGE_URL as CHATGPT_USAGE_URL,
};
pub use command_footer::{
    BUILTIN_FOOTER_ITEMS, COMMAND_TIMEOUT, CommandContributor, CommandSpec, FooterContext,
    FooterSnapshot, MAX_OUTPUT_BYTES, command_specs, is_builtin_footer_item,
    parse_command as parse_footer_command,
};
pub use config::default::{DEFAULT_CONFIG_TEMPLATE, ensure_default_config};
pub use config::migrate_claude::{
    ClaudeDraft, ClaudeMcpServer, ClaudeMigrateError, ClaudeSkill, ClaudeTargets,
    ClaudeWriteReport, import_claude, write_claude_draft,
};
pub use config::migrate_pi::{PiDraft, PiMigrateError, import_pi, write_draft as write_pi_draft};
pub use config::model_write::{ModelWriteError, write_model_route};
pub use config::theme_write::{ThemeWriteError, write_theme};
pub use config::{
    CONFIG_FILE_NAME, CompactionConfig, Config, ConfigError, ControlConfig, ModelConfig,
    PermissionsConfig, RADIAN_CONFIG_ENV, SUPPORTED_SCHEMA_VERSION, TuiConfig, load as load_config,
    parse as parse_config, resolve_allow_unix_sockets, resolve_path as resolve_config_path,
    resolved_path as resolved_config_path, unix_sockets_ignored_warning,
};
pub use context::{ContextFiles, ImportedFile, LoadedFile, load as load_context_files};
pub use contrib::{
    Contribution, ContributionStore, ContributorRunner, GitContributor, NOTICE_AFTER, Segment,
    SegmentRole, StatusContributor, Tick, spawn as spawn_contributor,
    spawn_gated as spawn_contributor_gated, spawn_once as spawn_contributor_once,
};
pub use control::{ControlServer, ControlTarget, SUBSCRIBER_BUFFER, SocketGuard};
pub use describe::{CLI_WORDING, TUI_WORDING, Wording, budget_exceeded_line, describe};
pub use diagnostics::{
    CleanReport, MAX_TOTAL_BYTES, RETENTION, append, cache_dir, clean, clean_startup, clean_with,
    diagnostics_dir, is_diagnostic_file, job_notice_log_name, now_millis, panic_log_name,
};
pub use errors::{CONTEXT_OVERFLOW_HELP, CONTEXT_OVERFLOW_PREFIX, is_context_overflow};
pub use extra_body::{EXTRA_BODY_ENV, ExtraBody, parse_extra_body, read_extra_body, reserved_keys};
pub use headers::{
    CHILD_HEADERS_ENV, SESSION_PLACEHOLDER, encode_child_headers, parse_headers,
    take_child_headers_from_env,
};
pub use headers_usage::{
    ANTHROPIC_HEADERS, HeadersUsage, HeadersWindow, OPENAI_HEADERS, headers_snapshot,
};
pub use home::{
    RADIAN_HOME_ENV, access_dir_for, agents_skills_dir, agents_skills_dir_for, cache_dir_for,
    catalog_cache, catalog_cache_for, config_dir, config_dir_for, credentials_dir,
    credentials_dir_for, diagnostics_dir_for, ensure_private_dir, file_tool_deny_read,
    legacy_sessions_dirs_for, legacy_sessions_hint, legacy_sessions_hint_for, memory_dir,
    memory_dir_for, radian_home, radian_home_for, run_dir_for, sessions_dir, sessions_dir_for,
    skills_dir, skills_dir_for, themes_dir, themes_dir_for, trust_dir, trust_dir_for, user_home,
};
pub use hooks::{CommandHooks, HooksWarning, REASON_CHAR_LIMIT};
pub use job_notify::{JobNotifier, NOTIFY_INLINE_BYTES, orphan_jobs, orphan_message};
pub use mcp::{
    CREDENTIAL_VARS, LoadScope, McpLoad, ProjectPreview, ProjectPreviewEntry, load_mcp_config,
    load_mcp_from_toml, load_project, load_project_file, merge_mcp, merge_project_mcp,
    preview_lines, preview_project, project_path,
};
pub use model::{
    COMPACTION_THINKING_CURRENT, ChatGptModel, DEFAULT_MAX_MODEL_CALLS, DEFAULT_MAX_TOKENS,
    HostModel, MIN_THINKING_BUDGET, ModelArgs, Provider, RouteContext, SwitchableModel, Thinking,
    build_model, check_provider_flags, compaction_settings_for, compaction_settings_for_route,
    redact_base_url, resolve_reasoning_replay, resolve_thinking, resolve_thinking_ref,
    route_replays_reasoning,
};
pub use oauth_chatgpt::{
    BrowserLogin, CallbackListener, DevicePoll, DeviceStart, Endpoints as OAuthEndpoints,
    OAuthError, PROVIDER_ID as CHATGPT_PROVIDER_ID, RefreshError as OAuthRefreshError,
    TokenResponse as OAuthTokenResponse, device_start as oauth_device_start,
    ensure_fresh as oauth_ensure_fresh, ensure_fresh_in as oauth_ensure_fresh_in,
    poll_device_until_done as oauth_poll_device_until_done, store_token as oauth_store_token,
};
pub use observer::{JournalObserver, SessionObserver};
pub use opencode_go::{
    DEFAULT_PERIOD as OPENCODE_GO_DEFAULT_PERIOD, MAX_PERIOD as OPENCODE_GO_MAX_PERIOD,
    MIN_PERIOD as OPENCODE_GO_MIN_PERIOD, OpencodeGoUsage, USAGE_URL as OPENCODE_GO_USAGE_URL,
    clamp_period as clamp_opencode_go_period,
};
pub use output::{StdoutError, write_line, write_stdout};
pub use pending::{
    Abandoned, Answered, PendingAnswer, PendingQueue, PendingRequest, PendingResponse, PendingView,
    QueuedApprover, QueuedPrompter,
};
pub use project::{ProjectConfig, effective as project_effective_tier, narrow as narrow_tier};
pub use project_hooks::{
    ProjectHooksPreview, ProjectHooksPreviewEntry, load_project as load_project_hooks,
    load_project_file as load_project_hooks_file, preview_lines as preview_project_hooks_lines,
    preview_project as preview_project_hooks, project_path as project_hooks_path,
};
pub use projection::{
    LastToolCall, ProjectionRead, SessionProjection, StreamKind, StreamSnapshot, TurnState,
};
pub use prompt::RoleHooks;
pub use provider::{
    CHATGPT_DEVICE_LOGIN, CUSTOM_LOGIN, DEFAULT_THINKING_LEVEL, DESCRIPTORS, DescriptorModel,
    EndpointUsage, GENERIC_ID, LoginMethod, NamedProvider, NamedSource, Protocol, ProviderDecl,
    ProviderDescriptor, ProviderRef, RADIAN_KIND, ReasoningReplayOverride, Route, THINKING_LEVELS,
    ThinkingMap, UsageSource, clamp_thinking, clamp_thinking_with, default_include_usage,
    descriptor, descriptor_base_url, descriptors, has_usage_source, level_fits, login_choices,
    name_error, parse_thinking_level, resolve_provider, resolve_route, resolve_route_for,
    supported_thinking_levels, supported_thinking_levels_ref, supported_thinking_levels_ref_with,
    supported_thinking_levels_with, supported_wire, thinking_map_for, thinking_map_for_ref,
    thinking_rank, unsupported_api_message, usage_endpoint, usage_item_alias, valid_name,
    wire_budget,
};
pub use radian_runtime::ModelRoute;
pub use secrets::{register as register_secret, registered as registered_secrets};
pub use session::session_hooks;
pub use session::{
    AccessBuilder, AccessParts, ApprovalRouting, CommandRefused, DriverEnd, HostBindings,
    LocalSession, SessionCapability, SessionCommand, SessionError, SessionFacade, SessionFacts,
    SessionId, SessionQueue, SessionSetup, UserBashRecord, open_session, resumed_access,
    resumed_model_route,
};
pub use session_id::{generate_session_id, validate_session_id};
pub use sessions::{
    ExportSettings, Format, SEARCH_LIMIT, SUMMARY_MAX_CHARS, SearchRow, SessionRow,
};
pub use skills::{
    BUILTIN, Discovery, SECTION_BUDGET_TOKENS, SKILL_FILE_NAME, Skill, SkillSource, SkillsHooks,
    completion_candidates, discover, discover_with_extras, discover_with_user_roots, expand,
    has_skill_file, notices, prompt_section,
};
pub use startup::{
    LeadSetup, ListError, SessionSettings, is_local_base_url, list_sessions,
    provider_in_model_hint, session_dir, session_workspace,
};
pub use subagent::{MAX_CONCLUSION_CHARS, SubagentParts, SubagentSession, SubagentTools};
pub use tools::{HostSession, HostTools, merge_tool_specs};
pub use usage::{
    Remaining, USAGE_ITEM, UsageArgs, UsageFetcher, UsageSnapshot, UsageWindow,
    spawn_gated as spawn_usage_gated,
};
pub use workspace::resolve_workspace;
