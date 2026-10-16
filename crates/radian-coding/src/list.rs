use agent_core::{ContentPart, ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::args;
use crate::fail;
use crate::provider::Limits;
use crate::workspace::{WorkspaceRoot, io_failure};

#[derive(Deserialize)]
struct ListArgs {
    path: Option<String>,
    max_entries: Option<u64>,
}

pub(crate) fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Workspace-relative directory; defaults to the workspace root."
            },
            "max_entries": {
                "type": "integer",
                "minimum": 1,
                "maximum": 5000,
                "description": "Maximum entries to return; defaults to 500."
            }
        },
        "additionalProperties": false
    })
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "list".into(),
        description: "List a workspace directory. Entries are sorted by name and reported \
                      with their kind (`file`, `dir`, `symlink`, `other`) and size; symlinks \
                      are shown, never followed. Truncated past max_entries."
            .into(),
        input_schema: schema(),
        concurrency: ToolConcurrency::Shared,
    }
}

struct Entry {
    name: String,
    kind: &'static str,
    size: u64,
}

pub(crate) async fn run(
    root: &WorkspaceRoot,
    limits: &Limits,
    arguments: &serde_json::Value,
) -> Result<ToolOutput, ToolFailure> {
    let args: ListArgs = args::parse("list", &schema(), arguments)?;
    let requested = args.path.as_deref().unwrap_or(".");
    let cap = match args.max_entries {
        Some(0) => {
            return Err(fail(
                "list_limit_exceeded",
                "max_entries must be at least 1",
            ));
        }
        Some(n) if n as usize > limits.max_list_entries => {
            return Err(fail(
                "list_limit_exceeded",
                format!(
                    "max_entries {n} exceeds the {} limit",
                    limits.max_list_entries
                ),
            ));
        }
        Some(n) => n as usize,
        None => limits.default_list_entries,
    };

    // ADR 0039 decision 2: the directory is opened through the root's directory fd (every
    // component `O_NOFOLLOW | O_DIRECTORY`) and listed through that fd; each entry's kind
    // comes from `fstatat(AT_SYMLINK_NOFOLLOW)`, so a link is shown and never followed.
    let dir = root.open_dir(requested)?;
    let listed = crate::safe_fs::read_dir(&dir).map_err(|e| io_failure(requested, e))?;
    let mut entries: Vec<Entry> = Vec::new();
    for entry in listed {
        // The workspace-local worktree root (ADR 0030 O7) holds one full checkout each;
        // `list` omits the entry the same way `glob`/`grep` prune its subtree. Task agents'
        // worktrees are outside the workspace (ADR 0039 decision 4).
        if entry.name == std::ffi::OsStr::new(crate::search::RADIAN_WORKTREES) {
            continue;
        }
        let kind = match entry.kind {
            crate::safe_fs::EntryKind::Symlink => "symlink",
            crate::safe_fs::EntryKind::Dir => "dir",
            crate::safe_fs::EntryKind::File => "file",
            crate::safe_fs::EntryKind::Other => "other",
        };
        entries.push(Entry {
            name: entry.name.to_string_lossy().into_owned(),
            kind,
            size: entry.size,
        });
    }
    // `read_dir` sorts by name already; keep the explicit sort so the order is a contract here.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let total = entries.len();
    let truncated = total > cap;
    entries.truncate(cap);

    let mut text = String::new();
    for entry in &entries {
        text.push_str(&format!("{}\t{}\t{}\n", entry.kind, entry.size, entry.name));
    }

    Ok(ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(json!({
            "path": requested,
            "entries": entries.len(),
            "total_entries": total,
            "truncated": truncated,
        })),
    })
}
