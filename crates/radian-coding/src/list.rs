use std::fs::File;

use radian_core::{ContentPart, ToolFailure, ToolOutput};
use serde_json::json;

use crate::fail;
use crate::provider::Limits;
use crate::workspace::io_failure;

struct Entry {
    name: String,
    kind: &'static str,
    size: u64,
}

/// `read` on a directory: lists it (ADR 0043 decision 1).
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

/// `read` on a directory: the same listing `read` runs; `limit` caps entries.
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

/// The listing itself, used by `read` on a directory: entries sorted
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
