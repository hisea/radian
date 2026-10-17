//! The configuration file (ADR 0008; its location is ADR 0020's `~/.radian/config/config.toml`).
//!
//! Reading is the main job; the reader never rewrites a file it read. The one writer is
//! [`theme_write::write_theme`] (`/theme`, ADR 0008 revision): it edits **only** `tui.theme`
//! through `toml_edit` and refuses when the result would differ in anything else (ADR 0008
//! §5.4's "faithful or refused").
//!
//! The rules the reader enforces (each one is a test):
//!
//! - `schema_version` is required and an integer; `1` is current, a **higher** version is read
//!   in read-only mode with a warning (unknown keys are ignored), a **lower** version is
//!   refused (there is no v0→v1 migration path);
//! - a secret slot's value must be a `SecretRef` table (`from`/`name`); a bare string is
//!   refused, and **only `from = "env"` is supported** this version;
//! - an obviously secret-looking string anywhere is refused (a heuristic, not a boundary);
//! - an unknown `permissions` value is refused (fail-closed); an unknown `tui.*` value is
//!   ignored with a warning;
//! - a missing file is an empty configuration (never created).
//!
//! No error message ever carries a value that could be a secret: the plaintext and heuristic
//! errors name the key path and nothing else.

pub mod migrate_pi;
pub mod theme_write;

use std::fmt;
use std::path::{Path, PathBuf};

use toml::Value;
use toml::map::Map;

/// The `schema_version` this reader understands.
pub const SUPPORTED_SCHEMA_VERSION: u64 = 1;

/// The environment variable that overrides the config path.
pub const RADIAN_CONFIG_ENV: &str = "RADIAN_CONFIG";

/// The file name inside `~/.radian/config/`.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Keys whose bare (string) value is a plaintext secret (ADR 0008 §4.2 rule 1).
const SENSITIVE_KEYS: [&str; 7] = [
    "api_key",
    "apikey",
    "token",
    "secret",
    "password",
    "passwd",
    "authorization",
];

/// The obvious secret prefixes (ADR 0008 §4.2 rule 2, matching the reference checker).
const SECRET_PREFIXES: [&str; 6] = ["sk-", "sk_", "xoxb-", "ghp_", "AKIA", "Bearer "];

/// Why the configuration was refused. The `Display` never contains a secret value.
#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        message: String,
    },
    MissingVersion {
        path: PathBuf,
    },
    VersionTooOld {
        path: PathBuf,
        supported: u64,
        found: i64,
    },
    PlaintextSecret {
        path: PathBuf,
        key: String,
    },
    SecretLookalike {
        path: PathBuf,
        key: String,
    },
    SecretRef {
        path: PathBuf,
        key: String,
        message: String,
    },
    WrongType {
        path: PathBuf,
        key: String,
        expected: &'static str,
    },
    UnknownProfile {
        path: PathBuf,
        value: String,
    },
    /// `[permissions] approval` has a value this build does not know (ADR 0008 §5.1:
    /// `permissions` is fail-closed). Refused, never guessed at.
    UnknownApproval {
        path: PathBuf,
        value: String,
    },
    /// `model.api_key` is no longer a key source (ADR 0026 D2): the key comes only from
    /// `auth.json`.
    ApiKeyRemoved {
        path: PathBuf,
    },
    /// A `[providers.<name>]` entry that can never be used (ADR 0026 D10): the name is not a
    /// valid provider name, or it shadows a built-in id. Refused, never guessed around.
    InvalidProviderName {
        path: PathBuf,
        name: String,
        reason: String,
    },
    /// `[orchestration]` (ADR 0030 slice 9): a wrong-typed key. Refused (the ADR 0008 discipline:
    /// a bad value is never guessed at); an unknown key is only a warning.
    Orchestration {
        path: PathBuf,
        message: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, source } => {
                write!(f, "{}: cannot read: {source}", path.display())
            }
            ConfigError::Parse { path, message } => {
                write!(f, "{}: invalid TOML: {message}", path.display())
            }
            ConfigError::MissingVersion { path } => write!(
                f,
                "{}: missing or non-integer schema_version",
                path.display()
            ),
            ConfigError::VersionTooOld {
                path,
                supported,
                found,
            } => write!(
                f,
                "{}: schema_version {found} is older than the supported v{supported}; there is no \
                 migration for it in this version (the file is never rewritten)",
                path.display()
            ),
            ConfigError::PlaintextSecret { path, key } => write!(
                f,
                "{}: `{key}` must be a SecretRef table ({{ from = \"env\", name = \"…\" }}), \
                 never a plaintext value",
                path.display()
            ),
            ConfigError::SecretLookalike { path, key } => write!(
                f,
                "{}: `{key}` looks like a plaintext secret; configuration files must not contain \
                 secret values",
                path.display()
            ),
            ConfigError::SecretRef { path, key, message } => {
                write!(f, "{}: `{key}`: {message}", path.display())
            }
            ConfigError::WrongType {
                path,
                key,
                expected,
            } => write!(f, "{}: `{key}` must be {expected}", path.display()),
            ConfigError::UnknownProfile { path, value } => write!(
                f,
                "{}: permissions.profile = {value:?} is not one of safe|workspace|full \
                 (or the aliases); refusing rather than guessing",
                path.display()
            ),
            ConfigError::UnknownApproval { path, value } => write!(
                f,
                "{}: permissions.approval = {value:?} is not one of on-ask|always|never|auto \
                 (ADR 0026 D9); refusing rather than guessing",
                path.display()
            ),
            ConfigError::ApiKeyRemoved { path } => write!(
                f,
                "{}: `model.api_key` is no longer read (ADR 0026 D2); the API key comes only \
                 from ~/.radian/credentials/auth.json. Add an entry for the provider there, e.g. \
                 {{\"opencode-go\": {{\"type\": \"api_key\", \"key\": \"$MY_KEY\"}}}}, or use the \
                 `/login` command (next slice)",
                path.display()
            ),
            ConfigError::InvalidProviderName { path, name, reason } => {
                write!(f, "{}: providers.{name}: {reason}", path.display())
            }
            ConfigError::Orchestration { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// The wired model route (ADR 0008 §2). Every field is `None` when the file did not set it;
/// the host then keeps its own default and the command line always wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelConfig {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub headers: Vec<(String, String)>,
    pub context_window: Option<u64>,
    /// `[model] retries` (ADR 0017): how many times a transient model failure is
    /// retried inside one model call. `None` means the host's default
    /// ([`agent_core::DEFAULT_MODEL_RETRIES`]); `0` disables retrying.
    pub retries: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionsConfig {
    pub profile: Option<String>,
    /// `[permissions] approval` (ADR 0012, ADR 0026 D9): how an `ask` resolves. The name
    /// set is `ApprovalPolicy::parse`'s; an unknown value is refused (fail-closed, the
    /// `permissions` rule).
    pub approval: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionConfig {
    pub auto: Option<bool>,
    /// `[compaction] thinking` (ADR 0018, decision 1): the reasoning level the
    /// **summary** call runs at, one of the seven unified levels, or `"current"` to use the
    /// conversation's level (no override). `None` means the default, `low`. The host clamps it
    /// against the route's map and converts it to the provider's wire value; the core only
    /// sees the converted object.
    pub thinking: Option<String>,
}

/// `[skills] extra_paths` (ADR 0025 slice 4, ruling 2): more skill roots, **as written** in the
/// file. The reader keeps the strings; the host resolves them with
/// [`resolve_extra_paths`] (the resolution needs the config file's directory and the user's
/// home, neither of which belongs in a TOML value). A directory that is not listed here is
/// never scanned — in particular `~/.claude/skills` is read only when it appears in this list
/// (ruling 2).
/// `[git]` (ADR 0030 R10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitConfig {
    /// `protected_branches = ["release"]`: branches `git_commit` refuses besides
    /// `main`/`master`.
    pub protected_branches: Vec<String>,
    /// `allowed_filters = ["lfs"]` (ADR 0030, ADR 0039 decision 3): the filter
    /// drivers `git_commit` may run **itself**, with a fixed command — only `lfs` is known. Any
    /// other driver, named here or not, is refused, and no command is ever read from the
    /// repository config. `None` means the default (`["lfs"]`); an empty list allows none.
    pub allowed_filters: Option<Vec<String>>,
}

impl GitConfig {
    /// The effective `allowed_filters`: the configured list, or the default (`["lfs"]`).
    pub fn allowed_filters(&self) -> Vec<String> {
        self.allowed_filters
            .clone()
            .unwrap_or_else(crate::git_commit::default_allowed_filters)
    }
}

/// `[shell]` (ADR 0037): the shell tool's limits a host exposes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellConfig {
    /// `auto_background_secs`: a **foreground** `shell` call still running after this many
    /// seconds is adopted by the job registry in place. `None` means the built-in default
    /// ([`radian_coding::Limits::default`]'s value); `Some(0)` disables adoption.
    pub auto_background_secs: Option<u64>,
}

impl ShellConfig {
    /// The effective adoption threshold a host passes to the session: `None` for
    /// `auto_background_secs = 0` (off), the configured seconds, or the built-in default
    /// ([`radian_coding::Limits`]'s `auto_background_after`) when unset.
    pub fn auto_background_after(&self) -> Option<std::time::Duration> {
        match self.auto_background_secs {
            Some(0) => None,
            Some(secs) => Some(std::time::Duration::from_secs(secs)),
            None => radian_coding::Limits::default().auto_background_after,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillsConfig {
    pub extra_paths: Vec<String>,
    /// `[skills] builtin` (ADR 0025): include the compiled-in skills. `None` means
    /// the default, **on**; the host reads `unwrap_or(true)`.
    pub builtin: Option<bool>,
    /// `[skills] disabled`: built-in skill names to leave out.
    pub disabled: Vec<String>,
}

/// `[memory]` (ADR 0021). Only the two keys this revision adds are read;
/// the rest of the D6 injection table (`enabled`, `max_tokens`, `global_budget`,
/// `project_budget`, `overflow`) is still the documented default and is named in
/// [`Config::skipped`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryConfig {
    /// `[memory.injection] additions`: append later turns' new hits as one System message.
    /// `None` means the default, **on**; the host reads `unwrap_or(true)`.
    pub additions: Option<bool>,
    /// `[memory.injection] addition_budget`: the token cap on one turn's addition. `None`
    /// means the default (500).
    pub addition_budget: Option<u64>,
}

impl MemoryConfig {
    /// The injection configuration this section describes, with the D6 defaults for everything
    /// the file does not set.
    pub fn injection(&self) -> crate::memory::InjectionConfig {
        let defaults = crate::memory::InjectionConfig::default();
        crate::memory::InjectionConfig {
            additions: self.additions.unwrap_or(defaults.additions),
            addition_budget: self.addition_budget.unwrap_or(defaults.addition_budget),
            ..defaults
        }
    }
}

/// One `[tui.colors]` color value (ADR 0029 decision 5): a color literal (`#hex`,
/// `oklch(..)`, `okhsl(..)` or `""` for the terminal default) or a 256-color index.
/// The literal text is resolved by the TUI, which owns the color grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiColorValue {
    Text(String),
    Index(u8),
}

/// One `[tui.colors]` entry (ADR 0029 decision 5): the foreground/background colors and the
/// modifier forces. An absent field leaves the theme's value alone; `bold = false` etc.
/// removes the modifier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiColorOverride {
    pub fg: Option<TuiColorValue>,
    pub bg: Option<TuiColorValue>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub dim: Option<bool>,
    pub underline: Option<bool>,
}

impl TuiColorOverride {
    /// Parses one color entry (ADR 0029 decisions 1 and 5): a bare string or a 0–255 index is
    /// the foreground; a table carries `fg`/`bg` and the `bold`/`italic`/`dim`/`underline`
    /// forces. `Err(reason)` names the first wrong field, so the caller can warn and skip.
    ///
    /// This is the **one** value-shape parser: `[tui.colors]` (ADR 0029 decision 5) and a theme
    /// file's `[roles]` (ADR 0029) both go through it, so the two cannot
    /// drift. The color grammar inside a value (`#hex`, `oklch()`, `okhsl()`, `""`, a variable)
    /// belongs to the TUI and is resolved after this shape is parsed.
    pub fn from_toml(value: &Value) -> Result<Self, String> {
        let mut parsed = Self::default();
        match value {
            Value::String(text) => {
                parsed.fg = Some(TuiColorValue::Text(text.clone()));
            }
            Value::Integer(index) if (0..=255).contains(index) => {
                parsed.fg = Some(TuiColorValue::Index(*index as u8));
            }
            Value::Table(spec) => {
                for (field, field_value) in spec {
                    match (field.as_str(), field_value) {
                        ("fg" | "bg", Value::String(text)) => {
                            let value = TuiColorValue::Text(text.clone());
                            if field == "fg" {
                                parsed.fg = Some(value);
                            } else {
                                parsed.bg = Some(value);
                            }
                        }
                        ("fg" | "bg", Value::Integer(index)) if (0..=255).contains(index) => {
                            let value = TuiColorValue::Index(*index as u8);
                            if field == "fg" {
                                parsed.fg = Some(value);
                            } else {
                                parsed.bg = Some(value);
                            }
                        }
                        ("bold", Value::Boolean(flag)) => parsed.bold = Some(*flag),
                        ("italic", Value::Boolean(flag)) => parsed.italic = Some(*flag),
                        ("dim", Value::Boolean(flag)) => parsed.dim = Some(*flag),
                        ("underline", Value::Boolean(flag)) => parsed.underline = Some(*flag),
                        _ => return Err(format!("`{field}` is not a color field")),
                    }
                }
            }
            _ => {
                return Err(
                    "not a color or a `{ fg, bg, bold, italic, dim, underline }` table".into(),
                );
            }
        }
        Ok(parsed)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiConfig {
    /// `tui.theme`: a built-in name (`dark|light|mono`) or a theme file name (ADR 0029). The
    /// reader accepts any string — whether the name resolves is the TUI's finding, reported
    /// as a startup warning.
    pub theme: Option<String>,
    /// `tui.colors`: per-role color overrides, raw. The TUI validates the role names and
    /// colors; unknown or malformed entries are startup warnings (the `tui.*` rule).
    pub colors: Vec<(String, TuiColorOverride)>,
    /// The raw `tui.keybindings` table: action name → key strings. The TUI parses and
    /// validates it (its key grammar belongs to the terminal layer).
    pub keybindings: Vec<(String, Vec<String>)>,
    /// `tui.mouse`: whether the terminal reports the mouse wheel (ADR 0023 U2, plan B).
    /// `None` means the default, **on**; the TUI reads `unwrap_or(true)`. `false` sends no
    /// mouse sequence at all and leaves the terminal's native selection/copy alone.
    pub mouse: Option<bool>,
    /// `tui.input_style`: `"lines"` (the default: a rule above and below with a `❯ ` prompt) or
    /// `"box"` (the thin half frame, ADR 0035's 2026-10-16 note). Carried raw; the TUI parses it,
    /// and a value that is not `lines`/`box` — including the removed `"bar"` — warns once and
    /// falls back to `lines`, never a refusal (the `tui.*` display-field rule, ADR 0008).
    pub input_style: Option<String>,
    /// `tui.clipboard`: how a transcript-selection copy reaches the system clipboard —
    /// `"osc52"` (the default), `"pbcopy"` (macOS only; elsewhere a warning and OSC 52) or
    /// `"off"`. `None` means the default; an unknown value is ignored with a warning.
    pub clipboard: Option<String>,
    /// `tui.tips`: whether the startup screen shows one Tip line. `None`
    /// means the default, on.
    pub tips: Option<bool>,
    /// `tui.banner`: whether the startup screen shows the wordmark. `None`
    /// means the default, on.
    pub banner: Option<bool>,
    /// `tui.brand`: the brand text (ADR 0027). `None` means the glyph set's default
    /// (`nerd` `󰰠`, `unicode` `R`, `ascii` `rad`). Any string at most 4 display columns wide;
    /// wider is a config error (it would break the footer and the banner).
    pub brand: Option<String>,
    /// `tui.spinner`: `"pulse"` (the default), `"arc"`, `"quadrant"` or `"braille"`
    /// (ADR 0027). The TUI validates it; an unknown value warns and uses `pulse`.
    pub spinner: Option<String>,
    /// `tui.usage_period_secs`: the opencode-go usage refresh period (ADR 0027 C4). `None` means
    /// the default (60 s); `0` disables the contributor; other values are clamped to 15–600 s.
    pub usage_period_secs: Option<u64>,
    /// `tui.display.reasoning`: `"collapsed"` (the default), `"expanded"` or `"hidden"`
    /// (ADR 0028 R3). `None` means the default; an unknown value is ignored with a warning.
    pub display_reasoning: Option<String>,
    /// `tui.display.tool_output`: `"collapsed"`, `"expanded"` or `"errors"` (the default,
    /// ADR 0028 R4). `None` means the default; an unknown value is ignored with a warning.
    pub display_tool_output: Option<String>,
    /// `tui.display.tool_preview_lines`: how many body lines a collapsed tool card shows
    /// (default 5). `None` means the default; a non-positive or non-integer value is skipped
    /// with a warning.
    pub display_tool_preview_lines: Option<u64>,
    /// `tui.display.mermaid` (ADR 0028 R8): draw a closed ```` ```mermaid ```` block as a
    /// Unicode diagram. `None` means the default (`true`); an unknown value is ignored with a
    /// warning.
    pub display_mermaid: Option<bool>,
    /// `tui.display.highlight` (ADR 0028 R2): syntax-highlight a closed code block whose info
    /// string names a common language. `None` means the default (`true`); an unknown value is
    /// ignored with a warning.
    pub display_highlight: Option<bool>,
    /// `[tui.footer]` (ADR 0027 C2): the footer's rows and their items. Carried in a shape the
    /// TUI can consume; the reader validates types and the known top-level keys only. An
    /// unknown item id, placeholder or parameter is the TUI's warning, never a refusal.
    pub footer: TuiFooterConfig,
    /// `[tui.widgets]` (ADR 0027 C3): contributor items shown on a line above or below the
    /// input box, as item ids.
    pub widgets: TuiWidgetsConfig,
}

/// `[tui.widgets]` (ADR 0027 C3): the item ids shown `above_input` and `below_input`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiWidgetsConfig {
    pub above_input: Vec<String>,
    pub below_input: Vec<String>,
}

/// `[tui.footer]` (ADR 0027 C2). Each row has a `left` and a `right` group of items; an item
/// is a bare id string or a table with `item` plus options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiFooterConfig {
    /// `preset`: `"full"` (the default, ADR 0027), `"pi"` or `"classic"`. Any
    /// other value is the TUI's warning.
    pub preset: Option<String>,
    /// `separator`: the glyph(s) between items; a row may override it.
    pub separator: Option<String>,
    /// `separator_style`: the separator's style — a theme role or a color (ADR 0027 revision
    /// 3). The TUI parses it; the default is the `footer_separator` role.
    pub separator_style: Option<String>,
    /// `glyphs`: `"unicode"` (the default), `"nerd"` or `"ascii"`. The TUI validates it.
    pub glyphs: Option<String>,
    /// `context_line`: `"off"` (the default) or `"percentage"`. ADR 0027 item 4:
    /// `percentage` fills the gap between a row's left and right groups with the context
    /// usage (the used fraction in the budget role, the rest in the border role).
    pub context_line: Option<String>,
    /// `transparent`: `false` (the default) or `true`. ADR 0027 item 5 (omp's
    /// `statusLine.transparent`): the footer inherits the terminal's background - every
    /// footer cell's background is cleared, a configured item style's included.
    pub transparent: Option<bool>,
    /// `[tui.footer.items.<id>]`: per-field defaults for one item, applied when a row names the
    /// item by bare id (ADR 0027). A row's inline table wins over this.
    pub items: std::collections::BTreeMap<String, TuiFooterItemConfig>,
    pub rows: Vec<TuiFooterRowConfig>,
}

/// One `[[tui.footer.rows]]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiFooterRowConfig {
    pub left: Vec<TuiFooterItemConfig>,
    pub right: Vec<TuiFooterItemConfig>,
    /// The row's own `separator`, overriding the footer's.
    pub separator: Option<String>,
}

/// One footer item: either a bare id or a table with `item` plus options. Options the reader
/// does not know are kept as text in `params`, so the TUI can warn about (and show) them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiFooterItemConfig {
    pub id: String,
    pub icon: Option<String>,
    /// `symbol_style`: the icon's own style, when it differs from the text's.
    pub symbol_style: Option<String>,
    /// `disabled = true` hides the item everywhere it is named (ADR 0027).
    pub disabled: bool,
    pub format: Option<String>,
    pub style: Option<String>,
    /// The item's priority for narrow-terminal truncation (higher survives longer).
    pub priority: Option<i64>,
    /// `thresholds = [{ at = 70, style = "warn" }, …]` in file order.
    pub thresholds: Vec<(i64, String)>,
    /// The item's own parameters (`width`, `fill`, `empty`, …) as text.
    pub params: Vec<(String, String)>,
}

/// `[sandbox]` (ADR 0016). Only the user-level configuration may set it: a
/// cloned repository must not be able to open a socket for itself. There is no project-level
/// configuration layer today, so that is documented rather than enforced here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxConfig {
    /// Unix sockets the sandbox may connect to, as written (`~` is expanded when the policy
    /// is built; the path is canonicalized then). A path that does not resolve is skipped and
    /// named on the startup line.
    pub allow_unix_sockets: Vec<String>,
}

/// `[integrations]` (ADR 0034 H6): which local integrations are on. `herdr` defaults to
/// **true**: it only takes effect inside a herdr pane (ADR 0034 H1), and `RADIAN_HERDR=0`
/// turns it off for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationsConfig {
    pub herdr: bool,
}

impl Default for IntegrationsConfig {
    fn default() -> Self {
        Self { herdr: true }
    }
}

/// `[control]` (ADR 0030 O6, slice 8): the process control port. `enabled` defaults to
/// **true** — every hosting process binds `$RADIAN_HOME/run/<pid>.sock` — and `false` turns
/// the socket off for this process. The socket carries the same verbs O6 lists; a client can
/// never declare the `User` source and there is no approval verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlConfig {
    pub enabled: bool,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Where a configuration file came from, for the provenance gate (ADR 0027
/// decision 4, ADR 0031). A **project-level** file may have arrived with a cloned repository,
/// so the settings that run code as the user (today only the footer `command` item) are
/// refused from one. There is no project config *loader* today (ADR 0031 verified this): the
/// only way a file can be project-level is that it lives inside the workspace, whichever path
/// selected it. An explicit `--config` path is **not** project-level merely because it is not
/// the default; only its location decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigOrigin {
    /// The user's own configuration: the default `$RADIAN_HOME/config/config.toml`, a file
    /// outside the workspace, or no file at all. Fully trusted.
    #[default]
    User,
    /// A file inside the project workspace (and outside the user config directory). ADR 0031's
    /// gate applies: a project-provided resource is not trusted by default.
    Project,
}

/// Classifies a resolved configuration path (ADR 0027). Pure: the caller
/// passes the workspace and the **user config directory** (`$RADIAN_HOME/config`), so the rule is
/// testable without the environment.
///
/// * no source, or a source not inside `workspace` -> [`ConfigOrigin::User`];
/// * a source inside the user config directory -> [`ConfigOrigin::User`] (the user's own
///   `~/.radian/config/config.toml` must stay trusted even when Radian is run from the home
///   directory, and this is a narrower rule than "under `$RADIAN_HOME`", which would also swallow
///   a workspace under `$RADIAN_HOME/worktrees/`);
/// * any other source inside `workspace` -> [`ConfigOrigin::Project`].
///
/// A path that cannot be canonicalized is compared lexically, so this never fails.
pub fn classify_origin(
    source: Option<&Path>,
    workspace: Option<&Path>,
    user_config_dir: Option<&Path>,
) -> ConfigOrigin {
    let Some(source) = source else {
        return ConfigOrigin::User;
    };
    let Some(workspace) = workspace else {
        return ConfigOrigin::User;
    };
    // Canonicalize where possible so a symlinked workspace or source is compared by identity;
    // fall back to the given paths so a not-yet-existing workspace still classifies.
    let canon = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let source = canon(source);
    let workspace = canon(workspace);
    if !source.starts_with(&workspace) {
        return ConfigOrigin::User;
    }
    if let Some(config_dir) = user_config_dir {
        let config_dir = canon(config_dir);
        if source.starts_with(&config_dir) {
            return ConfigOrigin::User;
        }
    }
    ConfigOrigin::Project
}

/// One parsed configuration file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// The path actually read, or `None` when there was no file.
    pub source: Option<PathBuf>,
    /// Where the file came from (ADR 0027): user-level or a project file
    /// inside the workspace. [`load`] cannot know the workspace, so it reports
    /// [`ConfigOrigin::User`]; [`load_in_workspace`] classifies the path.
    pub origin: ConfigOrigin,
    /// The file's `schema_version`, when read.
    pub schema_version: Option<u64>,
    /// True when the file's version is **higher** than supported: the known keys are
    /// interpreted, unknown ones ignored, and the file must never be rewritten.
    pub read_only: bool,
    pub model: ModelConfig,
    pub permissions: PermissionsConfig,
    pub sandbox: SandboxConfig,
    pub integrations: IntegrationsConfig,
    /// `[control]` (ADR 0030 O6, slice 8): the process control port, on by default.
    pub control: ControlConfig,
    pub compaction: CompactionConfig,
    pub skills: SkillsConfig,
    /// `[memory]` (ADR 0021): the injection keys this revision adds.
    pub memory: MemoryConfig,
    /// `[git]` (ADR 0030 R10): the branches `git_commit` refuses besides `main`/`master`.
    pub git: GitConfig,
    /// `[shell]` (ADR 0037): the shell tool's limits.
    pub shell: ShellConfig,
    /// `[orchestration]` (ADR 0030 slice 9): the user-level orchestration config. A project-level
    /// `<workspace>/.radian/orchestration.toml` overrides it key by key when the workspace is
    /// trusted (ADR 0031).
    pub orchestration: crate::orchestration::OrchestrationConfig,
    pub tui: TuiConfig,
    /// `[providers.<name>]`: named custom services (ADR 0026 D10). Carried raw; the hosts
    /// resolve them through `radian_host::provider::resolve_provider`.
    pub providers: std::collections::BTreeMap<String, crate::provider::ProviderDecl>,
    /// Known-but-unwired and unknown keys, as dotted paths, for the startup line. Never a
    /// value.
    pub skipped: Vec<String>,
}

/// The path a configuration is read from: `explicit` > `RADIAN_CONFIG` > `<root>/config/config.toml`.
/// Pure, so the precedence is testable without the environment.
pub fn resolve_path(
    explicit: Option<&Path>,
    env: Option<&str>,
    radian_home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(path) = explicit.filter(|path| !path.as_os_str().is_empty()) {
        return Some(path.to_path_buf());
    }
    if let Some(path) = env.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    radian_home.map(|root| crate::home::config_dir_for(root).join(CONFIG_FILE_NAME))
}

/// Resolves the path for this process and reads it. A missing file is an empty configuration
/// (it is never created). The command line's `--config` wins over `RADIAN_CONFIG`, which wins
/// over `~/.radian/config/config.toml`.
pub fn load(explicit: Option<&Path>) -> Result<Config, ConfigError> {
    load_in_workspace(explicit, None)
}

/// Like [`load`], but classifies the file's [`ConfigOrigin`] against `workspace`: a config file
/// inside the workspace (and outside `$RADIAN_HOME/config`) is **project-level** (ADR 0027
/// revision 14 decision 4, ADR 0031). The reading behaviour is identical to [`load`].
pub fn load_in_workspace(
    explicit: Option<&Path>,
    workspace: Option<&Path>,
) -> Result<Config, ConfigError> {
    let env = std::env::var(RADIAN_CONFIG_ENV).ok();
    let home = crate::home::radian_home();
    let path = resolve_path(explicit, env.as_deref(), home.as_deref());
    match path {
        None => Ok(Config::default()),
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut config = parse(&path, &text)?;
                let user_config_dir = home.as_deref().map(crate::home::config_dir_for);
                config.origin = classify_origin(
                    config.source.as_deref(),
                    workspace,
                    user_config_dir.as_deref(),
                );
                Ok(config)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(ConfigError::Read {
                path,
                source: error,
            }),
        },
    }
}

/// The path this process reads its configuration from (`--config` > `RADIAN_CONFIG` >
/// `<root>/config/config.toml`), even when the file does not exist yet. [`Config::source`]
/// is `None` in that case; the `/theme` write-back needs the path to create it (ADR 0008
/// revision).
pub fn resolved_path(explicit: Option<&Path>) -> Option<PathBuf> {
    let env = std::env::var(RADIAN_CONFIG_ENV).ok();
    let home = crate::home::radian_home();
    resolve_path(explicit, env.as_deref(), home.as_deref())
}

/// Parses one configuration document. Split out so the fixture consistency test can feed the
/// `docs/schema/config/` files directly.
pub fn parse(path: &Path, text: &str) -> Result<Config, ConfigError> {
    let doc: Value = toml::from_str(text).map_err(|error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let table = doc.as_table().ok_or_else(|| ConfigError::Parse {
        path: path.to_path_buf(),
        message: "the top level is not a table".into(),
    })?;

    // §5.1/§5.2/§5.3: the version is checked before anything else.
    let version = match table.get("schema_version") {
        Some(Value::Integer(version)) => *version,
        _ => {
            return Err(ConfigError::MissingVersion {
                path: path.to_path_buf(),
            });
        }
    };
    if version < SUPPORTED_SCHEMA_VERSION as i64 {
        return Err(ConfigError::VersionTooOld {
            path: path.to_path_buf(),
            supported: SUPPORTED_SCHEMA_VERSION,
            found: version,
        });
    }
    let read_only = version > SUPPORTED_SCHEMA_VERSION as i64;

    // §4.2: plaintext (and lookalike) secrets are refused wherever they appear.
    check_no_plaintext(table, "", path)?;
    check_secret_refs(table, "", path)?;

    let mut config = Config {
        source: Some(path.to_path_buf()),
        schema_version: Some(version as u64),
        read_only,
        ..Config::default()
    };

    for (key, value) in table {
        match key.as_str() {
            "schema_version" => {}
            "model" => parse_model(value, path, read_only, &mut config)?,
            "permissions" => parse_permissions(value, path, read_only, &mut config)?,
            "sandbox" => parse_sandbox(value, path, &mut config)?,
            "integrations" => parse_integrations(value, path, &mut config)?,
            "control" => parse_control(value, path, &mut config)?,
            "compaction" => parse_compaction(value, path, &mut config)?,
            "skills" => parse_skills(value, path, &mut config)?,
            "memory" => parse_memory(value, path, &mut config)?,
            "git" => parse_git(value, &mut config)?,
            "shell" => parse_shell(value, &mut config)?,
            "orchestration" => parse_orchestration(value, path, &mut config)?,
            "tui" => parse_tui(value, path, &mut config)?,
            "providers" => parse_providers(value, path, &mut config)?,
            // Known by the ADR, not wired in this slice: the hosts take MCP servers from
            // `--mcp-config`. Ignored, and named on the startup line.
            "mcp" => config.skipped.push("mcp".into()),
            other => config.skipped.push(other.to_owned()),
        }
    }
    config.skipped.sort();
    config.skipped.dedup();
    Ok(config)
}

/// `[providers.<name>]`: named custom services (ADR 0026 D10). `kind` must be one of the two
/// wire protocols, `base_url` a non-empty string; the name must be valid and must not shadow
/// a built-in id — a bad name is refused, never warned around (the hosts would silently
/// resolve the built-in instead).
fn parse_providers(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "providers", "a table"));
    };
    for (name, value) in table {
        let Some(spec) = value.as_table() else {
            return Err(wrong_type(path, &format!("providers.{name}"), "a table"));
        };
        if let Some(reason) = crate::provider::name_error(name) {
            return Err(ConfigError::InvalidProviderName {
                path: path.to_path_buf(),
                name: name.clone(),
                reason,
            });
        }
        let mut decl = crate::provider::ProviderDecl {
            kind: String::new(),
            base_url: String::new(),
            reasoning_replay: None,
        };
        for (field, value) in spec {
            match (field.as_str(), value) {
                ("kind", Value::String(kind))
                    if matches!(kind.as_str(), "openai-compatible" | "anthropic") =>
                {
                    decl.kind = kind.clone();
                }
                // D10: only the two wire protocols exist in this build; anything else is a
                // structural error, not a display preference to skip.
                ("kind", _) => {
                    return Err(wrong_type(
                        path,
                        &format!("providers.{name}.kind"),
                        "one of openai-compatible|anthropic",
                    ));
                }
                ("base_url", Value::String(base_url)) if !base_url.is_empty() => {
                    decl.base_url = base_url.clone();
                }
                ("base_url", Value::String(_)) => {
                    return Err(ConfigError::InvalidProviderName {
                        path: path.to_path_buf(),
                        name: name.clone(),
                        reason: "base_url must not be empty".into(),
                    });
                }
                ("base_url", _) => {
                    return Err(wrong_type(
                        path,
                        &format!("providers.{name}.base_url"),
                        "a string",
                    ));
                }
                // ADR 0014 Q2: a field name overrides the catalog, `false` turns
                // replay off. `true` names no field, so it is refused rather than guessed.
                ("reasoning_replay", Value::String(field)) if !field.is_empty() => {
                    decl.reasoning_replay = Some(crate::provider::ReasoningReplayOverride::Field(
                        field.clone(),
                    ));
                }
                ("reasoning_replay", Value::Boolean(false)) => {
                    decl.reasoning_replay = Some(crate::provider::ReasoningReplayOverride::Off);
                }
                ("reasoning_replay", _) => {
                    return Err(wrong_type(
                        path,
                        &format!("providers.{name}.reasoning_replay"),
                        "a non-empty string or false",
                    ));
                }
                (other, _) => config.skipped.push(format!("providers.{name}.{other}")),
            }
        }
        config.providers.insert(name.clone(), decl);
    }
    Ok(())
}

fn parse_model(
    value: &Value,
    path: &Path,
    read_only: bool,
    config: &mut Config,
) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "model", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("provider", Value::String(s)) => config.model.provider = Some(s.clone()),
            ("base_url", Value::String(s)) => config.model.base_url = Some(s.clone()),
            ("model", Value::String(s)) => config.model.model = Some(s.clone()),
            // ADR 0026 D2: `model.api_key` is refused, never silently ignored (a silent ignore
            // would make the user think the key took effect).
            ("api_key", _) => {
                return Err(ConfigError::ApiKeyRemoved {
                    path: path.to_path_buf(),
                });
            }
            ("headers", Value::Table(headers)) => {
                for (name, value) in headers {
                    if let Value::String(value) = value {
                        config.model.headers.push((name.clone(), value.clone()));
                    } else {
                        config.skipped.push(format!("model.headers.{name}"));
                    }
                }
            }
            // ADR 0015 (with ADR 0018's "0 or missing is unknown"): a zero window means
            // **unset**, not a real zero-token window — the guard stays off exactly as if the
            // key were absent. It is a valid "off" value in the same shape as `retries = 0`
            // below. A negative or non-integer value is still refused rather than guessed
            // (fail-closed).
            ("context_window", Value::Integer(window)) if *window >= 0 => {
                config.model.context_window = (*window > 0).then_some(*window as u64);
            }
            ("context_window", _) => {
                return Err(wrong_type(
                    path,
                    "model.context_window",
                    "a non-negative integer",
                ));
            }
            // ADR 0017: `[model] retries = 3`; `0` disables retrying. A negative or
            // non-integer value is refused rather than guessed (fail-closed).
            ("retries", Value::Integer(n)) if *n >= 0 => {
                config.model.retries = Some(*n as u32);
            }
            ("retries", _) => {
                return Err(wrong_type(path, "model.retries", "a non-negative integer"));
            }
            // Known by the ADR, not wired here (the host's `--include-usage` controls it).
            ("include_usage", _) => config.skipped.push("model.include_usage".into()),
            (other, _) => {
                if !read_only || table.contains_key(other) {
                    config.skipped.push(format!("model.{other}"));
                }
            }
        }
    }
    Ok(())
}

/// The profile names ADR 0008 §2 lists (`ask`/`allow`/`deny`) plus the tiers ADR 0012/0016
/// define. A value outside this set is refused rather than guessed (fail-closed).
const KNOWN_PROFILES: [&str; 8] = [
    "safe",
    "workspace",
    "workspace-write",
    "full",
    "full-access",
    "ask",
    "allow",
    "deny",
];

fn parse_permissions(
    value: &Value,
    path: &Path,
    read_only: bool,
    config: &mut Config,
) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "permissions", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("profile", Value::String(profile)) => {
                if !KNOWN_PROFILES.contains(&profile.as_str()) {
                    if read_only {
                        // A higher schema_version may define a profile this version does not
                        // know; read-only mode ignores it instead of refusing the file.
                        config.skipped.push("permissions.profile".into());
                    } else {
                        return Err(ConfigError::UnknownProfile {
                            path: path.to_path_buf(),
                            value: profile.clone(),
                        });
                    }
                } else if matches!(
                    profile.as_str(),
                    "safe" | "workspace" | "workspace-write" | "full" | "full-access"
                ) {
                    config.permissions.profile = Some(profile.clone());
                } else {
                    // The ADR's `ask`/`allow`/`deny` names have no tier in this build; ignoring
                    // them keeps the host's default (never widens permissions).
                    config.skipped.push("permissions.profile".into());
                }
            }
            ("approval", Value::String(approval)) => {
                if radian_permissions::ApprovalPolicy::parse(approval).is_none() {
                    if read_only {
                        // A higher schema_version may define a policy this version does not
                        // know; read-only mode ignores it instead of refusing the file.
                        config.skipped.push("permissions.approval".into());
                    } else {
                        // Fail-closed (ADR 0008 §5.1): a `permissions` value this build does
                        // not know is refused, never guessed at.
                        return Err(ConfigError::UnknownApproval {
                            path: path.to_path_buf(),
                            value: approval.clone(),
                        });
                    }
                } else {
                    config.permissions.approval = Some(approval.clone());
                }
            }
            ("approval", _) => {
                return Err(wrong_type(path, "permissions.approval", "a string"));
            }
            (other, _) => config.skipped.push(format!("permissions.{other}")),
        }
    }
    Ok(())
}

fn parse_integrations(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "integrations", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("herdr", Value::Boolean(on)) => config.integrations.herdr = *on,
            ("herdr", _) => {
                return Err(wrong_type(path, "integrations.herdr", "a boolean"));
            }
            (other, _) => config.skipped.push(format!("integrations.{other}")),
        }
    }
    Ok(())
}

/// Parses `[control]` (ADR 0030 O6, slice 8). Only the `enabled` key exists today; an unknown
/// key is reported and ignored (the `tui.*` rule, not refuse).
fn parse_control(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "control", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("enabled", Value::Boolean(on)) => config.control.enabled = *on,
            ("enabled", _) => {
                return Err(wrong_type(path, "control.enabled", "a boolean"));
            }
            (other, _) => config.skipped.push(format!("control.{other}")),
        }
    }
    Ok(())
}

fn parse_sandbox(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "sandbox", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("allow_unix_sockets", Value::Array(items)) => {
                for (index, item) in items.iter().enumerate() {
                    match item {
                        Value::String(entry) if !entry.trim().is_empty() => {
                            config.sandbox.allow_unix_sockets.push(entry.clone())
                        }
                        _ => {
                            return Err(wrong_type(
                                path,
                                &format!("sandbox.allow_unix_sockets[{index}]"),
                                "a non-empty string",
                            ));
                        }
                    }
                }
            }
            ("allow_unix_sockets", _) => {
                return Err(wrong_type(
                    path,
                    "sandbox.allow_unix_sockets",
                    "an array of strings",
                ));
            }
            (other, _) => config.skipped.push(format!("sandbox.{other}")),
        }
    }
    Ok(())
}

fn parse_compaction(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "compaction", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("auto", Value::Boolean(auto)) => config.compaction.auto = Some(*auto),
            ("auto", _) => return Err(wrong_type(path, "compaction.auto", "a boolean")),
            ("thinking", Value::String(level))
                if level == crate::COMPACTION_THINKING_CURRENT
                    || crate::THINKING_LEVELS.contains(&level.as_str()) =>
            {
                config.compaction.thinking = Some(level.clone());
            }
            ("thinking", Value::String(_)) => {
                return Err(wrong_type(
                    path,
                    "compaction.thinking",
                    "one of off|minimal|low|medium|high|xhigh|max, or \"current\"",
                ));
            }
            ("thinking", _) => return Err(wrong_type(path, "compaction.thinking", "a string")),
            (other, _) => config.skipped.push(format!("compaction.{other}")),
        }
    }
    Ok(())
}

/// `[skills] extra_paths = ["…"]` (ADR 0025 slice 4). The strings are kept raw; the host
/// resolves them with [`resolve_extra_paths`]. A non-string or empty entry is a structural
/// error (an empty path would silently resolve to the config directory); an unknown key under
/// `[skills]` is skipped and named on the startup line, like every other unknown key.
/// Parses `[git]` (ADR 0030 R10). An unknown key or a non-string entry is skipped, never a
/// refusal (the `tui.*` rule's spirit: a preference never blocks startup).
fn parse_git(value: &Value, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Ok(());
    };
    for (key, value) in table {
        if let ("protected_branches", Value::Array(branches)) = (key.as_str(), value) {
            for branch in branches {
                if let Value::String(branch) = branch {
                    config.git.protected_branches.push(branch.clone());
                }
            }
        }
        // ADR 0030: the filter drivers `git_commit` may run. `[]` allows none.
        if let ("allowed_filters", Value::Array(filters)) = (key.as_str(), value) {
            let mut names = Vec::with_capacity(filters.len());
            for filter in filters {
                if let Value::String(filter) = filter {
                    names.push(filter.clone());
                }
            }
            config.git.allowed_filters = Some(names);
        }
    }
    Ok(())
}

/// Parses `[shell]` (ADR 0037). A non-integer or negative `auto_background_secs`
/// (or an unknown key) is skipped and named on the startup line, never a refusal (the
/// preferences rule: a bad limit must not block startup).
fn parse_shell(value: &Value, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Ok(());
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("auto_background_secs", Value::Integer(secs)) if *secs >= 0 => {
                config.shell.auto_background_secs = Some(*secs as u64);
            }
            ("auto_background_secs", _) => {
                config.skipped.push("shell.auto_background_secs".into());
            }
            (other, _) => config.skipped.push(format!("shell.{other}")),
        }
    }
    Ok(())
}

/// Parses `[orchestration]` (ADR 0030 slice 9, O7/O8). The keys are shared with the project-level
/// `<workspace>/.radian/orchestration.toml` (see [`crate::orchestration`]). A wrong-typed key is
/// refused (never guessed at); an unknown key is skipped and named on the startup line (the
/// preferences rule).
fn parse_orchestration(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    match crate::orchestration::OrchestrationConfig::from_table(value) {
        Ok(parsed) => {
            config.skipped.extend(parsed.skipped.iter().cloned());
            config.orchestration = parsed;
            Ok(())
        }
        Err(message) => Err(ConfigError::Orchestration {
            path: path.to_path_buf(),
            message,
        }),
    }
}

fn parse_memory(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "memory", "a table"));
    };
    for (key, value) in table {
        match key.as_str() {
            "injection" => {
                let Some(injection) = value.as_table() else {
                    return Err(wrong_type(path, "memory.injection", "a table"));
                };
                for (key, value) in injection {
                    match (key.as_str(), value) {
                        ("additions", Value::Boolean(on)) => {
                            config.memory.additions = Some(*on);
                        }
                        ("additions", _) => {
                            return Err(wrong_type(
                                path,
                                "memory.injection.additions",
                                "a boolean",
                            ));
                        }
                        ("addition_budget", Value::Integer(budget)) if *budget > 0 => {
                            config.memory.addition_budget = Some(*budget as u64);
                        }
                        ("addition_budget", _) => {
                            return Err(wrong_type(
                                path,
                                "memory.injection.addition_budget",
                                "a positive integer",
                            ));
                        }
                        // The rest of the D6 table is known but not wired in this slice: it is
                        // interpreted as the documented default and named on the startup line.
                        (other, _) => config.skipped.push(format!("memory.injection.{other}")),
                    }
                }
            }
            other => config.skipped.push(format!("memory.{other}")),
        }
    }
    Ok(())
}

fn parse_skills(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "skills", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("extra_paths", Value::Array(items)) => {
                for (index, item) in items.iter().enumerate() {
                    match item {
                        Value::String(entry) if !entry.trim().is_empty() => {
                            config.skills.extra_paths.push(entry.clone());
                        }
                        _ => {
                            return Err(wrong_type(
                                path,
                                &format!("skills.extra_paths[{index}]"),
                                "a non-empty string",
                            ));
                        }
                    }
                }
            }
            ("extra_paths", _) => {
                return Err(wrong_type(
                    path,
                    "skills.extra_paths",
                    "an array of strings",
                ));
            }
            ("builtin", Value::Boolean(on)) => config.skills.builtin = Some(*on),
            ("builtin", _) => {
                return Err(wrong_type(path, "skills.builtin", "a boolean"));
            }
            ("disabled", Value::Array(items)) => {
                for (index, item) in items.iter().enumerate() {
                    match item {
                        Value::String(name) if !name.trim().is_empty() => {
                            config.skills.disabled.push(name.clone());
                        }
                        _ => {
                            return Err(wrong_type(
                                path,
                                &format!("skills.disabled[{index}]"),
                                "a non-empty string",
                            ));
                        }
                    }
                }
            }
            ("disabled", _) => {
                return Err(wrong_type(path, "skills.disabled", "an array of strings"));
            }
            (other, _) => config.skipped.push(format!("skills.{other}")),
        }
    }
    Ok(())
}

/// Turns `[skills] extra_paths` into filesystem paths (ADR 0025 slice 4). In order:
///
/// - a leading `~` (or exactly `~`) is the user's home;
/// - a relative path is relative to the **config file's directory** — the list lives next to
///   the config that declares it, not next to the workspace;
/// - an absolute path is kept.
///
/// Pure, so the rules are testable without touching the environment. A `~` with no home is left
/// alone (it will then be reported as a missing directory, never silently expanded).
pub fn resolve_extra_paths(
    raw: &[String],
    config_dir: Option<&Path>,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    raw.iter()
        .map(|entry| resolve_extra_path(entry, config_dir, home))
        .collect()
}

fn resolve_extra_path(entry: &str, config_dir: Option<&Path>, home: Option<&Path>) -> PathBuf {
    let tilde = entry == "~" || entry.starts_with("~/");
    let expanded = if entry == "~" {
        home.map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(entry))
    } else if let Some(rest) = entry.strip_prefix("~/") {
        match home {
            Some(home) => home.join(rest),
            None => PathBuf::from(entry),
        }
    } else {
        PathBuf::from(entry)
    };
    // An unexpanded `~` is left as written (and reported as a missing directory), never joined
    // to the config directory: `~/.claude/skills` is a clearer warning than a path under `~/.radian/config/`.
    if expanded.is_absolute() || tilde {
        expanded
    } else if let Some(dir) = config_dir {
        dir.join(expanded)
    } else {
        expanded
    }
}

/// Expands `~` and canonicalizes `[sandbox] allow_unix_sockets` (ADR 0016). Only
/// absolute paths are accepted (a relative one is skipped, never joined to the config dir:
/// the sandbox rule must name exactly one socket). Returns the canonical paths and one
/// warning per entry that was skipped. Pure over `home`, so the rules are testable.
pub fn resolve_allow_unix_sockets(
    raw: &[String],
    home: Option<&Path>,
) -> (Vec<PathBuf>, Vec<String>) {
    let mut paths = Vec::new();
    let mut warnings = Vec::new();
    for entry in raw {
        let expanded = if entry == "~" {
            home.map(Path::to_path_buf)
        } else if let Some(rest) = entry.strip_prefix("~/") {
            home.map(|home| home.join(rest))
        } else if entry.starts_with('~') {
            // `~user` is not expanded.
            None
        } else {
            Some(PathBuf::from(entry))
        };
        let Some(expanded) = expanded else {
            warnings.push(format!(
                "sandbox.allow_unix_sockets: {entry}: cannot expand the home directory"
            ));
            continue;
        };
        if !expanded.is_absolute() {
            warnings.push(format!(
                "sandbox.allow_unix_sockets: {entry}: must be an absolute path"
            ));
            continue;
        }
        match std::fs::canonicalize(&expanded) {
            Ok(path) => {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
            Err(error) => warnings.push(format!("sandbox.allow_unix_sockets: {entry}: {error}")),
        }
    }
    (paths, warnings)
}

fn parse_tui(value: &Value, path: &Path, config: &mut Config) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "tui", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            // Any name is accepted (ADR 0029 decision 1): built-in names resolve directly,
            // anything else is looked up in `~/.radian/themes/<name>.toml` by the TUI, which
            // warns and falls back when neither exists. A display preference never refuses
            // the file.
            ("theme", Value::String(theme)) => config.tui.theme = Some(theme.clone()),
            ("theme", _) => config.skipped.push("tui.theme".into()),
            ("colors", Value::Table(colors)) => {
                for (key, value) in colors {
                    // The one value-shape parser (ADR 0029 decisions 1/5); a malformed entry is
                    // a display preference: skipped by name, never a refusal.
                    match TuiColorOverride::from_toml(value) {
                        Ok(parsed) => config.tui.colors.push((key.clone(), parsed)),
                        Err(_) => config.skipped.push(format!("tui.colors.{key}")),
                    }
                }
            }
            ("colors", _) => return Err(wrong_type(path, "tui.colors", "a table")),
            ("keybindings", Value::Table(bindings)) => {
                for (action, keys) in bindings {
                    match keys.as_array() {
                        Some(keys) => {
                            let keys: Vec<String> = keys
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect();
                            config.tui.keybindings.push((action.clone(), keys));
                        }
                        None => config.skipped.push(format!("tui.keybindings.{action}")),
                    }
                }
            }
            ("keybindings", _) => return Err(wrong_type(path, "tui.keybindings", "a table")),
            ("mouse", Value::Boolean(mouse)) => config.tui.mouse = Some(*mouse),
            // The input style is a display preference carried raw (like `tui.spinner`): the TUI
            // parses it, so an unknown value (the removed `bar` included) warns there, with the
            // exact spelling, and falls back to `lines` -- never a refusal (ADR 0008).
            ("input_style", Value::String(style)) => config.tui.input_style = Some(style.clone()),
            ("input_style", _) => config.skipped.push("tui.input_style".into()),
            ("footer", Value::Table(footer)) => {
                if let Err(key) = parse_footer(footer, config) {
                    config.skipped.push(key);
                }
            }
            ("footer", _) => config.skipped.push("tui.footer".into()),
            ("widgets", Value::Table(widgets)) => {
                if let Err(key) = parse_widgets(widgets, config) {
                    config.skipped.push(key);
                }
            }
            ("widgets", _) => config.skipped.push("tui.widgets".into()),
            ("usage_period_secs", Value::Integer(seconds)) => {
                config.tui.usage_period_secs = Some(*seconds as u64);
            }
            ("display", Value::Table(display)) => {
                for (key, value) in display {
                    match (key.as_str(), value) {
                        ("reasoning", Value::String(mode))
                            if matches!(mode.as_str(), "collapsed" | "expanded" | "hidden") =>
                        {
                            config.tui.display_reasoning = Some(mode.clone());
                        }
                        ("tool_output", Value::String(mode))
                            if matches!(mode.as_str(), "collapsed" | "expanded" | "errors") =>
                        {
                            config.tui.display_tool_output = Some(mode.clone());
                        }
                        ("tool_preview_lines", Value::Integer(lines)) if *lines > 0 => {
                            config.tui.display_tool_preview_lines = Some(*lines as u64);
                        }
                        ("mermaid", Value::Boolean(on)) => {
                            config.tui.display_mermaid = Some(*on);
                        }
                        ("highlight", Value::Boolean(on)) => {
                            config.tui.display_highlight = Some(*on);
                        }
                        // A display preference: skip the key with a warning, never refuse the
                        // file. The TUI keeps its default.
                        _ => config.skipped.push(format!("tui.display.{key}")),
                    }
                }
            }
            ("display", _) => config.skipped.push("tui.display".into()),
            ("clipboard", Value::String(mode))
                if matches!(mode.as_str(), "osc52" | "pbcopy" | "off") =>
            {
                config.tui.clipboard = Some(mode.clone());
            }
            // An unknown clipboard transport is a display preference too: skip it with the
            // same warning, and the TUI keeps its default.
            ("clipboard", _) => config.skipped.push("tui.clipboard".into()),
            // A non-boolean mouse is a display preference: ignore it (and say so), never
            // refuse the file.
            ("mouse", _) => config.skipped.push("tui.mouse".into()),
            ("tips", Value::Boolean(tips)) => config.tui.tips = Some(*tips),
            // A non-boolean tips flag is the same kind of display preference.
            ("tips", _) => config.skipped.push("tui.tips".into()),
            ("banner", Value::Boolean(banner)) => config.tui.banner = Some(*banner),
            ("banner", _) => config.skipped.push("tui.banner".into()),
            // ADR 0027: the brand is any short text (≤ 4 display columns). Wider is
            // a config error, not a skipped key: it would misalign the footer and the banner.
            ("brand", Value::String(brand)) => {
                if unicode_width::UnicodeWidthStr::width(brand.as_str()) > 4 {
                    return Err(wrong_type(
                        path,
                        "tui.brand",
                        "a string at most 4 display columns wide",
                    ));
                }
                config.tui.brand = Some(brand.clone());
            }
            // A non-string brand is a display preference: skip it with a warning.
            ("brand", _) => config.skipped.push("tui.brand".into()),
            // ADR 0027: the spinner kind is a display preference; the TUI
            // validates the name and falls back with a warning.
            ("spinner", Value::String(spinner)) => config.tui.spinner = Some(spinner.clone()),
            ("spinner", _) => config.skipped.push("tui.spinner".into()),
            (other, _) => config.skipped.push(format!("tui.{other}")),
        }
    }
    Ok(())
}

/// Parses `[tui.widgets]` (ADR 0027 C3). A non-string entry is skipped with a warning.
fn parse_widgets(table: &toml::map::Map<String, Value>, config: &mut Config) -> Result<(), String> {
    let mut widgets = TuiWidgetsConfig::default();
    for (key, value) in table {
        let slot = match key.as_str() {
            "above_input" => &mut widgets.above_input,
            "below_input" => &mut widgets.below_input,
            _ => return Err(format!("tui.widgets.{key}")),
        };
        let Value::Array(entries) = value else {
            return Err(format!("tui.widgets.{key}"));
        };
        for entry in entries {
            match entry {
                Value::String(id) => slot.push(id.clone()),
                _ => return Err(format!("tui.widgets.{key}")),
            }
        }
    }
    config.tui.widgets = widgets;
    Ok(())
}

/// Parses `[tui.footer]` (ADR 0027 C2). A structural problem names the key it skipped; a
/// display preference never refuses the file (the `tui.*` rule).
fn parse_footer(table: &toml::map::Map<String, Value>, config: &mut Config) -> Result<(), String> {
    let mut footer = TuiFooterConfig::default();
    for (key, value) in table {
        match (key.as_str(), value) {
            ("preset", Value::String(preset)) => footer.preset = Some(preset.clone()),
            ("separator", Value::String(separator)) => footer.separator = Some(separator.clone()),
            ("separator_style", Value::String(style)) => {
                footer.separator_style = Some(style.clone())
            }
            ("glyphs", Value::String(glyphs)) => footer.glyphs = Some(glyphs.clone()),
            // ADR 0027 item 4: the left/right gap fill.
            ("context_line", Value::String(mode)) => footer.context_line = Some(mode.clone()),
            ("transparent", Value::Boolean(on)) => footer.transparent = Some(*on),
            ("items", Value::Table(items)) => {
                for (id, value) in items {
                    let Value::Table(table) = value else {
                        return Err(format!("tui.footer.items.{id}"));
                    };
                    let item = parse_footer_item_table(table, id)?;
                    footer.items.insert(id.clone(), item);
                }
            }
            ("rows", Value::Array(rows)) => {
                for row in rows {
                    let Some(row) = row.as_table() else {
                        return Err("tui.footer.rows".into());
                    };
                    footer.rows.push(parse_footer_row(row)?);
                }
            }
            (other, _) => return Err(format!("tui.footer.{other}")),
        }
    }
    config.tui.footer = footer;
    Ok(())
}

fn parse_footer_row(table: &toml::map::Map<String, Value>) -> Result<TuiFooterRowConfig, String> {
    let mut row = TuiFooterRowConfig::default();
    for (key, value) in table {
        match (key.as_str(), value) {
            ("left", Value::Array(items)) => {
                for item in items {
                    row.left.push(parse_footer_item(item)?);
                }
            }
            ("right", Value::Array(items)) => {
                for item in items {
                    row.right.push(parse_footer_item(item)?);
                }
            }
            ("separator", Value::String(separator)) => row.separator = Some(separator.clone()),
            (other, _) => return Err(format!("tui.footer.rows.{other}")),
        }
    }
    Ok(row)
}

/// One item: a bare id string, or a table with `item` plus options. Anything else is skipped.
fn parse_footer_item(value: &Value) -> Result<TuiFooterItemConfig, String> {
    match value {
        Value::String(id) => Ok(TuiFooterItemConfig {
            id: id.clone(),
            ..Default::default()
        }),
        Value::Table(table) => {
            let id = table
                .get("item")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if id.is_empty() {
                return Err("tui.footer.rows.item".into());
            }
            parse_footer_item_table(table, &id)
        }
        _ => Err("tui.footer.rows.item".into()),
    }
}

/// The options of one item table. `id` is known already: from the `item` key in a row, or from
/// the key of a `[tui.footer.items.<id>]` table. `icon` and `symbol` are the same field
/// (ADR 0027 C2 used `icon`, revision 3 calls it `symbol`); a known option with the wrong type
/// is skipped, not a refusal.
fn parse_footer_item_table(
    table: &toml::map::Map<String, Value>,
    id: &str,
) -> Result<TuiFooterItemConfig, String> {
    let mut item = TuiFooterItemConfig {
        id: id.to_owned(),
        ..Default::default()
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("item", Value::String(_)) => {}
            ("icon" | "symbol", Value::String(symbol)) => item.icon = Some(symbol.clone()),
            ("symbol_style", Value::String(style)) => item.symbol_style = Some(style.clone()),
            ("format", Value::String(format)) => item.format = Some(format.clone()),
            ("style", Value::String(style)) => item.style = Some(style.clone()),
            ("disabled", Value::Boolean(disabled)) => item.disabled = *disabled,
            ("priority", Value::Integer(priority)) => item.priority = Some(*priority),
            ("thresholds", Value::Array(thresholds)) => {
                for threshold in thresholds {
                    let Some(threshold) = threshold.as_table() else {
                        return Err("tui.footer.rows.thresholds".into());
                    };
                    let at = threshold.get("at").and_then(Value::as_integer);
                    let style = threshold.get("style").and_then(Value::as_str);
                    match (at, style) {
                        (Some(at), Some(style)) => {
                            item.thresholds.push((at, style.to_owned()));
                        }
                        _ => return Err("tui.footer.rows.thresholds".into()),
                    }
                }
            }
            (
                "item" | "icon" | "symbol" | "symbol_style" | "format" | "style" | "disabled"
                | "priority",
                _,
            ) => {
                return Err("tui.footer.rows.item".into());
            }
            // Everything else is the item's own parameter, kept as text.
            (param, value) => {
                if let Some(text) = value_to_text(value) {
                    item.params.push((param.to_owned(), text));
                } else {
                    return Err(format!("tui.footer.rows.{param}"));
                }
            }
        }
    }
    Ok(item)
}

/// A scalar parameter as the text the TUI parses (`20`, `"─"`, `true`).
fn value_to_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Integer(number) => Some(number.to_string()),
        Value::Float(number) => Some(number.to_string()),
        Value::Boolean(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn wrong_type(path: &Path, key: &str, expected: &'static str) -> ConfigError {
    ConfigError::WrongType {
        path: path.to_path_buf(),
        key: key.to_owned(),
        expected,
    }
}

/// The `path.key` spelling the checker uses, for error messages and the skipped list.
fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// §4.2: a bare string on a sensitive key, or an obviously secret-looking string anywhere, is
/// refused. The `key` in the error is a path, never the value.
fn check_no_plaintext(
    table: &Map<String, Value>,
    path: &str,
    file: &Path,
) -> Result<(), ConfigError> {
    for (key, value) in table {
        let here = join(path, key);
        match value {
            Value::Table(table) => check_no_plaintext(table, &here, file)?,
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    if let Value::Table(table) = item {
                        check_no_plaintext(table, &format!("{here}[{index}]"), file)?;
                    }
                }
            }
            Value::String(text) => {
                if SENSITIVE_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
                    return Err(ConfigError::PlaintextSecret {
                        path: file.to_path_buf(),
                        key: here,
                    });
                }
                if looks_like_secret(text) {
                    return Err(ConfigError::SecretLookalike {
                        path: file.to_path_buf(),
                        key: here,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The heuristic of ADR 0008 §4.2 rule 2, byte-for-byte the reference checker's.
fn looks_like_secret(text: &str) -> bool {
    if SECRET_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return true;
    }
    text.len() >= 40
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// §4.1: every sensitive key must be a `SecretRef` table. `from`/`name` are required, `name`
/// is a non-empty string, and **only `from = "env"` is supported by this version**.
fn check_secret_refs(
    table: &Map<String, Value>,
    path: &str,
    file: &Path,
) -> Result<(), ConfigError> {
    for (key, value) in table {
        let here = join(path, key);
        if SENSITIVE_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
            let Some(secret) = value.as_table() else {
                return Err(ConfigError::SecretRef {
                    path: file.to_path_buf(),
                    key: here,
                    message: "must be a SecretRef table ({ from = \"env\", name = \"…\" })".into(),
                });
            };
            let from = secret
                .get("from")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if from != "env" {
                let message = if from.is_empty() {
                    "SecretRef.from must be \"env\"".to_owned()
                } else {
                    format!("SecretRef.from = {from:?}; this version only supports \"env\"")
                };
                return Err(ConfigError::SecretRef {
                    path: file.to_path_buf(),
                    key: here,
                    message,
                });
            }
            match secret.get("name").and_then(Value::as_str) {
                Some(name) if !name.is_empty() => {}
                _ => {
                    return Err(ConfigError::SecretRef {
                        path: file.to_path_buf(),
                        key: here,
                        message: "SecretRef.name must be a non-empty string".into(),
                    });
                }
            }
            let extra: Vec<&String> = secret
                .keys()
                .filter(|key| !matches!(key.as_str(), "from" | "name"))
                .collect();
            if !extra.is_empty() {
                return Err(ConfigError::SecretRef {
                    path: file.to_path_buf(),
                    key: here,
                    message: format!("SecretRef has unknown fields {extra:?}"),
                });
            }
        } else {
            match value {
                Value::Table(table) => check_secret_refs(table, &here, file)?,
                Value::Array(items) => {
                    for (index, item) in items.iter().enumerate() {
                        if let Value::Table(table) = item {
                            check_secret_refs(table, &format!("{here}[{index}]"), file)?;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(text: &str) -> Result<Config, ConfigError> {
        parse(Path::new("/tmp/test/config.toml"), text)
    }

    /// ADR 0034 H6: `[integrations] herdr` defaults to true; `false` turns it off and a
    /// non-boolean is refused.
    #[test]
    fn integrations_herdr_defaults_on_and_can_be_turned_off() {
        assert!(
            parse_str("schema_version = 1\n")
                .unwrap()
                .integrations
                .herdr
        );
        assert!(
            !parse_str("schema_version = 1\n[integrations]\nherdr = false\n")
                .unwrap()
                .integrations
                .herdr
        );
        assert!(
            parse_str("schema_version = 1\n[integrations]\nherdr = \"no\"\n").is_err(),
            "a non-boolean is refused"
        );
        let config = parse_str("schema_version = 1\n[integrations]\nfuture = 1\n").unwrap();
        assert!(config.skipped.contains(&"integrations.future".to_owned()));
    }

    /// ADR 0016: `[sandbox] allow_unix_sockets` is an array of strings; a bad
    /// shape is refused and an unknown key under `[sandbox]` is only skipped.
    #[test]
    fn sandbox_allow_unix_sockets_is_parsed_and_a_bad_shape_is_refused() {
        let config = parse_str(
            "schema_version = 1\n[sandbox]\nallow_unix_sockets = [\"~/.config/herdr/herdr.sock\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.sandbox.allow_unix_sockets,
            vec!["~/.config/herdr/herdr.sock"]
        );
        assert!(
            parse_str("schema_version = 1\n[sandbox]\nallow_unix_sockets = \"x\"\n").is_err(),
            "a non-array is refused"
        );
        let config = parse_str("schema_version = 1\n[sandbox]\nfuture = true\n").unwrap();
        assert!(config.sandbox.allow_unix_sockets.is_empty());
        assert!(config.skipped.contains(&"sandbox.future".to_owned()));
    }

    /// The host expands `~`, canonicalizes, and skips (with a warning) a relative or missing
    /// entry. Only absolute paths are accepted.
    #[test]
    fn resolve_allow_unix_sockets_expands_home_and_skips_bad_entries() {
        let dir = std::env::temp_dir().join("radian-host-sandbox-sock-test");
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("h.sock");
        std::fs::write(&sock, b"").unwrap();
        let raw = vec![
            "~/.config/herdr/herdr.sock".to_owned(),
            "relative.sock".to_owned(),
            sock.to_string_lossy().into_owned(),
        ];
        let (paths, warnings) = resolve_allow_unix_sockets(&raw, Some(Path::new("/home/u")));
        assert_eq!(paths.len(), 1, "only the existing absolute path resolves");
        assert_eq!(paths[0], std::fs::canonicalize(&sock).unwrap());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_path_prefers_explicit_then_env_then_the_radian_home() {
        let home = Path::new("/home/u/.radian");
        assert_eq!(
            resolve_path(
                Some(Path::new("/etc/radian.toml")),
                Some("/env.toml"),
                Some(home)
            ),
            Some(PathBuf::from("/etc/radian.toml"))
        );
        assert_eq!(
            resolve_path(None, Some("/env.toml"), Some(home)),
            Some(PathBuf::from("/env.toml"))
        );
        assert_eq!(
            resolve_path(None, None, Some(home)),
            Some(PathBuf::from("/home/u/.radian/config/config.toml"))
        );
        assert_eq!(resolve_path(None, None, None), None);
    }

    /// ADR 0025 slice 4: `~` is the user's home, a relative entry is relative to the **config
    /// file's directory**, and an absolute entry is kept. No home means `~` is left alone.
    #[test]
    fn extra_paths_resolve_tilde_relative_and_absolute() {
        let config_dir = Path::new("/home/u/.radian/config");
        let home = Path::new("/home/u");
        let raw = vec![
            "~/.claude/skills".to_owned(),
            "~".to_owned(),
            "./team-skills".to_owned(),
            "shared".to_owned(),
            "/opt/shared/skills".to_owned(),
        ];
        assert_eq!(
            resolve_extra_paths(&raw, Some(config_dir), Some(home)),
            vec![
                PathBuf::from("/home/u/.claude/skills"),
                PathBuf::from("/home/u"),
                PathBuf::from("/home/u/.radian/config/team-skills"),
                PathBuf::from("/home/u/.radian/config/shared"),
                PathBuf::from("/opt/shared/skills"),
            ]
        );
        // No home: `~` is not guessed at (it will be reported as a missing directory).
        let no_home = resolve_extra_paths(&raw[..2], Some(config_dir), None);
        assert_eq!(
            no_home,
            vec![PathBuf::from("~/.claude/skills"), PathBuf::from("~"),]
        );
        // No config file: a relative path stays relative.
        assert_eq!(
            resolve_extra_paths(&["shared".to_owned()], None, Some(home)),
            vec![PathBuf::from("shared")]
        );
    }

    /// `[skills] extra_paths` must be an array of non-empty strings; a bad shape is refused
    /// (an empty string would silently mean the config directory), an unknown key is skipped.
    #[test]
    fn skills_extra_paths_validate_the_shape() {
        let config =
            parse_str("schema_version = 1\n[skills]\nextra_paths = [\"a\", \"b\"]\nfuture = 1\n")
                .unwrap();
        assert_eq!(
            config.skills.extra_paths,
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(config.skipped.contains(&"skills.future".to_owned()));
        assert!(parse_str("schema_version = 1\n[skills]\nextra_paths = \"a\"\n").is_err());
        assert!(parse_str("schema_version = 1\n[skills]\nextra_paths = [\"\"]\n").is_err());
        assert!(parse_str("schema_version = 1\n[skills]\nextra_paths = [1]\n").is_err());
    }

    #[test]
    fn a_minimal_file_wires_the_known_keys() {
        let config = parse_str(
            r#"
schema_version = 1
[model]
provider = "anthropic"
base_url = "https://example.com"
model = "m-1"
context_window = 200000
[model.headers]
x-a = "1"
[permissions]
profile = "full"
[compaction]
auto = false
thinking = "high"
[tui]
theme = "light"
"#,
        )
        .unwrap();
        assert_eq!(config.model.provider.as_deref(), Some("anthropic"));
        assert_eq!(
            config.model.base_url.as_deref(),
            Some("https://example.com")
        );
        assert_eq!(config.model.model.as_deref(), Some("m-1"));
        assert_eq!(config.model.context_window, Some(200_000));
        assert_eq!(
            config.model.headers,
            vec![("x-a".to_string(), "1".to_string())]
        );
        assert_eq!(config.permissions.profile.as_deref(), Some("full"));
        assert_eq!(config.compaction.auto, Some(false));
        assert_eq!(config.compaction.thinking.as_deref(), Some("high"));
        assert_eq!(config.tui.theme.as_deref(), Some("light"));
    }

    /// ADR 0018: `[compaction] thinking` accepts the seven levels or `"current"`;
    /// anything else is a config error, never a silent clamp from the default.
    #[test]
    fn compaction_thinking_accepts_the_levels_and_current_and_refuses_anything_else() {
        let config =
            parse_str("schema_version = 1\n[compaction]\nthinking = \"current\"\n").unwrap();
        assert_eq!(config.compaction.thinking.as_deref(), Some("current"));
        for level in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let config = parse_str(&format!(
                "schema_version = 1\n[compaction]\nthinking = \"{level}\"\n"
            ))
            .unwrap();
            assert_eq!(config.compaction.thinking.as_deref(), Some(level));
        }
        assert!(parse_str("schema_version = 1\n[compaction]\nthinking = \"loud\"\n").is_err());
        assert!(parse_str("schema_version = 1\n[compaction]\nthinking = 3\n").is_err());
    }

    /// `[model] retries` (ADR 0017) is carried; a negative or non-integer value is
    /// refused (fail-closed), and `0` means "no retrying".
    #[test]
    fn model_retries_must_be_a_non_negative_integer() {
        let config = parse_str("schema_version = 1\n[model]\nretries = 3\n").unwrap();
        assert_eq!(config.model.retries, Some(3));
        let config = parse_str("schema_version = 1\n[model]\nretries = 0\n").unwrap();
        assert_eq!(config.model.retries, Some(0));
        assert!(parse_str("schema_version = 1\n[model]\nretries = -1\n").is_err());
        assert!(parse_str("schema_version = 1\n[model]\nretries = \"3\"\n").is_err());
    }

    /// ADR 0015 (2026-10-16): `[model] context_window` is a non-negative integer, and `0` means
    /// **unset** — a valid "off" in the same shape as `retries = 0`, never a zero-token window.
    /// A negative or non-integer value is refused rather than guessed (fail-closed).
    #[test]
    fn context_window_zero_is_unset_and_a_positive_value_is_carried() {
        let config = parse_str("schema_version = 1\n[model]\ncontext_window = 200000\n").unwrap();
        assert_eq!(config.model.context_window, Some(200_000));
        let config = parse_str("schema_version = 1\n[model]\ncontext_window = 0\n").unwrap();
        assert_eq!(config.model.context_window, None);
        assert!(parse_str("schema_version = 1\n[model]\ncontext_window = -1\n").is_err());
        assert!(parse_str("schema_version = 1\n[model]\ncontext_window = \"200000\"\n").is_err());
    }

    /// `tui.input_style` is carried as a string; the TUI parses it (the removed `bar` and any
    /// unknown value warn there), and a non-string is a skipped key (the `tui.*` rule), not a
    /// refusal.
    #[test]
    fn the_tui_input_style_is_carried_and_a_non_string_is_skipped() {
        // ADR 0035's 2026-10-16 note removed `bar`; the reader still carries the spelling so the
        // TUI can warn with it. `lines` and `box` are the accepted styles.
        for style in ["bar", "lines", "box"] {
            let config = parse_str(&format!(
                "schema_version = 1\n[tui]\ninput_style = \"{style}\"\n"
            ))
            .unwrap();
            assert_eq!(config.tui.input_style.as_deref(), Some(style));
            assert!(config.skipped.is_empty(), "{style}");
        }

        // Any other string is carried too (the TUI validates it), never a refusal.
        let config = parse_str("schema_version = 1\n[tui]\ninput_style = \"future\"\n").unwrap();
        assert_eq!(config.tui.input_style.as_deref(), Some("future"));
        assert!(config.skipped.is_empty());

        // A non-string is skipped.
        let config = parse_str("schema_version = 1\n[tui]\ninput_style = 3\n").unwrap();
        assert_eq!(config.tui.input_style, None);
        assert!(config.skipped.contains(&"tui.input_style".to_string()));
    }

    /// `tui.display.mermaid` (ADR 0028 R8): a bool is carried; an unknown value is skipped.
    #[test]
    fn the_display_mermaid_flag_is_carried_and_a_non_bool_is_skipped() {
        let config = parse_str("schema_version = 1\n[tui.display]\nmermaid = false\n").unwrap();
        assert_eq!(config.tui.display_mermaid, Some(false));
        let config = parse_str("schema_version = 1\n[tui.display]\nmermaid = \"yes\"\n").unwrap();
        assert_eq!(config.tui.display_mermaid, None);
        assert!(config.skipped.contains(&"tui.display.mermaid".to_string()));
    }

    /// `tui.display.highlight` (ADR 0028 R2): a bool is carried; an unknown value is skipped.
    #[test]
    fn the_display_highlight_flag_is_carried_and_a_non_bool_is_skipped() {
        let config = parse_str("schema_version = 1\n[tui.display]\nhighlight = false\n").unwrap();
        assert_eq!(config.tui.display_highlight, Some(false));
        assert_eq!(
            parse_str("schema_version = 1\n")
                .unwrap()
                .tui
                .display_highlight,
            None
        );
        let config = parse_str("schema_version = 1\n[tui.display]\nhighlight = \"yes\"\n").unwrap();
        assert_eq!(config.tui.display_highlight, None);
        assert!(
            config
                .skipped
                .contains(&"tui.display.highlight".to_string())
        );
    }

    /// `tui.display.reasoning` (ADR 0028 R3): the three modes are carried; an unknown one is
    /// skipped with the `tui.*` warning and the TUI keeps its default.
    #[test]
    fn the_display_reasoning_mode_is_carried_and_an_unknown_value_is_skipped() {
        assert_eq!(
            parse_str("schema_version = 1\n")
                .unwrap()
                .tui
                .display_reasoning,
            None
        );
        for mode in ["collapsed", "expanded", "hidden"] {
            let config = parse_str(&format!(
                "schema_version = 1\n[tui.display]\nreasoning = \"{mode}\"\n"
            ))
            .unwrap();
            assert_eq!(config.tui.display_reasoning.as_deref(), Some(mode));
        }
        let config =
            parse_str("schema_version = 1\n[tui.display]\nreasoning = \"sometimes\"\n").unwrap();
        assert_eq!(config.tui.display_reasoning, None);
        assert!(
            config
                .skipped
                .contains(&"tui.display.reasoning".to_string())
        );
        // A `tui.display` that is not a table is skipped, not refused.
        let config = parse_str("schema_version = 1\n[tui]\ndisplay = 3\n").unwrap();
        assert!(config.skipped.contains(&"tui.display".to_string()));
    }

    /// `tui.mouse` is an optional tri-state: absent = the default (on), `true`/`false` are
    /// carried verbatim, and a wrong type is a display preference (skipped, never a refusal).
    #[test]
    fn the_tui_mouse_flag_is_carried_and_a_wrong_type_is_skipped() {
        assert_eq!(parse_str("schema_version = 1\n").unwrap().tui.mouse, None);
        assert_eq!(
            parse_str("schema_version = 1\n[tui]\nmouse = false\n")
                .unwrap()
                .tui
                .mouse,
            Some(false)
        );
        let config = parse_str("schema_version = 1\n[tui]\nmouse = \"yes\"\n").unwrap();
        assert_eq!(config.tui.mouse, None);
        assert!(config.skipped.contains(&"tui.mouse".to_string()));
    }

    #[test]
    fn a_missing_version_and_a_lower_version_are_refused() {
        assert!(matches!(
            parse_str("schema_version = \"1\"\n").unwrap_err(),
            ConfigError::MissingVersion { .. }
        ));
        assert!(matches!(
            parse_str("schema_version = 0\n").unwrap_err(),
            ConfigError::VersionTooOld { found: 0, .. }
        ));
    }

    #[test]
    fn a_higher_version_is_read_only_with_a_warning_not_a_refusal() {
        let config = parse_str("schema_version = 99\n[model]\nmodel = \"m-99\"\n").unwrap();
        assert!(config.read_only);
        assert_eq!(config.schema_version, Some(99));
        assert_eq!(config.model.model.as_deref(), Some("m-99"));
    }

    #[test]
    fn a_bare_secret_is_refused_without_echoing_the_value() {
        let error =
            parse_str("schema_version = 1\n[model]\napi_key = \"super-secret\"\n").unwrap_err();
        let text = error.to_string();
        assert!(!text.contains("super-secret"), "{text}");
        assert!(text.contains("api_key"), "{text}");
        assert!(matches!(error, ConfigError::PlaintextSecret { .. }));
    }

    #[test]
    fn a_secret_lookalike_anywhere_is_refused_without_echoing_it() {
        let error =
            parse_str("schema_version = 1\n[tui]\nstatus_template = \"sk-live-abcdef123456\"\n")
                .unwrap_err();
        let text = error.to_string();
        assert!(!text.contains("sk-live"), "{text}");
        assert!(matches!(error, ConfigError::SecretLookalike { .. }));
    }

    /// ADR 0026 D2: `model.api_key` is refused with a pointer at `auth.json`, never silently
    /// ignored.
    #[test]
    fn model_api_key_is_refused_with_an_auth_json_pointer() {
        let error =
            parse_str("schema_version = 1\n[model]\napi_key = { from = \"env\", name = \"K\" }\n")
                .unwrap_err();
        assert!(
            matches!(error, ConfigError::ApiKeyRemoved { .. }),
            "{error}"
        );
        let text = error.to_string();
        assert!(text.contains("auth.json"), "{text}");
        assert!(text.contains("/login"), "{text}");
    }

    #[test]
    fn a_secret_ref_with_a_non_env_source_is_refused() {
        let error = parse_str(
            "schema_version = 1\n[model]\napi_key = { from = \"keychain\", name = \"x\" }\n",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("only supports \"env\""),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_profile_is_refused_and_an_unknown_theme_is_carried() {
        let error =
            parse_str("schema_version = 1\n[permissions]\nprofile = \"future\"\n").unwrap_err();
        assert!(
            matches!(error, ConfigError::UnknownProfile { .. }),
            "{error}"
        );

        // ADR 0029: any theme name is carried; whether it resolves (built-in or
        // `~/.radian/themes/<name>.toml`) is the TUI's finding, a startup warning.
        let config = parse_str("schema_version = 1\n[tui]\ntheme = \"future\"\n").unwrap();
        assert_eq!(config.tui.theme.as_deref(), Some("future"));
        assert!(config.skipped.is_empty());
    }

    /// ADR 0026 D9: `[permissions] approval` carries the four known policy names; an
    /// unknown value is refused (fail-closed), a non-string is a type error, and in
    /// read-only mode an unknown future value is skipped instead.
    #[test]
    fn permissions_approval_carries_known_names_and_refuses_unknown_ones() {
        for name in ["on-ask", "always", "never", "auto"] {
            let config = parse_str(&format!(
                "schema_version = 1\n[permissions]\napproval = \"{name}\"\n"
            ))
            .unwrap();
            assert_eq!(config.permissions.approval.as_deref(), Some(name), "{name}");
            assert!(config.skipped.is_empty(), "{name}");
        }
        let error =
            parse_str("schema_version = 1\n[permissions]\napproval = \"yolo\"\n").unwrap_err();
        assert!(
            matches!(error, ConfigError::UnknownApproval { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("auto"), "{error}");
        let error = parse_str("schema_version = 1\n[permissions]\napproval = true\n").unwrap_err();
        assert!(error.to_string().contains("a string"), "{error}");
        // Read-only mode (a higher schema_version): ignored, not refused.
        let config =
            parse_str("schema_version = 99\n[permissions]\napproval = \"future\"\n").unwrap();
        assert_eq!(config.permissions.approval, None);
        assert!(config.skipped.contains(&"permissions.approval".to_string()));
    }

    /// ADR 0029 decision 5: `[tui.colors]` entries are carried raw; the TUI validates names
    /// and colors. A malformed entry is a skipped key, never a refusal.
    #[test]
    fn tui_colors_entries_are_carried_and_malformed_ones_skipped() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[tui]\n",
            "theme = \"kanagawa-wave\"\n",
            "[tui.colors]\n",
            "user = \"#0af\"\n",
            "tool_call = 39\n",
            "notice = { fg = \"oklch(62% 0.1 200)\", bold = true }\n",
            "hint = { fg = \"#fff\", neon = true }\n",
            "muted = 2.5\n",
        ))
        .unwrap();
        assert_eq!(config.tui.theme.as_deref(), Some("kanagawa-wave"));
        assert_eq!(config.tui.colors.len(), 3, "{:?}", config.tui.colors);
        let get = |key: &str| {
            config
                .tui
                .colors
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value)
                .unwrap()
        };
        assert_eq!(get("user").fg, Some(TuiColorValue::Text("#0af".to_owned())));
        assert_eq!(get("tool_call").fg, Some(TuiColorValue::Index(39)));
        let notice = get("notice");
        assert_eq!(
            notice.fg,
            Some(TuiColorValue::Text("oklch(62% 0.1 200)".to_owned()))
        );
        assert_eq!(notice.bold, Some(true));
        assert_eq!(
            config.skipped,
            vec!["tui.colors.hint".to_owned(), "tui.colors.muted".to_owned()]
        );

        // A non-table `tui.colors` is a wrong type (structural), like `tui.keybindings`.
        let error = parse_str("schema_version = 1\n[tui]\ncolors = 3\n").unwrap_err();
        assert!(matches!(error, ConfigError::WrongType { .. }), "{error}");
    }

    /// ADR 0026 D10: `[providers.<name>]` is carried into `Config.providers`; a bad or
    /// shadowing name refuses the file (fail-closed, like `permissions.profile`).
    #[test]
    fn named_providers_are_carried_and_bad_names_refused() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.corp]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://llm.example.com/v1\"\n",
            "[providers.relay]\n",
            "kind = \"anthropic\"\n",
            "base_url = \"https://relay.example\"\n",
        ))
        .unwrap();
        assert_eq!(config.providers.len(), 2);
        assert_eq!(config.providers["corp"].kind, "openai-compatible");
        assert_eq!(config.providers["relay"].base_url, "https://relay.example");
        assert!(config.skipped.is_empty(), "{:?}", config.skipped);

        // A name shadowing a built-in refuses the file, naming the rule.
        let error = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.anthropic]\n",
            "kind = \"anthropic\"\n",
            "base_url = \"https://x\"\n",
        ))
        .unwrap_err();
        assert!(
            matches!(error, ConfigError::InvalidProviderName { .. }),
            "{error}"
        );
        let text = error.to_string();
        assert!(text.contains("providers.anthropic"), "{text}");
        assert!(text.contains("built-in"), "{text}");

        // An invalid name too.
        let error = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.\"has space\"]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://x\"\n",
        ))
        .unwrap_err();
        assert!(
            matches!(error, ConfigError::InvalidProviderName { .. }),
            "{error}"
        );

        // `openai-compatible` is reserved.
        let error = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.openai-compatible]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://x\"\n",
        ))
        .unwrap_err();
        assert!(
            matches!(error, ConfigError::InvalidProviderName { .. }),
            "{error}"
        );

        // An unknown kind is refused (never guessed at the host).
        let error = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.ok-name]\n",
            "kind = \"grpc\"\n",
            "base_url = \"https://x\"\n",
        ))
        .unwrap_err();
        assert!(matches!(error, ConfigError::WrongType { .. }), "{error}");
        assert!(
            error.to_string().contains("openai-compatible|anthropic"),
            "{error}"
        );
    }

    /// ADR 0030: `[git] allowed_filters` is carried; the default (`["lfs"]`) applies
    /// when it is absent, and `[]` allows none.
    #[test]
    fn the_git_allowed_filters_are_carried_with_an_lfs_default() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[git]\n",
            "protected_branches = [\"release\"]\n",
            "allowed_filters = [\"lfs\", \"custom\"]\n",
        ))
        .unwrap();
        assert_eq!(config.git.protected_branches, vec!["release".to_string()]);
        assert_eq!(
            config.git.allowed_filters(),
            vec!["lfs".to_string(), "custom".to_string()]
        );
        // Absent: the default.
        let config =
            parse_str("schema_version = 1\n[git]\nprotected_branches = [\"release\"]\n").unwrap();
        assert_eq!(config.git.allowed_filters(), vec!["lfs".to_string()]);
        // An empty list allows none.
        let config = parse_str("schema_version = 1\n[git]\nallowed_filters = []\n").unwrap();
        assert!(config.git.allowed_filters().is_empty());
    }

    /// ADR 0037: `[shell] auto_background_secs` is carried; `0` means off, unset
    /// means the built-in default, and a bad value is skipped (never a refusal).
    #[test]
    fn the_shell_auto_background_secs_is_carried_with_an_off_zero() {
        let config = parse_str("schema_version = 1\n[shell]\nauto_background_secs = 30\n").unwrap();
        assert_eq!(config.shell.auto_background_secs, Some(30));
        assert_eq!(
            config.shell.auto_background_after(),
            Some(std::time::Duration::from_secs(30))
        );

        let config = parse_str("schema_version = 1\n[shell]\nauto_background_secs = 0\n").unwrap();
        assert_eq!(config.shell.auto_background_secs, Some(0));
        assert_eq!(config.shell.auto_background_after(), None);

        let config = parse_str("schema_version = 1\n").unwrap();
        assert_eq!(
            config.shell.auto_background_after(),
            radian_coding::Limits::default().auto_background_after
        );

        let config = parse_str("schema_version = 1\n[shell]\nauto_background_secs = -1\n").unwrap();
        assert_eq!(config.shell.auto_background_secs, None);
        assert!(
            config
                .skipped
                .iter()
                .any(|key| key == "shell.auto_background_secs"),
            "{:?}",
            config.skipped
        );
    }

    /// ADR 0014 Q2: `[providers.<id>] reasoning_replay` overrides the catalog — a
    /// non-empty field name, or `false` to switch replay off. `true` names no field and is
    /// refused rather than guessed.
    #[test]
    fn a_named_providers_reasoning_replay_override_is_carried() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.corp]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://llm.example.com/v1\"\n",
            "reasoning_replay = \"reasoning_content\"\n",
            "[providers.relay]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://relay.example/v1\"\n",
            "reasoning_replay = false\n",
        ))
        .unwrap();
        assert_eq!(
            config.providers["corp"].reasoning_replay,
            Some(crate::provider::ReasoningReplayOverride::Field(
                "reasoning_content".into()
            ))
        );
        assert_eq!(
            config.providers["relay"].reasoning_replay,
            Some(crate::provider::ReasoningReplayOverride::Off)
        );
        // Absent: the catalog decides.
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.corp]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://llm.example.com/v1\"\n",
        ))
        .unwrap();
        assert_eq!(config.providers["corp"].reasoning_replay, None);

        // `true` is not a field name.
        let error = parse_str(concat!(
            "schema_version = 1\n",
            "[providers.corp]\n",
            "kind = \"openai-compatible\"\n",
            "base_url = \"https://llm.example.com/v1\"\n",
            "reasoning_replay = true\n",
        ))
        .unwrap_err();
        assert!(matches!(error, ConfigError::WrongType { .. }), "{error}");
        assert!(error.to_string().contains("reasoning_replay"), "{error}");
    }

    #[test]
    fn the_adrs_ask_allow_deny_profiles_are_known_and_ignored() {
        let config = parse_str("schema_version = 1\n[permissions]\nprofile = \"ask\"\n").unwrap();
        assert_eq!(config.permissions.profile, None, "no tier in this build");
        assert!(config.skipped.contains(&"permissions.profile".to_string()));
    }

    #[test]
    fn unknown_and_unwired_keys_are_collected() {
        let config = parse_str(
            r#"
schema_version = 1
future_top = 1
[model]
include_usage = true
future_option = "alpha"
[mcp.servers]
"#,
        )
        .unwrap();
        assert!(config.skipped.contains(&"future_top".to_string()));
        assert!(config.skipped.contains(&"model.include_usage".to_string()));
        assert!(config.skipped.contains(&"model.future_option".to_string()));
        assert!(config.skipped.contains(&"mcp".to_string()));
    }

    #[test]
    fn keybindings_are_carried_raw_for_the_tui() {
        let config =
            parse_str("schema_version = 1\n[tui.keybindings]\nsend = [\"ctrl-s\"]\n").unwrap();
        assert_eq!(
            config.tui.keybindings,
            vec![("send".to_string(), vec!["ctrl-s".to_string()])]
        );
    }
}

#[cfg(test)]
mod startup_pref_tests {
    use super::*;
    use std::path::Path;

    fn parse_str(text: &str) -> Result<Config, ConfigError> {
        parse(Path::new("/tmp/test/config.toml"), text)
    }

    /// ADR 0008: `tui.tips`/`tui.banner` are display booleans; a non-boolean is skipped with a
    /// warning, never a refusal (the `tui.*` rule).
    #[test]
    fn tui_tips_and_banner_are_carried_and_non_booleans_are_skipped() {
        let config =
            parse_str("schema_version = 1\n[tui]\ntips = false\nbanner = false\n").unwrap();
        assert_eq!(config.tui.tips, Some(false));
        assert_eq!(config.tui.banner, Some(false));

        let config = parse_str("schema_version = 1\n[tui]\ntips = \"yes\"\n").unwrap();
        assert_eq!(config.tui.tips, None);
        assert!(config.skipped.contains(&"tui.tips".to_string()));
    }

    /// ADR 0027: `tui.brand` is carried, a non-string is skipped (the `tui.*`
    /// rule), and a value wider than 4 display columns is a config error (it would misalign
    /// the footer and the banner).
    #[test]
    fn tui_brand_is_carried_and_over_wide_is_refused() {
        let config = parse_str("schema_version = 1\n[tui]\nbrand = \"ʀ\"\n").unwrap();
        assert_eq!(config.tui.brand.as_deref(), Some("ʀ"));

        // A non-string is a display preference: skipped, never a refusal.
        let config = parse_str("schema_version = 1\n[tui]\nbrand = 3\n").unwrap();
        assert_eq!(config.tui.brand, None);
        assert!(config.skipped.contains(&"tui.brand".to_string()));

        // `㎭` is East-Asian-Wide (2 columns): two are 4 (fine), three are 5 (refused).
        assert!(parse_str("schema_version = 1\n[tui]\nbrand = \"㎭㎭\"\n").is_ok());
        let error = parse_str("schema_version = 1\n[tui]\nbrand = \"㎭㎭R\"\n").unwrap_err();
        assert!(matches!(error, ConfigError::WrongType { .. }), "{error}");
    }

    /// ADR 0027: `tui.spinner` is carried; a non-string is skipped (the `tui.*`
    /// rule). The TUI validates the name and falls back to `pulse`.
    #[test]
    fn tui_spinner_is_carried_and_non_strings_are_skipped() {
        let config = parse_str("schema_version = 1\n[tui]\nspinner = \"quadrant\"\n").unwrap();
        assert_eq!(config.tui.spinner.as_deref(), Some("quadrant"));

        let config = parse_str("schema_version = 1\n[tui]\nspinner = 3\n").unwrap();
        assert_eq!(config.tui.spinner, None);
        assert!(config.skipped.contains(&"tui.spinner".to_string()));
    }
}

#[cfg(test)]
mod footer_tests {
    use super::*;
    use std::path::Path;

    fn parse_str(text: &str) -> Result<Config, ConfigError> {
        parse(Path::new("/tmp/test/config.toml"), text)
    }

    /// ADR 0027 C2: the full `[tui.footer]` shape — preset, separator, glyphs, two rows, both
    /// item spellings, an option table and a threshold.
    #[test]
    fn the_footer_config_is_carried() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[tui.footer]\n",
            "preset = \"pi\"\n",
            "separator = \" | \"\n",
            "glyphs = \"unicode\"\n",
            "context_line = \"percentage\"\n",
            "transparent = true\n",
            "[[tui.footer.rows]]\n",
            "left = [\n",
            "  { item = \"model\", format = \"{display_name}\" },\n",
            "  { item = \"context_bar\", width = 20, fill = \"━\", empty = \"─\", format = \"{bar} {percent}%\" },\n",
            "]\n",
            "right = [\"reasoning\"]\n",
            "[[tui.footer.rows]]\n",
            "left = [\"tokens\", \"cost\"]\n",
            "separator = \" · \"\n",
            "[[tui.footer.rows]]\n",
            "left = [{ item = \"context\", thresholds = [{ at = 70, style = \"warn\" }, { at = 90, style = \"error\" }] }]\n",
        ))
        .unwrap();
        let footer = &config.tui.footer;
        assert_eq!(footer.preset.as_deref(), Some("pi"));
        assert_eq!(footer.separator.as_deref(), Some(" | "));
        assert_eq!(footer.glyphs.as_deref(), Some("unicode"));
        assert_eq!(footer.context_line.as_deref(), Some("percentage"));
        assert_eq!(footer.transparent, Some(true));
        assert_eq!(footer.rows.len(), 3);
        assert_eq!(footer.rows[0].left.len(), 2);
        assert_eq!(footer.rows[0].left[0].id, "model");
        assert_eq!(
            footer.rows[0].left[0].format.as_deref(),
            Some("{display_name}")
        );
        assert_eq!(footer.rows[0].left[1].params.len(), 3);
        assert_eq!(footer.rows[0].right[0].id, "reasoning");
        assert_eq!(footer.rows[1].separator.as_deref(), Some(" · "));
        assert_eq!(footer.rows[1].left[1].id, "cost");
        assert_eq!(
            footer.rows[2].left[0].thresholds,
            vec![(70, "warn".into()), (90, "error".into())]
        );
        assert!(config.skipped.is_empty(), "{:?}", config.skipped);
    }

    /// An unknown item id is **carried** (the TUI warns and shows it raw), but an unknown
    /// `[tui.footer]` key or a wrong type is skipped with a warning, never refused.
    #[test]
    fn unknown_footer_keys_and_types_are_skipped_never_refused() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[tui.footer]\n",
            "future_key = true\n",
            "[[tui.footer.rows]]\n",
            "left = [{ item = \"future_item\" }, 5, \"model\"]\n",
        ))
        .unwrap();
        // `future_key` skipped; the row is dropped whole (its second item is not a string or a
        // table), so the footer falls back to the default.
        assert!(
            config
                .skipped
                .iter()
                .any(|key| key.starts_with("tui.footer")),
            "{:?}",
            config.skipped
        );
        assert!(config.tui.footer.rows.is_empty());

        // A well-formed row with an unknown item id keeps the id.
        let config = parse_str(
            "schema_version = 1\n[tui.footer]\n[[tui.footer.rows]]\nleft = [\"future_item\"]\n",
        )
        .unwrap();
        assert_eq!(config.tui.footer.rows[0].left[0].id, "future_item");
        assert!(config.skipped.is_empty(), "{:?}", config.skipped);
    }
}

#[cfg(test)]
mod widget_tests {
    use super::*;
    use std::path::Path;

    fn parse_str(text: &str) -> Result<Config, ConfigError> {
        parse(Path::new("/tmp/test/config.toml"), text)
    }

    /// ADR 0027 C3: `[tui.widgets]` carries the item ids above/below the input box.
    #[test]
    fn tui_widgets_carry_the_item_ids() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[tui.widgets]\n",
            "above_input = [\"status\"]\n",
            "below_input = [\"opencode-go.usage\"]\n",
        ))
        .unwrap();
        assert_eq!(config.tui.widgets.above_input, vec!["status".to_string()]);
        assert_eq!(
            config.tui.widgets.below_input,
            vec!["opencode-go.usage".to_string()]
        );
        // An unknown key or a non-string entry is skipped, never refused.
        let config = parse_str("schema_version = 1\n[tui.widgets]\nfuture = [\"x\"]\n").unwrap();
        assert!(config.skipped.iter().any(|k| k == "tui.widgets.future"));
    }
}
