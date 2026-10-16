//! Project orchestration configuration (ADR 0030 slice 9, O7/O8).
//!
//! Two sources, one shape:
//!
//! - the user level, `[orchestration]` in `~/.radian/config/config.toml` (ADR 0008's schema and
//!   rules); the reader carries it in [`crate::config::Config::orchestration`];
//! - the project level, `<workspace>/.radian/orchestration.toml`, read **only** when the workspace
//!   is trusted (ADR 0031 T1). A project value overrides the user value, key by key.
//!
//! [`OrchestrationConfig`] is the raw, merged shape. [`Orchestration`] is the resolved snapshot the
//! registry uses: the O8 limits, the `verify` command list, the resolved `policy` command, and the
//! spawn defaults. It is resolved **once**, from the LEAD's workspace, and installed process-wide
//! (mirroring [`radian_coding::HeavySlots::set_process`]); a test injects one per registry with
//! [`crate::task::TaskRegistry::with_orchestration`]. That is the ADR 0030 O7 "snapshot at spawn"
//! rule: a task reads none of its own worktree's `.radian/` resources, and editing the project file
//! after the snapshot does not change a running task.
//!
//! Nothing here executes a command. [`run_verify`] runs the `verify` commands **in a task's own
//! sandbox** (ADR 0030 O7 rev 2 item 4); [`crate::task_pending::run_policy`] is unchanged and keeps
//! its no-sandbox skip.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agent_core::CancellationToken;
use radian_coding::{HeavyPermit, HeavySlots};
use radian_sandbox::{Sandbox, SandboxPolicy};
use serde::Serialize;
use toml::Value;

use crate::agents::SpawnDefaults;
use crate::task::OrchestrationLimits;
use crate::task_pending::{DEFAULT_POLICY_TIMEOUT, PolicyCommand};

/// The project-level file, relative to the workspace (ADR 0030 O7).
pub const PROJECT_FILE: &str = "orchestration.toml";
/// The `<workspace>/.radian/` directory.
pub const PROJECT_DIR: &str = ".radian";
/// The project-level agent definitions (ADR 0030 O11), relative to `.radian/`.
pub const AGENTS_DIR: &str = "agents";
/// The project-level prompt templates (ADR 0030 O7), relative to `.radian/`. No consumer exists
/// yet (slice 10); the directory is a trust trigger and is recorded in the snapshot.
pub const PROMPTS_DIR: &str = "prompts";

/// How long one `verify` command may run before it is killed (ADR 0030 O7; the ADR does not fix a
/// number, so this slice does): the same 10 minutes as the R9 pending timeout. A command that
/// times out is recorded as such, never as a pass.
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The tail of a verify command's output the result keeps, in characters.
pub const VERIFY_TAIL_CHARS: usize = 4000;

/// `<workspace>/.radian/orchestration.toml`.
pub fn project_config_path(workspace: &Path) -> PathBuf {
    workspace.join(PROJECT_DIR).join(PROJECT_FILE)
}

/// `<workspace>/.radian/agents`.
pub fn project_agents_dir(workspace: &Path) -> PathBuf {
    workspace.join(PROJECT_DIR).join(AGENTS_DIR)
}

/// `<workspace>/.radian/prompts`.
pub fn project_prompts_dir(workspace: &Path) -> PathBuf {
    workspace.join(PROJECT_DIR).join(PROMPTS_DIR)
}

/// Whether `dir` holds at least one file whose name ends in `.<ext>`.
pub fn has_files_with_ext(dir: &Path, ext: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.path().extension().is_some_and(|actual| actual == ext))
}

/// The raw orchestration settings, from either source. Every field is `None` when the file did not
/// set it, so a project file overrides the user's value key by key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrchestrationConfig {
    pub max_tasks: Option<usize>,
    pub isolation: Option<String>,
    pub model: Option<String>,
    pub profile: Option<String>,
    /// ADR 0030 O7 lists `worktree_root`; ADR 0039 decision 4 moved the task worktrees out of the
    /// workspace, so this key is carried and ignored (it names the human/integration worktree
    /// root, O7's `.radian-worktrees/`). Kept so a project file using it is not an error.
    pub worktree_root: Option<String>,
    /// `Some` (not empty) distinguishes "set to `[]`" from "unset".
    pub verify: Option<Vec<String>>,
    pub policy: Option<String>,
    pub heavy: Option<Vec<String>>,
    pub heavy_slots: Option<usize>,
    /// Whole seconds (ADR 0030 O8's `stall_after_secs`); stored as an integer so the config stays
    /// `Eq`, and converted to the limits' `f64`.
    pub stall_after_secs: Option<u64>,
    pub merge_needs_human: Option<bool>,
    /// Keys this build does not know, as `orchestration.<key>`; reported, never a refusal.
    pub skipped: Vec<String>,
}

impl OrchestrationConfig {
    /// Parses one `[orchestration]` table (user level) or the project file's body (which is just
    /// that table). `Err(message)` names the wrong key; the caller decides whether it is a
    /// configuration refusal (user level) or a warning (project level, where it is skipped).
    pub fn from_table(value: &Value) -> Result<Self, String> {
        let Some(table) = value.as_table() else {
            return Err("orchestration must be a table".into());
        };
        let mut config = Self::default();
        for (key, value) in table {
            match (key.as_str(), value) {
                ("max_tasks", Value::Integer(n)) if *n >= 1 => {
                    config.max_tasks = Some(*n as usize);
                }
                ("max_tasks", _) => {
                    return Err("`orchestration.max_tasks` must be a positive integer".into());
                }
                ("heavy_slots", Value::Integer(n)) if *n >= 0 => {
                    config.heavy_slots = Some(*n as usize);
                }
                ("heavy_slots", _) => {
                    return Err("`orchestration.heavy_slots` must be a non-negative integer".into());
                }
                ("stall_after_secs", Value::Integer(n)) if *n >= 0 => {
                    config.stall_after_secs = Some(*n as u64);
                }
                ("stall_after_secs", Value::Float(f)) if *f >= 0.0 && f.fract() == 0.0 => {
                    config.stall_after_secs = Some(*f as u64);
                }
                ("stall_after_secs", _) => {
                    return Err(
                        "`orchestration.stall_after_secs` must be a non-negative whole number"
                            .into(),
                    );
                }
                ("merge_needs_human", Value::Boolean(flag)) => {
                    config.merge_needs_human = Some(*flag);
                }
                ("merge_needs_human", _) => {
                    return Err("`orchestration.merge_needs_human` must be a boolean".into());
                }
                ("isolation", Value::String(s)) => config.isolation = Some(s.clone()),
                ("isolation", _) => {
                    return Err("`orchestration.isolation` must be a string".into());
                }
                ("model", Value::String(s)) => config.model = Some(s.clone()),
                ("model", _) => return Err("`orchestration.model` must be a string".into()),
                ("profile", Value::String(s)) => config.profile = Some(s.clone()),
                ("profile", _) => return Err("`orchestration.profile` must be a string".into()),
                ("worktree_root", Value::String(s)) => config.worktree_root = Some(s.clone()),
                ("worktree_root", _) => {
                    return Err("`orchestration.worktree_root` must be a string".into());
                }
                ("policy", Value::String(s)) => config.policy = Some(s.clone()),
                ("policy", _) => return Err("`orchestration.policy` must be a string".into()),
                ("verify", Value::Array(items)) => {
                    config.verify = Some(string_list("verify", items)?);
                }
                ("verify", _) => {
                    return Err("`orchestration.verify` must be an array of strings".into());
                }
                ("heavy", Value::Array(items)) => {
                    config.heavy = Some(string_list("heavy", items)?);
                }
                ("heavy", _) => {
                    return Err("`orchestration.heavy` must be an array of strings".into());
                }
                (other, _) => config.skipped.push(format!("orchestration.{other}")),
            }
        }
        Ok(config)
    }

    /// Merges two configs, `over` (the project file) winning key by key over `self` (the user
    /// level) (ADR 0030 slice 9: "project overrides user"). `skipped` keys are unioned.
    pub fn merged(mut self, over: &Self) -> Self {
        macro_rules! take {
            ($field:ident) => {
                if over.$field.is_some() {
                    self.$field = over.$field.clone();
                }
            };
        }
        take!(max_tasks);
        take!(isolation);
        take!(model);
        take!(profile);
        take!(worktree_root);
        take!(verify);
        take!(policy);
        take!(heavy);
        take!(heavy_slots);
        take!(stall_after_secs);
        take!(merge_needs_human);
        self.skipped.extend(over.skipped.iter().cloned());
        self
    }

    /// The O8 limits this config describes, over the ADR defaults.
    pub fn limits(&self) -> OrchestrationLimits {
        let mut limits = OrchestrationLimits::default();
        if let Some(max_tasks) = self.max_tasks {
            limits.max_tasks = max_tasks;
        }
        if let Some(slots) = self.heavy_slots {
            limits.heavy_slots = slots;
        }
        if let Some(patterns) = &self.heavy {
            limits.heavy_patterns = patterns.clone();
        }
        if let Some(secs) = self.stall_after_secs {
            limits.stall_after_secs = secs as f64;
        }
        limits
    }
}

fn string_list(key: &str, items: &[Value]) -> Result<Vec<String>, String> {
    items
        .iter()
        .map(|item| match item {
            Value::String(s) if !s.trim().is_empty() => Ok(s.clone()),
            _ => Err(format!(
                "`orchestration.{key}` must be an array of non-empty strings"
            )),
        })
        .collect()
}

/// The resolved orchestration snapshot (ADR 0030 O7 rev 2 item 4): what one lead resolved from its
/// own workspace and hands to its [`crate::task::TaskRegistry`].
#[derive(Debug, Clone)]
pub struct Orchestration {
    pub limits: OrchestrationLimits,
    pub verify: Vec<String>,
    pub policy: Option<PolicyCommand>,
    pub defaults: SpawnDefaults,
    pub merge_needs_human: bool,
    /// The LEAD's `<workspace>/.radian/agents`, when the workspace is trusted: the project-level
    /// agent definitions the snapshot reads (never the task's own worktree).
    pub project_agents: Option<PathBuf>,
    /// The LEAD's `<workspace>/.radian/prompts`, when trusted. Recorded; no consumer yet.
    pub project_prompts: Option<PathBuf>,
    /// The project file that was read, when trusted and present (for the startup line).
    pub project_file: Option<PathBuf>,
    /// Non-fatal problems (an unknown key, a widening default, a bad `policy`): startup warnings.
    pub warnings: Vec<String>,
}

impl Default for Orchestration {
    fn default() -> Self {
        Self {
            limits: OrchestrationLimits::default(),
            verify: Vec::new(),
            policy: None,
            defaults: SpawnDefaults::default(),
            merge_needs_human: true,
            project_agents: None,
            project_prompts: None,
            project_file: None,
            warnings: Vec::new(),
        }
    }
}

impl Orchestration {
    /// Resolves a merged config over the ADR defaults, from `workspace` (the LEAD's) and whether
    /// it is `trusted` (ADR 0031). Only a trusted workspace gets the project agent/prompt dirs and
    /// the project `policy` command can only come from a trusted project file or the user config.
    pub fn resolve(config: &OrchestrationConfig, workspace: &Path, trusted: bool) -> Self {
        let mut warnings = config.skipped.clone();
        let defaults = SpawnDefaults::from_raw(
            config.isolation.as_deref(),
            config.model.as_deref(),
            config.profile.as_deref(),
            &mut warnings,
        );
        let policy = config
            .policy
            .as_deref()
            .map(|spec| policy_command(spec, workspace));
        if let Some(spec) = &config.policy
            && spec.trim().is_empty()
        {
            warnings.push("orchestration.policy is empty; no edge policy runs".into());
        }
        let (project_agents, project_prompts, project_file) = if trusted {
            (
                Some(project_agents_dir(workspace)),
                Some(project_prompts_dir(workspace)),
                project_config_path(workspace)
                    .is_file()
                    .then(|| project_config_path(workspace)),
            )
        } else {
            (None, None, None)
        };
        Self {
            limits: config.limits(),
            verify: config.verify.clone().unwrap_or_default(),
            policy,
            defaults,
            // ADR 0030 O7: default true; only `merge_needs_human = false` (a trusted project or
            // the user config) relaxes it, and never for the user's own branch.
            merge_needs_human: config.merge_needs_human.unwrap_or(true),
            project_agents,
            project_prompts,
            project_file,
            warnings,
        }
    }

    /// The startup lines listing the commands that will run (ADR 0030 O7/O slice 9): the `verify`
    /// commands and the `policy` command, when there are any. A trusted workspace only.
    pub fn startup_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.verify.is_empty() {
            lines.push(format!(
                "orchestration: verify: {}",
                self.verify.join(" ; ")
            ));
        }
        if let Some(policy) = &self.policy {
            lines.push(format!(
                "orchestration: policy: {}",
                std::iter::once(policy.program.to_string_lossy().into_owned())
                    .chain(policy.args.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        lines
    }
}

/// Splits a `policy = "<command>"` string into a [`PolicyCommand`] (ADR 0030 O2 layer 2). The
/// grammar is a program plus whitespace-separated arguments — deliberately not a shell, so the
/// configuration cannot smuggle shell metacharacters into a host-run program. A relative program
/// path is resolved against the lead's workspace (the directory the config came from), so the
/// command is found even though it runs inside the task's sandbox.
fn policy_command(spec: &str, workspace: &Path) -> PolicyCommand {
    let mut parts = spec.split_whitespace();
    let program = parts.next().unwrap_or(spec.trim());
    let args: Vec<String> = parts.map(str::to_owned).collect();
    let program = Path::new(program);
    let program = if program.is_absolute() {
        program.to_path_buf()
    } else {
        workspace.join(program)
    };
    let name = program
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| spec.trim().to_owned());
    PolicyCommand::new(name, program)
        .with_args(args)
        .with_timeout(DEFAULT_POLICY_TIMEOUT)
}

/// The installed process snapshot (ADR 0030 slice 9; mirrors
/// [`radian_coding::HeavySlots::set_process`]). First configuration wins.
static PROCESS: OnceLock<Orchestration> = OnceLock::new();

/// Installs the process orchestration snapshot. `true` on the first call; a later call is ignored
/// (the config is read once at startup).
pub fn set_process(orchestration: Orchestration) -> bool {
    PROCESS.set(orchestration).is_ok()
}

/// The installed snapshot, or the ADR defaults when none was installed.
pub fn process() -> Orchestration {
    PROCESS.get().cloned().unwrap_or_default()
}

/// Reads the project-level `orchestration.toml` under `workspace` **only** when `trusted` (ADR
/// 0031 T1). Returns the parsed config and warning lines; an untrusted workspace, a missing file,
/// or a malformed file yields the empty config (a malformed file is a warning, never a refusal:
/// the project file is preferences, and startup must not be blocked).
pub fn load_project(workspace: &Path, trusted: bool) -> (OrchestrationConfig, Vec<String>) {
    if !trusted {
        return (OrchestrationConfig::default(), Vec::new());
    }
    let path = project_config_path(workspace);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (OrchestrationConfig::default(), Vec::new());
        }
        Err(error) => {
            return (
                OrchestrationConfig::default(),
                vec![format!("{}: cannot read: {error}", path.display())],
            );
        }
    };
    let doc: Value = match toml::from_str(&text) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                OrchestrationConfig::default(),
                vec![format!("{}: invalid TOML: {error}", path.display())],
            );
        }
    };
    // The project file *is* the `[orchestration]` table (no `schema_version`): it is not the
    // ADR 0008 config file and its version does not gate it.
    match OrchestrationConfig::from_table(&doc) {
        Ok(config) => (config, Vec::new()),
        Err(message) => (
            OrchestrationConfig::default(),
            vec![format!("{}: {message}", path.display())],
        ),
    }
}

/// Merges the user-level config with the trusted project file (project wins), then resolves the
/// snapshot. `user` is the user-level `[orchestration]` ([`crate::config::Config::orchestration`]).
pub fn resolve(
    workspace: &Path,
    trusted: bool,
    user: &OrchestrationConfig,
) -> (Orchestration, Vec<String>) {
    let (project, mut warnings) = load_project(workspace, trusted);
    let merged = user.clone().merged(&project);
    let mut orchestration = Orchestration::resolve(&merged, workspace, trusted);
    warnings.append(&mut orchestration.warnings);
    (orchestration, warnings)
}

// ---------------------------------------------------------------------------
// The verify runner (ADR 0030 O7 / revision 1 R2)
// ---------------------------------------------------------------------------

/// One `verify` command's result (ADR 0030 R2: the runtime's fact, filled after the end-of-task
/// commit, separate from the model's own report).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifyResult {
    /// The command as configured.
    pub command: String,
    /// The process exit code, or `None` when it was killed (timeout) or could not start.
    pub exit_code: Option<i32>,
    /// Whether the command was killed at [`VERIFY_TIMEOUT`].
    pub timed_out: bool,
    /// The tail of stdout+stderr, bounded to [`VERIFY_TAIL_CHARS`].
    pub output_tail: String,
    /// Whether `output_tail` was cut.
    pub truncated: bool,
    /// How long the command queued for a heavy slot before running (milliseconds).
    pub queued_ms: u64,
    /// `ok` (exit 0), `failed`, `timed_out`, or `spawn_failed`.
    pub status: &'static str,
}

/// Runs every `verify` command in the task's own sandbox (ADR 0030 O7 rev 2 item 4), through the
/// process heavy gate, with [`VERIFY_TIMEOUT`]. The command's cwd is the task's worktree. A
/// command is never run outside the sandbox; if `policy` cannot build a sandbox the command is
/// recorded as `spawn_failed` and nothing runs.
pub async fn run_verify(
    commands: &[String],
    policy: &SandboxPolicy,
    worktree: &Path,
    heavy: &Arc<HeavySlots>,
) -> Vec<VerifyResult> {
    let mut results = Vec::with_capacity(commands.len());
    for command in commands {
        results.push(run_one(command, policy, worktree, heavy).await);
    }
    results
}

async fn run_one(
    command: &str,
    policy: &SandboxPolicy,
    worktree: &Path,
    heavy: &Arc<HeavySlots>,
) -> VerifyResult {
    let sandbox = match Sandbox::from_policy(policy.clone()) {
        Ok(sandbox) => sandbox,
        Err(error) => {
            return VerifyResult {
                command: command.to_owned(),
                exit_code: None,
                timed_out: false,
                output_tail: format!("could not build the task sandbox: {error}"),
                truncated: false,
                queued_ms: 0,
                status: "spawn_failed",
            };
        }
    };
    let mut process = match sandbox.command_str("/bin/sh", &["-c", command]) {
        Ok(process) => process,
        Err(error) => {
            return VerifyResult {
                command: command.to_owned(),
                exit_code: None,
                timed_out: false,
                output_tail: format!("could not build the sandboxed command: {error}"),
                truncated: false,
                queued_ms: 0,
                status: "spawn_failed",
            };
        }
    };
    process.current_dir(worktree);
    if !sandbox.mode().is_enforced() {
        // The transparent backend does not filter the environment; apply the policy whitelist
        // here, exactly as the shell tool does (ADR 0012), so a verify command never inherits the
        // host's API keys.
        process.env_clear();
        for name in &sandbox.policy().allow_env {
            if let Some(value) = std::env::var_os(name) {
                process.env(name, value);
            }
        }
    }

    // The heavy gate (ADR 0030 O8/R4): a `cargo test` verify queues behind the same slots as every
    // session's shell. The token is never cancelled: verify runs to its deadline or its end.
    let cancel = CancellationToken::new();
    let permit: Option<HeavyPermit> = heavy.acquire(command, &cancel).await.ok().flatten();
    let queued_ms = permit
        .as_ref()
        .map(|p| p.waited().as_millis() as u64)
        .unwrap_or(0);

    let child = match tokio::process::Command::from(process)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return VerifyResult {
                command: command.to_owned(),
                exit_code: None,
                timed_out: false,
                output_tail: format!("could not start the command: {error}"),
                truncated: false,
                queued_ms,
                status: "spawn_failed",
            };
        }
    };

    let output = match tokio::time::timeout(VERIFY_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        // A timeout (or a wait error): the child is killed by `kill_on_drop` when it is dropped.
        Ok(Err(error)) => {
            drop(permit);
            return VerifyResult {
                command: command.to_owned(),
                exit_code: None,
                timed_out: false,
                output_tail: format!("could not read the command's output: {error}"),
                truncated: false,
                queued_ms,
                status: "spawn_failed",
            };
        }
        Err(_) => {
            drop(permit);
            return VerifyResult {
                command: command.to_owned(),
                exit_code: None,
                timed_out: true,
                output_tail: format!(
                    "the command did not finish within {} s and was killed",
                    VERIFY_TIMEOUT.as_secs()
                ),
                truncated: false,
                queued_ms,
                status: "timed_out",
            };
        }
    };
    drop(permit);

    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    let (output_tail, truncated) = tail_chars(&combined, VERIFY_TAIL_CHARS);
    let exit_code = output.status.code();
    VerifyResult {
        command: command.to_owned(),
        exit_code,
        timed_out: false,
        output_tail,
        truncated,
        queued_ms,
        status: if exit_code == Some(0) { "ok" } else { "failed" },
    }
}

/// Keeps the last `limit` characters of `text`, prefixing a marker when it cut. Unlike a head
/// truncation, the interesting part of a failed test run is usually at the end.
pub fn tail_chars(text: &str, limit: usize) -> (String, bool) {
    let count = text.chars().count();
    if count <= limit {
        return (text.to_owned(), false);
    }
    let skip = count - limit;
    let mut tail: String = text.chars().skip(skip).collect();
    tail.insert_str(0, "[… output truncated, showing the tail]\n");
    (tail, true)
}

// ---------------------------------------------------------------------------
// The hook points (ADR 0030 O7): stable names and payloads
// ---------------------------------------------------------------------------

/// ADR 0030 O7's four hook points. They are control-port events with these exact names; the
/// payloads are the JSON below. `before_merge` is defined here and has **no producer** until slice
/// 10 (the integration worktree), so nothing emits it yet.
pub mod hook {
    use serde_json::{Value, json};

    pub const TASK_SPAWNED: &str = "task_spawned";
    pub const TASK_FINISHED: &str = "task_finished";
    pub const STALLED: &str = "stalled";
    pub const BEFORE_MERGE: &str = "before_merge";

    /// The four names, so a subscriber and a test agree on the set.
    pub const ALL: [&str; 4] = [TASK_SPAWNED, TASK_FINISHED, STALLED, BEFORE_MERGE];

    pub fn task_spawned(task: &str, worktree: Option<&str>, label: Option<&str>) -> Value {
        json!({ "task": task, "worktree": worktree, "label": label })
    }

    pub fn task_finished(task: &str, outcome: &str, error: Option<&str>) -> Value {
        json!({ "task": task, "outcome": outcome, "error": error })
    }

    pub fn stalled(task: &str, idle_secs: u64) -> Value {
        json!({ "task": task, "idle_secs": idle_secs })
    }

    /// Defined for slice 10; no producer emits it yet (ADR 0030 O7).
    pub fn before_merge(task: &str, branch: &str, commit: Option<&str>) -> Value {
        json!({ "task": task, "branch": branch, "commit": commit })
    }

    /// One hook event's wire shape for a subscriber: `{ "event": { "source": "hook", "name": …,
    /// "data": … } }`. The same envelope the runtime fanout uses, with `source: "hook"`.
    pub fn envelope(name: &str, data: &Value) -> Value {
        json!({ "event": { "source": "hook", "name": name, "data": data } })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> Value {
        toml::from_str::<Value>(text).unwrap()
    }

    #[test]
    fn every_key_round_trips_and_an_unknown_key_is_skipped() {
        let config = OrchestrationConfig::from_table(&table(
            "max_tasks = 3\nisolation = \"shared\"\nmodel = \"p/m\"\nprofile = \"safe\"\n\
             worktree_root = \"/tmp/wt\"\nverify = [\"a\", \"b\"]\npolicy = \"/bin/policy\"\n\
             heavy = [\"cargo test\"]\nheavy_slots = 1\nstall_after_secs = 30\n\
             merge_needs_human = false\nfuture = 1\n",
        ))
        .unwrap();
        assert_eq!(config.max_tasks, Some(3));
        assert_eq!(config.isolation.as_deref(), Some("shared"));
        assert_eq!(config.model.as_deref(), Some("p/m"));
        assert_eq!(config.profile.as_deref(), Some("safe"));
        assert_eq!(config.worktree_root.as_deref(), Some("/tmp/wt"));
        assert_eq!(
            config.verify.as_deref(),
            Some(&["a".to_owned(), "b".to_owned()][..])
        );
        assert_eq!(config.policy.as_deref(), Some("/bin/policy"));
        assert_eq!(
            config.heavy.as_deref(),
            Some(&["cargo test".to_owned()][..])
        );
        assert_eq!(config.heavy_slots, Some(1));
        assert_eq!(config.stall_after_secs, Some(30));
        assert_eq!(config.merge_needs_human, Some(false));
        assert_eq!(config.skipped, ["orchestration.future"]);
    }

    #[test]
    fn a_wrong_type_is_an_error_naming_the_key() {
        for (text, expected) in [
            ("max_tasks = 0\n", "max_tasks"),
            ("max_tasks = \"four\"\n", "max_tasks"),
            ("verify = \"cargo test\"\n", "verify"),
            ("heavy_slots = -1\n", "heavy_slots"),
            ("merge_needs_human = 1\n", "merge_needs_human"),
            ("stall_after_secs = -1\n", "stall_after_secs"),
        ] {
            let error = OrchestrationConfig::from_table(&table(text)).unwrap_err();
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    #[test]
    fn the_project_overrides_the_user_key_by_key() {
        let user = OrchestrationConfig::from_table(&table(
            "max_tasks = 4\nverify = [\"a\"]\nheavy_slots = 2\n",
        ))
        .unwrap();
        let project =
            OrchestrationConfig::from_table(&table("max_tasks = 8\nverify = [\"b\", \"c\"]\n"))
                .unwrap();
        let merged = user.merged(&project);
        assert_eq!(merged.max_tasks, Some(8));
        assert_eq!(
            merged.verify.as_deref(),
            Some(&["b".to_owned(), "c".to_owned()][..])
        );
        // A key only the user set survives.
        assert_eq!(merged.heavy_slots, Some(2));
    }

    #[test]
    fn the_limits_take_the_configured_values_and_the_adr_defaults_otherwise() {
        let config = OrchestrationConfig::from_table(&table(
            "max_tasks = 7\nheavy_slots = 3\nheavy = [\"ninja\"]\nstall_after_secs = 12\n",
        ))
        .unwrap();
        let limits = config.limits();
        assert_eq!(limits.max_tasks, 7);
        assert_eq!(limits.heavy_slots, 3);
        assert_eq!(limits.heavy_patterns, ["ninja"]);
        assert_eq!(limits.stall_after_secs, 12.0);
        // The default config keeps every ADR default.
        let defaults = OrchestrationConfig::default().limits();
        assert_eq!(defaults.max_tasks, 4);
        assert_eq!(defaults.stall_after_secs, 600.0);
    }

    #[test]
    fn an_untrusted_workspace_reads_no_project_config_and_no_policy() {
        let root = std::env::temp_dir().join(format!("radian-orch-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(PROJECT_DIR)).unwrap();
        std::fs::write(
            project_config_path(&root),
            "policy = \"/bin/policy\"\nverify = [\"cargo test\"]\n",
        )
        .unwrap();
        let user = OrchestrationConfig::default();

        // Untrusted: nothing is read.
        let (orchestration, warnings) = resolve(&root, false, &user);
        assert!(
            orchestration.policy.is_none(),
            "an untrusted project policy"
        );
        assert!(
            orchestration.verify.is_empty(),
            "an untrusted project verify"
        );
        assert!(orchestration.project_agents.is_none());
        assert!(warnings.is_empty(), "{warnings:?}");

        // Trusted: the project file is read and its commands are listed.
        let (orchestration, warnings) = resolve(&root, true, &user);
        assert!(orchestration.policy.is_some(), "a trusted project policy");
        assert_eq!(orchestration.verify, ["cargo test"]);
        assert!(orchestration.project_agents.is_some());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            orchestration.startup_lines(),
            vec![
                "orchestration: verify: cargo test".to_owned(),
                "orchestration: policy: /bin/policy".to_owned(),
            ]
        );
    }

    #[test]
    fn a_project_wide_policy_that_is_relative_resolves_against_the_workspace() {
        let config =
            OrchestrationConfig::from_table(&table("policy = \"tools/policy.sh --strict\"\n"))
                .unwrap();
        let orchestration = Orchestration::resolve(&config, Path::new("/work/repo"), true);
        let policy = orchestration.policy.expect("a policy");
        assert_eq!(policy.program, Path::new("/work/repo/tools/policy.sh"));
        assert_eq!(policy.args, ["--strict"]);
        assert_eq!(policy.name, "policy.sh");
    }

    #[test]
    fn a_profile_default_that_widens_is_dropped_with_a_warning() {
        let config =
            OrchestrationConfig::from_table(&table("profile = \"full\"\nisolation = \"shared\"\n"))
                .unwrap();
        let orchestration = Orchestration::resolve(&config, Path::new("/work"), true);
        assert!(orchestration.defaults.profile.is_none(), "full is refused");
        assert!(orchestration.defaults.isolation.is_some());
        assert!(
            orchestration
                .warnings
                .iter()
                .any(|warning| warning.contains("full")),
            "{:?}",
            orchestration.warnings
        );
    }

    #[test]
    fn the_tail_truncation_keeps_the_end_and_says_so() {
        let text: String = (0..100)
            .map(|n| char::from(b'0' + (n % 10) as u8))
            .collect();
        let (tail, truncated) = tail_chars(&text, 10);
        assert!(truncated);
        assert!(tail.ends_with("0123456789"));
        assert_eq!(tail.chars().filter(|c| c.is_ascii_digit()).count(), 10);
        let (whole, truncated) = tail_chars("short", 10);
        assert!(!truncated);
        assert_eq!(whole, "short");
    }

    #[test]
    fn the_hook_names_and_payloads_are_stable() {
        assert_eq!(
            hook::ALL,
            ["task_spawned", "task_finished", "stalled", "before_merge"]
        );
        assert_eq!(
            hook::task_spawned("t1", Some("/wt"), None),
            serde_json::json!({ "task": "t1", "worktree": "/wt", "label": null })
        );
        assert_eq!(
            hook::task_finished("t1", "completed", None),
            serde_json::json!({ "task": "t1", "outcome": "completed", "error": null })
        );
        assert_eq!(
            hook::stalled("t1", 42),
            serde_json::json!({ "task": "t1", "idle_secs": 42 })
        );
        assert_eq!(
            hook::before_merge("t1", "radian/t1", Some("abc")),
            serde_json::json!({ "task": "t1", "branch": "radian/t1", "commit": "abc" })
        );
        assert_eq!(
            hook::envelope(hook::STALLED, &hook::stalled("t1", 1)),
            serde_json::json!({
                "event": { "source": "hook", "name": "stalled",
                           "data": { "task": "t1", "idle_secs": 1 } }
            })
        );
    }
}
