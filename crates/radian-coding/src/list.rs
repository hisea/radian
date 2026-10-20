use std::fs::File;

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

/// The line every `list` alias result starts with (ADR 0043 decision 3): the old tool's
/// job — listing a directory — is now `read`'s, so a call to the old name runs the same
/// listing and says where it moved.
pub(crate) const LIST_RENAME: &str = "`list` was renamed: use `read` to list a directory.";

pub(crate) fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Workspace-relative directory, or a read-only absolute or `~` \
                                directory under the cargo registry sources or a toolchain's \
                                `lib/rustlib/src`; defaults to the workspace root."
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

/// The one-version alias for the removed `list` (ADR 0043 decision 3). It stays registered
/// so Core's preflight accepts a call from a stale client, and the host strips it from every
/// model request (it is never advertised).
pub(crate) fn alias_spec() -> ToolSpec {
    ToolSpec {
        name: "list".into(),
        description: "Deprecated alias for `read` on a directory; kept for one version.".into(),
        input_schema: schema(),
        concurrency: ToolConcurrency::Shared,
    }
}

struct Entry {
    name: String,
    kind: &'static str,
    size: u64,
}

/// The entry cap the alias's own `max_entries` names: the old behaviour, unchanged.
fn alias_cap(limits: &Limits, max_entries: Option<u64>) -> Result<usize, ToolFailure> {
    match max_entries {
        Some(0) => Err(fail(
            "list_limit_exceeded",
            "max_entries must be at least 1",
        )),
        Some(n) if n as usize > limits.max_list_entries => Err(fail(
            "list_limit_exceeded",
            format!(
                "max_entries {n} exceeds the {} limit",
                limits.max_list_entries
            ),
        )),
        Some(n) => Ok(n as usize),
        None => Ok(limits.default_list_entries),
    }
}

/// The entry cap `read`'s `limit` names for a directory: the default and the hard cap are
/// the listing's, not the file read's.
fn dir_cap(limits: &Limits, limit: Option<usize>) -> Result<usize, ToolFailure> {
    match limit {
        Some(n) if n > limits.max_list_entries => Err(fail(
            "read_limit_exceeded",
            format!(
                "`limit` {n} exceeds the {} entry cap for a directory",
                limits.max_list_entries
            ),
        )),
        Some(n) => Ok(n),
        None => Ok(limits.default_list_entries),
    }
}

/// `list`, the one-version alias for `read` on a directory (ADR 0043 decision 3). The old
/// arguments run the same listing `read` runs; the result opens with the rename line.
pub(crate) async fn run_alias(
    root: &WorkspaceRoot,
    limits: &Limits,
    arguments: &serde_json::Value,
) -> Result<ToolOutput, ToolFailure> {
    let args: ListArgs = args::parse("list", &schema(), arguments)?;
    let requested = args.path.as_deref().unwrap_or(".");
    let cap = alias_cap(limits, args.max_entries)?;

    // ADR 0039 decision 2: the directory is opened through the root's directory fd (every
    // component `O_NOFOLLOW | O_DIRECTORY`) and listed through that fd; each entry's kind
    // comes from `fstatat(AT_SYMLINK_NOFOLLOW)`, so a link is shown and never followed.
    let dir = root.open_dir(requested)?;
    let mut output = render(requested, &dir, cap)?;
    for part in &mut output.content {
        if let ContentPart::Text { text, .. } = part {
            *text = format!("{LIST_RENAME}\n{text}");
            break;
        }
    }
    Ok(output)
}

/// `read` on a directory: the same listing the old `list` returned (ADR 0043 decision 1).
/// `limit` caps entries; `offset` and `max_bytes` are file arguments and are ignored, said
/// once when the caller passed them.
pub(crate) fn run_dir(
    limits: &Limits,
    user_path: &str,
    dir: &File,
    limit: Option<usize>,
    offset_given: bool,
    max_bytes_given: bool,
) -> Result<ToolOutput, ToolFailure> {
    let cap = dir_cap(limits, limit)?;
    let mut output = render(user_path, dir, cap)?;
    if offset_given || max_bytes_given {
        for part in &mut output.content {
            if let ContentPart::Text { text, .. } = part {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(
                    "[offset and max_bytes are file arguments and were ignored for this \
                     directory; `limit` caps entries]\n",
                );
                break;
            }
        }
    }
    Ok(output)
}

/// The listing itself, shared by `read` on a directory and the `list` alias: entries sorted
/// by name with kind and size, symlinks shown but never followed, past `cap` truncated with
/// a note. The text and the `structured` shape are the old `list` output verbatim.
fn render(requested: &str, dir: &File, cap: usize) -> Result<ToolOutput, ToolFailure> {
    let listed = crate::safe_fs::read_dir(dir).map_err(|e| io_failure(requested, e))?;
    let mut entries: Vec<Entry> = Vec::new();
    for entry in listed {
        // The workspace-local worktree root (the workspace-local worktree root) holds one full checkout each;
        // `read` omits the entry the same way `glob`/`grep` prune its subtree. Task agents'
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
    if truncated {
        text.push_str(&format!(
            "[truncated: showing {cap} of {total} entries; narrow it with `glob` or `grep`]\n"
        ));
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
