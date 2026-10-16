//! Host plumbing that `agent-cli` and `radian-tui` both need, kept in one place so the two
//! hosts cannot drift: `--header` parsing (with `{session_id}` expansion), session-id
//! generation and validation, the `--mcp-config` file, and the "built-in tools + MCP servers"
//! provider.
//!
//! Nothing here knows about a terminal, a prompt loop or a session directory; it is the part
//! of a host that is the same whether the user is at a line prompt or a full-screen UI. The
//! two binaries still decide their own flags, startup text and error wording.

pub mod access;
pub mod agents;
pub mod approval;
pub mod atomic_write;
pub mod auth;
mod catalog;
pub mod config;
pub mod context;
pub mod contrib;
pub mod cost;
mod describe;
mod diagnostics;
mod errors;
mod escape;
pub mod git_commit;
mod headers;
pub mod herdr;
mod home;
pub mod job_notify;
pub mod lead_tools;
pub mod login;
mod mcp;
pub mod memory;
pub mod mermaid;
mod mermaid_layout;
mod model;
pub mod observer;
pub mod opencode_go;
mod output;
pub mod pending;
pub mod projection;
pub mod prompt;
pub mod provider;
pub mod session;
mod session_id;
pub mod sessions;
pub mod skills;
pub mod startup;
pub mod task;
pub mod task_host;
pub mod task_pending;
mod tools;
pub mod trust;
mod workspace;

pub use agents::{
    AgentDef, AgentError, AgentSource, SpawnAgent, SpawnOverrides, builtin_def, load as load_agent,
    parse_file as parse_agent_file, parse_model_route, parse_tier, resolve as resolve_agent,
};
pub use approval::{StartupApproval, resolve_startup, restored_approval};
pub use auth::{
    AUTH_FILE_NAME, AuthEntry, AuthError, AuthStore, RADIAN_BASE_URL, ResolvedKey, auth_path,
    list as list_auth, remove as remove_auth, resolve_key, resolve_note, resolved_keys,
    upsert_api_key as upsert_auth_api_key,
};
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
    CACHE_TTL, CATALOG_URL, Catalog, FETCH_TIMEOUT, ModelEntry, ModelLimit, ModelPrice,
    OUTPUT_RESERVATION_CAP, PriceRates, PriceTier, ProviderInfo, ProviderModels, Window,
    WindowCheck, WindowQuery, WindowSource, cache_path, fetch_provider_models, host_of,
    input_budget, output_ceiling_warning, output_reservation, provider_for_host,
    provider_models_cache, provider_models_stale, read_provider_models, resolve, resolve_offline,
    write_provider_models,
};
pub use config::migrate_pi::{PiDraft, PiMigrateError, import_pi, write_draft as write_pi_draft};
pub use config::theme_write::{ThemeWriteError, write_theme};
pub use config::{
    CONFIG_FILE_NAME, CompactionConfig, Config, ConfigError, ModelConfig, PermissionsConfig,
    RADIAN_CONFIG_ENV, SUPPORTED_SCHEMA_VERSION, TuiConfig, load as load_config,
    parse as parse_config, resolve_allow_unix_sockets, resolve_path as resolve_config_path,
    resolved_path as resolved_config_path,
};
pub use context::{ContextFiles, LoadedFile, load as load_context_files};
pub use contrib::{
    Contribution, ContributionStore, ContributorRunner, GitContributor, Segment, SegmentRole,
    StatusContributor, Tick, spawn as spawn_contributor, spawn_gated as spawn_contributor_gated,
};
pub use describe::{CLI_WORDING, TUI_WORDING, Wording, describe};
pub use diagnostics::{
    CleanReport, MAX_TOTAL_BYTES, RETENTION, append, cache_dir, clean, clean_startup, clean_with,
    diagnostics_dir, is_diagnostic_file, now_millis, panic_log_name,
};
pub use errors::{CONTEXT_OVERFLOW_HELP, CONTEXT_OVERFLOW_PREFIX, is_context_overflow};
pub use headers::{SESSION_PLACEHOLDER, parse_headers};
pub use home::{
    RADIAN_HOME_ENV, access_dir_for, agents_dir, agents_dir_for, cache_dir_for, catalog_cache,
    catalog_cache_for, config_dir, config_dir_for, credentials_dir, credentials_dir_for,
    diagnostics_dir_for, ensure_private_dir, legacy_sessions_dirs_for, legacy_sessions_hint,
    legacy_sessions_hint_for, memory_dir, memory_dir_for, radian_home, radian_home_for,
    sessions_dir, sessions_dir_for, skills_dir, skills_dir_for, themes_dir, themes_dir_for,
    trust_dir, trust_dir_for, user_home, worktrees_dir_for,
};
pub use job_notify::{
    JobNotifier, MAX_CONSECUTIVE_WAKES, NOTIFY_INLINE_BYTES, orphan_jobs, orphan_message,
};
pub use lead_tools::{
    DEFAULT_WAIT_TIMEOUT_SECS, LEAD_TOOL_NAMES, LeadSession, LeadTools, TASK_DETAIL_LIMIT_CHARS,
    TASK_SUMMARY_LIMIT_CHARS, run as run_lead_tool,
};
pub use mcp::load_mcp_config;
pub use mermaid::{MermaidCheckSession, MermaidCheckTools, check};
pub use model::{
    COMPACTION_THINKING_CURRENT, DEFAULT_MAX_TOKENS, HostModel, MIN_THINKING_BUDGET, ModelArgs,
    Provider, RouteContext, SwitchableModel, Thinking, build_model, check_provider_flags,
    compaction_settings_for, compaction_settings_for_route, redact_base_url,
    resolve_reasoning_replay, resolve_thinking, resolve_thinking_ref, route_replays_reasoning,
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
pub use projection::{
    LastToolCall, ProjectionRead, SessionProjection, StreamKind, StreamSnapshot, TurnState,
};
pub use prompt::{Mode, RoleHooks};
pub use provider::{
    CUSTOM_LOGIN, DEFAULT_THINKING_LEVEL, DESCRIPTORS, DescriptorModel, GENERIC_ID, LoginMethod,
    NamedProvider, NamedSource, Protocol, ProviderDecl, ProviderDescriptor, ProviderRef,
    RADIAN_KIND, ReasoningReplayOverride, Route, THINKING_LEVELS, ThinkingMap, clamp_thinking,
    clamp_thinking_with, descriptor, descriptors, level_fits, login_choices, name_error,
    parse_thinking_level, resolve_provider, resolve_route, resolve_route_for,
    supported_thinking_levels, supported_thinking_levels_ref, supported_thinking_levels_ref_with,
    supported_thinking_levels_with, supported_wire, thinking_map_for, thinking_map_for_ref,
    thinking_rank, valid_name, wire_budget,
};
pub use radian_runtime::ModelRoute;
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
    completion_candidates, discover, discover_with_extras, expand, has_skill_file, notices,
    prompt_section,
};
pub use startup::{
    LeadSetup, ListError, SessionSettings, is_local_base_url, list_sessions, session_dir,
    session_workspace,
};
pub use task::{
    CommitGate, EndCommit, Isolation, OrchestrationLimits, QueuedTask, SpawnHook, SpawnRequest,
    SpawnStage, Spawned, StallNotice, TaskBuilder, TaskContext, TaskError, TaskHandle,
    TaskNotifier, TaskRecord, TaskRegistry, TaskSeams, TaskSession, end_of_task_commit,
    task_sandbox,
};
pub use task_host::{TaskHost, TaskModelFactory, task_builder, task_lines};
pub use task_pending::{
    DEFAULT_PENDING_TIMEOUT, DEFAULT_POLICY_TIMEOUT, PolicyCommand, TaskApprover,
    TaskPendingContent, TaskPendingError, TaskPendingKind, TaskPendingQueue, TaskPendingView,
    TaskPrompter,
};
pub use tools::{HostSession, HostTools, merge_tool_specs};
pub use workspace::resolve_workspace;
