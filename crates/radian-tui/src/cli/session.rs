//! The session's own plumbing: the host facts `/sessions` and `/export` need ([`SessionHost`]),
//! the session commands' handler, the picker's load, the export writer, the session/workspace
//! path helpers, and [`JournalAudit`], which records every permission decision and policy switch
//! into the journal without blocking the hook.

use super::*;
use radian_permissions::{PermissionResolved, runtime_event};

/// The host facts `/sessions` and `/export` need: where sessions live, which workspace this
/// process belongs to, the current session id and the values to redact.
pub(super) struct SessionHost {
    pub(super) dir: PathBuf,
    pub(super) workspace: String,
    pub(super) session_id: String,
    /// Live redaction list (shared with the panic hook); snapshot it for a renderer.
    pub(super) secrets: radian_tui::terminal::SharedSecrets,
    /// The parent's writer, so `/fork` records the lineage through the same single-writer lock
    /// instead of opening a second one (ADR 0022 §1).
    pub(super) journal: Arc<SessionJournal>,
    /// The child's `SessionCreated.config_snapshot` (a model-route reference).
    pub(super) config_snapshot: serde_json::Value,
    /// `[tui.colors]` overrides, so a `/theme` switch keeps them (ADR 0029).
    pub(super) theme_colors: Vec<(String, radian_host::config::TuiColorOverride)>,
    /// Where `/theme` writes `tui.theme` back: the file this process read, or the path it
    /// would read (ADR 0008). `None` when there is no config location at all.
    pub(super) theme_write_path: Option<PathBuf>,
    /// The reader's read-only verdict (a newer `schema_version`): `/theme` applies in the
    /// session but never writes.
    pub(super) config_read_only: bool,
    /// True when the session started with no model configured (ADR 0008 §2): the first
    /// `/model` writes `[model] provider`/`model` back to the config file. A session that
    /// already had a configured model never writes (ADR 0026 D5).
    pub(super) started_without_model: bool,
}

/// Runs one `/sessions`, `/export` or `/fork` request and returns the notices to show. Pure
/// from `App`'s point of view: the app turns these lines into entries (ADR 0023 U3).
pub(super) fn handle_session_request(
    host: &SessionHost,
    runtime: &tokio::runtime::Runtime,
    request: radian_tui::command::SessionOutcome,
) -> Vec<String> {
    match request {
        radian_tui::command::SessionOutcome::Export(request) => {
            match export_current(host, &request) {
                Ok(path) => vec![format!("exported to {path}")],
                Err(message) => vec![message],
            }
        }
        radian_tui::command::SessionOutcome::Fork => {
            // ADR 0022 §1: `update` only asks for a fork when the session is idle, so there is
            // no state check here.
            let child_id = radian_host::generate_session_id();
            let child_path = host.dir.join(format!("{child_id}.jsonl"));
            let request = radian_session::ForkRequest {
                parent_path: host.journal.path().to_path_buf(),
                parent_id: host.session_id.clone(),
                seq: None,
                child_path,
                child_id: child_id.clone(),
                workspace: host.workspace.clone(),
                config_snapshot: host.config_snapshot.clone(),
            };
            match runtime.block_on(radian_session::fork_with_journal(request, &host.journal)) {
                Ok(outcome) => vec![format!(
                    "forked at seq {} into {}; open it with `radian --resume {}`                      (this window keeps the parent)",
                    outcome.seq, outcome.child_id, outcome.child_id
                )],
                Err(error) => vec![format!("fork failed: {error}")],
            }
        }
    }
}

/// Loads the data the picker overlay shows: the workspace's session rows and its index. A
/// missing session directory is one notice instead of an empty overlay (dogfooding D2 style).
pub(super) fn load_picker(host: &SessionHost, query: Option<String>) -> AppInput {
    match radian_host::sessions::open_index(&host.dir) {
        Ok(Some(index)) => match radian_host::sessions::list(&index, &host.workspace) {
            Ok(rows) => AppInput::PickerOpened(PickerData {
                rows,
                index: Some(index),
                workspace: host.workspace.clone(),
                query,
                secrets: radian_tui::terminal::shared_secrets(&host.secrets),
            }),
            Err(message) => AppInput::SessionDone(vec![message]),
        },
        Ok(None) => AppInput::SessionDone(vec!["no sessions for this workspace yet".to_owned()]),
        Err(message) => AppInput::SessionDone(vec![message]),
    }
}

/// `/export`: render the current session with the shared host layer, check overwrite, and
/// write atomically. Returns the absolute path for the notice.
fn export_current(
    host: &SessionHost,
    request: &radian_tui::command::ExportRequest,
) -> Result<String, String> {
    let extension = request.format.extension();
    let path = match &request.path {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                PathBuf::from(&host.workspace).join(path)
            }
        }
        None => {
            PathBuf::from(&host.workspace).join(format!("radian-{}.{extension}", host.session_id))
        }
    };
    if path.exists() && !request.force {
        return Err(format!(
            "{} already exists; `/export {} {} --force` overwrites it",
            path.display(),
            match request.format {
                radian_host::Format::Markdown => "md",
                radian_host::Format::Json => "json",
            },
            path.display()
        ));
    }
    let settings = radian_host::ExportSettings {
        include_reasoning: false,
        include_provider_payload: false,
        secrets: radian_tui::terminal::shared_secrets(&host.secrets),
    };
    let rendered = radian_host::sessions::export(
        &host.dir,
        &host.workspace,
        &host.session_id,
        request.format,
        &settings,
    )?;
    write_atomic(&path, &rendered)?;
    Ok(std::fs::canonicalize(&path)
        .unwrap_or(path)
        .display()
        .to_string())
}

/// Writes `contents` to `path` atomically: a sibling temp file, then a rename. 0644 (the file
/// is for the user; it is not a session file).
fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    static EXPORT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let seq = EXPORT_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let tmp = parent.join(format!(".radian-export-{}-{seq}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o644);
        }
        let mut file = options.open(&tmp)?;
        use std::io::Write;
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    };
    if let Err(error) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {error}", path.display()));
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot replace {}: {error}", path.display()));
    }
    Ok(())
}

/// Where session files live: `--session-dir`, then `RADIAN_SESSION_DIR`, then the user's data
/// directory (ADR 0020: `~/.radian/sessions`). The rule lives in `radian-host` so a session
/// written by one host is resumable by the other (ADR 0032 slice 5).
pub(super) fn session_dir(cli: &Cli) -> Result<PathBuf, String> {
    radian_host::session_dir(cli.session_dir.as_deref())
}

/// The workspace a session belongs to: `--workspace` when given, else the current directory,
/// through the TUI's coding root (`$HOME` becomes `$HOME/tmp`, the filesystem root is refused)
/// and then the shared `radian-host` rule. `--list` filters by the same value, so a session is
/// only listed in the workspace it was created in, and a bad `--workspace` fails as a workspace
/// error (dogfooding D1).
fn session_workspace(cli: &Cli) -> Result<String, String> {
    radian_host::session_workspace(Some(&coding_root(cli.workspace.as_deref())?.path))
}

/// The coding root: `--workspace` as given, else the current directory, with the TUI's two
/// exceptions — `$HOME` becomes `$HOME/tmp` and the filesystem root
/// is refused. The CLI (`radian exec`) does **not** do this: it is the unattended entry point and
/// gets no write access by default. `workspace` is `None` for a caller that has no flag to pass
/// (`attach`).
pub(super) fn coding_root(
    workspace: Option<&Path>,
) -> Result<radian_tui::workspace::CodingRoot, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("cannot read the current directory: {error}"))?;
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    radian_tui::workspace::resolve_coding_root(workspace, &cwd, home.as_deref())
}

/// `--list`: this workspace's sessions, newest activity first (the shared table, ADR 0032
/// slice 5).
pub(super) fn list_sessions(cli: &Cli) -> Result<(), radian_host::ListError> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    radian_host::list_sessions(&dir, &workspace)
}

/// The Phase 4 startup picker: opens the workspace index, lists the sessions, and (when there
/// are any) runs the full-screen picker. `Ok(None)` means there are no sessions yet, so the
/// caller starts a new one with a notice instead of flashing an empty list.
pub(super) fn prepare_picker(
    dir: &Path,
    workspace: &str,
) -> Result<Option<radian_tui::picker::Picker>, String> {
    let index = radian_host::sessions::open_index(dir)?;
    let rows = match &index {
        Some(index) => radian_host::sessions::list(index, workspace)?,
        None => Vec::new(),
    };
    if rows.is_empty() {
        return Ok(None);
    }
    Ok(Some(radian_tui::picker::Picker::new(
        rows, index, workspace,
    )))
}

/// Writes every decision to the session journal as a `PermissionResolved` (ADR 0006 §5.9).
/// The append is spawned: `AuditSink::record` is synchronous and runs inside the hook, so it
/// must not block. The record is audit only (`critical = false`) and is never replayed.
/// A policy switch (ADR 0026 D9) is journaled the same way, as `ApprovalPolicyChanged`.
pub(super) struct JournalAudit {
    pub(super) journal: Arc<SessionJournal>,
    pub(super) runtime: tokio::runtime::Handle,
}

impl AuditSink for JournalAudit {
    fn record(&self, event: &PermissionResolved) {
        let event = runtime_event(event);
        let journal = Arc::clone(&self.journal);
        self.runtime.spawn(async move {
            let _ = journal.append_runtime_event(event).await;
        });
    }

    fn record_policy_change(&self, policy: radian_permissions::ApprovalPolicy) {
        let event = radian_permissions::policy_change_event(policy);
        let journal = Arc::clone(&self.journal);
        self.runtime.spawn(async move {
            let _ = journal.append_runtime_event(event).await;
        });
    }
}
