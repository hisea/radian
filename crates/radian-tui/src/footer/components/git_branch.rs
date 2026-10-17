//! The `git_branch` component (ADR 0027; the change counts are
//! revision 1). Its data is the background contributor's (ADR 0027 C3), with the host's one-shot
//! branch as the fallback when no contributor is running.

use radian_host::SegmentRole;

use crate::app::App;
use crate::footer::{Item, contrib_role, contribution, contribution_text, role_style};

use super::{Component, ItemData};

pub(crate) struct GitBranch;

impl Component for GitBranch {
    fn id(&self) -> &str {
        "git_branch"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["branch", "dirty", "modified", "untracked"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "\u{e0a0} ", "", ""]
    }

    fn priority(&self) -> i64 {
        70
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let (branch, role, stale) = match contribution(app, "git_branch") {
            Some(contribution) => contribution,
            // The host's one-shot value, when no contributor is running.
            None => (app.session.git_branch.clone()?, SegmentRole::Normal, false),
        };
        // ADR 0027: the change counts from the same background refresh.
        let modified =
            contribution_text(app, "git_modified").and_then(|text| text.parse::<u64>().ok());
        let untracked =
            contribution_text(app, "git_untracked").and_then(|text| text.parse::<u64>().ok());
        let dirty = match (modified.unwrap_or(0), untracked.unwrap_or(0)) {
            (0, 0) => String::new(),
            (modified, untracked) => format!(" ?{}", modified + untracked),
        };
        let role = if stale {
            crate::theme::Role::Truncation
        } else {
            contrib_role(role)
        };
        Some(
            ItemData::new("{branch}")
                .value("branch", branch)
                .value("dirty", dirty)
                .value(
                    "modified",
                    modified.map_or_else(String::new, |count| count.to_string()),
                )
                .value(
                    "untracked",
                    untracked.map_or_else(String::new, |count| count.to_string()),
                )
                .fixed_style(role_style(app, role)),
        )
    }
}
