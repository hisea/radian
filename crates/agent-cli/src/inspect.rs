//! Session inspection (`--list`, `--export`, `--search`, `--fork`): the
//! session-management commands that never touch the model or the network (ADR 0006 §10,
//! ADR 0022, ADR 0032 slice 5).
//!
//! Split from `lib.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use std::path::PathBuf;

use radian_host::validate_session_id;
use radian_session::{EventData, Index, Session};

use super::command::ExportFormat;
use super::redact::{export_secrets, redact};
use super::{Cli, OutputError, SetupError, USAGE_ERROR};

/// [`radian_host::session_dir`] with this host's error type (ADR 0032 slice 5: the rule lives in
/// `radian-host`, so the two hosts cannot drift).
pub(crate) fn session_dir(cli: &Cli) -> Result<PathBuf, SetupError> {
    radian_host::session_dir(cli.session_dir.as_deref().map(std::path::Path::new))
        .map_err(SetupError)
}

/// The workspace a session belongs to: `--workspace` when given, else the current directory
/// (canonicalized and validated by the shared `radian-host` rule). `--list` filters by the same
/// value, so a session is only listed in the workspace it was created in.
fn session_workspace(cli: &Cli) -> Result<String, SetupError> {
    radian_host::session_workspace(cli.workspace.as_deref().map(std::path::Path::new))
        .map_err(SetupError)
}

/// Opens the workspace's session index through the shared host layer (ADR 0006 §10: the
/// `.jsonl` files are the authority, the index is a disposable cache). `None` means the
/// session directory does not exist yet.
fn session_index(cli: &Cli) -> Result<Option<(Index, String)>, SetupError> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    let index = radian_host::sessions::open_index(&dir).map_err(SetupError)?;
    Ok(index.map(|index| (index, workspace)))
}

/// `--list`: the sessions of this workspace, newest activity first (the shared table, ADR 0032
/// slice 5).
pub(crate) fn list_sessions(cli: &Cli) -> Result<(), OutputError> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    Ok(radian_host::list_sessions(&dir, &workspace)?)
}

/// The maximum number of `--search` hits printed in one run.
const SEARCH_LIMIT: usize = 200;

/// `--export`: one session of the current workspace, Markdown or JSON. It reads the session
/// file directly (no index), so a read-only command writes nothing into the session
/// directory.
pub(crate) fn export_session(cli: &Cli, session_id: &str) -> Result<(), OutputError> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    let format = match cli.format {
        ExportFormat::Markdown => radian_host::Format::Markdown,
        ExportFormat::Json => radian_host::Format::Json,
    };
    let settings = radian_host::ExportSettings {
        include_reasoning: cli.include_reasoning,
        include_provider_payload: cli.include_provider_payload,
        secrets: export_secrets(cli),
    };
    // The shared host layer validates the id (`../x` cannot name a file), checks the workspace
    // (another workspace's session is refused by name) and redacts; this CLI only decides
    // where the bytes go.
    let rendered = radian_host::sessions::export(&dir, &workspace, session_id, format, &settings)
        .map_err(SetupError)?;
    match &cli.out {
        Some(path) => {
            if path.exists() && !cli.force {
                return Err(SetupError(format!(
                    "{} already exists; pass --force to overwrite it",
                    path.display()
                ))
                .into());
            }
            std::fs::write(path, rendered)
                .map_err(|e| SetupError(format!("cannot write {}: {e}", path.display())))?;
        }
        None => radian_host::write_stdout(&rendered)?,
    }
    Ok(())
}

/// `--search`: substring search over the current workspace's message text, using the same
/// `Index::search` the `--list` cache is built on. The index lives beside the sessions in
/// `index.db`; it is a disposable cache, never a session file (ADR 0006 §10).
pub(crate) fn search_sessions(cli: &Cli, query: &str) -> Result<(), OutputError> {
    let Some((index, workspace)) = session_index(cli)? else {
        eprintln!("no match");
        return Ok(());
    };
    let hits = radian_host::sessions::search(&index, &workspace, query, SEARCH_LIMIT)
        .map_err(SetupError)?;
    if hits.is_empty() {
        eprintln!("no match");
        return Ok(());
    }
    let secrets = export_secrets(cli);
    for hit in hits {
        // The snippet is session content and can carry a model-echoed key or header value;
        // redact the known secrets exactly like `--export` (ADR 0013 T3).
        let snippet = redact(&hit.snippet, &secrets);
        radian_host::write_line(&format!(
            "{}\t{}\t{}",
            hit.session_id,
            hit.last_ts.as_deref().unwrap_or("-"),
            snippet
        ))?;
    }
    Ok(())
}

/// Like the provider's own rule: localhost, 127.0.0.1 and ::1 may go without a key.
/// `--fork <parent>` (ADR 0022): create a child session from the parent's effective history
/// at a turn boundary. No model, no key, no network. A parent another process is writing exits
/// 1 rather than waiting; other failures are usage errors.
pub(crate) async fn fork_session(cli: &Cli, parent_id: &str) -> Result<(), (String, i32)> {
    let usage = |message: String| (message, USAGE_ERROR);
    validate_session_id(parent_id).map_err(usage)?;
    let dir = session_dir(cli).map_err(|e| (e.0, USAGE_ERROR))?;
    let parent_path = dir.join(format!("{parent_id}.jsonl"));
    if !parent_path.exists() {
        return Err(usage(format!(
            "no session {parent_id:?} in {}",
            dir.display()
        )));
    }
    let child_id = cli
        .session_id
        .clone()
        .unwrap_or_else(radian_host::generate_session_id);
    validate_session_id(&child_id).map_err(usage)?;
    let child_path = dir.join(format!("{child_id}.jsonl"));
    if child_path.exists() {
        return Err(usage(format!(
            "session {child_id:?} already exists; pass --session-id with a new id"
        )));
    }
    let parent = Session::open(&parent_path)
        .map_err(|e| usage(format!("cannot read session {parent_id:?}: {e}")))?;
    // ADR 0022 §5: the child shares the parent's workspace and inherits its config snapshot.
    let created = created_data(&parent);
    let workspace = created
        .and_then(|data| data["workspace"].as_str().map(str::to_owned))
        .ok_or_else(|| usage(format!("session {parent_id:?} has no workspace")))?;
    let config_snapshot = created
        .and_then(|data| data.get("config_snapshot").cloned())
        .unwrap_or_else(|| serde_json::json!({}));
    let request = radian_session::ForkRequest {
        parent_path,
        parent_id: parent_id.to_owned(),
        seq: cli.at,
        child_path,
        child_id: child_id.clone(),
        workspace,
        config_snapshot,
    };
    match radian_session::fork(request).await {
        Ok(outcome) => {
            println!(
                "forked {parent_id} at seq {} into {}",
                outcome.seq, outcome.child_id
            );
            println!("continue it with `--resume {}`", outcome.child_id);
            Ok(())
        }
        // The parent is being written by another process: say so and exit 1, do not wait.
        Err(radian_session::ForkError::Parent {
            source: radian_session::WriteError::Locked { .. },
            ..
        }) => Err((
            format!("session {parent_id:?} is being written by another process; stop it first"),
            1,
        )),
        Err(error) => Err(usage(error.to_string())),
    }
}

/// The parsed `SessionCreated` data of a session, so a fork can inherit the parent's
/// workspace and model-route reference.
fn created_data(session: &Session) -> Option<&serde_json::Value> {
    session.events().iter().find_map(|event| match &event.data {
        EventData::Other(data) if event.event == "SessionCreated" => Some(data),
        _ => None,
    })
}
