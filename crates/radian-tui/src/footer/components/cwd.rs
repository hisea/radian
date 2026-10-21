//! The `cwd` component (ADR 0027; `{repo}`/`{worktree}`).

use crate::app::App;
use crate::footer::{Item, contribution_text};

use super::{Component, ItemData};

pub(crate) struct Cwd;

impl Component for Cwd {
    fn id(&self) -> &str {
        "cwd"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["path", "path_home", "basename", "repo", "worktree"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "", "", ""]
    }

    fn priority(&self) -> i64 {
        80
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let path = app.session.cwd.as_deref()?;
        let text = path.display().to_string();
        let home_text = std::env::var("HOME")
            .ok()
            .and_then(|home| text.strip_prefix(&home).map(|rest| format!("~{rest}")))
            .unwrap_or_else(|| text.clone());
        let base = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| text.clone());
        // ADR 0027: `{repo}` and `{worktree}` come from the git contributor
        // (background-refreshed, never a git call here). Empty when not in a repo/worktree.
        // `{repo}` falls back to the directory's own name outside a repository, so the item
        // still shows something useful.
        let repo = contribution_text(app, "git_root").unwrap_or_else(|| base.clone());
        let worktree = contribution_text(app, "worktree").unwrap_or_default();
        Some(
            ItemData::new("{path_home}")
                .value("path", text)
                .value("path_home", home_text)
                .value("basename", base)
                .value("repo", repo)
                .value("worktree", worktree),
        )
    }
}
