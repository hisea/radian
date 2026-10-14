//! Loading a theme file and applying per-role overrides (ADR 0029).
//!
//! A theme file is **pi's theme JSON, read as-is** (ADR 0029 decision 1): `name`,
//! `appearance`, `vars`, `colors`, `export`. Differences from pi, both by decision:
//!
//! - **Leniency** (decision 4): pi invalidates the whole theme on a missing variable or a
//!   circular reference; Radian only falls the *affected roles* back to the built-in palette
//!   (the file's `appearance`, `dark` by default) and warns once per problem. A broken file
//!   falls back as a whole. Nothing here blocks startup.
//! - **Coverage** (decision 3): pi styles its own UI roles; Radian maps them onto its 23
//!   [`Role`]s. The mapping is in [`fg_source`]/[`bg_source`] and pinned by tests. The pi
//!   roles nothing consumes yet (the `md*`, `toolDiff*`, `syntax*`, `thinking*` families and
//!   the panel roles) are parsed into [`Theme::reserved`] so their values survive until
//!   ADR 0027/0028 wire them.
//!
//! Warnings are strings for the startup line; each problem is reported once.

use std::collections::BTreeMap;
use std::path::Path;

use radian_host::config::{TuiColorOverride, TuiColorValue};
use ratatui::style::{Color, Style};

use super::color::{ColorValue, approximate_256, parse_color_value};
use super::{Role, Theme, ThemeName};

/// Every role name pi's theme schema knows (56; the `required` 51 plus the 5 optional).
/// A name outside this list in a theme file is ignored with one warning, and a
/// `[tui.colors]` key outside this list and [`Role::ALL`] is the same.
const PI_ROLES: &[&str] = &[
    "accent",
    "border",
    "borderAccent",
    "borderMuted",
    "success",
    "error",
    "warning",
    "muted",
    "dim",
    "text",
    "thinkingText",
    "selectedBg",
    "scrollbarTrack",
    "scrollbarThumb",
    "searchMatchBg",
    "searchMatchText",
    "userMessageBg",
    "userMessageText",
    "customMessageBg",
    "customMessageText",
    "customMessageLabel",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "toolTitle",
    "toolOutput",
    "mdHeading",
    "mdLink",
    "mdLinkUrl",
    "mdCode",
    "mdCodeBlock",
    "mdCodeBlockBorder",
    "mdQuote",
    "mdQuoteBorder",
    "mdHr",
    "mdListBullet",
    "toolDiffAdded",
    "toolDiffRemoved",
    "toolDiffContext",
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "thinkingMax",
    "bashMode",
];

/// The pi role feeding a Radian role's **foreground** (ADR 0029 decision 3; the mapping is
/// also documented in `crates/radian-tui/README.md`). `None` for the roles pi styles by
/// background only — [`Role::Selection`] keeps whatever foreground the text under it has,
/// and a theme file must not force one on it (the renderer patches the selection over the
/// text's own style).
fn fg_source(role: Role) -> Option<&'static str> {
    match role {
        Role::Reasoning => Some("thinkingText"),
        Role::Interrupted => Some("warning"),
        Role::User => Some("userMessageText"),
        Role::ToolCall => Some("toolTitle"),
        Role::ToolResult => Some("toolOutput"),
        Role::ToolResultError => Some("error"),
        Role::Notice => Some("text"),
        Role::TurnIdle => Some("success"),
        Role::TurnRunning => Some("warning"),
        Role::TurnCancelled => Some("error"),
        Role::Model => Some("accent"),
        Role::Tier => Some("muted"),
        Role::Usage => Some("muted"),
        Role::Mcp => Some("accent"),
        Role::Budget => Some("muted"),
        Role::BudgetOver => Some("warning"),
        Role::Note => Some("warning"),
        Role::Truncation => Some("dim"),
        Role::ApprovalBorder => Some("borderAccent"),
        Role::Hint => Some("muted"),
        Role::Completion => Some("dim"),
        Role::Find => Some("accent"),
        Role::Selection => None,
        // pi has no auto role; the warning role is the closest "this needs attention" color.
        Role::Auto => Some("warning"),
    }
}

/// The pi role feeding a Radian role's **background**. The user line (`userMessageBg`) and
/// the transcript selection (`selectedBg`), ADR 0029 decision 3.
fn bg_source(role: Role) -> Option<&'static str> {
    match role {
        Role::User => Some("userMessageBg"),
        Role::Selection => Some("selectedBg"),
        _ => None,
    }
}

/// The Radian roles a pi role name feeds, and which channel. Most pi names feed one channel
/// of one role; a few feed several (`error` colors both the tool errors and a cancelled
/// turn), which is also how a `[tui.colors]` key by that name applies.
fn targets_for_pi_name(name: &str) -> Vec<(Role, Channel)> {
    let mut targets = Vec::new();
    for role in Role::ALL {
        if fg_source(role) == Some(name) {
            targets.push((role, Channel::Foreground));
        }
        if bg_source(role) == Some(name) {
            targets.push((role, Channel::Background));
        }
    }
    targets
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channel {
    Foreground,
    Background,
}

/// pi's optional roles fall back to these when absent (pi's docs, "Five colors are optional
/// and inherit another color when omitted").
const PI_FALLBACKS: &[(&str, &str)] = &[
    ("scrollbarTrack", "muted"),
    ("scrollbarThumb", "text"),
    ("searchMatchBg", "selectedBg"),
    ("searchMatchText", "text"),
    ("thinkingMax", "thinkingXhigh"),
];

/// Resolves the configured theme: a built-in name, or a theme file next to `themes_dir`
/// (ADR 0029 decisions 1–7), then the `[tui.colors]` overrides. `NO_COLOR` wins over both
/// (decision 7), so it returns plain `mono` and touches nothing else. The warnings are one
/// line per problem, for the startup diagnostics.
pub fn load(
    theme: Option<&str>,
    no_color: bool,
    truecolor: bool,
    themes_dir: Option<&Path>,
    overrides: &[(String, TuiColorOverride)],
) -> (Theme, Vec<String>) {
    if no_color {
        return (Theme::mono(), Vec::new());
    }
    let mut warnings = Vec::new();
    let built_in = theme.and_then(super::ThemeName::parse);
    let mut resolved = match (theme, built_in) {
        (None, _) => Theme::resolve(None, false),
        (Some(_), Some(name)) => Theme::resolve(Some(name), false),
        (Some(name), None) => load_file(name, themes_dir, truecolor, &mut warnings),
    };
    apply_overrides(&mut resolved, overrides, truecolor, &mut warnings);
    (resolved, warnings)
}

/// Loads `<themes_dir>/<name>.json`. A name that cannot be a file (or is pi's reserved
/// `system`) never touches the disk; a missing, unreadable or broken file falls back to the
/// built-in `dark` with one warning. The name itself is already known from the config, so a
/// missing `name` field in the file is tolerated.
fn load_file(
    name: &str,
    themes_dir: Option<&Path>,
    truecolor: bool,
    warnings: &mut Vec<String>,
) -> Theme {
    if name == "system"
        || name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name == ".."
    {
        warnings.push(format!(
            "theme `{name}` is not a usable theme name; using built-in dark"
        ));
        return Theme::dark();
    }
    let Some(dir) = themes_dir else {
        warnings.push(format!(
            "theme `{name}` needs a themes directory (RADIAN_HOME/HOME); using built-in dark"
        ));
        return Theme::dark();
    };
    let path = dir.join(format!("{name}.json"));
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            warnings.push(format!(
                "theme `{name}`: no file {} ; using built-in dark",
                path.display()
            ));
            return Theme::dark();
        }
        Err(error) => {
            warnings.push(format!(
                "theme `{name}`: could not read {} ({error}); using built-in dark",
                path.display()
            ));
            return Theme::dark();
        }
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => build(name, &value, truecolor, warnings),
        Err(error) => {
            warnings.push(format!(
                "theme `{name}`: could not parse {} ({error}); using built-in dark",
                path.display()
            ));
            Theme::dark()
        }
    }
}

/// Builds the [`Theme`] from one parsed theme file. Anything that cannot produce a color
/// (broken variable, unknown role) degrades per role, never as a whole.
fn build(
    name: &str,
    value: &serde_json::Value,
    truecolor: bool,
    warnings: &mut Vec<String>,
) -> Theme {
    let Some(table) = value.as_object() else {
        warnings.push(format!(
            "theme `{name}`: the top level is not an object; using built-in dark"
        ));
        return Theme::dark();
    };
    // The palette the missing roles fall back to: the file's `appearance`, dark by default
    // (ADR 0029 decision 4).
    let appearance = match table.get("appearance") {
        None | Some(serde_json::Value::Null) => ThemeName::Dark,
        Some(serde_json::Value::String(text)) if text == "dark" => ThemeName::Dark,
        Some(serde_json::Value::String(text)) if text == "light" => ThemeName::Light,
        Some(other) => {
            warnings.push(format!(
                "theme `{name}`: appearance {other} is not \"dark\" or \"light\"; assuming dark"
            ));
            ThemeName::Dark
        }
    };
    let base = Theme::for_name(appearance);
    let empty = serde_json::Map::new();
    let vars = table
        .get("vars")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);
    let Some(colors) = table.get("colors").and_then(|v| v.as_object()) else {
        warnings.push(format!(
            "theme `{name}`: no `colors` table; using built-in {appearance}",
            appearance = appearance.as_str()
        ));
        return base;
    };

    let mut problems = Problems::default();
    let mut resolver = VarResolver {
        vars,
        memo: BTreeMap::new(),
    };
    // Resolve every pi role once (missing roles are simply absent), then assemble.
    let mut resolved: BTreeMap<&str, ColorValue> = BTreeMap::new();
    for (role, value) in colors {
        match resolver.resolve(value, &mut Vec::new()) {
            Ok(color) => {
                resolved.insert(role.as_str(), color);
            }
            Err(error) => problems.variable(error),
        }
    }

    let mut theme = base;
    let mut missing: Vec<&'static str> = Vec::new();
    for role in Role::ALL {
        let mut style = *role.style_mut(&mut theme);
        let mut fell_back = false;
        for (channel, source) in [
            (Channel::Foreground, fg_source(role)),
            (Channel::Background, bg_source(role)),
        ] {
            let Some(source) = source else {
                continue;
            };
            let Some(&color) = resolved.get(source) else {
                fell_back = true;
                continue;
            };
            apply_color(&mut style, channel, color, truecolor);
        }
        if fell_back {
            missing.push(role.as_str());
        }
        *role.style_mut(&mut theme) = style;
    }

    // The pi roles no Radian role consumes yet: parse and keep (ADR 0029 decision 3), with
    // pi's documented optional-role fallbacks.
    let mut unknown: Vec<String> = Vec::new();
    for (pi_name, fallback) in PI_FALLBACKS {
        if !resolved.contains_key(pi_name)
            && let Some(&fallback_color) = resolved.get(fallback)
        {
            resolved.insert(pi_name, fallback_color);
        }
    }
    for (pi_name, &color) in &resolved {
        if targets_for_pi_name(pi_name).is_empty() {
            if PI_ROLES.contains(pi_name) {
                let mut style = Style::default();
                apply_color(&mut style, Channel::Foreground, color, truecolor);
                theme.reserved.insert((*pi_name).to_owned(), style);
            } else {
                unknown.push((*pi_name).to_owned());
            }
        }
    }
    problems.into_warnings(name, &missing, &unknown, warnings);
    theme.name = ThemeName::File;
    theme
}

/// One parsed `[tui.colors]` color: a literal, or a 256-color index (pi's grammar).
fn config_color(value: &TuiColorValue) -> Option<ColorValue> {
    match value {
        TuiColorValue::Text(text) => parse_color_value(text),
        TuiColorValue::Index(index) => Some(ColorValue::Indexed(*index)),
    }
}

/// Applies the `[tui.colors]` overrides (ADR 0029 decision 5). Keys are Radian role names
/// and (mapped) pi role names; a `fg`/`bg` string or index is a color, the booleans force a
/// modifier on (`true`) or off (`false`). A malformed entry warns once and is ignored.
fn apply_overrides(
    theme: &mut Theme,
    overrides: &[(String, TuiColorOverride)],
    truecolor: bool,
    warnings: &mut Vec<String>,
) {
    for (key, spec) in overrides {
        let mut warn = |message: String| {
            let line = format!("tui.colors.{key}: {message}");
            if !warnings.contains(&line) {
                warnings.push(line);
            }
        };
        if let Some(role) = Role::parse(key) {
            // A Radian role name may carry both channels.
            let mut targets = vec![(role, Channel::Foreground)];
            if spec.bg.is_some() {
                targets.push((role, Channel::Background));
            }
            apply_override(targets, spec, theme, truecolor, &mut warn);
            continue;
        }
        let targets = targets_for_pi_name(key);
        if !targets.is_empty() {
            // The channels this pi name actually feeds: `userMessageBg` and `selectedBg` are
            // backgrounds, the rest are foregrounds. A set channel the name does not carry
            // would land nowhere, so it is an honest warning (not a silent no-op).
            let carries = |channel: Channel| targets.iter().any(|&(_, target)| target == channel);
            if spec.fg.is_some() && !carries(Channel::Foreground) {
                warn(
                    "this pi name is a background; a bare color (or `fg`) has no effect here"
                        .into(),
                );
            }
            if spec.bg.is_some() && !carries(Channel::Background) {
                warn("this pi name names one channel; a `bg` has no effect here".into());
            }
            apply_override(targets, spec, theme, truecolor, &mut warn);
            continue;
        }
        if PI_ROLES.contains(&key.as_str()) {
            // A future role (ADR 0027/0028): keep the override with the reserved styles.
            let entry = theme.reserved.entry(key.clone()).or_default();
            if let Some(fg) = spec.fg.as_ref().and_then(config_color) {
                set_fg(entry, fg, truecolor);
            }
            if let Some(bg) = spec.bg.as_ref().and_then(config_color) {
                set_bg(entry, bg, truecolor);
            }
            apply_modifiers(entry, spec);
            continue;
        }
        warn("unknown role name; ignored".into());
    }
}

/// Applies one entry. For a Radian role name both channels may be set; for a pi name only
/// the channels that name means (pi roles are single colors), so `targets` carries which
/// channel each role receives from this key.
fn apply_override(
    targets: Vec<(Role, Channel)>,
    spec: &TuiColorOverride,
    theme: &mut Theme,
    truecolor: bool,
    mut warn: impl FnMut(String),
) {
    for (role, channel) in targets {
        let style = role.style_mut(theme);
        let raw = match channel {
            Channel::Foreground => spec.fg.as_ref(),
            Channel::Background => spec.bg.as_ref(),
        };
        if let Some(value) = raw {
            match config_color(value) {
                Some(color) => apply_color(style, channel, color, truecolor),
                // A config override has no `vars` to resolve, so any name here is simply
                // not a color.
                None => warn("not a color (variables do not exist here); ignored".into()),
            }
        }
        apply_modifiers(style, spec);
    }
}

fn apply_modifiers(style: &mut Style, spec: &TuiColorOverride) {
    use ratatui::style::Modifier;
    for (flag, modifier) in [
        (spec.bold, Modifier::BOLD),
        (spec.italic, Modifier::ITALIC),
        (spec.dim, Modifier::DIM),
        (spec.underline, Modifier::UNDERLINED),
    ] {
        match flag {
            Some(true) => *style = style.add_modifier(modifier),
            Some(false) => *style = style.remove_modifier(modifier),
            None => {}
        }
    }
}

fn apply_color(style: &mut Style, channel: Channel, color: ColorValue, truecolor: bool) {
    match channel {
        Channel::Foreground => set_fg(style, color, truecolor),
        Channel::Background => set_bg(style, color, truecolor),
    }
}

fn set_fg(style: &mut Style, color: ColorValue, truecolor: bool) {
    if let Some(color) = ratatui_color(color, truecolor) {
        *style = style.fg(color);
    }
}

fn set_bg(style: &mut Style, color: ColorValue, truecolor: bool) {
    if let Some(color) = ratatui_color(color, truecolor) {
        *style = style.bg(color);
    }
}

/// A theme color becomes a ratatui [`Color`]: `""` is the terminal's default, an index is an
/// index, and RGB is emitted as-is on a truecolor terminal and otherwise approximated to the
/// nearest 256-color entry (ADR 0029 decision 2).
fn ratatui_color(value: ColorValue, truecolor: bool) -> Option<Color> {
    match value {
        ColorValue::Default => Some(Color::Reset),
        ColorValue::Indexed(index) => Some(Color::Indexed(index)),
        ColorValue::Rgb(rgb) => Some(if truecolor {
            Color::Rgb(rgb.r, rgb.g, rgb.b)
        } else {
            Color::Indexed(approximate_256(rgb))
        }),
    }
}

/// Variable-resolution failures, collected so each is reported once.
#[derive(Default)]
struct Problems {
    /// `variable name -> the roles/values that wanted it` (deduplicated by name).
    missing: BTreeMap<String, usize>,
    cycles: BTreeMap<String, usize>,
    invalid: Vec<String>,
}

impl Problems {
    fn variable(&mut self, error: VarError) {
        match error {
            VarError::Missing(name) => {
                *self.missing.entry(name).or_default() += 1;
            }
            VarError::Cycle(name) => {
                *self.cycles.entry(name).or_default() += 1;
            }
            VarError::Invalid(value) => self.invalid.push(value),
        }
    }

    fn into_warnings(
        self,
        name: &str,
        missing_roles: &[&str],
        unknown_roles: &[String],
        warnings: &mut Vec<String>,
    ) {
        for (variable, uses) in self.missing {
            warnings.push(format!(
                "theme `{name}`: unknown variable `{variable}` ({uses} use(s) fell back to the \
                 built-in color)"
            ));
        }
        for (variable, uses) in self.cycles {
            warnings.push(format!(
                "theme `{name}`: circular variable `{variable}` ({uses} use(s) fell back to the \
                 built-in color)"
            ));
        }
        if !self.invalid.is_empty() {
            let list = self.invalid.join(", ");
            warnings.push(format!(
                "theme `{name}`: invalid color value(s) {list}; those roles fell back"
            ));
        }
        if !missing_roles.is_empty() {
            warnings.push(format!(
                "theme `{name}`: missing role(s) {} — using the built-in color",
                missing_roles.join(", ")
            ));
        }
        if !unknown_roles.is_empty() {
            warnings.push(format!(
                "theme `{name}`: unknown role(s) {} ignored",
                unknown_roles.join(", ")
            ));
        }
    }
}

#[derive(Clone)]
enum VarError {
    /// A name that is in no `vars` entry.
    Missing(String),
    /// A chain of variables that loops.
    Cycle(String),
    /// A value that is neither a color nor a usable variable chain.
    Invalid(String),
}

/// Resolves variable references, pi's `resolveVarRefs`: a string that is not a color literal
/// names a `vars` entry, whose value may name another. A missing name or a loop is an error
/// for that one value (ADR 0029 decision 4), not for the theme.
struct VarResolver<'a> {
    vars: &'a serde_json::Map<String, serde_json::Value>,
    memo: BTreeMap<String, Result<ColorValue, VarError>>,
}

impl VarResolver<'_> {
    fn resolve(
        &mut self,
        value: &serde_json::Value,
        stack: &mut Vec<String>,
    ) -> Result<ColorValue, VarError> {
        match value {
            serde_json::Value::Number(number) => {
                // pi's schema: an integer 0..=255 is a palette index, anything else invalid.
                match number.as_u64() {
                    Some(index) if index <= 255 => Ok(ColorValue::Indexed(index as u8)),
                    _ => Err(VarError::Invalid(value.to_string())),
                }
            }
            serde_json::Value::String(text) => match parse_color_value(text) {
                Some(color) => Ok(color),
                None => self.resolve_var(text, stack),
            },
            _ => Err(VarError::Invalid(value.to_string())),
        }
    }

    fn resolve_var(&mut self, name: &str, stack: &mut Vec<String>) -> Result<ColorValue, VarError> {
        if stack.iter().any(|seen| seen == name) {
            return Err(VarError::Cycle(name.to_owned()));
        }
        if let Some(cached) = self.memo.get(name) {
            return cached.clone();
        }
        let Some(value) = self.vars.get(name) else {
            return Err(VarError::Missing(name.to_owned()));
        };
        stack.push(name.to_owned());
        let resolved = self.resolve(value, stack);
        stack.pop();
        self.memo.insert(name.to_owned(), resolved.clone());
        resolved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme_json(vars: &str, colors: &str) -> String {
        format!("{{ \"name\": \"t\", \"vars\": {vars}, \"colors\": {colors} }}")
    }

    fn loaded(text: &str) -> (Theme, Vec<String>) {
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        let mut warnings = Vec::new();
        (build("t", &value, true, &mut warnings), warnings)
    }

    fn fg(style: Style) -> Option<Color> {
        style.fg
    }

    #[test]
    fn hex_and_index_roles_land_on_their_radian_roles() {
        let (theme, warnings) = loaded(&theme_json(
            "{}",
            "{\"userMessageText\": \"#00aaff\", \"toolTitle\": 39}",
        ));
        assert!(
            warnings.iter().all(|w| w.contains("missing role(s)")),
            "{warnings:?}"
        );
        assert_eq!(fg(theme.user), Some(Color::Rgb(0x00, 0xaa, 0xff)));
        assert_eq!(theme.tool_call.fg, Some(Color::Indexed(39)));
    }

    /// ADR 0029 decision 3's example: the user line takes pi's `userMessageText` foreground
    /// and `userMessageBg` background.
    #[test]
    fn the_user_line_takes_pi_text_and_background() {
        let (theme, warnings) = loaded(&theme_json(
            "{}",
            "{\"userMessageText\": \"#ffffff\", \"userMessageBg\": \"#222222\"}",
        ));
        assert!(
            warnings.iter().all(|w| w.contains("missing role(s)")),
            "{warnings:?}"
        );
        assert_eq!(fg(theme.user), Some(Color::Rgb(255, 255, 255)));
        assert_eq!(theme.user.bg, Some(Color::Rgb(0x22, 0x22, 0x22)));
        assert!(
            warnings.iter().all(|w| w.contains("missing role(s)")),
            "{warnings:?}"
        );
    }

    #[test]
    fn chained_variables_resolve_and_a_loop_only_breaks_its_own_roles() {
        let (theme, warnings) = loaded(&theme_json(
            "{\"a\": \"b\", \"b\": \"#112233\", \"loop1\": \"loop2\", \"loop2\": \"loop1\"}",
            "{\"userMessageText\": \"a\", \"toolTitle\": \"loop1\"}",
        ));
        assert_eq!(fg(theme.user), Some(Color::Rgb(0x11, 0x22, 0x33)));
        // The loop's role falls back to the built-in (dark) color; the rest of the theme lives.
        assert_eq!(theme.tool_call.fg, Theme::dark().tool_call.fg);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings[0].starts_with("theme `t`: circular variable `loop1`"),
            "{warnings:?}"
        );
        assert!(warnings[1].contains("missing role(s)"), "{warnings:?}");
    }

    #[test]
    fn a_missing_variable_warns_once_and_only_its_roles_fall_back() {
        let (theme, warnings) = loaded(&theme_json(
            "{}",
            "{\"userMessageText\": \"nosuch\", \"toolTitle\": \"nosuch\", \"toolOutput\": \"#010203\"}",
        ));
        assert_eq!(theme.tool_result.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(theme.user.fg, Theme::dark().user.fg);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings[0].starts_with("theme `t`: unknown variable `nosuch`"),
            "{warnings:?}"
        );
        assert!(warnings[1].contains("missing role(s)"), "{warnings:?}");
    }

    /// ADR 0029 decision 4: a missing role falls back to the built-in palette chosen by
    /// `appearance`.
    #[test]
    fn missing_roles_fall_back_by_appearance() {
        let (dark, warnings) = loaded("{\"name\": \"t\", \"colors\": {\"text\": \"#010203\"}}");
        assert_eq!(dark.notice.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(dark.user.fg, Theme::dark().user.fg);
        assert!(warnings.iter().any(|w| w.contains("missing role(s)")));

        let value: serde_json::Value =
            serde_json::from_str("{\"name\": \"t\", \"appearance\": \"light\", \"colors\": {}}")
                .unwrap();
        let mut warnings = Vec::new();
        let light = build("t", &value, true, &mut warnings);
        assert_eq!(light.user.fg, Theme::light().user.fg);
        assert_eq!(light.name, ThemeName::File);
    }

    #[test]
    fn an_unknown_appearance_warns_and_assumes_dark() {
        let value: serde_json::Value =
            serde_json::from_str("{\"name\": \"t\", \"appearance\": \"sepia\", \"colors\": {}}")
                .unwrap();
        let mut warnings = Vec::new();
        let theme = build("t", &value, true, &mut warnings);
        assert_eq!(theme.user.fg, Theme::dark().user.fg);
        assert!(warnings.iter().any(|w| w.contains("sepia")));
    }

    #[test]
    fn a_broken_file_falls_back_as_a_whole() {
        let (theme, warnings) = load(
            Some("broken"),
            false,
            true,
            Some(Path::new("/nonexistent-themes")),
            &[],
        );
        assert_eq!(theme.name, ThemeName::Dark);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("no file"), "{warnings:?}");
    }

    #[test]
    fn reserved_pi_roles_are_kept_not_dropped() {
        let (theme, warnings) = loaded(&theme_json(
            "{}",
            "{\"mdHeading\": \"#010203\", \"syntaxKeyword\": \"#040506\", \
             \"thinkingXhigh\": \"#070809\", \"selectedBg\": \"#0a0b0c\", \
             \"scrollbarThumb\": \"#0d0e0f\"}",
        ));
        assert!(
            warnings.iter().all(|w| w.contains("missing role(s)")),
            "{warnings:?}"
        );
        assert_eq!(theme.reserved["mdHeading"].fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(
            theme.reserved["syntaxKeyword"].fg,
            Some(Color::Rgb(4, 5, 6))
        );
        assert_eq!(
            theme.reserved["thinkingXhigh"].fg,
            Some(Color::Rgb(7, 8, 9))
        );
        // `selectedBg` is a role now: its color lands on the selection's background, never
        // on its foreground, and it is not kept as a reserved style.
        assert_eq!(theme.selection.bg, Some(Color::Rgb(10, 11, 12)));
        assert_eq!(
            theme.selection.fg, None,
            "selection keeps the text's own fg"
        );
        assert!(!theme.reserved.contains_key("selectedBg"));
        // pi's optional-role fallback: `scrollbarThumb` ← `text` when absent.
        let (with_fallback, _) = loaded(&theme_json(
            "{}",
            "{\"text\": \"#101112\", \"scrollbarThumb\": \"#0d0e0f\"}",
        ));
        assert_eq!(
            with_fallback.reserved["scrollbarThumb"].fg,
            Some(Color::Rgb(13, 14, 15))
        );
        let (fallback_used, _) = loaded(&theme_json("{}", "{\"text\": \"#101112\"}"));
        assert_eq!(
            fallback_used.reserved["scrollbarThumb"].fg,
            Some(Color::Rgb(16, 17, 18))
        );
    }

    #[test]
    fn unknown_roles_and_bad_values_warn_but_do_not_block() {
        let (theme, warnings) = loaded(&theme_json(
            "{}",
            "{\"userMessageText\": \"#010203\", \"fancyNewRole\": \"#040506\", \"toolTitle\": 999}",
        ));
        assert_eq!(fg(theme.user), Some(Color::Rgb(1, 2, 3)));
        assert!(warnings.iter().any(|w| w.contains("fancyNewRole")));
        assert!(warnings.iter().any(|w| w.contains("invalid color")));
        // The invalid role keeps the built-in color.
        assert_eq!(theme.tool_call.fg, Theme::dark().tool_call.fg);
    }

    /// ADR 0029 decision 5: overrides by Radian role name and by pi role name, a bare color
    /// (foreground only) and a table.
    #[test]
    fn overrides_accept_radian_and_pi_names_and_tables() {
        let overrides = |text: &str| -> Vec<(String, TuiColorOverride)> {
            radian_host::config::parse(Path::new("/tmp/c.toml"), text)
                .unwrap()
                .tui
                .colors
        };
        let (theme, warnings) = load(
            None,
            false,
            true,
            None,
            &overrides(concat!(
                "schema_version = 1\n",
                "[tui.colors]\n",
                "user = \"#111111\"\n",
                "tool_call = { fg = \"#222222\", bg = \"#333333\", bold = true }\n",
                "userMessageText = \"#444444\"\n",
                "mdHeading = \"#555555\"\n",
            )),
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        // The first `user` entry lands, then the pi name `userMessageText` (the same role's
        // foreground) overrides it: the later entry wins.
        assert_eq!(fg(theme.user), Some(Color::Rgb(0x44, 0x44, 0x44)));
        assert_eq!(theme.tool_call.fg, Some(Color::Rgb(0x22, 0x22, 0x22)));
        assert_eq!(theme.tool_call.bg, Some(Color::Rgb(0x33, 0x33, 0x33)));
        assert!(
            theme
                .tool_call
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert_eq!(
            theme.reserved["mdHeading"].fg,
            Some(Color::Rgb(0x55, 0x55, 0x55))
        );
    }

    #[test]
    fn malformed_overrides_warn_and_are_ignored() {
        let config = radian_host::config::parse(
            Path::new("/tmp/c.toml"),
            concat!(
                "schema_version = 1\n",
                "[tui.colors]\n",
                "user = \"not-a-color-or-var\"\n",
                "nobody = \"#010203\"\n",
            ),
        )
        .unwrap();
        // Both override entries warn (bad color text; unknown role) and neither applies.
        let (theme, warnings) = load(None, false, true, None, &config.tui.colors);
        assert_eq!(theme.user.fg, Theme::dark().user.fg);
        assert!(
            warnings.iter().any(|w| w.contains("tui.colors.user")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("tui.colors.nobody")),
            "{warnings:?}"
        );
    }

    /// ADR 0029 decision 7: `NO_COLOR` beats the theme file and the overrides.
    #[test]
    fn no_color_beats_theme_files_and_overrides() {
        let (theme, warnings) = load(
            Some("anything"),
            true,
            true,
            Some(Path::new("/nonexistent-themes")),
            &[("user".to_owned(), TuiColorOverride::default())],
        );
        assert!(theme.is_mono());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(theme.reserved.is_empty());
    }

    /// ADR 0029 decision 2: on a terminal without truecolor, RGB becomes the nearest
    /// 256-color entry (kanagawa's foreground `#DCD7BA` → 187).
    #[test]
    fn rgb_is_approximated_to_256_without_truecolor() {
        let (truecolor, _) = loaded(&theme_json("{}", "{\"userMessageText\": \"#DCD7BA\"}"));
        assert_eq!(fg(truecolor.user), Some(Color::Rgb(0xDC, 0xD7, 0xBA)));
        let value: serde_json::Value =
            serde_json::from_str(&theme_json("{}", "{\"userMessageText\": \"#DCD7BA\"}")).unwrap();
        let mut warnings = Vec::new();
        let approximated = build("t", &value, false, &mut warnings);
        assert_eq!(approximated.user.fg, Some(Color::Indexed(187)));
    }

    #[test]
    fn a_non_builtin_name_that_is_not_a_file_warns_once() {
        let (theme, warnings) = load(Some("future"), false, true, None, &[]);
        assert_eq!(theme.name, ThemeName::Dark);
        assert_eq!(warnings.len(), 1);
    }

    /// The real pi-deck fixture loads as-is and its colors land after variable resolution
    /// (ADR 0029 "testing requirements"). Spot-check values straight from the file's `vars`.
    #[test]
    fn the_kanagawa_wave_fixture_loads_with_all_roles_resolved() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/themes/kanagawa-wave.json"
        );
        let text = std::fs::read_to_string(path).expect("the fixture is readable");
        let (theme, warnings) = loaded(&text);
        assert!(warnings.is_empty(), "{warnings:?}");
        // `userMessageText: "fg"` and `vars.fg = "#DCD7BA"`; `userMessageBg: "bg_m2"` →
        // `#181820`. `accent: "accent"` → `#957FB8` (model, mcp, find); `border: "line"` →
        // `#54546D` (approval_border ← borderAccent = line).
        assert_eq!(
            fg(theme.user),
            Some(Color::Rgb(0xDC, 0xD7, 0xBA)),
            "userMessageText → var fg"
        );
        assert_eq!(theme.user.bg, Some(Color::Rgb(0x18, 0x18, 0x20)));
        assert_eq!(theme.model.fg, Some(Color::Rgb(0x95, 0x7F, 0xB8)));
        assert_eq!(theme.find.fg, Some(Color::Rgb(0x95, 0x7F, 0xB8)));
        assert_eq!(theme.approval_border.fg, Some(Color::Rgb(0x54, 0x54, 0x6D)));
        // The transcript selection comes from the file too: `selectedBg: "bg_p2"` →
        // `#363646`, background only (the text keeps its own foreground under it).
        assert_eq!(theme.selection.bg, Some(Color::Rgb(0x36, 0x36, 0x46)));
        assert_eq!(theme.selection.fg, None);
        // Every pi role in the file is either mapped or kept; nothing was dropped.
        assert!(theme.reserved.contains_key("mdHeading"));
        assert!(theme.reserved.contains_key("syntaxKeyword"));
        assert!(theme.reserved.contains_key("thinkingMax"));
        assert!(!theme.reserved.contains_key("selectedBg"));
        assert_eq!(theme.name, ThemeName::File);
    }

    /// The selection is settable everywhere: the three built-ins carry their own values, and
    /// `[tui.colors]` reaches it both by the Radian name (both channels) and by pi's
    /// `selectedBg` (background only — a bare color there warns, it cannot set an fg).
    #[test]
    fn the_selection_role_is_settable_by_builtins_and_overrides() {
        assert_eq!(Theme::dark().selection.bg, Some(Color::DarkGray));
        assert_eq!(Theme::light().selection.bg, Some(Color::Gray));
        let mono = Theme::mono().selection;
        assert_eq!(mono.bg, None);
        assert!(
            mono.add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );

        let overrides = |text: &str| -> Vec<(String, TuiColorOverride)> {
            radian_host::config::parse(Path::new("/tmp/c.toml"), text)
                .unwrap()
                .tui
                .colors
        };
        let (theme, warnings) = load(
            None,
            false,
            true,
            None,
            &overrides("schema_version = 1\n[tui.colors]\nselection = { bg = \"#111213\" }\n"),
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(theme.selection.bg, Some(Color::Rgb(0x11, 0x12, 0x13)));
        assert_eq!(theme.selection.fg, None);

        // pi's `selectedBg` name reaches the same background (file order is unspecified, so
        // each key is its own load).
        let (theme, warnings) = load(
            None,
            false,
            true,
            None,
            &overrides("schema_version = 1\n[tui.colors]\nselectedBg = { bg = \"#212223\" }\n"),
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(theme.selection.bg, Some(Color::Rgb(0x21, 0x22, 0x23)));
        assert_eq!(theme.selection.fg, None);

        // A bare color names a foreground, which `selectedBg` does not carry: warned, and
        // the selection's foreground stays unset.
        let (theme, warnings) = load(
            None,
            false,
            true,
            None,
            &overrides("schema_version = 1\n[tui.colors]\nselectedBg = \"#010203\"\n"),
        );
        assert_eq!(theme.selection.fg, None);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("tui.colors.selectedBg") && w.contains("background")),
            "{warnings:?}"
        );
    }

    /// pi's reserved name and path-like names never touch the filesystem.
    #[test]
    fn unusable_names_fall_back_without_touching_the_filesystem() {
        for name in ["system", "", "..", "a/b", "a\\b"] {
            let (theme, warnings) = load(Some(name), false, true, None, &[]);
            assert_eq!(theme.name, ThemeName::Dark, "{name}");
            assert_eq!(warnings.len(), 1, "{name}: {warnings:?}");
        }
    }
}
