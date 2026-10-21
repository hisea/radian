//! The footer's component registry (ADR 0027).
//!
//! Each built-in footer item is one [`Component`] with its own placeholders, default glyph in
//! each of the four glyph sets, default role, priority and thresholds, and a `data(&App, &Item)`
//! that reads the session's facts. The renderer dispatches **only** through this registry: it
//! no longer matches on item-id strings, so `git_branch`, `worktree`, `status` and any id a
//! contributor publishes are ordinary components. The trait is an internal detail — the only
//! extension point is decision 4's `command` component (a later slice); there is no in-process
//! plugin API.

use ratatui::style::Style;

use crate::app::App;
use crate::theme::Role;

use super::{Glyphs, Item, ItemStyle};

mod brand;
mod context;
mod cost;
mod cwd;
mod elapsed;
mod git_branch;
mod host;
mod jobs;
#[path = "loop.rs"]
mod loop_item;
mod mcp;
mod model;
mod pending;
mod queued;
mod reasoning;
mod scroll;
mod session;
mod speed;
mod status;
mod tier;
mod tokens;
mod usage;
mod worktree;

/// The facts one component reads at draw time. `values` are the `{placeholder}` values, `numeric`
/// is what the thresholds compare against (the context percentage, the fee in dollars, a count),
/// and `fixed_style` is a style the data itself decides — a contribution's role, a reasoning
/// level — which, as before the refactor, wins over a configured `style` and thresholds.
pub(crate) struct ItemData {
    pub(crate) values: Vec<(&'static str, String)>,
    pub(crate) default_format: String,
    pub(crate) numeric: Option<u64>,
    pub(crate) fixed_style: Option<Style>,
    /// An optional symbol the data itself carries (ADR 0027: a footer
    /// `command`'s JSON `symbol`). A configured `icon`/`symbol` on the item wins over it, and the
    /// component's own glyph is the last fallback.
    pub(crate) symbol: Option<String>,
}

impl ItemData {
    pub(crate) fn new(default_format: impl Into<String>) -> Self {
        Self {
            values: Vec::new(),
            default_format: default_format.into(),
            numeric: None,
            fixed_style: None,
            symbol: None,
        }
    }

    pub(crate) fn value(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.values.push((key, value.into()));
        self
    }

    pub(crate) fn numeric(mut self, value: Option<u64>) -> Self {
        self.numeric = value;
        self
    }

    pub(crate) fn fixed_style(mut self, style: Style) -> Self {
        self.fixed_style = Some(style);
        self
    }

    pub(crate) fn symbol(mut self, symbol: Option<String>) -> Self {
        self.symbol = symbol;
        self
    }
}

/// One built-in footer item (ADR 0027). Internal: see the module docs.
pub(crate) trait Component: Sync {
    /// The component's stable id: its key in `[tui.footer.items.<id>]` and a row.
    fn id(&self) -> &str;
    /// The `{placeholder}`s its `data` fills, for parse-time validation and its README row.
    fn placeholders(&self) -> &'static [&'static str];
    /// The default icon in each glyph set, `[unicode, nerd, ascii, emoji]` (ADR 0027 revision
    /// 14 decision 7 added the fourth). An empty string is "no icon".
    fn glyphs(&self) -> [&'static str; 4];
    /// The default theme role, after a threshold and a configured `style` (ADR 0027 C2).
    fn role(&self, _app: &App) -> Option<Role> {
        None
    }
    /// The default truncation priority: higher survives a narrow terminal longer.
    fn priority(&self) -> i64 {
        50
    }
    /// The default thresholds for a numeric item (ADR 0027 C2's 70/90 pair).
    fn thresholds(&self) -> Vec<(i64, ItemStyle)> {
        Vec::new()
    }
    /// The item's data at draw time, or `None` when it has nothing (the item hides; v5 §5: never
    /// a fabricated number). `item` carries the configured per-item parameters (`context`'s
    /// `width`/`fill`/`empty`, `status`'s `keys`).
    fn data(&self, app: &App, item: &Item) -> Option<ItemData>;
}

/// The registry, one entry per built-in component (ADR 0027).
static COMPONENTS: &[&'static dyn Component] = &[
    &brand::Brand,
    &host::Host,
    &model::Model,
    &reasoning::Reasoning,
    &tier::Tier,
    &cwd::Cwd,
    &git_branch::GitBranch,
    &worktree::Worktree,
    &status::Status,
    &session::Session,
    &context::Context,
    &usage::Usage,
    &tokens::Tokens,
    &speed::Speed,
    &elapsed::Elapsed,
    &cost::Cost,
    &pending::Pending,
    &queued::Queued,
    &loop_item::Loop,
    &jobs::Jobs,
    &mcp::Mcp,
    &scroll::Scroll,
];

/// The built-in component for `id`, or `None` when no compiled-in component claims it.
pub(crate) fn builtin(id: &str) -> Option<&'static dyn Component> {
    COMPONENTS
        .iter()
        .copied()
        .find(|component| component.id() == id)
}

/// Every registered id, in registry order (tests only: the render path never needs the list).
#[cfg(test)]
pub(crate) fn ids() -> Vec<&'static str> {
    COMPONENTS.iter().map(|component| component.id()).collect()
}

/// The placeholders a built-in id declares; `None` for an id no component claims.
pub(crate) fn placeholders(id: &str) -> Option<&'static [&'static str]> {
    builtin(id).map(Component::placeholders)
}

/// A built-in id's default truncation priority, or the generic 50 for an unknown id.
pub(crate) fn priority(id: &str) -> i64 {
    builtin(id).map_or(50, Component::priority)
}

/// A built-in id's default thresholds, empty for an unknown id.
pub(crate) fn thresholds(id: &str) -> Vec<(i64, ItemStyle)> {
    builtin(id).map_or_else(Vec::new, Component::thresholds)
}

/// A component's default glyph in the configured set.
pub(crate) fn glyph_of(component: &dyn Component, glyphs: Glyphs) -> &'static str {
    component.glyphs()[match glyphs {
        Glyphs::Unicode => 0,
        Glyphs::Nerd => 1,
        Glyphs::Ascii => 2,
        Glyphs::Emoji => 3,
    }]
}

/// The `jobs` item's displayed age and the idle loop's wait for its next change (ADR 0023
/// U2): re-exported so the loop ticks exactly the text the item shows.
pub(crate) use jobs::{age_text, tick_in};

/// What an id resolves to at draw time: a compiled-in component, or the dynamic fallback for an
/// arbitrary id a contributor has published (ADR 0027).
pub(crate) enum Resolved<'a> {
    Builtin(&'static dyn Component),
    Store(StoreComponent<'a>),
}

impl Resolved<'_> {
    pub(crate) fn get(&self) -> &dyn Component {
        match self {
            Resolved::Builtin(component) => *component,
            Resolved::Store(component) => component,
        }
    }
}

/// Resolves an id to its component: a built-in first, else the dynamic contribution fallback
/// when the store holds that id. `None` means an unknown id: the caller shows it raw (ADR 0008).
pub(crate) fn lookup<'a>(app: &'a App, id: &'a str) -> Option<Resolved<'a>> {
    if let Some(component) = builtin(id) {
        return Some(Resolved::Builtin(component));
    }
    app.contributions.get(id)?;
    Some(Resolved::Store(StoreComponent { id }))
}

/// The dynamic fallback (ADR 0027): any id a contributor publishes that
/// no built-in claims. It draws the store's segmented text with the contribution's own role
/// (dim when stale), exactly as the pre-refactor store fallback did. It is a normal component:
/// the renderer reaches it through [`lookup`], never through an id list.
pub(crate) struct StoreComponent<'a> {
    id: &'a str,
}

impl Component for StoreComponent<'_> {
    fn id(&self) -> &str {
        self.id
    }

    fn placeholders(&self) -> &'static [&'static str] {
        // The store text is the whole value; there is nothing to validate.
        &[]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["", "", "", ""]
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let contribution = app.contributions.get(self.id)?;
        if contribution.segments.is_empty() {
            return None;
        }
        let text: String = contribution
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect();
        let role = if contribution.stale {
            Role::Truncation
        } else {
            super::contrib_role(contribution.segments[0].role)
        };
        Some(
            ItemData::new("{value}")
                .value("value", text)
                .symbol(contribution.symbol.clone())
                .fixed_style(super::role_style(app, role)),
        )
    }
}
