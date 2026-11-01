//! The memory aging hook (ADR 0021 D3/D4): a model `read` of a memory record file sets its
//! frontmatter `last_used` to the time of the read. Nothing else touches it, and a failure to
//! write it is a warning, never a tool error.
//!
//! The hook lives on the combined tool provider's result path ([`crate::tools::HostSession`]):
//! that is the one place the host sees a settled `read` call together with the path the model
//! asked for. A user command (`list`, `show`, `stale`) goes through `MemoryService` and never
//! passes here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use radian_core::ToolCall;
use radian_memory::{FileStore, Scope, now_utc};

use super::service::MemoryService;

/// The memory-root read hook. It knows the two scope directories the file tools may reach
/// (ADR 0039 decision 4) and the store that owns them.
#[derive(Clone)]
pub struct MemoryTouch {
    files: FileStore,
    workspace: PathBuf,
    /// Canonical scope directories: `global`, then this workspace's project when linked.
    roots: Vec<(Scope, PathBuf)>,
    warn: Arc<dyn Fn(&str) + Send + Sync>,
}

impl std::fmt::Debug for MemoryTouch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTouch")
            .field("roots", &self.roots)
            .finish_non_exhaustive()
    }
}

impl MemoryTouch {
    /// Builds the hook for this session. The scope directories are the same ones the coding
    /// tools receive, so a read the tools allow is a read this hook can account for.
    pub fn new(service: &MemoryService, warn: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        let files = service.files();
        let roots = service
            .file_scope_roots()
            .into_iter()
            .map(|(scope, dir)| {
                let canonical = std::fs::canonicalize(&dir).unwrap_or(dir);
                (scope, canonical)
            })
            .collect();
        Self {
            files,
            workspace: service.workspace().to_path_buf(),
            roots,
            warn,
        }
    }

    /// Called once per tool call, after the tool ran. Only a successful `read` of a record
    /// file directly inside a scope directory does anything; `MEMORY.md`, a file outside the
    /// memory root, any other tool and any failure are ignored. A write failure is reported
    /// through the warning sink and never changes the tool result.
    pub fn after_tool(&self, call: &ToolCall, succeeded: bool) {
        if !succeeded || call.name != "read" {
            return;
        }
        let Some(path) = call
            .arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        self.touch_read(path);
    }

    fn touch_read(&self, path: &str) {
        let Some(real) = self.resolve(path) else {
            return;
        };
        // A record is a direct child of its scope directory: `<scope>/<name>.md`. A nested
        // path is not a record, and treating its stem as a name could touch a different file.
        let Some(parent) = real.parent() else {
            return;
        };
        let Some((scope, _)) = self.roots.iter().find(|(_, dir)| dir.as_path() == parent) else {
            return;
        };
        let Some(file_name) = real.file_name().and_then(|name| name.to_str()) else {
            return;
        };
        let Some(stem) = file_name.strip_suffix(".md") else {
            return;
        };
        // The index is not a record (ADR 0021 D3): reading it never changes it.
        if stem.is_empty() || stem == "MEMORY" {
            return;
        }
        if let Err(error) = self.files.touch_last_used(scope, stem, &now_utc()) {
            (self.warn)(&format!(
                "could not record last_used for {}: {error}",
                real.display()
            ));
        }
    }

    /// The path as the read tool resolved it: `~` expanded, a relative path against the
    /// workspace, then canonicalized so a scope directory compares equal in every spelling.
    fn resolve(&self, path: &str) -> Option<PathBuf> {
        let expanded = if path == "~" {
            crate::home::user_home()?
        } else if let Some(rest) = path.strip_prefix("~/") {
            crate::home::user_home()?.join(rest)
        } else {
            Path::new(path).to_path_buf()
        };
        let joined = if expanded.is_absolute() {
            expanded
        } else {
            self.workspace.join(expanded)
        };
        std::fs::canonicalize(joined).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn touch(label: &str) -> (crate::test_support::TempDir, MemoryService, MemoryTouch) {
        let dir = crate::test_support::TempDir::with_label("radian-memory-touch", label);
        let workspace = dir.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let service = MemoryService::open(&workspace, dir.join("memory"));
        service.create_project().unwrap();
        let touch = MemoryTouch::new(&service, Arc::new(|_: &str| {}));
        (dir, service, touch)
    }

    fn record_text(name: &str, last_used: Option<&str>) -> String {
        let last = last_used
            .map(|value| format!("last_used: {value}\n"))
            .unwrap_or_default();
        format!(
            "---\nname: {name}\ndescription: a note\ntype: project\nsource: model\n\
             created: 2026-01-01T00:00:00.000000Z\nmodified: 2026-01-01T00:00:00.000000Z\n\
             {last}---\nthe body\n"
        )
    }

    fn read_call(path: &Path) -> ToolCall {
        ToolCall {
            id: "r1".into(),
            name: "read".into(),
            arguments: json!({"path": path.to_str().unwrap()}),
        }
    }

    /// A successful read of a record directly in the project scope sets `last_used` and leaves
    /// every other line alone.
    #[test]
    fn a_read_of_a_record_sets_last_used_and_changes_no_other_line() {
        let (dir, service, touch) = touch("record");
        let project = service.project_id().unwrap();
        let path = service
            .files()
            .record_path(&Scope::Project(project), "note");
        std::fs::write(
            &path,
            record_text("note", Some("2020-01-01T00:00:00.000000Z")),
        )
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        touch.after_tool(&read_call(&path), true);

        let after = std::fs::read_to_string(&path).unwrap();
        let old = "last_used: 2020-01-01T00:00:00.000000Z";
        let new_line = after
            .lines()
            .find(|line| line.starts_with("last_used:"))
            .unwrap();
        assert_ne!(new_line, old, "last_used moved: {after}");
        let replaced = after.replace(new_line, old);
        assert_eq!(replaced, before, "only the last_used line changed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Edge: the index is not a record.
    #[test]
    fn a_read_of_memory_md_touches_nothing() {
        let (dir, service, touch) = touch("index");
        let index = service.files().index_path(&Scope::Global);
        std::fs::write(&index, "# the index\n").unwrap();
        let before = std::fs::read_to_string(&index).unwrap();

        touch.after_tool(&read_call(&index), true);

        assert_eq!(std::fs::read_to_string(&index).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Edge: a read outside the memory root touches nothing.
    #[test]
    fn a_read_outside_the_memory_root_touches_nothing() {
        let (dir, _service, touch) = touch("outside");
        let outside = dir.join("ws").join("notes.md");
        std::fs::write(&outside, record_text("notes", None)).unwrap();
        let before = std::fs::read_to_string(&outside).unwrap();

        touch.after_tool(&read_call(&outside), true);

        assert_eq!(std::fs::read_to_string(&outside).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Edge: a failed read is not a use.
    #[test]
    fn a_failed_read_touches_nothing() {
        let (dir, service, touch) = touch("failed");
        let project = service.project_id().unwrap();
        let path = service
            .files()
            .record_path(&Scope::Project(project), "note");
        std::fs::write(&path, record_text("note", None)).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        touch.after_tool(&read_call(&path), false);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A broken record is a warning, not a tool error, and the read result is untouched.
    #[test]
    fn a_touch_failure_is_a_warning() {
        let (dir, service, _) = touch("warn");
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&warnings);
        let touch = MemoryTouch::new(
            &service,
            Arc::new(move |message: &str| sink.lock().unwrap().push(message.to_owned())),
        );
        let project = service.project_id().unwrap();
        let path = service
            .files()
            .record_path(&Scope::Project(project), "broken");
        std::fs::write(&path, "---\nname: broken\n---\nthe body\n").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        touch.after_tool(&read_call(&path), true);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let warnings = warnings.lock().unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("could not record last_used"),
            "{warnings:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
