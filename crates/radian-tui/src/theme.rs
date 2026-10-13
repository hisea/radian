//! The TUI color theme (ADR 0009; config key `tui.theme`).
//!
//! Every color in the UI comes from one [`Theme`]; `render.rs` never names a `Color` itself.
//! `dark` is the original palette (the `TestBackend` screenshot test pins it), `light` is the
//! same roles re-picked for a light background, and `mono` uses no color at all — only bold,
//! underline and dim. The `NO_COLOR` convention (<https://no-color.org>) forces `mono`.

use ratatui::style::{Color, Modifier, Style};

/// The three named themes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeName {
    #[default]
    Dark,
    Light,
    Mono,
}

impl ThemeName {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "dark" => Some(ThemeName::Dark),
            "light" => Some(ThemeName::Light),
            "mono" => Some(ThemeName::Mono),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ThemeName::Dark => "dark",
            ThemeName::Light => "light",
            ThemeName::Mono => "mono",
        }
    }
}

/// One role's style, by theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub name: ThemeName,
    /// The model's separate reasoning channel (ADR 0014).
    pub reasoning: Style,
    /// The `[interrupted]` marker (ADR 0003).
    pub interrupted: Style,
    pub user: Style,
    pub tool_call: Style,
    pub tool_result: Style,
    pub tool_result_error: Style,
    pub notice: Style,
    pub turn_idle: Style,
    pub turn_running: Style,
    pub turn_cancelled: Style,
    pub model: Style,
    pub tier: Style,
    pub usage: Style,
    pub mcp: Style,
    pub budget: Style,
    pub budget_over: Style,
    pub note: Style,
    pub truncation: Style,
    pub approval_border: Style,
    pub hint: Style,
}

fn style(color: Option<Color>, modifiers: Modifier) -> Style {
    let mut style = Style::default().add_modifier(modifiers);
    if let Some(color) = color {
        style = style.fg(color);
    }
    style
}

impl Default for Theme {
    fn default() -> Self {
        Theme::dark()
    }
}

impl Theme {
    /// The original palette (pre-config). Do not change a color here without the
    /// `TestBackend` screenshot test: `dark` must stay byte-identical.
    pub fn dark() -> Self {
        Self {
            name: ThemeName::Dark,
            reasoning: style(Some(Color::DarkGray), Modifier::ITALIC | Modifier::DIM),
            interrupted: style(Some(Color::Yellow), Modifier::BOLD),
            user: style(Some(Color::Cyan), Modifier::BOLD),
            tool_call: style(Some(Color::Blue), Modifier::empty()),
            tool_result: style(Some(Color::Green), Modifier::empty()),
            tool_result_error: style(Some(Color::Red), Modifier::empty()),
            notice: style(Some(Color::DarkGray), Modifier::empty()),
            turn_idle: style(Some(Color::Green), Modifier::empty()),
            turn_running: style(Some(Color::Yellow), Modifier::empty()),
            turn_cancelled: style(Some(Color::Red), Modifier::empty()),
            model: style(Some(Color::Cyan), Modifier::empty()),
            tier: style(Some(Color::Blue), Modifier::empty()),
            usage: style(Some(Color::Green), Modifier::empty()),
            mcp: style(Some(Color::Cyan), Modifier::empty()),
            budget: style(Some(Color::Magenta), Modifier::empty()),
            budget_over: style(Some(Color::Yellow), Modifier::empty()),
            note: style(Some(Color::Yellow), Modifier::empty()),
            truncation: style(Some(Color::DarkGray), Modifier::empty()),
            approval_border: style(Some(Color::Yellow), Modifier::empty()),
            hint: style(Some(Color::DarkGray), Modifier::empty()),
        }
    }

    /// The same roles re-picked for a light background: cyan/yellow are unreadable there, so
    /// they become blue/red.
    pub fn light() -> Self {
        Self {
            name: ThemeName::Light,
            reasoning: style(Some(Color::DarkGray), Modifier::ITALIC),
            interrupted: style(Some(Color::Red), Modifier::BOLD),
            user: style(Some(Color::Blue), Modifier::BOLD),
            tool_call: style(Some(Color::Blue), Modifier::empty()),
            tool_result: style(Some(Color::Green), Modifier::empty()),
            tool_result_error: style(Some(Color::Red), Modifier::empty()),
            notice: style(Some(Color::DarkGray), Modifier::empty()),
            turn_idle: style(Some(Color::Green), Modifier::empty()),
            turn_running: style(Some(Color::Magenta), Modifier::empty()),
            turn_cancelled: style(Some(Color::Red), Modifier::empty()),
            model: style(Some(Color::Blue), Modifier::empty()),
            tier: style(Some(Color::Magenta), Modifier::empty()),
            usage: style(Some(Color::Green), Modifier::empty()),
            mcp: style(Some(Color::Blue), Modifier::empty()),
            budget: style(Some(Color::Magenta), Modifier::empty()),
            budget_over: style(Some(Color::Red), Modifier::empty()),
            note: style(Some(Color::Magenta), Modifier::empty()),
            truncation: style(Some(Color::DarkGray), Modifier::empty()),
            approval_border: style(Some(Color::Red), Modifier::empty()),
            hint: style(Some(Color::DarkGray), Modifier::empty()),
        }
    }

    /// No color at all: roles are told apart by bold / underline / dim (and reverse).
    pub fn mono() -> Self {
        Self {
            name: ThemeName::Mono,
            reasoning: style(None, Modifier::ITALIC | Modifier::DIM),
            interrupted: style(None, Modifier::BOLD | Modifier::UNDERLINED),
            user: style(None, Modifier::BOLD),
            tool_call: style(None, Modifier::REVERSED),
            tool_result: style(None, Modifier::empty()),
            tool_result_error: style(None, Modifier::BOLD | Modifier::UNDERLINED),
            notice: style(None, Modifier::DIM),
            turn_idle: style(None, Modifier::empty()),
            turn_running: style(None, Modifier::BOLD),
            turn_cancelled: style(None, Modifier::UNDERLINED),
            model: style(None, Modifier::BOLD),
            tier: style(None, Modifier::DIM),
            usage: style(None, Modifier::empty()),
            mcp: style(None, Modifier::DIM),
            budget: style(None, Modifier::DIM),
            budget_over: style(None, Modifier::BOLD),
            note: style(None, Modifier::BOLD),
            truncation: style(None, Modifier::DIM),
            approval_border: style(None, Modifier::BOLD),
            hint: style(None, Modifier::DIM),
        }
    }

    pub fn for_name(name: ThemeName) -> Self {
        match name {
            ThemeName::Dark => Theme::dark(),
            ThemeName::Light => Theme::light(),
            ThemeName::Mono => Theme::mono(),
        }
    }

    /// Resolves the configured theme name with the `NO_COLOR` override: any non-empty
    /// `NO_COLOR` forces `mono`.
    pub fn resolve(name: Option<ThemeName>, no_color: bool) -> Self {
        if no_color {
            return Theme::mono();
        }
        Theme::for_name(name.unwrap_or_default())
    }

    pub fn is_mono(&self) -> bool {
        self.name == ThemeName::Mono
    }

    /// Whether `NO_COLOR` is set to a non-empty value (the no-color.org convention).
    pub fn no_color_env() -> bool {
        std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
    }

    /// Every style this theme defines, for the "mono has no colors" test.
    pub fn styles(&self) -> [Style; 20] {
        [
            self.reasoning,
            self.interrupted,
            self.user,
            self.tool_call,
            self.tool_result,
            self.tool_result_error,
            self.notice,
            self.turn_idle,
            self.turn_running,
            self.turn_cancelled,
            self.model,
            self.tier,
            self.usage,
            self.mcp,
            self.budget,
            self.budget_over,
            self.note,
            self.truncation,
            self.approval_border,
            self.hint,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_unknown_names_are_none() {
        for name in [ThemeName::Dark, ThemeName::Light, ThemeName::Mono] {
            assert_eq!(ThemeName::parse(name.as_str()), Some(name));
        }
        assert_eq!(ThemeName::parse("future"), None);
    }

    #[test]
    fn dark_keeps_the_original_palette() {
        let dark = Theme::dark();
        assert_eq!(dark.reasoning.fg, Some(Color::DarkGray));
        assert_eq!(dark.user.fg, Some(Color::Cyan));
        assert_eq!(dark.tool_call.fg, Some(Color::Blue));
        assert_eq!(dark.tool_result.fg, Some(Color::Green));
        assert_eq!(dark.notice.fg, Some(Color::DarkGray));
        assert_eq!(dark.budget.fg, Some(Color::Magenta));
        assert_eq!(dark.approval_border.fg, Some(Color::Yellow));
        assert!(dark.reasoning.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn mono_uses_no_color_at_all() {
        let mono = Theme::mono();
        for style in mono.styles() {
            assert!(style.fg.is_none(), "{style:?}");
            assert!(style.bg.is_none(), "{style:?}");
        }
        assert!(mono.is_mono());
    }

    #[test]
    fn light_differs_from_dark() {
        assert_ne!(Theme::light().user, Theme::dark().user);
        assert_ne!(Theme::light().turn_running, Theme::dark().turn_running);
    }

    #[test]
    fn no_color_forces_mono() {
        assert!(Theme::resolve(Some(ThemeName::Dark), true).is_mono());
        assert!(!Theme::resolve(Some(ThemeName::Dark), false).is_mono());
        assert_eq!(
            Theme::resolve(Some(ThemeName::Light), false).name,
            ThemeName::Light
        );
    }
}
