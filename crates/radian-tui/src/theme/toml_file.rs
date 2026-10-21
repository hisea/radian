//! Loading a Radian TOML theme (ADR 0029).
//!
//! A theme file is `~/.radian/themes/<name>.toml`:
//!
//! ```toml
//! name = "tokyo-night"
//! appearance = "dark"          # dark | light; default dark
//!
//! [vars]
//! accent = "#7aa2f7"
//! bg_raised = "okhsl(229 5% 24%)"
//!
//! [roles]
//! text = "#c0caf5"
//! user = "$accent"             # one color: the foreground only
//! frame = { fg = "$accent", dim = true }
//! selection = { bg = "#283457" }
//! ```
//!
//! The value shapes are exactly `[tui.colors]`'s (ADR 0029 decision 5): a bare color string or
//! index is the foreground, a table is `{ fg, bg, bold, italic, dim, underline }`. Both go
//! through `radian_host::config::TuiColorOverride::from_toml`, the one shape parser, and the
//! color grammar is `color::parse_color_value`. On top of that a value may be `$name`, a
//! `[vars]` entry; chains are allowed and a loop only breaks the roles that used it.
//!
//! Leniency is decision 4's: an unknown role name, a bad value, a missing variable or a loop is
//! one warning line each and only the affected role falls back to the built-in palette chosen
//! by `appearance`. A broken file falls back as a whole. Nothing here blocks startup.

use std::collections::{BTreeMap, BTreeSet};

use radian_host::config::{TuiColorOverride, TuiColorValue};
use ratatui::style::Style;

use super::color::{ColorValue, parse_color_value};
use super::file::{apply_modifiers, ratatui_color};
use super::{Role, Theme, ThemeName};

/// Builds the [`Theme`] from one parsed TOML theme text. `name` is the configured theme name
/// (already known from the config), so a missing `name` field in the file is tolerated.
pub(crate) fn build(name: &str, text: &str, truecolor: bool, warnings: &mut Vec<String>) -> Theme {
    let value: toml::Value = match text.parse() {
        Ok(value) => value,
        Err(error) => {
            warnings.push(format!(
                "theme `{name}`: could not parse the TOML ({error}); using built-in dark"
            ));
            return Theme::dark();
        }
    };
    let Some(table) = value.as_table() else {
        warnings.push(format!(
            "theme `{name}`: the top level is not a table; using built-in dark"
        ));
        return Theme::dark();
    };
    let appearance = appearance(name, table, warnings);
    let base = Theme::builtin(appearance, truecolor);
    let Some(roles) = table.get("roles").and_then(|value| value.as_table()) else {
        warnings.push(format!(
            "theme `{name}`: no `[roles]` table; using built-in {appearance}",
            appearance = appearance.as_str()
        ));
        return base;
    };

    let mut variables = Variables::new(table.get("vars").and_then(|value| value.as_table()));
    let mut theme = base;
    let mut unknown: BTreeSet<String> = BTreeSet::new();
    for (role_name, value) in roles {
        let Some(role) = Role::parse(role_name) else {
            unknown.insert(role_name.clone());
            continue;
        };
        let parsed = match TuiColorOverride::from_toml(value) {
            Ok(parsed) => parsed,
            Err(reason) => {
                warnings.push(format!(
                    "theme `{name}`: role `{role_name}`: {reason}; using the built-in color"
                ));
                continue;
            }
        };
        let mut style = *role.style_mut(&mut theme);
        if let Some(channel) = parsed.fg.as_ref() {
            match resolve_channel(&mut variables, channel) {
                Ok(color) => set_fg(&mut style, color, truecolor),
                Err(error) => variables.problem(error),
            }
        }
        if let Some(channel) = parsed.bg.as_ref() {
            match resolve_channel(&mut variables, channel) {
                Ok(color) => set_bg(&mut style, color, truecolor),
                Err(error) => variables.problem(error),
            }
        }
        apply_modifiers(&mut style, &parsed);
        *role.style_mut(&mut theme) = style;
    }

    variables.into_warnings(name, warnings);
    if !unknown.is_empty() {
        warnings.push(format!(
            "theme `{name}`: unknown role(s) {} ignored",
            unknown.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    theme.name = ThemeName::File;
    theme
}

/// The file's `appearance`, `dark` by default; an unknown value warns and assumes `dark`
/// (ADR 0029 decision 4).
fn appearance(name: &str, table: &toml::Table, warnings: &mut Vec<String>) -> ThemeName {
    match table.get("appearance") {
        None => ThemeName::Dark,
        Some(toml::Value::String(text)) if text == "dark" => ThemeName::Dark,
        Some(toml::Value::String(text)) if text == "light" => ThemeName::Light,
        Some(other) => {
            warnings.push(format!(
                "theme `{name}`: appearance {other} is not \"dark\" or \"light\"; assuming dark"
            ));
            ThemeName::Dark
        }
    }
}

/// `TuiColorValue` is a raw string or an index; the loader resolves either into a `ColorValue`.
fn resolve_channel(
    variables: &mut Variables<'_>,
    channel: &TuiColorValue,
) -> Result<ColorValue, VarError> {
    match channel {
        TuiColorValue::Text(text) => variables.color(text),
        TuiColorValue::Index(index) => Ok(ColorValue::Indexed(*index)),
    }
}

fn set_fg(style: &mut Style, color: ColorValue, truecolor: bool) {
    if let Some(color) = ratatui_color(color, truecolor) {
        style.fg = Some(color);
    }
}

fn set_bg(style: &mut Style, color: ColorValue, truecolor: bool) {
    if let Some(color) = ratatui_color(color, truecolor) {
        style.bg = Some(color);
    }
}

/// A `[vars]` value that cannot become a color: reported once and only the roles that want it
/// fall back (ADR 0029 decision 4).
#[derive(Clone)]
enum VarError {
    Missing(String),
    Cycle(String),
    Invalid(String),
}

/// Resolves `$name` references over `[vars]` (ADR 0029). A `[vars]` entry
/// may itself be `$other`, so chains resolve; a loop is an error for that one value.
struct Variables<'a> {
    vars: Option<&'a toml::Table>,
    memo: BTreeMap<String, Result<ColorValue, VarError>>,
    missing: BTreeSet<String>,
    cycles: BTreeSet<String>,
    invalid: BTreeSet<String>,
}

impl<'a> Variables<'a> {
    fn new(vars: Option<&'a toml::Table>) -> Self {
        Self {
            vars,
            memo: BTreeMap::new(),
            missing: BTreeSet::new(),
            cycles: BTreeSet::new(),
            invalid: BTreeSet::new(),
        }
    }

    /// Resolves one `[roles]` channel string: a color literal or a `$variable` reference.
    fn color(&mut self, text: &str) -> Result<ColorValue, VarError> {
        if let Some(name) = text.strip_prefix('$') {
            return self.resolve_var(name, &mut Vec::new());
        }
        parse_color_value(text).ok_or_else(|| VarError::Invalid(text.to_owned()))
    }

    fn resolve_var(&mut self, name: &str, stack: &mut Vec<String>) -> Result<ColorValue, VarError> {
        if stack.iter().any(|seen| seen == name) {
            return Err(VarError::Cycle(name.to_owned()));
        }
        if let Some(cached) = self.memo.get(name) {
            return cached.clone();
        }
        let Some(value) = self.vars.and_then(|vars| vars.get(name)) else {
            return Err(VarError::Missing(name.to_owned()));
        };
        stack.push(name.to_owned());
        let resolved = self.resolve_value(value, stack);
        stack.pop();
        self.memo.insert(name.to_owned(), resolved.clone());
        resolved
    }

    fn resolve_value(
        &mut self,
        value: &toml::Value,
        stack: &mut Vec<String>,
    ) -> Result<ColorValue, VarError> {
        match value {
            toml::Value::String(text) => match text.strip_prefix('$') {
                Some(name) => self.resolve_var(name, stack),
                None => parse_color_value(text).ok_or_else(|| VarError::Invalid(text.clone())),
            },
            toml::Value::Integer(index) if (0..=255).contains(index) => {
                Ok(ColorValue::Indexed(*index as u8))
            }
            other => Err(VarError::Invalid(other.to_string())),
        }
    }

    fn problem(&mut self, error: VarError) {
        match error {
            VarError::Missing(name) => {
                self.missing.insert(name);
            }
            VarError::Cycle(name) => {
                self.cycles.insert(name);
            }
            VarError::Invalid(value) => {
                self.invalid.insert(value);
            }
        }
    }

    fn into_warnings(self, name: &str, warnings: &mut Vec<String>) {
        for variable in self.missing {
            warnings.push(format!(
                "theme `{name}`: unknown variable `${variable}`; the roles that used it fell back"
            ));
        }
        for variable in self.cycles {
            warnings.push(format!(
                "theme `{name}`: circular variable `${variable}`; the roles that used it fell back"
            ));
        }
        if !self.invalid.is_empty() {
            warnings.push(format!(
                "theme `{name}`: invalid color value(s) {}; those roles fell back",
                self.invalid.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    fn loaded(text: &str) -> (Theme, Vec<String>) {
        let mut warnings = Vec::new();
        (build("t", text, true, &mut warnings), warnings)
    }

    fn fg(style: Style) -> Option<Color> {
        style.fg
    }

    /// ADR 0029: `[vars]` resolve (a chain too), a role takes a bare color
    /// or a table, and `$var` reaches both channels.
    #[test]
    fn vars_chains_and_the_role_shapes_resolve() {
        let (theme, warnings) = loaded(
            r##"
name = "t"
appearance = "dark"

[vars]
accent = "#7aa2f7"
bg = "#283457"
alias = "$accent"

[roles]
user = "$alias"
text = { fg = "#c0caf5" }
frame = { fg = "$accent", dim = true }
selection = { bg = "$bg" }
reasoning_off = { fg = "#010203", bold = true, italic = false, underline = true }
"##,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(fg(theme.user), Some(Color::Rgb(0x7a, 0xa2, 0xf7)), "chain");
        assert_eq!(fg(theme.text), Some(Color::Rgb(0xc0, 0xca, 0xf5)));
        assert_eq!(fg(theme.frame), Some(Color::Rgb(0x7a, 0xa2, 0xf7)));
        assert!(
            theme.frame.add_modifier.contains(Modifier::DIM),
            "frame dim"
        );
        // `selection` takes the `$bg` variable on its background channel only.
        assert_eq!(
            theme.selection.bg,
            Some(Color::Rgb(0x28, 0x34, 0x57)),
            "selection bg from $bg"
        );
        assert_eq!(theme.selection.fg, Theme::dark().selection.fg);
        // The bare index shape (a role value of 39).
        let (theme, warnings) = loaded("name = \"t\"\n[roles]\ntool_call = 39\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(fg(theme.tool_call), Some(Color::Indexed(39)));
    }

    /// A cycle only breaks the roles that used it; the rest of the theme lives (decision 4).
    #[test]
    fn a_cycle_only_breaks_its_own_roles() {
        let (theme, warnings) = loaded(
            r##"
[vars]
loop1 = "$loop2"
loop2 = "$loop1"

[roles]
tool_call = "$loop1"
tool_result = "#010203"
"##,
        );
        assert_eq!(fg(theme.tool_result), Some(Color::Rgb(1, 2, 3)));
        assert_eq!(
            theme.tool_call.fg,
            Theme::dark().tool_call.fg,
            "the loop's role falls back to the built-in"
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("circular variable `$loop1`"),
            "{warnings:?}"
        );
    }

    /// A missing variable warns once; a bad value warns and the role keeps the built-in; an
    /// unknown role name is ignored with one warning.
    #[test]
    fn missing_vars_bad_values_and_unknown_roles_are_one_line_each() {
        let (theme, warnings) = loaded(
            r##"
[roles]
tool_call = "$nosuch"
tool_result = "notacolor"
fancy_role = "#010203"
"##,
        );
        assert_eq!(theme.tool_call.fg, Theme::dark().tool_call.fg);
        assert_eq!(theme.tool_result.fg, Theme::dark().tool_result.fg);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("unknown variable `$nosuch`")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("invalid color value(s) notacolor")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("unknown role(s) fancy_role")),
            "{warnings:?}"
        );
    }

    /// `appearance` decides which built-in the untouched roles take (decision 4).
    #[test]
    fn a_missing_role_falls_back_by_appearance() {
        let (dark, _) = loaded("appearance = \"dark\"\n[roles]\ntext = \"#010203\"\n");
        assert_eq!(dark.user.fg, Theme::dark().user.fg);
        let (light, warnings) = loaded("appearance = \"light\"\n[roles]\ntext = \"#010203\"\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(light.user.fg, Theme::light().user.fg);
        assert_eq!(light.name, ThemeName::File);
    }

    /// The `/context` grid roles (spec `tui-context-grid`): missing roles fall
    /// back to the built-in defaults, and a file that names them wins.
    #[test]
    fn the_context_roles_fall_back_and_override() {
        let (theme, warnings) = loaded("[roles]\ntext = \"#010203\"\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            theme.ctx_system.fg,
            Theme::dark().tool_result.fg,
            "system = muted"
        );
        assert_eq!(theme.ctx_tools.fg, Theme::dark().hint.fg, "tools = dim");
        assert_eq!(theme.ctx_free.fg, Theme::dark().frame.fg, "free = frame");
        assert_eq!(theme.ctx_user.fg, Theme::dark().model.fg, "user = accent");
        let (theme, warnings) =
            loaded("[roles]\nctx_system = \"#112233\"\nctx_buffer = \"#445566\"\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(theme.ctx_system.fg, Some(Color::Rgb(0x11, 0x22, 0x33)));
        assert_eq!(theme.ctx_buffer.fg, Some(Color::Rgb(0x44, 0x55, 0x66)));
        // The neighbors the file did not name still fall back.
        assert_eq!(theme.ctx_files.fg, Theme::dark().ctx_files.fg);
    }

    /// A broken file falls back as a whole to `dark` (decision 4).
    #[test]
    fn a_broken_file_falls_back_as_a_whole() {
        let (theme, warnings) = loaded("this is not toml = = =\n");
        assert_eq!(theme.name, ThemeName::Dark);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("could not parse the TOML"),
            "{warnings:?}"
        );
    }

    /// The foreground-only grammar: `""` is the terminal default, `oklch`/`okhsl` convert, and
    /// without truecolor an RGB value becomes the nearest 256-color entry.
    #[test]
    fn the_color_grammar_and_the_256_fallback_apply() {
        let (theme, warnings) =
            loaded("appearance = \"dark\"\n[roles]\nuser = \"oklch(62% 0.1 200)\"\ntext = \"\"\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(fg(theme.user), Some(Color::Rgb(28, 152, 158)));
        assert_eq!(fg(theme.text), Some(Color::Reset));
        let mut warnings = Vec::new();
        let approximated = build("t", "[roles]\nuser = \"#DCD7BA\"\n", false, &mut warnings);
        assert_eq!(approximated.user.fg, Some(Color::Indexed(187)));
    }
}
