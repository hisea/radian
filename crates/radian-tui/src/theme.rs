//! The TUI color theme (ADR 0009; config key `tui.theme`; ADR 0029 for theme files).
//!
//! Every color in the UI comes from one [`Theme`]; `render.rs` never names a `Color` itself.
//! `dark` is the original palette (the `TestBackend` screenshot test pins it), `light` is the
//! same roles re-picked for a light background, and `mono` uses no color at all — only bold,
//! underline and dim. The `NO_COLOR` convention (<https://no-color.org>) forces `mono`.

mod color;
mod file;

pub use color::{
    ColorValue, Rgb, approximate_256, parse_color_value, truecolor_env, truecolor_for,
};
pub use file::load;

use std::collections::BTreeMap;

use ratatui::style::{Color, Modifier, Style};

/// The named built-in themes, plus `File` for a theme loaded from
/// `~/.radian/themes/<name>.json` (ADR 0029 decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeName {
    #[default]
    Dark,
    Light,
    Mono,
    /// A theme file. Which built-in palette its missing roles fell back to is already baked
    /// into the styles (chosen by the file's `appearance`, dark by default).
    File,
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
            ThemeName::File => "file",
        }
    }
}

/// The 22 roles a [`Theme`] styles today (ADR 0009, ADR 0023). The config keys are the
/// snake_case names; `[tui.colors]` accepts them and (mapped) pi's role names alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Reasoning,
    Interrupted,
    User,
    ToolCall,
    ToolResult,
    ToolResultError,
    Notice,
    TurnIdle,
    TurnRunning,
    TurnCancelled,
    Model,
    Tier,
    Usage,
    Mcp,
    Budget,
    BudgetOver,
    Note,
    Truncation,
    ApprovalBorder,
    Hint,
    Completion,
    Find,
}

impl Role {
    pub const ALL: [Role; 22] = [
        Role::Reasoning,
        Role::Interrupted,
        Role::User,
        Role::ToolCall,
        Role::ToolResult,
        Role::ToolResultError,
        Role::Notice,
        Role::TurnIdle,
        Role::TurnRunning,
        Role::TurnCancelled,
        Role::Model,
        Role::Tier,
        Role::Usage,
        Role::Mcp,
        Role::Budget,
        Role::BudgetOver,
        Role::Note,
        Role::Truncation,
        Role::ApprovalBorder,
        Role::Hint,
        Role::Completion,
        Role::Find,
    ];

    /// The `[tui.colors]` key for this role (ADR 0029 decision 5).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Reasoning => "reasoning",
            Role::Interrupted => "interrupted",
            Role::User => "user",
            Role::ToolCall => "tool_call",
            Role::ToolResult => "tool_result",
            Role::ToolResultError => "tool_result_error",
            Role::Notice => "notice",
            Role::TurnIdle => "turn_idle",
            Role::TurnRunning => "turn_running",
            Role::TurnCancelled => "turn_cancelled",
            Role::Model => "model",
            Role::Tier => "tier",
            Role::Usage => "usage",
            Role::Mcp => "mcp",
            Role::Budget => "budget",
            Role::BudgetOver => "budget_over",
            Role::Note => "note",
            Role::Truncation => "truncation",
            Role::ApprovalBorder => "approval_border",
            Role::Hint => "hint",
            Role::Completion => "completion",
            Role::Find => "find",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Role::ALL.into_iter().find(|role| role.as_str() == name)
    }

    fn style_mut(self, theme: &mut Theme) -> &mut Style {
        match self {
            Role::Reasoning => &mut theme.reasoning,
            Role::Interrupted => &mut theme.interrupted,
            Role::User => &mut theme.user,
            Role::ToolCall => &mut theme.tool_call,
            Role::ToolResult => &mut theme.tool_result,
            Role::ToolResultError => &mut theme.tool_result_error,
            Role::Notice => &mut theme.notice,
            Role::TurnIdle => &mut theme.turn_idle,
            Role::TurnRunning => &mut theme.turn_running,
            Role::TurnCancelled => &mut theme.turn_cancelled,
            Role::Model => &mut theme.model,
            Role::Tier => &mut theme.tier,
            Role::Usage => &mut theme.usage,
            Role::Mcp => &mut theme.mcp,
            Role::Budget => &mut theme.budget,
            Role::BudgetOver => &mut theme.budget_over,
            Role::Note => &mut theme.note,
            Role::Truncation => &mut theme.truncation,
            Role::ApprovalBorder => &mut theme.approval_border,
            Role::Hint => &mut theme.hint,
            Role::Completion => &mut theme.completion,
            Role::Find => &mut theme.find,
        }
    }
}

/// One role's style, by theme.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The `@` file-reference completion list (Phase 4).
    pub completion: Style,
    /// The find match position in the status bar (Phase 5).
    pub find: Style,
    /// Pi roles no current Radian role consumes — the `md*`, `toolDiff*`, `syntax*` and
    /// `thinking*` families ADR 0027/0028 will wire, plus the panel roles (`selectedBg`,
    /// `scrollbar*`, `searchMatch*`, `customMessage*`, `toolPendingBg`, `toolSuccessBg`,
    /// `toolErrorBg`, `bashMode`). ADR 0029 decision 3: parsed and kept so a theme file's
    /// values are not thrown away; nothing renders them yet. Keyed by pi's role name.
    pub reserved: BTreeMap<String, Style>,
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
            reserved: BTreeMap::new(),
            reasoning: style(Some(Color::DarkGray), Modifier::ITALIC | Modifier::DIM),
            interrupted: style(Some(Color::Yellow), Modifier::BOLD),
            user: style(Some(Color::Cyan), Modifier::BOLD),
            tool_call: style(Some(Color::Blue), Modifier::empty()),
            tool_result: style(Some(Color::Green), Modifier::empty()),
            tool_result_error: style(Some(Color::Red), Modifier::empty()),
            // The owner's UI review (2026-10-13): `DarkGray` (ANSI 8) is too dark to read on a
            // dark background. A notice is ordinary information, so it uses the terminal's
            // **default foreground** (readable on any background); a hint steps down to `Gray`
            // (ANSI 7, light on dark) but stays legible.
            notice: style(None, Modifier::empty()),
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
            hint: style(Some(Color::Gray), Modifier::empty()),
            completion: style(Some(Color::DarkGray), Modifier::empty()),
            find: style(Some(Color::Yellow), Modifier::empty()),
        }
    }

    /// The same roles re-picked for a light background: cyan/yellow are unreadable there, so
    /// they become blue/red.
    pub fn light() -> Self {
        Self {
            name: ThemeName::Light,
            reserved: BTreeMap::new(),
            reasoning: style(Some(Color::DarkGray), Modifier::ITALIC),
            interrupted: style(Some(Color::Red), Modifier::BOLD),
            user: style(Some(Color::Blue), Modifier::BOLD),
            tool_call: style(Some(Color::Blue), Modifier::empty()),
            tool_result: style(Some(Color::Green), Modifier::empty()),
            tool_result_error: style(Some(Color::Red), Modifier::empty()),
            // The owner's UI review (2026-10-13): `DarkGray` (ANSI 8) is too dark to read on a
            // dark background. A notice is ordinary information, so it uses the terminal's
            // **default foreground** (readable on any background); a hint steps down to `Gray`
            // (ANSI 7, light on dark) but stays legible.
            notice: style(None, Modifier::empty()),
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
            completion: style(Some(Color::DarkGray), Modifier::empty()),
            find: style(Some(Color::Magenta), Modifier::empty()),
        }
    }

    /// No color at all: roles are told apart by bold / underline / dim (and reverse).
    pub fn mono() -> Self {
        Self {
            name: ThemeName::Mono,
            reserved: BTreeMap::new(),
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
            completion: style(None, Modifier::DIM),
            find: style(None, Modifier::BOLD),
        }
    }

    pub fn for_name(name: ThemeName) -> Self {
        match name {
            ThemeName::Dark | ThemeName::File => Theme::dark(),
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
    pub fn styles(&self) -> [Style; 22] {
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
            self.completion,
            self.find,
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
        assert_eq!(dark.budget.fg, Some(Color::Magenta));
        assert_eq!(dark.approval_border.fg, Some(Color::Yellow));
        assert!(dark.reasoning.add_modifier.contains(Modifier::ITALIC));
    }

    /// The owner's UI review: a notice or a hint has to be readable on the background it is
    /// for. `DarkGray` (ANSI 8) is too dark on the dark palette; on the light palette it is
    /// dark text, which is fine, so only the dark one is pinned away from it.
    #[test]
    fn dark_notices_and_hints_are_not_dark_gray() {
        let dark = Theme::dark();
        assert_eq!(
            dark.notice.fg, None,
            "a notice uses the terminal's default foreground"
        );
        assert_ne!(dark.hint.fg, Some(Color::DarkGray));
        assert_eq!(dark.hint.fg, Some(Color::Gray));
        // Reasoning stays deliberately weak — the owner asked for that explicitly.
        assert_eq!(dark.reasoning.fg, Some(Color::DarkGray));

        let light = Theme::light();
        assert_eq!(light.notice.fg, None, "readable on light too");
        assert_eq!(
            light.hint.fg,
            Some(Color::DarkGray),
            "dark text on a light background"
        );
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
