//! The startup helpers the two hosts share (ADR 0032 slice 5): where session files live, which
//! workspace a session belongs to, the `--list` table, and the loopback-endpoint rule.
//!
//! Both `radian exec` (`agent-cli`) and `radian` (`radian-tui`) used to carry their own copy of
//! each of these, so a change to one could leave the other behind. The rules live here; a host
//! keeps what is really its own — its flags, its error type and exit code, its wording. Every
//! host-specific input arrives as a plain value (a path, a string), never as a host's struct, and
//! every failure comes back as a plain message the host maps into its own error.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_core::{ContextBudget, TurnOptions};
use agent_extensions::McpServerConfig;
use radian_permissions::{Preset, Profile};
use radian_runtime::{BudgetFor, CompactionSettingsFor, ConfigQueue, ConfigSlot};
use radian_sandbox::Sandbox;

use crate::config::{Config, resolve_extra_paths};
use crate::cost::RoutePrice;
use crate::describe::Wording;
use crate::memory::InjectionConfig;
use crate::model::SwitchableModel;
use crate::output::{StdoutError, write_line};
use crate::provider::ProviderRef;
use crate::session::{AccessBuilder, SessionCapability, SessionId, SessionSetup};
use crate::sessions;
use crate::trust;
use crate::workspace::resolve_workspace;

/// Where session files live: an explicit `--session-dir` wins, then `RADIAN_SESSION_DIR`, then
/// the user's data directory (ADR 0020: `~/.radian/sessions`, or `$RADIAN_HOME/sessions`).
///
/// Sessions are durable records, not a cache (ADR 0013 decision 4). The default directory is
/// created owner-only on first use and the sandbox denies read on it (ADR 0020 decision 3).
pub fn session_dir(flag: Option<&Path>) -> Result<PathBuf, String> {
    session_dir_with(
        flag,
        session_dir_env(std::env::var("RADIAN_SESSION_DIR").ok()).as_deref(),
        crate::sessions_dir,
    )
}

/// `RADIAN_SESSION_DIR`'s value as a directory, or `None` when it is unset or empty.
fn session_dir_env(raw: Option<String>) -> Option<PathBuf> {
    raw.filter(|dir| !dir.is_empty()).map(PathBuf::from)
}

/// [`session_dir`] with the environment and the default directory passed in: the rule, so it is
/// testable without touching the process environment or the user's home. `default` is called
/// only when neither the flag nor the variable names a directory, and it is a closure because
/// resolving it **creates** `~/.radian/sessions` (a run with an explicit `--session-dir` must
/// not create the default one).
fn session_dir_with(
    flag: Option<&Path>,
    env: Option<&Path>,
    default: impl FnOnce() -> Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(dir) = flag {
        return Ok(dir.to_path_buf());
    }
    if let Some(dir) = env {
        return Ok(dir.to_path_buf());
    }
    default().ok_or_else(|| {
        "no RADIAN_HOME or HOME to place sessions under; pass --session-dir".to_owned()
    })
}

/// The workspace a session belongs to: `root` when the host has one, else the current
/// directory, canonicalized and validated by the shared [`resolve_workspace`] rule (dogfooding
/// D1). `--list` filters by the same value, so a session is only listed in the workspace it was
/// created in.
///
/// The host resolves `root` itself: the TUI applies its two coding-root exceptions (`$HOME`
/// becomes `$HOME/tmp`, the filesystem root is refused, owner UI review 2026-10-13), while the
/// CLI — the unattended entry point — does not.
pub fn session_workspace(root: Option<&Path>) -> Result<String, String> {
    Ok(resolve_workspace(root)?.to_string_lossy().into_owned())
}

/// Why a `--list` stopped. `Failed` and `Write` are real errors: the host prints the message and
/// picks its own exit code (the CLI exits 2 for a setup or index failure, the TUI 1 for a failure
/// of the run). `BrokenPipe` is stdout's reader going away (`radian exec --list | head`),
/// a normal way to stop: exit 0 with no message (dogfooding D21).
#[derive(Debug)]
pub enum ListError {
    /// A setup or index failure.
    Failed(String),
    /// A stdout write error other than a closed reader.
    Write(std::io::Error),
    /// stdout's reader went away.
    BrokenPipe,
}

impl From<String> for ListError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<StdoutError> for ListError {
    fn from(error: StdoutError) -> Self {
        match error {
            StdoutError::BrokenPipe => Self::BrokenPipe,
            StdoutError::Failed(error) => Self::Write(error),
        }
    }
}

/// `--list`: the sessions of `workspace`, newest activity first, one
/// `id<TAB>created<TAB>last<TAB>messages` line each. Nothing is printed when the session
/// directory does not exist yet (nothing was ever created).
pub fn list_sessions(dir: &Path, workspace: &str) -> Result<(), ListError> {
    for row in session_rows(dir, workspace)? {
        write_line(&format!(
            "{}\t{}\t{}\t{}",
            row.session_id,
            row.created_ts.as_deref().unwrap_or("-"),
            row.last_ts.as_deref().unwrap_or("-"),
            row.message_count
        ))?;
    }
    Ok(())
}

/// The rows [`list_sessions`] prints, through the shared index (a disposable cache: the `.jsonl`
/// files are the authority, ADR 0006 §10). Empty when the session directory does not exist yet.
fn session_rows(dir: &Path, workspace: &str) -> Result<Vec<sessions::SessionRow>, String> {
    let Some(index) = sessions::open_index(dir)? else {
        return Ok(Vec::new());
    };
    sessions::list(&index, workspace)
}

/// Whether `base_url` points at this machine (loopback), where a server needs no credential
/// (ADR 0026 D2). A URL with no scheme is not a URL this rule can read, so it is not local.
pub fn is_local_base_url(base_url: &str) -> bool {
    let Some((_, rest)) = base_url.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.strip_prefix('[').map_or_else(
        || {
            authority
                .rsplit_once(':')
                .filter(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
                .map_or(authority, |(host, _)| host)
        },
        |bracketed| bracketed.split(']').next().unwrap_or_default(),
    );
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

/// The session settings both hosts read from the **config file** the same way (ADR 0032 slice 5).
///
/// The resolution used to be written out three times — the CLI's `setup`, the TUI's lead session
/// and the TUI's task builder — with the CLI's task builder reusing whatever the lead had
/// resolved, so the four call sites could disagree. The defaults stay where they are documented
/// (the config reader and the ADRs); this only stops the copies from drifting apart.
#[derive(Clone)]
pub struct SessionSettings {
    /// `[model] retries` (ADR 0017): how many times a transient model failure is
    /// retried inside one model call. `0` disables.
    pub retries: u32,
    /// `[compaction] auto` unless `--no-auto-compact` (ADR 0018): whether the core compacts a
    /// request over the budget before sending it.
    pub auto_compact: bool,
    /// The request-level settings the **summary** call carries (ADR 0018),
    /// converted from `[compaction] thinking` to the route's wire value.
    pub compaction_settings: serde_json::Value,
    /// Recomputes [`SessionSettings::compaction_settings`] for a switched route (ADR 0018
    /// revision 8, ADR 0026 D7). `None` means a switch cannot move the settings (they stay at
    /// the startup value); both hosts have one.
    pub compaction_settings_for: Option<CompactionSettingsFor>,
    /// `[shell] auto_background_secs` (ADR 0037): a **foreground** `shell` call still
    /// running after this is adopted by the job registry in place. `None` disables adoption.
    pub shell_auto_background: Option<Duration>,
    /// `[skills] extra_paths` (ADR 0025 slice 4), resolved against the config file's directory
    /// and the user's home.
    pub skills_extra_paths: Vec<PathBuf>,
    /// `[skills] builtin` (ADR 0025): include the compiled-in skills. Default true.
    pub skills_builtin: bool,
    /// `[skills] disabled`: built-in skill names to leave out.
    pub skills_disabled: Vec<String>,
    /// The memory injection configuration (ADR 0021).
    pub memory_injection: InjectionConfig,
}

impl SessionSettings {
    /// The settings for the route this run started with. `max_tokens` is the host's resolved
    /// request ceiling (`--max-tokens` or the built-in default) and `no_auto_compact` the host's
    /// `--no-auto-compact`. The second value is the summary level's clamp notice, when there is
    /// one: it is **not** printed here, because the two hosts word their startup diagnostics
    /// differently (the TUI prefixes them with `radian: `).
    pub fn from_config(
        config: &Config,
        provider_ref: &ProviderRef,
        model_name: &str,
        max_tokens: u32,
        no_auto_compact: bool,
    ) -> (Self, Option<String>) {
        let level = config.compaction.thinking.as_deref().unwrap_or("low");
        let (compaction_settings, notice) =
            crate::model::compaction_settings_for(provider_ref, model_name, level, max_tokens);
        let compaction_settings_for = {
            let providers = config.providers.clone();
            let level = config
                .compaction
                .thinking
                .clone()
                .unwrap_or_else(|| "low".to_owned());
            Arc::new(move |provider: &str, model: &str| {
                crate::model::compaction_settings_for_route(
                    &providers, provider, model, &level, max_tokens,
                )
            }) as CompactionSettingsFor
        };
        let settings = Self {
            // `--no-auto-compact` wins; otherwise the config may turn it off.
            auto_compact: !no_auto_compact && config.compaction.auto.unwrap_or(true),
            retries: config
                .model
                .retries
                .unwrap_or(agent_core::DEFAULT_MODEL_RETRIES),
            compaction_settings,
            compaction_settings_for: Some(compaction_settings_for),
            shell_auto_background: config.shell.auto_background_after(),
            skills_extra_paths: resolve_extra_paths(
                &config.skills.extra_paths,
                config.source.as_deref().and_then(Path::parent),
                crate::user_home().as_deref(),
            ),
            skills_builtin: config.skills.builtin.unwrap_or(true),
            skills_disabled: config.skills.disabled.clone(),
            memory_injection: config.memory.injection(),
        };
        (settings, notice)
    }
}

/// Everything a session a host opens itself needs (ADR 0032 slice 5), and the one place the two
/// hosts build it: `radian exec` and the TUI each fill this and call [`LeadSetup::setup`], and
/// the task builder fills it too — the fields a task does not decide are the same defaults, and
/// the registry overwrites the ones a task owns.
///
/// The fields are the [`SessionSetup`] fields a host really decides (see each one's doc there);
/// the ones that are the same for every host-opened session — the empty config queue, no role, no
/// agent definition's allow-lists, not a task agent, `/mode` allowed — are set by
/// [`LeadSetup::setup`] and cannot drift.
pub struct LeadSetup {
    /// The settings both hosts resolve from the config the same way (see [`SessionSettings`]),
    /// and the same value the task builder is built from.
    pub settings: SessionSettings,
    pub session_id: SessionId,
    pub session_dir: PathBuf,
    pub workspace: PathBuf,
    /// The `--workspace` flag: the built-in coding tools exist only when it was given (the TUI
    /// passes its coding root, which is always some directory).
    pub coding_root: Option<PathBuf>,
    pub model: SwitchableModel,
    pub config_slot: Arc<ConfigSlot>,
    /// Computes `B` for a switched model from the local catalog (ADR 0026 D7).
    pub budget_for: Option<BudgetFor>,
    /// Resolves a route's catalog prices from the warm local cache (ADR 0027).
    pub price_for: Option<RoutePrice>,
    /// Branch names `git_commit` refuses besides `main`/`master` (ADR 0030 R10).
    pub protected_branches: Vec<String>,
    /// Filter drivers `git add` may run (ADR 0030).
    pub allowed_filters: Option<Vec<String>>,
    pub servers: Vec<McpServerConfig>,
    /// `--system`/`--system-file`, already read: the fixed layer 1 (ADR 0036).
    pub system_prompt: Option<String>,
    pub context_budget: Option<ContextBudget>,
    pub sandbox: Option<Sandbox>,
    pub tier: Preset,
    pub profile: Profile,
    /// Rebuilds the profile and sandbox for a run-time tier switch (ADR 0038 slice 2). `None`
    /// for a host with no `/access` command (the CLI): a switch is then refused.
    pub access_builder: Option<AccessBuilder>,
    pub memory_root: Option<PathBuf>,
    pub trust: trust::Setup,
    pub resume: bool,
    pub config_snapshot: serde_json::Value,
    pub turn_options: TurnOptions,
    pub turn_timeout: Option<Duration>,
    pub wording: Wording,
    pub capabilities: Vec<SessionCapability>,
}

impl LeadSetup {
    /// The [`SessionSetup`] for a session a host opens itself, with the fields every host agrees
    /// on filled in.
    pub fn setup(self) -> SessionSetup {
        let settings = self.settings;
        SessionSetup {
            session_id: self.session_id,
            session_dir: self.session_dir,
            workspace: self.workspace,
            coding_root: self.coding_root,
            model: self.model,
            config_slot: self.config_slot,
            // A host starts with an empty queue; the driver drains what `/model` and the
            // approval backfill push at the turn boundary (ADR 0026 D5/D9).
            config_queue: ConfigQueue::new(),
            budget_for: self.budget_for,
            price_for: self.price_for,
            protected_branches: self.protected_branches,
            allowed_filters: self.allowed_filters,
            shell_auto_background: settings.shell_auto_background,
            servers: self.servers,
            // A host-opened session is neither an agent definition (ADR 0030 O11: no role, no
            // built-in tool allow-list and no skills allow-list) nor a task agent (O13): `/mode`
            // is allowed and `git_commit` is not restricted to one branch. The task builder
            // starts from these defaults as well; the registry overwrites the ones a task owns.
            role: None,
            system_prompt: self.system_prompt,
            context_budget: self.context_budget,
            auto_compact: settings.auto_compact,
            compaction_settings: settings.compaction_settings,
            compaction_settings_for: settings.compaction_settings_for,
            retries: settings.retries,
            sandbox: self.sandbox,
            tier: self.tier,
            profile: self.profile,
            access_builder: self.access_builder,
            task_agent: false,
            parent: None,
            allowed_branch: None,
            git_commit_requires_branch: false,
            memory_root: self.memory_root,
            skills_extra_paths: settings.skills_extra_paths,
            skills_builtin: settings.skills_builtin,
            skills_disabled: settings.skills_disabled,
            tools_allow: None,
            skills_allow: None,
            memory_injection: settings.memory_injection,
            trust: self.trust,
            resume: self.resume,
            config_snapshot: self.config_snapshot,
            turn_options: self.turn_options,
            turn_timeout: self.turn_timeout,
            wording: self.wording,
            capabilities: self.capabilities,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique directory under the temp dir, cleaned up by the caller.
    fn temp_dir(label: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-host-startup-{}-{label}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The `--session-dir` / `RADIAN_SESSION_DIR` / default precedence (ADR 0020), the rule both
    /// hosts used to carry a copy of.
    #[test]
    fn the_session_directory_prefers_the_flag_then_the_variable_then_the_default() {
        let flag = Path::new("/flag/sessions");
        let env = Path::new("/env/sessions");
        let default = || Some(PathBuf::from("/default/sessions"));
        assert_eq!(
            session_dir_with(Some(flag), Some(env), default),
            Ok(flag.into())
        );
        assert_eq!(session_dir_with(None, Some(env), default), Ok(env.into()));
        assert_eq!(
            session_dir_with(None, None, default),
            Ok(PathBuf::from("/default/sessions"))
        );
        // No flag, no variable and no `RADIAN_HOME`/`HOME`: the one message both copies had.
        let error = session_dir_with(None, None, || None).unwrap_err();
        assert_eq!(
            error,
            "no RADIAN_HOME or HOME to place sessions under; pass --session-dir"
        );
        // The default is only resolved — and so only created — when nothing else names a
        // directory.
        assert_eq!(
            session_dir_with(Some(flag), None, || panic!("the default is not resolved")),
            Ok(flag.into())
        );
    }

    /// An empty `RADIAN_SESSION_DIR` is unset, not "the current directory".
    #[test]
    fn an_empty_session_directory_variable_is_ignored() {
        assert_eq!(session_dir_env(None), None);
        assert_eq!(session_dir_env(Some(String::new())), None);
        assert_eq!(
            session_dir_env(Some("/env/sessions".to_owned())),
            Some(PathBuf::from("/env/sessions"))
        );
    }

    /// The shared workspace rule: the root is canonicalized and validated, and a bad one is a
    /// workspace error that names the path (dogfooding D1).
    #[test]
    fn the_session_workspace_is_the_canonical_root() {
        let dir = temp_dir("workspace");
        let nested = dir.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        let workspace = session_workspace(Some(&dir.join("sub").join("..").join("sub"))).unwrap();
        assert_eq!(workspace, nested.canonicalize().unwrap().to_string_lossy());
        let missing = dir.join("does-not-exist");
        let error = session_workspace(Some(&missing)).unwrap_err();
        assert!(error.contains(&missing.display().to_string()), "{error}");
        assert!(!error.contains("sandbox"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `--list` table: one line per session of the workspace, and nothing at all when the
    /// session directory does not exist yet.
    #[test]
    fn the_list_rows_are_one_per_session_of_the_workspace() {
        let dir = temp_dir("list");
        let workspace = dir.join("work");
        std::fs::create_dir_all(&workspace).unwrap();

        // Nothing was ever created: no rows, and `list_sessions` is not an error.
        assert!(session_rows(&dir, "/anywhere").unwrap().is_empty());
        assert!(list_sessions(&dir, "/anywhere").is_ok());

        write_session(&dir, "aaaa", &workspace.to_string_lossy(), "hello");
        write_session(&dir, "bbbb", "/elsewhere", "other");
        let rows = session_rows(&dir, &workspace.to_string_lossy()).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].session_id, "aaaa");
        assert_eq!(
            rows[0].created_ts.as_deref(),
            Some("2026-10-16T09:00:00.000000Z")
        );
        assert_eq!(
            rows[0].last_ts.as_deref(),
            Some("2026-10-16T09:00:01.000000Z")
        );
        assert_eq!(rows[0].message_count, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A minimal session file: one `SessionCreated` and one message, the shape the index reads.
    fn write_session(dir: &Path, session_id: &str, workspace: &str, text: &str) {
        let line = |seq: u64, ts: &str, event: &str, data: serde_json::Value| {
            format!(
                "{}\n",
                serde_json::json!({
                    "schema_version": 1,
                    "session_id": session_id,
                    "seq": seq,
                    "ts": ts,
                    "event": event,
                    "critical": true,
                    "data": data,
                })
            )
        };
        let mut contents = line(
            0,
            "2026-10-16T09:00:00.000000Z",
            "SessionCreated",
            serde_json::json!({"workspace": workspace, "config_snapshot": {}}),
        );
        contents.push_str(&line(
            1,
            "2026-10-16T09:00:01.000000Z",
            "MessageAppended",
            serde_json::json!({
                "index": 0,
                "message": {"user": {"content": [{"text": {"text": text}}]}},
            }),
        ));
        std::fs::write(dir.join(format!("{session_id}.jsonl")), contents).unwrap();
    }

    /// The loopback rule (ADR 0026 D2): a local server needs no credential, and only a host that
    /// really is this machine counts.
    #[test]
    fn only_a_loopback_host_is_local() {
        assert!(is_local_base_url("http://localhost:8080"));
        assert!(is_local_base_url("http://localhost"));
        assert!(is_local_base_url("http://127.0.0.1:11434/v1"));
        assert!(is_local_base_url("http://[::1]:8080"));
        assert!(is_local_base_url("https://LOCALHOST/v1"));
        // A URL with no scheme is not readable by this rule.
        assert!(!is_local_base_url("localhost:8080"));
        assert!(!is_local_base_url("/tmp/socket"));
        // A host that merely starts with or contains a loopback name is not loopback.
        assert!(!is_local_base_url("https://localhost.example.com"));
        assert!(!is_local_base_url("https://127.0.0.1.example.com"));
        assert!(!is_local_base_url("https://example.com"));
        // A non-numeric or empty port is not a port, so the authority keeps its colon and no
        // host matches.
        assert!(!is_local_base_url("http://localhost:/x"));
        assert!(!is_local_base_url("http://127.0.0.1:notaport"));
    }
}
