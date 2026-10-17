//! The `reasoning` component (ADR 0027; one role per level, C2).

use crate::app::App;
use crate::footer::{Item, role_style};
use crate::theme::Role;

use super::{Component, ItemData};

pub(crate) struct Reasoning;

/// The role for the `reasoning` item at `level` (ADR 0027 C2, one role per level).
pub(crate) fn reasoning_role(level: &str) -> Role {
    match level {
        "off" => Role::ReasoningOff,
        "minimal" => Role::ReasoningMinimal,
        "low" => Role::ReasoningLow,
        "medium" => Role::ReasoningMedium,
        "high" => Role::ReasoningHigh,
        "xhigh" => Role::ReasoningXhigh,
        "max" => Role::ReasoningMax,
        _ => Role::ReasoningMedium,
    }
}

impl Component for Reasoning {
    fn id(&self) -> &str {
        "reasoning"
    }

    fn placeholders(&self) -> &'static [&'static str] {
        &["level"]
    }

    fn glyphs(&self) -> [&'static str; 4] {
        ["* ", "\u{f0eb} ", "* ", "🧠 "]
    }

    fn priority(&self) -> i64 {
        60
    }

    fn data(&self, app: &App, _item: &Item) -> Option<ItemData> {
        let level = app
            .session
            .next_thinking
            .as_deref()
            .or(app.session.thinking_level.as_deref())?;
        // The level's role is the item's style, as before the refactor: a configured `style`
        // does not override it.
        Some(
            ItemData::new("{level}")
                .value("level", level.to_string())
                .fixed_style(role_style(app, reasoning_role(level))),
        )
    }
}
