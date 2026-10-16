//! The TUI color theme (ADR 0009; config key `tui.theme`; ADR 0029 for theme files).
//!
//! Every color in the UI comes from one [`Theme`]; `render.rs` never names a `Color` itself.
//! A rendered Mermaid diagram (ADR 0028 R8) uses three roles — `mermaid_node` (accent),
//! `mermaid_edge` (dim) and `mermaid_text` (primary) — mapped to pi's `accent`/`dim`/`text`
//! in `file.rs`'s `fg_source`.
//! `dark` is the original palette (the `TestBackend` screenshot test pins it), `light` is the
//! same roles re-picked for a light background, and `mono` uses no color at all — only bold,
//! underline and dim. The `NO_COLOR` convention (<https://no-color.org>) forces `mono`.

mod color;
mod file;
pub mod importer;
mod toml_file;

pub use color::{
    ColorValue, Rgb, approximate_256, parse_color_value, truecolor_env, truecolor_for,
};
pub use file::load;

/// The outcome of `/theme <name>` (ADR 0008 revision): the theme to apply (`None` when the
/// switch did not happen — an unknown name or `NO_COLOR`), and the notices to show.
pub struct Switch {
    pub theme: Option<Theme>,
    pub notices: Vec<String>,
}

/// Loads `/theme <name>` and writes `tui.theme` back to the config file.
///
/// I/O lives here (the loop calls it); `App::update` only applies the result. `no_color`,
/// `themes_dir` and `write_path` are passed in so the caller owns the environment and the
/// function is testable without touching process-wide state.
pub fn switch(
    name: &str,
    no_color: bool,
    themes_dir: Option<&std::path::Path>,
    colors: &[(String, radian_host::config::TuiColorOverride)],
    write_path: Option<&std::path::Path>,
    read_only: bool,
) -> Switch {
    // NO_COLOR wins over every theme (ADR 0029 decision 7): switching does nothing.
    if no_color {
        return Switch {
            theme: None,
            notices: vec![
                "NO_COLOR is set: the theme is fixed, so /theme does not switch it".to_string(),
            ],
        };
    }
    let builtin = matches!(name, "dark" | "light" | "mono");
    let is_file = themes_dir.is_some_and(|dir| {
        // ADR 0029: `<name>.toml` first, then the old `<name>.json`.
        dir.join(format!("{name}.toml")).is_file() || dir.join(format!("{name}.json")).is_file()
    });
    if !builtin && !is_file {
        return Switch {
            theme: None,
            notices: vec![format!(
                "no theme {name:?}: built-ins are dark, light, mono; theme files live in {}",
                themes_dir.map_or_else(
                    || "~/.radian/themes/ (no RADIAN_HOME)".to_string(),
                    |dir| dir.display().to_string()
                )
            )],
        };
    }
    let (theme, warnings) = load(
        Some(name),
        false,
        crate::theme::truecolor_env(),
        themes_dir,
        colors,
    );
    let mut notices = warnings;
    match write_path {
        Some(path) => match radian_host::write_theme(path, read_only, name) {
            Ok(path) => notices.push(format!("theme saved to {}", path.display())),
            Err(error) => notices.push(format!(
                "theme applied for this session; could not save: {error}"
            )),
        },
        None => notices.push(
            "theme applied for this session; there is no configuration file location to save it"
                .to_string(),
        ),
    }
    Switch {
        theme: Some(theme),
        notices,
    }
}

use std::collections::BTreeMap;

use ratatui::style::{Color, Modifier, Style};

/// The named built-in themes, plus `File` for a theme loaded from
/// `~/.radian/themes/<name>.toml` (ADR 0029; a `<name>.json` is the
/// one-version transition, decision 4).
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

/// The 70 roles a [`Theme`] styles (ADR 0009, ADR 0023, ADR 0029). The config keys
/// are the snake_case names; `[tui.colors]` accepts them and (mapped) pi's role names alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Reasoning,
    /// The `reasoning` footer item's per-level colors (ADR 0027 C2): one role per thinking
    /// level, so a level's marker can be told apart at a glance. `reasoning` (above) stays the
    /// body style of the reasoning channel itself.
    ReasoningOff,
    ReasoningMinimal,
    ReasoningLow,
    ReasoningMedium,
    ReasoningHigh,
    ReasoningXhigh,
    ReasoningMax,
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
    /// The status bar's `AUTO` marker (ADR 0026 D9): the session auto-approves.
    Auto,
    /// The thin transcript's structural lines (ADR 0029 and 6): the
    /// `│ `/`╭─`/`╰──` rules, the `• `/`●` markers, table rules and the compaction rule. The
    /// weak level, the same value as `dim`.
    Frame,
    /// A tool block's head line's tool name (ADR 0029); the same value as
    /// `syntax_keyword`.
    ToolName,
    /// A diff line's gutter line number (ADR 0029). The weak level.
    DiffLineNumber,
    /// A tool head's `✓` and an orchestration panel's `✓`/`●` (ADR 0029).
    /// pi's `success`.
    StatusOk,
    /// A tool head's warning marker (ADR 0029). pi's `warning`.
    StatusWarn,
    /// A tool head's `×` (ADR 0029; `✗` was replaced, ). pi's `error`.
    StatusError,
    Selection,
    ToolPendingBg,
    ToolSuccessBg,
    ToolErrorBg,
    NoticeLabel,
    NoticeBody,
    Text,
    DiffAdded,
    DiffRemoved,
    DiffContext,
    DiffHunk,
    MermaidNode,
    MermaidEdge,
    MermaidText,
    /// Assistant-text Markdown roles (ADR 0028 R2, pi's same-named `md*` roles).
    MdHeading,
    /// A heading's `#` mark (ADR 0035): one level weaker than the heading text.
    MdHeadingMark,
    MdLink,
    MdLinkUrl,
    MdCode,
    MdQuote,
    MdQuoteBorder,
    MdHr,
    MdListBullet,
    /// Code-block syntax roles (ADR 0028 R2 decision 2), pi's same-named `syntax*`
    /// roles. The highlighter maps a syntect scope to exactly one of these; anything unmapped
    /// falls back to the code block's uniform style.
    SyntaxComment,
    SyntaxKeyword,
    SyntaxFunction,
    SyntaxVariable,
    SyntaxString,
    SyntaxNumber,
    SyntaxType,
    SyntaxOperator,
    SyntaxPunctuation,
    /// The footer's separator between items (ADR 0027): weaker than the item text,
    /// so it reads as structure. Configurable through `[tui.footer] separator_style`.
    FooterSeparator,
}

impl Role {
    pub const ALL: [Role; 69] = [
        Role::Reasoning,
        Role::ReasoningOff,
        Role::ReasoningMinimal,
        Role::ReasoningLow,
        Role::ReasoningMedium,
        Role::ReasoningHigh,
        Role::ReasoningXhigh,
        Role::ReasoningMax,
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
        Role::Auto,
        Role::Frame,
        Role::ToolName,
        Role::DiffLineNumber,
        Role::StatusOk,
        Role::StatusWarn,
        Role::StatusError,
        Role::Selection,
        Role::ToolPendingBg,
        Role::ToolSuccessBg,
        Role::ToolErrorBg,
        Role::NoticeLabel,
        Role::NoticeBody,
        Role::Text,
        Role::DiffAdded,
        Role::DiffRemoved,
        Role::DiffContext,
        Role::DiffHunk,
        Role::MermaidNode,
        Role::MermaidEdge,
        Role::MermaidText,
        Role::MdHeading,
        Role::MdHeadingMark,
        Role::MdLink,
        Role::MdLinkUrl,
        Role::MdCode,
        Role::MdQuote,
        Role::MdQuoteBorder,
        Role::MdHr,
        Role::MdListBullet,
        Role::SyntaxComment,
        Role::SyntaxKeyword,
        Role::SyntaxFunction,
        Role::SyntaxVariable,
        Role::SyntaxString,
        Role::SyntaxNumber,
        Role::SyntaxType,
        Role::SyntaxOperator,
        Role::SyntaxPunctuation,
        Role::FooterSeparator,
    ];

    /// The `[tui.colors]` key for this role (ADR 0029 decision 5).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Reasoning => "reasoning",
            Role::ReasoningOff => "reasoning_off",
            Role::ReasoningMinimal => "reasoning_minimal",
            Role::ReasoningLow => "reasoning_low",
            Role::ReasoningMedium => "reasoning_medium",
            Role::ReasoningHigh => "reasoning_high",
            Role::ReasoningXhigh => "reasoning_xhigh",
            Role::ReasoningMax => "reasoning_max",
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
            Role::Auto => "auto",
            Role::Frame => "frame",
            Role::ToolName => "tool_name",
            Role::DiffLineNumber => "diff_line_number",
            Role::StatusOk => "status_ok",
            Role::StatusWarn => "status_warn",
            Role::StatusError => "status_error",
            Role::Selection => "selection",
            Role::ToolPendingBg => "tool_pending_bg",
            Role::ToolSuccessBg => "tool_success_bg",
            Role::ToolErrorBg => "tool_error_bg",
            Role::NoticeLabel => "notice_label",
            Role::NoticeBody => "notice_body",
            Role::Text => "text",
            Role::DiffAdded => "diff_added",
            Role::DiffRemoved => "diff_removed",
            Role::DiffContext => "diff_context",
            Role::DiffHunk => "diff_hunk",
            Role::MermaidNode => "mermaid_node",
            Role::MermaidEdge => "mermaid_edge",
            Role::MermaidText => "mermaid_text",
            Role::MdHeading => "md_heading",
            Role::MdHeadingMark => "md_heading_mark",
            Role::MdLink => "md_link",
            Role::MdLinkUrl => "md_link_url",
            Role::MdCode => "md_code",
            Role::MdQuote => "md_quote",
            Role::MdQuoteBorder => "md_quote_border",
            Role::MdHr => "md_hr",
            Role::MdListBullet => "md_list_bullet",
            Role::SyntaxComment => "syntax_comment",
            Role::SyntaxKeyword => "syntax_keyword",
            Role::SyntaxFunction => "syntax_function",
            Role::SyntaxVariable => "syntax_variable",
            Role::SyntaxString => "syntax_string",
            Role::SyntaxNumber => "syntax_number",
            Role::SyntaxType => "syntax_type",
            Role::SyntaxOperator => "syntax_operator",
            Role::SyntaxPunctuation => "syntax_punctuation",
            Role::FooterSeparator => "footer_separator",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Role::ALL.into_iter().find(|role| role.as_str() == name)
    }

    /// This role's style on a theme. `pub(crate)`: the footer renders its items through
    /// the same table the renderer uses, so a role has no second source.
    pub(crate) fn style_mut(self, theme: &mut Theme) -> &mut Style {
        match self {
            Role::Reasoning => &mut theme.reasoning,
            Role::ReasoningOff => &mut theme.reasoning_off,
            Role::ReasoningMinimal => &mut theme.reasoning_minimal,
            Role::ReasoningLow => &mut theme.reasoning_low,
            Role::ReasoningMedium => &mut theme.reasoning_medium,
            Role::ReasoningHigh => &mut theme.reasoning_high,
            Role::ReasoningXhigh => &mut theme.reasoning_xhigh,
            Role::ReasoningMax => &mut theme.reasoning_max,
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
            Role::Auto => &mut theme.auto,
            Role::Frame => &mut theme.frame,
            Role::ToolName => &mut theme.tool_name,
            Role::DiffLineNumber => &mut theme.diff_line_number,
            Role::StatusOk => &mut theme.status_ok,
            Role::StatusWarn => &mut theme.status_warn,
            Role::StatusError => &mut theme.status_error,
            Role::Selection => &mut theme.selection,
            Role::ToolPendingBg => &mut theme.tool_pending_bg,
            Role::ToolSuccessBg => &mut theme.tool_success_bg,
            Role::ToolErrorBg => &mut theme.tool_error_bg,
            Role::NoticeLabel => &mut theme.notice_label,
            Role::NoticeBody => &mut theme.notice_body,
            Role::Text => &mut theme.text,
            Role::DiffAdded => &mut theme.diff_added,
            Role::DiffRemoved => &mut theme.diff_removed,
            Role::DiffContext => &mut theme.diff_context,
            Role::DiffHunk => &mut theme.diff_hunk,
            Role::MermaidNode => &mut theme.mermaid_node,
            Role::MermaidEdge => &mut theme.mermaid_edge,
            Role::MermaidText => &mut theme.mermaid_text,
            Role::MdHeading => &mut theme.md_heading,
            Role::MdHeadingMark => &mut theme.md_heading_mark,
            Role::MdLink => &mut theme.md_link,
            Role::MdLinkUrl => &mut theme.md_link_url,
            Role::MdCode => &mut theme.md_code,
            Role::MdQuote => &mut theme.md_quote,
            Role::MdQuoteBorder => &mut theme.md_quote_border,
            Role::MdHr => &mut theme.md_hr,
            Role::MdListBullet => &mut theme.md_list_bullet,
            Role::SyntaxComment => &mut theme.syntax_comment,
            Role::SyntaxKeyword => &mut theme.syntax_keyword,
            Role::SyntaxFunction => &mut theme.syntax_function,
            Role::SyntaxVariable => &mut theme.syntax_variable,
            Role::SyntaxString => &mut theme.syntax_string,
            Role::SyntaxNumber => &mut theme.syntax_number,
            Role::SyntaxType => &mut theme.syntax_type,
            Role::SyntaxOperator => &mut theme.syntax_operator,
            Role::SyntaxPunctuation => &mut theme.syntax_punctuation,
            Role::FooterSeparator => &mut theme.footer_separator,
        }
    }
}

/// One role's style, by theme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: ThemeName,
    /// The model's separate reasoning channel (ADR 0014).
    pub reasoning: Style,
    /// The reasoning level markers (ADR 0027 C2): the footer's `reasoning` item, one role per
    /// level so `/thinking` changes the color.
    pub reasoning_off: Style,
    pub reasoning_minimal: Style,
    pub reasoning_low: Style,
    pub reasoning_medium: Style,
    pub reasoning_high: Style,
    pub reasoning_xhigh: Style,
    pub reasoning_max: Style,
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
    /// The footer's item separator (ADR 0027).
    pub footer_separator: Style,
    /// The `@` file-reference completion list (Phase 4).
    pub completion: Style,
    /// The find match position in the status bar (Phase 5).
    pub find: Style,
    /// The status bar's `AUTO` marker (ADR 0026 D9): the session auto-approves.
    pub auto: Style,
    /// The thin transcript's structural lines (ADR 0029): the open frames' `│ ` /
    /// `╭─` / `╰──`, the `• `/`●` markers, table rules and the compaction rule. The weak level.
    pub frame: Style,
    /// A tool block's head line's tool name (ADR 0029).
    pub tool_name: Style,
    /// A diff line's gutter line number (ADR 0029). The weak level.
    pub diff_line_number: Style,
    /// A tool head's `✓` (and an orchestration panel's `✓`/`●`), ADR 0029.
    pub status_ok: Style,
    /// A tool head's warning marker, ADR 0029.
    pub status_warn: Style,
    /// A tool head's `×`, ADR 0029.
    pub status_error: Style,
    /// A transcript selection (mouse drag). pi's role map: `selectedBg`.
    pub selection: Style,
    /// A tool card's background by state (running / success / failure). pi's role map:
    /// `toolPendingBg`, `toolSuccessBg`, `toolErrorBg`. With no color at all (`mono`) they
    /// carry a modifier instead, so the cards still read apart.
    pub tool_pending_bg: Style,
    pub tool_success_bg: Style,
    pub tool_error_bg: Style,
    /// The main text level (pi's `text`): the assistant's answers and the user's words.
    pub text: Style,
    /// The unified-diff line roles (ADR 0028 R4 + ADR 0029 decision 3): pi's `toolDiffAdded`
    /// / `toolDiffRemoved` / `toolDiffContext`, and `diff_hunk` for the file headers and the
    /// `@@` markers (pi has no role for those; the mapping takes `borderAccent`).
    pub diff_added: Style,
    pub diff_removed: Style,
    pub diff_context: Style,
    pub diff_hunk: Style,
    /// A rendered Mermaid diagram's **nodes** (ADR 0028 R8) — pi's `accent` (强调 = 名字、标签;
    /// a node is a named box).
    pub mermaid_node: Style,
    /// A diagram's **edges**: pi's `dim` (弱 — connectors are structure, not content).
    pub mermaid_edge: Style,
    /// A diagram's plain **text** (labels outside a node): pi's `text` (主).
    pub mermaid_text: Style,
    /// Assistant-text Markdown roles (ADR 0028 R2): headings, links, inline code, quotes,
    /// rules and list bullets.
    pub md_heading: Style,
    /// A heading's `#` mark (ADR 0035).
    pub md_heading_mark: Style,
    pub md_link: Style,
    pub md_link_url: Style,
    pub md_code: Style,
    pub md_quote: Style,
    pub md_quote_border: Style,
    pub md_hr: Style,
    pub md_list_bullet: Style,
    /// Code-block syntax roles (ADR 0028 R2 decision 2): the highlighter's nine roles,
    /// pi's `syntax*` family. Colors come from here, never from a syntect theme.
    ///
    /// The scope → role table (the highlighter's `src/highlight.rs` is the authority; the
    /// README repeats it): `comment*` → comment; `keyword*`/`storage*` → keyword;
    /// `entity.name.function*`/`support.function*`/`entity.name.tag*` → function;
    /// `variable*`/`meta.function.parameters*` → variable; `string*` → string;
    /// `constant.numeric*` → number; `entity.name.type/class/struct/enum/namespace*` and
    /// `support.type/class*` → type; `keyword.operator*` → operator; `punctuation*` →
    /// punctuation; everything else → the code block's own style (`Plain`). Comment and
    /// string outrank the punctuation inside them.
    pub syntax_comment: Style,
    pub syntax_keyword: Style,
    pub syntax_function: Style,
    pub syntax_variable: Style,
    pub syntax_string: Style,
    pub syntax_number: Style,
    pub syntax_type: Style,
    pub syntax_operator: Style,
    pub syntax_punctuation: Style,
    /// A structured notice's label (`profile:`, `[memory]`) — pi's `accent` (ADR 0028 owner
    /// decision: 强调 = 名字、标签).
    pub notice_label: Style,
    /// The rest of a structured notice — pi's `muted` (次). In the built-in themes it stays
    /// the terminal's default foreground, the ui-r1 readability fix; a theme file's `muted`
    /// is what colors it there.
    pub notice_body: Style,
    /// Pi roles no current Radian role consumes — the `md*`, `toolDiff*`, `syntax*` and
    /// `thinking*` families ADR 0027/0028 will wire, plus the panel roles (`scrollbar*`,
    /// `searchMatch*`, `customMessage*`, `toolPendingBg`, `toolSuccessBg`, `toolErrorBg`,
    /// `bashMode`; `selectedBg` is a role now). ADR 0029 decision 3: parsed and kept so a
    /// theme file's values are not thrown away; nothing renders them yet. Keyed by pi's
    /// role name.
    pub reserved: BTreeMap<String, Style>,
}

fn style(color: Option<Color>, modifiers: Modifier) -> Style {
    let mut style = Style::default().add_modifier(modifiers);
    if let Some(color) = color {
        style = style.fg(color);
    }
    style
}

/// A built-in role's foreground from one of pi's own color specs (the owner's 2026-10-14
/// decision: the built-ins take pi's OKHSL values through the ADR 0029 conversion). Without
/// truecolor, RGB becomes the nearest 256-color entry, exactly like a theme file's color.
fn spec(spec: &str, modifiers: Modifier, truecolor: bool) -> Style {
    let color = crate::theme::color::parse_color_value(spec)
        .and_then(|value| crate::theme::file::ratatui_color(value, truecolor))
        .expect("a built-in color spec");
    style(Some(color), modifiers)
}

/// A built-in background from a spec (`bg` only, like a theme file's background channel).
fn spec_bg(spec: &str, modifiers: Modifier, truecolor: bool) -> Style {
    let color = crate::theme::color::parse_color_value(spec)
        .and_then(|value| crate::theme::file::ratatui_color(value, truecolor))
        .expect("a built-in color spec");
    Style::default().add_modifier(modifiers).bg(color)
}

/// pi's dark palette (`dist/modes/interactive/theme/dark.json`, read 2026-10-14). The three
/// text levels are pi's brightness gradient — text 89%, muted 67%, dim 56% — and every
/// semantic color sits at 67%, distinguished by hue, exactly as pi has it.
mod pi_dark {
    pub const TEXT: &str = "okhsl(234 3% 89%)";
    pub const MUTED: &str = "okhsl(229 6% 67%)";
    pub const DIM: &str = "okhsl(229 8% 56%)";
    pub const VIOLET: &str = "okhsl(295 50% 67%)"; // accent
    pub const BLUE: &str = "okhsl(232 54% 67%)"; // syntaxKeyword
    pub const SYNTAX_VARIABLE: &str = "okhsl(202 58% 67%)";
    pub const SYNTAX_STRING: &str = "okhsl(52 67% 67%)";
    pub const GREEN: &str = "okhsl(159 59% 67%)"; // success
    pub const RED: &str = "okhsl(20 72% 67%)"; // error
    pub const YELLOW: &str = "okhsl(83 88% 67%)"; // warning
    pub const THINKING: &str = "okhsl(226 7% 65%)";
    pub const BORDER_ACCENT: &str = "okhsl(295 53% 64%)";
    pub const TOOL_PENDING_BG: &str = "okhsl(229 5% 24%)";
    pub const TOOL_SUCCESS_BG: &str = "okhsl(158 46% 25%)";
    pub const TOOL_ERROR_BG: &str = "okhsl(19 54% 25%)";
    /// pi's `selectedBg` is the same as `userMessageBg`; Radian lifts it a step so a
    /// selection over a user block stays visible (the owner's acceptance), a deliberate
    /// deviation from pi's file.
    pub const SELECTION_BG: &str = "okhsl(233 41% 34%)";
}

/// pi's light palette (`light.json`, same directory, read 2026-10-14).
mod pi_light {
    pub const TEXT: &str = "okhsl(225 5% 27%)";
    pub const MUTED: &str = "okhsl(229 8% 47%)";
    pub const DIM: &str = "okhsl(229 7% 59%)";
    pub const VIOLET: &str = "okhsl(295 60% 46%)";
    pub const BLUE: &str = "okhsl(231 68% 47%)"; // syntaxKeyword
    pub const SYNTAX_VARIABLE: &str = "okhsl(203 73% 46%)";
    pub const SYNTAX_STRING: &str = "okhsl(52 84% 46%)";
    pub const GREEN: &str = "okhsl(159 75% 46%)";
    pub const RED: &str = "okhsl(20 91% 47%)";
    pub const YELLOW: &str = "okhsl(83 99% 47%)";
    pub const THINKING: &str = "okhsl(234 8% 55%)";
    pub const BORDER_ACCENT: &str = "okhsl(295 59% 55%)";
    pub const TOOL_PENDING_BG: &str = "okhsl(248 3% 91%)";
    pub const TOOL_SUCCESS_BG: &str = "okhsl(156 21% 91%)";
    pub const TOOL_ERROR_BG: &str = "okhsl(24 23% 91%)";
    /// One step darker than the user band, same reason as dark's.
    pub const SELECTION_BG: &str = "okhsl(235 19% 81%)";
}

impl Default for Theme {
    fn default() -> Self {
        Theme::dark()
    }
}

impl Theme {
    /// pi's dark palette with truecolor. Kept as the default constructor.
    pub fn dark() -> Self {
        Self::dark_palette(true)
    }

    /// pi's light palette with truecolor.
    pub fn light() -> Self {
        Self::light_palette(true)
    }

    /// The built-in palette for `name`, resolved for the terminal's color capability
    /// (ADR 0029 decision 2: without truecolor, an RGB color becomes the nearest 256-color
    /// entry, the same conversion a theme file's colors go through).
    pub fn builtin(name: ThemeName, truecolor: bool) -> Self {
        match name {
            ThemeName::Dark => Self::dark_palette(truecolor),
            ThemeName::Light => Self::light_palette(truecolor),
            // `File` is only ever a placeholder before the file loads; mono is the safe
            // colorless base.
            ThemeName::Mono | ThemeName::File => Self::mono(),
        }
    }

    /// pi's palette, resolved for this terminal (ADR 0029 decision 2).
    fn dark_palette(truecolor: bool) -> Self {
        use pi_dark as pi;
        let text = spec(pi::TEXT, Modifier::empty(), truecolor);
        let muted = spec(pi::MUTED, Modifier::empty(), truecolor);
        let dim = spec(pi::DIM, Modifier::empty(), truecolor);
        Self {
            name: ThemeName::Dark,
            reserved: BTreeMap::new(),
            // The three text levels: main = answers, user text and tool
            // titles; secondary = tool output, notices and thinking; weak = hints,
            // truncation, counters and completion descriptions.
            text,
            // ADR 0029/6: `user` is a foreground role in the accent
            // colour — the `❯ ` marker and the user's words — with no band.
            user: spec(pi::VIOLET, Modifier::empty(), truecolor),
            // ADR 0035: reasoning is upright now (no italic).
            reasoning: spec(pi::THINKING, Modifier::empty(), truecolor),
            reasoning_off: spec(pi::DIM, Modifier::DIM, truecolor),
            reasoning_minimal: spec(pi::MUTED, Modifier::empty(), truecolor),
            reasoning_low: spec(pi::THINKING, Modifier::empty(), truecolor),
            reasoning_medium: spec(pi::GREEN, Modifier::empty(), truecolor),
            reasoning_high: spec(pi::YELLOW, Modifier::empty(), truecolor),
            reasoning_xhigh: spec(pi::BORDER_ACCENT, Modifier::empty(), truecolor),
            reasoning_max: spec(pi::RED, Modifier::BOLD, truecolor),
            interrupted: spec(pi::YELLOW, Modifier::BOLD, truecolor),
            tool_call: text,
            tool_result: muted,
            tool_result_error: spec(pi::RED, Modifier::empty(), truecolor),
            notice: text,
            notice_label: spec(pi::VIOLET, Modifier::BOLD, truecolor),
            notice_body: muted,
            // A diagram (ADR 0028 R8): nodes are names (accent), edges are structure (dim),
            // plain text is the primary level.
            mermaid_node: spec(pi::VIOLET, Modifier::empty(), truecolor),
            mermaid_edge: dim,
            mermaid_text: text,
            // Markdown (ADR 0028 R2): pi's `md*` roles.
            // ADR 0035: the heading text is the accent, bold (the `#`
            // mark is gone; `md_heading_mark` is kept for the transition).
            md_heading: spec(pi::VIOLET, Modifier::BOLD, truecolor),
            md_heading_mark: dim,
            md_link: spec(pi::VIOLET, Modifier::UNDERLINED, truecolor),
            md_link_url: muted,
            md_code: spec(pi::YELLOW, Modifier::empty(), truecolor),
            md_quote: muted,
            md_quote_border: dim,
            md_hr: dim,
            md_list_bullet: spec(pi::VIOLET, Modifier::empty(), truecolor),
            // Syntax (ADR 0028 R2): pi's `syntax*` values, read from its dark/light themes.
            syntax_comment: muted,
            syntax_keyword: spec(pi::BLUE, Modifier::empty(), truecolor),
            syntax_function: spec(pi::YELLOW, Modifier::empty(), truecolor),
            syntax_variable: spec(pi::SYNTAX_VARIABLE, Modifier::empty(), truecolor),
            syntax_string: spec(pi::SYNTAX_STRING, Modifier::empty(), truecolor),
            syntax_number: spec(pi::GREEN, Modifier::empty(), truecolor),
            syntax_type: spec(pi::VIOLET, Modifier::empty(), truecolor),
            syntax_operator: muted,
            syntax_punctuation: muted,
            footer_separator: dim,
            turn_idle: spec(pi::GREEN, Modifier::empty(), truecolor),
            turn_running: spec(pi::YELLOW, Modifier::empty(), truecolor),
            turn_cancelled: spec(pi::RED, Modifier::empty(), truecolor),
            model: spec(pi::VIOLET, Modifier::empty(), truecolor),
            tier: muted,
            usage: muted,
            mcp: spec(pi::VIOLET, Modifier::empty(), truecolor),
            budget: muted,
            budget_over: spec(pi::YELLOW, Modifier::empty(), truecolor),
            note: spec(pi::YELLOW, Modifier::empty(), truecolor),
            truncation: dim,
            approval_border: spec(pi::BORDER_ACCENT, Modifier::empty(), truecolor),
            hint: dim,
            completion: dim,
            find: spec(pi::VIOLET, Modifier::empty(), truecolor),
            // ADR 0026 D9: the status bar's AUTO marker, reversed so it reads at a glance
            // as "this session behaves differently"; the warning hue underneath.
            auto: spec(pi::YELLOW, Modifier::REVERSED, truecolor),
            // ADR 0029: frame = the dim level; tool_name = the keyword
            // colour; diff_line_number = dim; the status markers reuse the turn/notice/error
            // colours.
            frame: dim,
            tool_name: spec(pi::BLUE, Modifier::empty(), truecolor),
            diff_line_number: dim,
            status_ok: spec(pi::GREEN, Modifier::empty(), truecolor),
            status_warn: spec(pi::YELLOW, Modifier::empty(), truecolor),
            status_error: spec(pi::RED, Modifier::empty(), truecolor),
            // The unified-diff lines: pi's own toolDiff* roles.
            diff_added: spec(pi::GREEN, Modifier::empty(), truecolor),
            diff_removed: spec(pi::RED, Modifier::empty(), truecolor),
            diff_context: muted,
            // ADR 0029/6: `diff_hunk` steps down to the weak level (the
            // thin design draws `@@`/file headers with `frame`).
            diff_hunk: dim,
            selection: spec_bg(pi::SELECTION_BG, Modifier::empty(), truecolor),
            tool_pending_bg: spec_bg(pi::TOOL_PENDING_BG, Modifier::empty(), truecolor),
            tool_success_bg: spec_bg(pi::TOOL_SUCCESS_BG, Modifier::empty(), truecolor),
            tool_error_bg: spec_bg(pi::TOOL_ERROR_BG, Modifier::empty(), truecolor),
        }
    }

    /// pi's palette, resolved for this terminal (ADR 0029 decision 2).
    fn light_palette(truecolor: bool) -> Self {
        use pi_light as pi;
        let text = spec(pi::TEXT, Modifier::empty(), truecolor);
        let muted = spec(pi::MUTED, Modifier::empty(), truecolor);
        let dim = spec(pi::DIM, Modifier::empty(), truecolor);
        Self {
            name: ThemeName::Light,
            reserved: BTreeMap::new(),
            // The three text levels: main = answers, user text and tool
            // titles; secondary = tool output, notices and thinking; weak = hints,
            // truncation, counters and completion descriptions.
            text,
            // ADR 0029/6: `user` is a foreground role in the accent
            // colour — the `❯ ` marker and the user's words — with no band.
            user: spec(pi::VIOLET, Modifier::empty(), truecolor),
            // ADR 0035: reasoning is upright now (no italic).
            reasoning: spec(pi::THINKING, Modifier::empty(), truecolor),
            reasoning_off: spec(pi::DIM, Modifier::DIM, truecolor),
            reasoning_minimal: spec(pi::MUTED, Modifier::empty(), truecolor),
            reasoning_low: spec(pi::THINKING, Modifier::empty(), truecolor),
            reasoning_medium: spec(pi::GREEN, Modifier::empty(), truecolor),
            reasoning_high: spec(pi::YELLOW, Modifier::empty(), truecolor),
            reasoning_xhigh: spec(pi::BORDER_ACCENT, Modifier::empty(), truecolor),
            reasoning_max: spec(pi::RED, Modifier::BOLD, truecolor),
            interrupted: spec(pi::YELLOW, Modifier::BOLD, truecolor),
            tool_call: text,
            tool_result: muted,
            tool_result_error: spec(pi::RED, Modifier::empty(), truecolor),
            notice: text,
            notice_label: spec(pi::VIOLET, Modifier::BOLD, truecolor),
            notice_body: muted,
            // A diagram (ADR 0028 R8): nodes are names (accent), edges are structure (dim),
            // plain text is the primary level.
            mermaid_node: spec(pi::VIOLET, Modifier::empty(), truecolor),
            mermaid_edge: dim,
            mermaid_text: text,
            // Markdown (ADR 0028 R2): pi's `md*` roles.
            // ADR 0035: the heading text is the accent, bold (the `#`
            // mark is gone; `md_heading_mark` is kept for the transition).
            md_heading: spec(pi::VIOLET, Modifier::BOLD, truecolor),
            md_heading_mark: dim,
            md_link: spec(pi::VIOLET, Modifier::UNDERLINED, truecolor),
            md_link_url: muted,
            md_code: spec(pi::YELLOW, Modifier::empty(), truecolor),
            md_quote: muted,
            md_quote_border: dim,
            md_hr: dim,
            md_list_bullet: spec(pi::VIOLET, Modifier::empty(), truecolor),
            // Syntax (ADR 0028 R2): pi's `syntax*` values, read from its dark/light themes.
            syntax_comment: muted,
            syntax_keyword: spec(pi::BLUE, Modifier::empty(), truecolor),
            syntax_function: spec(pi::YELLOW, Modifier::empty(), truecolor),
            syntax_variable: spec(pi::SYNTAX_VARIABLE, Modifier::empty(), truecolor),
            syntax_string: spec(pi::SYNTAX_STRING, Modifier::empty(), truecolor),
            syntax_number: spec(pi::GREEN, Modifier::empty(), truecolor),
            syntax_type: spec(pi::VIOLET, Modifier::empty(), truecolor),
            syntax_operator: muted,
            syntax_punctuation: muted,
            footer_separator: dim,
            turn_idle: spec(pi::GREEN, Modifier::empty(), truecolor),
            turn_running: spec(pi::YELLOW, Modifier::empty(), truecolor),
            turn_cancelled: spec(pi::RED, Modifier::empty(), truecolor),
            model: spec(pi::VIOLET, Modifier::empty(), truecolor),
            tier: muted,
            usage: muted,
            mcp: spec(pi::VIOLET, Modifier::empty(), truecolor),
            budget: muted,
            budget_over: spec(pi::YELLOW, Modifier::empty(), truecolor),
            note: spec(pi::YELLOW, Modifier::empty(), truecolor),
            truncation: dim,
            approval_border: spec(pi::BORDER_ACCENT, Modifier::empty(), truecolor),
            hint: dim,
            completion: dim,
            find: spec(pi::VIOLET, Modifier::empty(), truecolor),
            // ADR 0026 D9: the status bar's AUTO marker, reversed so it reads at a glance
            // as "this session behaves differently"; the warning hue underneath.
            auto: spec(pi::YELLOW, Modifier::REVERSED, truecolor),
            // ADR 0029: frame = the dim level; tool_name = the keyword
            // colour; diff_line_number = dim; the status markers reuse the turn/notice/error
            // colours.
            frame: dim,
            tool_name: spec(pi::BLUE, Modifier::empty(), truecolor),
            diff_line_number: dim,
            status_ok: spec(pi::GREEN, Modifier::empty(), truecolor),
            status_warn: spec(pi::YELLOW, Modifier::empty(), truecolor),
            status_error: spec(pi::RED, Modifier::empty(), truecolor),
            // The unified-diff lines: pi's own toolDiff* roles.
            diff_added: spec(pi::GREEN, Modifier::empty(), truecolor),
            diff_removed: spec(pi::RED, Modifier::empty(), truecolor),
            diff_context: muted,
            // ADR 0029/6: `diff_hunk` steps down to the weak level (the
            // thin design draws `@@`/file headers with `frame`).
            diff_hunk: dim,
            selection: spec_bg(pi::SELECTION_BG, Modifier::empty(), truecolor),
            tool_pending_bg: spec_bg(pi::TOOL_PENDING_BG, Modifier::empty(), truecolor),
            tool_success_bg: spec_bg(pi::TOOL_SUCCESS_BG, Modifier::empty(), truecolor),
            tool_error_bg: spec_bg(pi::TOOL_ERROR_BG, Modifier::empty(), truecolor),
        }
    }

    /// No color at all: roles are told apart by bold / underline / dim (and reverse).
    pub fn mono() -> Self {
        Self {
            name: ThemeName::Mono,
            reserved: BTreeMap::new(),
            // ADR 0035: reasoning is upright now.
            reasoning: style(None, Modifier::DIM),
            reasoning_off: style(None, Modifier::DIM),
            reasoning_minimal: style(None, Modifier::DIM),
            reasoning_low: style(None, Modifier::empty()),
            reasoning_medium: style(None, Modifier::empty()),
            reasoning_high: style(None, Modifier::BOLD),
            reasoning_xhigh: style(None, Modifier::BOLD | Modifier::UNDERLINED),
            reasoning_max: style(None, Modifier::BOLD | Modifier::UNDERLINED),
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
            // ADR 0026 D9: the mono theme has no color, so `AUTO` stands out by reversal.
            auto: style(None, Modifier::REVERSED),
            // ADR 0029, in modifiers: frame and the diff gutter are dim,
            // tool_name plain, the status markers borrow turn_idle/note/tool_result_error.
            frame: style(None, Modifier::DIM),
            tool_name: style(None, Modifier::empty()),
            diff_line_number: style(None, Modifier::DIM),
            status_ok: style(None, Modifier::empty()),
            status_warn: style(None, Modifier::BOLD),
            status_error: style(None, Modifier::BOLD | Modifier::UNDERLINED),
            // No color at all: a selection is the reversed video, as the terminal itself
            // would draw it, and the tool states are told apart by modifiers.
            selection: style(None, Modifier::REVERSED),
            // No colors: the states are modifiers, and none uses REVERSED — that is the
            // selection's, so a selection stays visible over a card.
            tool_pending_bg: style(None, Modifier::DIM),
            tool_success_bg: style(None, Modifier::empty()),
            tool_error_bg: style(None, Modifier::BOLD | Modifier::UNDERLINED),
            notice_label: style(None, Modifier::BOLD),
            notice_body: style(None, Modifier::DIM),
            text: style(None, Modifier::empty()),
            // No color: the diff's own `+`/`-` prefixes distinguish added and removed, so the
            // roles carry no modifier; the hunk/file-header structure is bold.
            diff_added: style(None, Modifier::empty()),
            diff_removed: style(None, Modifier::empty()),
            diff_context: style(None, Modifier::empty()),
            diff_hunk: style(None, Modifier::BOLD),
            // `mono` uses no color: the three roles differ by modifier only.
            mermaid_node: style(None, Modifier::BOLD),
            mermaid_edge: style(None, Modifier::DIM),
            mermaid_text: style(None, Modifier::empty()),
            // `mono` uses modifiers only: bold headings and links, dim quotes/rules/urls.
            md_heading: style(None, Modifier::BOLD),
            md_heading_mark: style(None, Modifier::DIM),
            md_link: style(None, Modifier::UNDERLINED),
            md_link_url: style(None, Modifier::DIM),
            md_code: style(None, Modifier::BOLD),
            md_quote: style(None, Modifier::DIM),
            md_quote_border: style(None, Modifier::DIM),
            md_hr: style(None, Modifier::DIM),
            md_list_bullet: style(None, Modifier::BOLD),
            // `mono` uses no color: the syntax roles are plain, except a comment, which the
            // owner allows one modifier (italic).
            syntax_comment: style(None, Modifier::ITALIC),
            syntax_keyword: style(None, Modifier::empty()),
            syntax_function: style(None, Modifier::empty()),
            syntax_variable: style(None, Modifier::empty()),
            syntax_string: style(None, Modifier::empty()),
            syntax_number: style(None, Modifier::empty()),
            syntax_type: style(None, Modifier::empty()),
            syntax_operator: style(None, Modifier::empty()),
            syntax_punctuation: style(None, Modifier::empty()),
            footer_separator: style(None, Modifier::DIM),
        }
    }

    pub fn for_name(name: ThemeName) -> Self {
        Theme::builtin(name, true)
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

    /// Whether the default pulse can interpolate between its two ends (ADR 0027
    /// decision 3): both ends of the breath — the accent ([`Self::notice_label`]) and the dim
    /// level ([`Self::truncation`]) — must be RGB. A 16/256-colour resolution and `mono` are
    /// not, and keep revision 10's two-colour switch.
    pub fn pulse_can_breathe(&self) -> bool {
        matches!(self.notice_label.fg, Some(Color::Rgb(..)))
            && matches!(self.truncation.fg, Some(Color::Rgb(..)))
    }

    /// Whether `NO_COLOR` is set to a non-empty value (the no-color.org convention).
    pub fn no_color_env() -> bool {
        std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
    }

    /// Every style this theme defines, for the "mono has no colors" test.
    pub fn styles(&self) -> [Style; 43] {
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
            self.selection,
            self.tool_pending_bg,
            self.tool_success_bg,
            self.tool_error_bg,
            self.notice_label,
            self.notice_body,
            self.text,
            self.auto,
            self.frame,
            self.tool_name,
            self.diff_line_number,
            self.status_ok,
            self.status_warn,
            self.status_error,
            self.diff_added,
            self.diff_removed,
            self.diff_context,
            self.diff_hunk,
            self.mermaid_node,
            self.mermaid_edge,
            self.mermaid_text,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0027: only a theme whose two ends of the breath are RGB lets
    /// the pulse interpolate. A 256-colour resolution (an RGB value downgraded for the
    /// terminal) and `mono` fall back to revision 10's two-colour switch.
    #[test]
    fn only_an_rgb_theme_can_breathe() {
        assert!(Theme::dark().pulse_can_breathe());
        assert!(Theme::light().pulse_can_breathe());
        assert!(
            !Theme::builtin(ThemeName::Dark, false).pulse_can_breathe(),
            "without truecolor the ends are indexed colors"
        );
        assert!(!Theme::mono().pulse_can_breathe());
    }

    /// A unique scratch directory per test, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "radian-theme-switch-{}-{label}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `/theme light` loads the built-in, writes the config and says where.
    #[test]
    fn switch_loads_a_builtin_and_saves_it() {
        let scratch = Scratch::new("builtin");
        let config = scratch.file("config.toml");
        std::fs::write(&config, "schema_version = 1\n[tui]\ntheme = \"dark\"\n").unwrap();
        let outcome = switch("light", false, None, &[], Some(&config), false);
        assert_eq!(outcome.theme.expect("a theme").name, ThemeName::Light);
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.contains("theme = \"light\""), "{text}");
        assert!(
            outcome
                .notices
                .iter()
                .any(|n| n.starts_with("theme saved to ")),
            "{:?}",
            outcome.notices
        );
    }

    /// An unknown name is refused: no theme, a notice naming the built-ins.
    #[test]
    fn switch_refuses_an_unknown_name() {
        let scratch = Scratch::new("unknown");
        let config = scratch.file("config.toml");
        let outcome = switch("nope", false, None, &[], Some(&config), false);
        assert!(outcome.theme.is_none());
        assert!(!config.exists(), "a refused switch writes nothing");
        assert!(
            outcome.notices[0].contains("no theme \"nope\""),
            "{:?}",
            outcome.notices
        );
    }

    /// NO_COLOR wins: the switch does nothing and says why.
    #[test]
    fn switch_does_nothing_under_no_color() {
        let scratch = Scratch::new("no-color");
        let config = scratch.file("config.toml");
        let outcome = switch("light", true, None, &[], Some(&config), false);
        assert!(outcome.theme.is_none());
        assert!(!config.exists(), "NO_COLOR must not write the config");
        assert!(
            outcome.notices[0].contains("NO_COLOR"),
            "{:?}",
            outcome.notices
        );
    }

    /// A read-only config applies in the session and reports the refusal.
    #[test]
    fn switch_applies_but_does_not_save_a_read_only_config() {
        let scratch = Scratch::new("read-only");
        let config = scratch.file("config.toml");
        let original = "schema_version = 99\n[tui]\ntheme = \"dark\"\n";
        std::fs::write(&config, original).unwrap();
        let outcome = switch("light", false, None, &[], Some(&config), true);
        assert!(outcome.theme.is_some(), "the theme still applies");
        assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
        assert!(
            outcome.notices.iter().any(|n| n.contains("could not save")),
            "{:?}",
            outcome.notices
        );
    }

    /// A theme file in the themes directory resolves by name.
    #[test]
    fn switch_resolves_a_theme_file() {
        let scratch = Scratch::new("file");
        let themes = scratch.file("themes");
        std::fs::create_dir_all(&themes).unwrap();
        std::fs::write(
            themes.join("my-theme.json"),
            r##"{"name": "my-theme", "colors": {"userMessageText": "#ff0000"}}"##,
        )
        .unwrap();
        let config = scratch.file("config.toml");
        let outcome = switch("my-theme", false, Some(&themes), &[], Some(&config), false);
        assert!(outcome.theme.is_some());
        assert!(config.exists());
    }

    #[test]
    fn names_round_trip_and_unknown_names_are_none() {
        for name in [ThemeName::Dark, ThemeName::Light, ThemeName::Mono] {
            assert_eq!(ThemeName::parse(name.as_str()), Some(name));
        }
        assert_eq!(ThemeName::parse("future"), None);
    }

    /// The built-in dark palette is **pi's**, through the ADR 0029 OKHSL conversion (owner
    /// decision 2026-10-14, replacing ADR 0029's "dark stays byte-identical"): the three text
    /// levels are pi's brightness gradient (89% / 67% / 56%), pinned here by their RGB
    /// values, and every semantic color sits at the 67% level.
    #[test]
    fn the_dark_palette_is_pis_brightness_gradient() {
        let dark = Theme::dark();
        assert_eq!(dark.text.fg, Some(Color::Rgb(222, 224, 225)), "text 89%");
        assert_eq!(
            dark.tool_result.fg,
            Some(Color::Rgb(157, 165, 169)),
            "muted 67%"
        );
        assert_eq!(dark.hint.fg, Some(Color::Rgb(126, 136, 142)), "dim 56%");
        // The main level is what answers and tool titles use; the secondary is the tool
        // output; the weak level is hints, truncation and completion. ADR 0029:
        // `user` is the accent foreground now, not the main text level.
        assert_eq!(dark.user.fg, dark.model.fg);
        assert_eq!(dark.tool_call.fg, dark.text.fg);
        assert_eq!(dark.notice.fg, dark.text.fg);
        assert_eq!(dark.tool_result.fg, dark.notice_body.fg);
        assert_eq!(dark.truncation.fg, dark.hint.fg);
        assert_eq!(dark.completion.fg, dark.hint.fg);
        // The thinking channel is pi's `thinkingText`, between the secondary and weak levels.
        assert_eq!(dark.reasoning.fg, Some(Color::Rgb(150, 160, 164)));
        // ADR 0029: reasoning is upright now, not italic.
        assert!(!dark.reasoning.add_modifier.contains(Modifier::ITALIC));
        // The semantic colors are pi's 67%-level hues: each role takes a named pi color,
        // and none of them is a text level (they are told apart by hue, not brightness).
        for (name, style, source) in [
            ("turn_idle", dark.turn_idle, pi_dark::GREEN),
            ("turn_running", dark.turn_running, pi_dark::YELLOW),
            ("turn_cancelled", dark.turn_cancelled, pi_dark::RED),
            ("model", dark.model, pi_dark::VIOLET),
            ("mcp", dark.mcp, pi_dark::VIOLET),
            ("find", dark.find, pi_dark::VIOLET),
        ] {
            assert_eq!(
                style,
                spec(source, Modifier::empty(), true),
                "{name} takes {source}"
            );
            assert_ne!(style.fg, dark.text.fg, "{name} is not the main level");
            assert_ne!(style.fg, dark.tool_result.fg, "{name} is not the secondary");
        }
    }

    /// ADR 0026 D9's `AUTO` marker keeps its reversal in all three built-ins (it is the
    /// "this session behaves differently" signal); the file mapping gives it pi's `warning`.
    #[test]
    fn the_auto_marker_is_reversed_in_every_builtin() {
        for theme in [Theme::dark(), Theme::light(), Theme::mono()] {
            assert!(
                theme.auto.add_modifier.contains(Modifier::REVERSED),
                "{:?}",
                theme.name
            );
        }
        assert_eq!(
            Theme::dark().auto.fg,
            spec(pi_dark::YELLOW, Modifier::REVERSED, true).fg
        );
        assert_eq!(
            crate::theme::file::fg_source_for_test(Role::Auto),
            Some("warning")
        );
    }

    /// The ui-r1 readability concern (`DarkGray` was too dark on a dark background) is now
    /// satisfied structurally: pi's own levels are readable by construction, and the light
    /// palette is darker text on a light background. Neither level is ANSI `DarkGray`.
    #[test]
    fn the_text_levels_are_readable_in_both_builtins() {
        let dark = Theme::dark();
        assert_ne!(dark.notice.fg, Some(Color::DarkGray));
        assert_eq!(dark.notice.fg, dark.text.fg, "a notice is main-level text");
        assert_eq!(
            dark.notice_body.fg, dark.tool_result.fg,
            "its body secondary"
        );
        let light = Theme::light();
        assert_ne!(light.hint.fg, Some(Color::DarkGray));
        // Light text is darker than the dark palette's text (dark-on-light).
        let (Some(Color::Rgb(dr, _, _)), Some(Color::Rgb(lr, _, _))) =
            (dark.text.fg, light.text.fg)
        else {
            panic!("both palettes are RGB");
        };
        assert!(lr < dr, "the light palette's text is darker");
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

    /// ADR 0035 V3 + ADR 0029: the user message carries no band any more
    /// (the `user` role is a foreground). `mono` has no color to give.
    #[test]
    fn the_user_line_has_no_band() {
        for theme in [Theme::dark(), Theme::light()] {
            assert_eq!(theme.user.bg, None, "{:?}", theme.name);
        }
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

    /// ADR 0029: the thin redesign's new roles exist and parse from their
    /// snake_case names (`[tui.colors]` and the theme format use the same set).
    #[test]
    fn the_thin_roles_parse_from_their_config_names() {
        for role in [
            Role::Frame,
            Role::ToolName,
            Role::DiffLineNumber,
            Role::StatusOk,
            Role::StatusWarn,
            Role::StatusError,
        ] {
            assert_eq!(Role::parse(role.as_str()), Some(role), "{role:?}");
        }
    }

    /// ADR 0029 and 6: one assertion per role whose built-in value
    /// changed, on both built-ins. `frame`/`diff_line_number`/`diff_hunk` are the dim level;
    /// `tool_name` is the keyword colour; the status markers reuse the turn/notice/error
    /// colours; `user` is a foreground role in the accent colour; `md_heading` is the accent
    /// bold; `reasoning` is no longer italic.
    #[test]
    fn the_thin_roles_and_the_changed_builtin_values() {
        for theme in [Theme::dark(), Theme::light()] {
            let dim = theme.hint.fg;
            let accent = theme.model.fg;
            assert_eq!(
                theme.frame.fg, dim,
                "{:?}: frame is the dim level",
                theme.name
            );
            assert_eq!(
                theme.diff_line_number.fg, dim,
                "{:?}: diff_line_number is dim",
                theme.name
            );
            assert_eq!(
                theme.diff_hunk.fg, dim,
                "{:?}: diff_hunk is dim",
                theme.name
            );
            assert_eq!(
                theme.tool_name.fg, theme.syntax_keyword.fg,
                "{:?}: tool_name is the keyword colour",
                theme.name
            );
            assert_eq!(
                theme.status_ok, theme.turn_idle,
                "{:?}: status_ok",
                theme.name
            );
            assert_eq!(
                theme.status_warn, theme.note,
                "{:?}: status_warn",
                theme.name
            );
            assert_eq!(
                theme.status_error, theme.tool_result_error,
                "{:?}: status_error",
                theme.name
            );
            assert_eq!(
                theme.user.fg, accent,
                "{:?}: user is the accent foreground",
                theme.name
            );
            assert_eq!(
                theme.user.bg, None,
                "{:?}: user has no background",
                theme.name
            );
            assert_eq!(
                theme.md_heading.fg, accent,
                "{:?}: md_heading is the accent",
                theme.name
            );
            assert!(
                theme.md_heading.add_modifier.contains(Modifier::BOLD),
                "{:?}: md_heading is bold",
                theme.name
            );
            assert!(
                !theme.reasoning.add_modifier.contains(Modifier::ITALIC),
                "{:?}: reasoning is not italic",
                theme.name
            );
        }
        let mono = Theme::mono();
        assert_eq!(mono.user.bg, None, "user has no background in mono either");
    }
}
