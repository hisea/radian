//! Session support (ADR 0022, ADR 0025 S6, ADR 0039 decision 1): the hook chain
//! ([`session_hooks`]), the journal open, the coding-tool build, the memory notice, and
//! the resume readers ([`resumed_model_route`], [`resumed_access`]).
//!
//! Split from `session/mod.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_core::Hooks;
use radian_coding::CodingTools;
use radian_permissions::{PermissionHooks, Preset};
#[cfg(test)]
use radian_sandbox::Sandbox;
use radian_sandbox::SandboxSlot;
use radian_session::{ForkParent, SessionCreatedData, SessionJournal};

use super::facade::SessionError;
use super::ids::SessionId;
use crate::memory::{InjectionConfig, MemoryHooks, MemoryService, ProjectState};
use crate::skills::SkillsHooks;
use crate::trust;

/// The agent's hook chain (ADR 0025 S6, ADR 0045): the role hook writes layers 0a, 1
/// and 2 first, then the permission rules run, then the user-level command hooks (which can
/// only narrow what permissions already allowed), then the memory section is appended, then
/// the skills section. The order is a promise — the role layer comes first, and the sections
/// must be deterministic with memory before skills — so it lives in this one function, which
/// `open_session` calls; `tests/skills_order.rs` pins it and `tests/command_hooks.rs` pins the
/// command hooks after permissions. Swapping `memory` and `skills` here is the mutation that
/// `skills_order` catches; moving `commands` before `permissions` is the one `command_hooks`
/// catches.
pub fn session_hooks(
    role: Arc<crate::prompt::RoleHooks>,
    permissions: Arc<PermissionHooks>,
    commands: Arc<crate::CommandHooks>,
    memory: Arc<MemoryHooks>,
    skills: Arc<SkillsHooks>,
    facts: Arc<crate::prompt::SessionFactsHooks>,
) -> impl Hooks {
    // The order is the promise: the role layers, permissions, commands, memory, skills, then
    // the volatile <session> facts (ADR 0025 S6, ADR 0036 D1, ADR 0045).
    (((((role, permissions), commands), memory), skills), facts)
}

/// The durable audit event for one trust write (ADR 0031 T2).
pub(super) fn trust_event(change: &trust::ProjectTrustChanged) -> radian_session::RuntimeEvent {
    radian_session::RuntimeEvent::trust_changed(
        change.path.to_string_lossy().into_owned(),
        change.decision.as_bool(),
    )
}

/// Creates or resumes the session file. The caller has already validated everything that can
/// fail, so this only reports the file's own state. `parent` is the derived-session lineage
/// (ADR 0022's fork or ADR 0043's subagent): `Some` writes `SessionCreated.parent` with
/// its `relation`, `None` writes an ordinary session.
pub(super) fn open_journal(
    session_id: &SessionId,
    dir: &Path,
    workspace: &Path,
    config_snapshot: &serde_json::Value,
    parent: Option<&ForkParent>,
    path: &Path,
) -> Result<Arc<SessionJournal>, SessionError> {
    crate::ensure_private_dir(dir).map_err(|error| {
        SessionError::Message(format!("cannot create {}: {error}", dir.display()))
    })?;
    // The snapshot is a model route reference only: never the key, a header value or a base
    // URL that could carry credentials (ADR 0008 §4, ADR 0013).
    let created = match parent {
        Some(parent) => SessionCreatedData {
            workspace: workspace.to_string_lossy().into_owned(),
            config_snapshot: config_snapshot.clone(),
            parent: Some(parent.clone()),
        },
        None => SessionCreatedData::new(workspace.to_string_lossy(), config_snapshot.clone()),
    };
    match SessionJournal::create(path, session_id.as_str(), created) {
        Ok(journal) => Ok(Arc::new(journal)),
        Err(radian_session::WriteError::NotEmpty { .. }) => Err(SessionError::Message(format!(
            "session {:?} already exists in {}; continue it with `--resume {}`",
            session_id.as_str(),
            dir.display(),
            session_id
        ))),
        Err(error) => Err(SessionError::Message(format!(
            "cannot create session {:?}: {error}",
            session_id.as_str()
        ))),
    }
}

pub(super) fn build_coding_tools(
    workspace: Option<&Path>,
    sandbox: SandboxSlot,
    jobs_dir: &Path,
    shell_auto_background: Option<Duration>,
) -> Result<(Option<CodingTools>, Option<Arc<radian_coding::Jobs>>), SessionError> {
    match workspace {
        Some(dir) => {
            // The slot decides whether `shell` exists: an empty slot means no shell
            // advertised, a non-empty one is a sandbox the shell can run under. The
            // tool set is fixed here at `open_session`; a later switch to a *narrower* tier
            // still denies `shell` through the permission rules (ADR 0038 slice 2).
            let tools = if sandbox.is_some() {
                CodingTools::with_sandbox_slot(dir, sandbox).map_err(|error| {
                    SessionError::Message(format!("--workspace {}: {error}", dir.display()))
                })?
            } else {
                CodingTools::new(dir).map_err(|error| {
                    SessionError::Message(format!("--workspace {}: {error}", dir.display()))
                })?
            };
            // ADR 0037: the adoption threshold comes from `[shell]`; the host
            // resolved the default already, so `None` here means off.
            let tools = tools.with_limits(radian_coding::Limits {
                auto_background_after: shell_auto_background,
                ..radian_coding::Limits::default()
            });
            // ADR 0039 decision 1: the jobs directory is the **session's own** directory, in
            // the sessions dir the sandbox already denies reading. It is created 0700 with the
            // same helper the sessions dir uses, before the registry can write anything, and
            // passed in — the registry never derives a path from the workspace.
            crate::ensure_private_dir(jobs_dir).map_err(|error| {
                SessionError::Message(format!(
                    "cannot create the jobs directory {}: {error}",
                    jobs_dir.display()
                ))
            })?;
            // The host creates the registry so it can subscribe to job lifecycle notices and
            // write the durable start/settle records (ADR 0037).
            let jobs = Arc::new(radian_coding::Jobs::new(jobs_dir));
            Ok((Some(tools.with_jobs(Arc::clone(&jobs))), Some(jobs)))
        }
        None => Ok((None, None)),
    }
}

pub(super) fn memory_notice(memory: &Option<MemoryService>, config: &InjectionConfig) -> String {
    match memory {
        Some(service) => match service.state() {
            Ok(ProjectState::Known(project)) => format!(
                "[memory] project {project}; injection on, up to {} tokens",
                config.max_tokens
            ),
            Ok(ProjectState::Unlinked { .. }) => format!(
                "[memory] no project linked; global memory only (run `radian memory project new` \
                 or `radian memory relink <ProjectId>`); injection on, up to {} tokens",
                config.max_tokens
            ),
            Err(error) => format!("[memory] unavailable: {error}"),
        },
        None => "[memory] disabled (no RADIAN_HOME or HOME)".to_owned(),
    }
}

/// The model route a **resume** would restore (ADR 0026 D8): the journal's last model
/// `ConfigChanged`. `None` when the file has none — the caller then falls back to the config
/// and, failing that, requires `--model`. Reads the session file line by line; it is the one
/// source of truth, and this runs before anything durable is created.
pub fn resumed_model_route(
    session_dir: &Path,
    session_id: &str,
) -> Option<radian_runtime::ModelRoute> {
    let text = std::fs::read_to_string(session_dir.join(format!("{session_id}.jsonl"))).ok()?;
    let mut route = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value["event"] == "ConfigChanged"
            && let Some(found) = radian_runtime::ModelRoute::from_value(&value["data"]["change"])
        {
            route = Some(found);
        }
    }
    route
}

/// The permission tier a **resume** would restore (ADR 0038 decision 2): the journal's last
/// `ConfigChanged` whose change carries an `access` key. `None` when the file has none — the
/// caller then falls back to the remembered per-project tier, the config and the default.
/// Reads the session file line by line; this runs before anything durable is created.
pub fn resumed_access(session_dir: &Path, session_id: &str) -> Option<Preset> {
    let text = std::fs::read_to_string(session_dir.join(format!("{session_id}.jsonl"))).ok()?;
    let mut tier = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value["event"] == "ConfigChanged"
            && let Some(name) = value["data"]["change"]["access"].as_str()
            && let Some(found) = crate::access::parse_tier(name)
        {
            tier = Some(found);
        }
    }
    tier
}

#[cfg(test)]
mod tests {
    use super::*;
    use radian_sandbox::{SandboxMode, SandboxPolicy};

    fn full_access(ws: &Path) -> Sandbox {
        let policy = SandboxPolicy::new(SandboxMode::FullAccess, ws).expect("full-access policy");
        Sandbox::from_policy(policy).expect("full-access needs no backend")
    }

    /// ADR 0039 decision 1: the host builds the registry over the **session's own** jobs
    /// directory (0700), never over `<workspace>/.radian/jobs`, and the output file lands
    /// there at 0600.
    #[cfg(unix)]
    #[test]
    fn the_jobs_directory_is_the_sessions_own_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("radian-host-jobs-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let jobs_dir = root.join("sessions/sid.jobs");

        let slot = SandboxSlot::from_option(Some(full_access(&ws)));
        let (_coding, jobs) = build_coding_tools(Some(&ws), slot, &jobs_dir, None).expect("tools");
        let jobs = jobs.expect("a registry");

        assert_eq!(
            jobs.dir(),
            jobs_dir,
            "the registry uses the session jobs directory"
        );
        assert_eq!(
            std::fs::metadata(&jobs_dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "the jobs directory is 0700"
        );

        let info = jobs
            .start(&full_access(&ws), &ws, "echo hi")
            .expect("start the job");
        let output = jobs.output_path(&info.id);
        assert!(output.starts_with(&jobs_dir), "{}", output.display());
        assert_eq!(
            std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o600,
            "the output file is 0600"
        );
        assert!(
            !ws.join(".radian").join("jobs").exists(),
            "the workspace must not grow a jobs directory"
        );

        jobs.kill_all();
        let _ = std::fs::remove_dir_all(&root);
    }
}
