//! The `worktree` component (ADR 0027). It shows only when the git
//! contributor published one (ADR 0027 C2/C3).

use crate::app::App;
use crate::footer::{Item, contrib_role, contribution, role_style};
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Worktree;

impl Component for Worktree {
    fn id(&self) -> &str {
        "worktree"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["worktree"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "", "", ""]
    }

    fn priority(&self) -> i64 {
        70
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let (name, role, stale) = contribution(app, "worktree")?;
        let role = if stale {
            Role::Truncation
        } else {
            contrib_role(role)
        };
        Some(
            ItemData::new("worktree {worktree}")
                .value("worktree", name)
                .fixed_style(role_style(app, role)),
        )
    }
}
