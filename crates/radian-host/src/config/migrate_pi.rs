//! Pi → Radian configuration migration (Phase 5).
//!
//! Reads a Pi **agent directory the user named explicitly** (`--from <dir>`; there is no
//! default, so nothing ever reads `~/.pi` on its own) and drafts a Radian configuration from
//! it. Nothing is written unless the caller asks for it, and the draft is only ever a
//! **best-effort translation**: Radian does not promise compatibility with Pi's file format.
//!
//! Safety rules this module is built around (each one has a test):
//!
//! - **`auth.json` is never read.** It is not opened, not even `stat`ed, so a credential file
//!   that is unreadable (mode `0000`) cannot make the migration fail or leak anything. Saved
//!   credentials are never migrated; the draft references an environment variable instead.
//! - **No secret value is ever copied or printed.** A `$NAME`/`${NAME}` value becomes
//!   `{ from = "env", name = "NAME" }`; a literal value is **not** copied (the draft gets an
//!   environment reference with a suggested name, and the note says a literal was skipped
//!   without showing it); a `!command` value is listed and **never executed**.
//! - **No network, no command execution.** The only inputs are files in the given directory.
//! - The draft is validated with the real reader ([`super::parse`]) before it is printed or
//!   written, and [`write_draft`] refuses an existing target (ADR 0008 §4: this version has
//!   no faithful write-back).

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use toml::Value as Toml;

/// A drafted Radian configuration and the report that goes with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiDraft {
    /// The TOML text: `schema_version = 1` and only the keys the Radian reader implements.
    pub toml: String,
    /// What was mapped, what was skipped and why, and what the user still has to do. The host
    /// prints these to stderr. They name keys, never Pi values that could be secrets.
    pub notes: Vec<String>,
}

/// Why the migration stopped.
#[derive(Debug)]
pub enum PiMigrateError {
    /// `--from` is not an existing directory.
    NotADirectory { path: PathBuf },
    /// A file could not be read.
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A JSON file did not parse (the message carries the line and column).
    Parse { path: PathBuf, message: String },
    /// The generated draft is not accepted by the reader: an internal error, never written.
    InvalidDraft { message: String },
    /// `--write` target already exists; this version never overwrites (ADR 0008 §4).
    TargetExists { path: PathBuf },
    /// The write itself failed.
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for PiMigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PiMigrateError::NotADirectory { path } => {
                write!(f, "{}: not a directory", path.display())
            }
            PiMigrateError::Read { path, source } => {
                write!(f, "{}: cannot read: {source}", path.display())
            }
            PiMigrateError::Parse { path, message } => {
                write!(f, "{}: invalid JSON: {message}", path.display())
            }
            PiMigrateError::InvalidDraft { message } => {
                write!(
                    f,
                    "the generated draft is not a valid Radian configuration: {message}"
                )
            }
            PiMigrateError::TargetExists { path } => write!(
                f,
                "{}: already exists; refusing to overwrite (this version has no faithful \
                 write-back — remove it yourself if you want the draft there)",
                path.display()
            ),
            PiMigrateError::Write { path, source } => {
                write!(f, "{}: cannot write: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for PiMigrateError {}

/// Reads `settings.json`, `models.json` and `keybindings.json` from `dir` and returns the
/// draft. Never touches the network, `auth.json`, or anything outside `dir`.
pub fn import_pi(dir: &Path) -> Result<PiDraft, PiMigrateError> {
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(PiMigrateError::NotADirectory {
                path: dir.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(PiMigrateError::Read {
                path: dir.to_path_buf(),
                source,
            });
        }
    }

    let mut draft = Draft::default();
    // The credential file is deliberately never opened — not even to check whether it exists.
    draft.note(
        "credentials are not migrated: `auth.json` is never read. Add an entry to \
         `~/.radian/credentials/auth.json` yourself.",
    );

    let settings = read_json(dir, "settings.json", &mut draft)?;
    let models = read_json(dir, "models.json", &mut draft)?;
    let keybindings = read_json(dir, "keybindings.json", &mut draft)?;

    map_settings(settings.as_ref(), &mut draft);
    map_models(settings.as_ref(), models.as_ref(), &mut draft);
    map_keybindings(keybindings.as_ref(), &mut draft);

    if dir.join("mcp.json").exists() {
        draft.note(
            "`mcp.json`: MCP servers are not migrated (this version of Radian's configuration \
             has no MCP section); pass the host's `--mcp-config` file instead",
        );
    }

    let toml = draft.render();
    // A draft the reader refuses must never leave this function: validating here means the
    // printed draft and the written draft are the same, accepted document.
    super::parse(Path::new("<draft>"), &toml).map_err(|error| PiMigrateError::InvalidDraft {
        message: error.to_string(),
    })?;
    Ok(PiDraft {
        toml,
        notes: draft.notes,
    })
}

/// Writes the draft to `path` atomically (temp file + rename), owner-only, **only when `path`
/// does not exist**. The draft is validated with the reader before and after the write, so
/// what lands on disk is what the host will load.
pub fn write_draft(path: &Path, toml_text: &str) -> Result<(), PiMigrateError> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(PiMigrateError::TargetExists {
            path: path.to_path_buf(),
        });
    }
    super::parse(path, toml_text).map_err(|error| PiMigrateError::InvalidDraft {
        message: error.to_string(),
    })?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        // Owner-only, like every other Radian data directory (ADR 0013/0020). A failure here
        // is not fatal: the write below reports it.
        let _ = crate::home::ensure_private_dir(parent);
    }
    let Some(name) = path.file_name() else {
        return Err(PiMigrateError::Write {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the path has no file name",
            ),
        });
    };
    let temp = path.with_file_name(format!(
        "{}.tmp-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    write_private(&temp, toml_text)?;
    if let Err(source) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(PiMigrateError::Write {
            path: path.to_path_buf(),
            source,
        });
    }
    // Read it back through the same reader: the file on disk must be the accepted document.
    let text = std::fs::read_to_string(path).map_err(|source| PiMigrateError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    super::parse(path, &text).map_err(|error| PiMigrateError::InvalidDraft {
        message: error.to_string(),
    })?;
    Ok(())
}

/// Creates `path` with mode `0600` (unix) and writes `text`, removing the file again if the
/// write fails. `create_new` means an existing file is never truncated.
fn write_private(path: &Path, text: &str) -> Result<(), PiMigrateError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| PiMigrateError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(source) = written {
        let _ = std::fs::remove_file(path);
        return Err(PiMigrateError::Write {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

/// Reads one JSON file, or notes that it is missing. A read error other than "not found" (a
/// directory in its place, a permission problem) is a real failure.
fn read_json(dir: &Path, name: &str, draft: &mut Draft) -> Result<Option<Json>, PiMigrateError> {
    let path = dir.join(name);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            draft.note(format!("`{name}`: not found, skipped"));
            return Ok(None);
        }
        Err(source) => return Err(PiMigrateError::Read { path, source }),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| PiMigrateError::Parse {
            path,
            message: error.to_string(),
        })
}

// ---------- settings.json ----------

fn map_settings(settings: Option<&Json>, draft: &mut Draft) {
    let Some(settings) = settings else { return };
    let Some(table) = settings.as_object() else {
        draft.note("`settings.json`: the top level is not an object, skipped");
        return;
    };
    let mut unmapped: Vec<&str> = Vec::new();
    for (key, value) in table {
        match key.as_str() {
            "defaultProvider" => match value.as_str() {
                Some(name) => match provider_protocol(name) {
                    Some(protocol) => draft.provider = Some(protocol.to_owned()),
                    None => draft.note(format!(
                        "`settings.defaultProvider`: Radian has no `{name}` provider; set \
                         `model.provider` (`openai-compatible` or `anthropic`) yourself"
                    )),
                },
                None => draft.note("`settings.defaultProvider`: not a string, skipped"),
            },
            "defaultModel" => match value.as_str() {
                Some(model) => draft.model = Some(model.to_owned()),
                None => draft.note("`settings.defaultModel`: not a string, skipped"),
            },
            "theme" => match value.as_str() {
                Some(theme) if matches!(theme, "dark" | "light" | "mono") => {
                    draft.theme = Some(theme.to_owned());
                }
                Some("system") => draft.note(
                    "`settings.theme`: Pi's `system` has no Radian equivalent (Radian's themes \
                     are `dark`, `light` and `mono`); set `tui.theme` yourself",
                ),
                Some(_) => draft
                    .note("`settings.theme`: not one of Radian's `dark`/`light`/`mono`; skipped"),
                None => draft.note("`settings.theme`: not a string, skipped"),
            },
            "compaction" => match value.as_object().and_then(|table| table.get("enabled")) {
                Some(Json::Bool(enabled)) => draft.auto_compact = Some(*enabled),
                Some(_) => draft.note("`settings.compaction.enabled`: not a boolean, skipped"),
                None => {}
            },
            "defaultThinkingLevel" => draft.note(
                "`settings.defaultThinkingLevel`: Radian has no configuration key for the \
                 thinking level (`--thinking` is a flag); skipped",
            ),
            other => unmapped.push(other),
        }
    }
    if !unmapped.is_empty() {
        draft.note(format!(
            "`settings.json`: {} other setting(s) not migrated: {}",
            unmapped.len(),
            name_list(&unmapped)
        ));
    }
}

/// Pi provider names that speak a protocol Radian has an adapter for. A name outside this
/// table is reported rather than guessed.
fn provider_protocol(name: &str) -> Option<&'static str> {
    match name {
        "anthropic" => Some("anthropic"),
        "openai" | "openai-compatible" | "ollama" | "lmstudio" | "lm-studio" | "vllm"
        | "sglang" | "openrouter" | "opencode" | "vercel-ai-gateway" | "groq" | "mistral"
        | "xai" | "deepseek" | "zai" | "cerebras" | "together" | "fireworks" | "moonshot" => {
            Some("openai-compatible")
        }
        _ => None,
    }
}

/// Pi `models.json` API types that Radian can reach.
fn api_protocol(api: &str) -> Option<&'static str> {
    match api {
        "anthropic" | "anthropic-messages" => Some("anthropic"),
        "openai-completions" | "openai-responses" | "azure-openai-responses" => {
            Some("openai-compatible")
        }
        _ => None,
    }
}

// ---------- models.json ----------

fn map_models(settings: Option<&Json>, models: Option<&Json>, draft: &mut Draft) {
    let Some(models) = models else { return };
    let Some(table) = models.as_object() else {
        draft.note("`models.json`: the top level is not an object, skipped");
        return;
    };
    let providers = table.get("providers").and_then(Json::as_object);
    let requested = settings
        .and_then(Json::as_object)
        .and_then(|settings| settings.get("defaultProvider"))
        .and_then(Json::as_str);

    let active: Option<(&str, &Json)> = match (providers, requested) {
        (Some(providers), Some(name)) => match providers.get(name) {
            Some(entry) => Some((name, entry)),
            None => {
                draft.note(format!(
                    "`models.json`: no `providers.{name}` entry; the endpoint, the model and \
                     the key are not migrated"
                ));
                None
            }
        },
        (Some(providers), None) => {
            if providers.len() == 1 {
                providers
                    .iter()
                    .next()
                    .map(|(name, entry)| (name.as_str(), entry))
            } else if providers.is_empty() {
                None
            } else {
                draft.note(format!(
                    "`models.json` defines {} providers and `settings.defaultProvider` is \
                     unset; set `model.base_url` and `model.api_key` yourself",
                    providers.len()
                ));
                None
            }
        }
        (None, _) => None,
    };

    if let Some((name, entry)) = active {
        let Some(entry) = entry.as_object() else {
            draft.note(format!("`models.providers.{name}`: not an object, skipped"));
            return;
        };
        if draft.provider.is_none()
            && let Some(api) = entry.get("api").and_then(Json::as_str)
        {
            match api_protocol(api) {
                Some(protocol) => draft.provider = Some(protocol.to_owned()),
                None => draft.note(format!(
                    "`models.providers.{name}.api`: Radian has no adapter for `{api}`; set \
                     `model.provider` yourself"
                )),
            }
        }
        match entry.get("baseUrl").and_then(Json::as_str) {
            Some(url) if url_has_credentials(url) => draft.note(format!(
                "`models.providers.{name}.baseUrl`: the URL carries credentials, so it is not \
                 migrated (its value is never shown); use a URL without credentials"
            )),
            Some(url) => draft.base_url = Some(url.to_owned()),
            None => {}
        }
        if let Some(value) = entry.get("apiKey") {
            secret_ref(value, name, draft);
        }
        if let Some(Json::Object(headers)) = entry.get("headers") {
            for (header, value) in headers {
                map_header(name, header, value, draft);
            }
        }
        if draft.model.is_none()
            && let Some(Json::Array(models)) = entry.get("models")
        {
            let ids: Vec<&str> = models
                .iter()
                .filter_map(|model| model.get("id").and_then(Json::as_str))
                .collect();
            match ids.len() {
                0 => {}
                1 => draft.model = Some(ids[0].to_owned()),
                count => {
                    draft.model = Some(ids[0].to_owned());
                    draft.note(format!(
                        "`models.providers.{name}.models` lists {count} models; `model.model` \
                         uses the first one"
                    ));
                }
            }
        }
        let unmapped: Vec<&str> = entry
            .keys()
            .filter(|key| {
                !matches!(
                    key.as_str(),
                    "baseUrl" | "api" | "apiKey" | "headers" | "models"
                )
            })
            .map(String::as_str)
            .collect();
        if !unmapped.is_empty() {
            draft.note(format!(
                "`models.providers.{name}`: {} field(s) not migrated: {}",
                unmapped.len(),
                name_list(&unmapped)
            ));
        }
    }

    let unmapped: Vec<&str> = table
        .keys()
        .filter(|key| key.as_str() != "providers")
        .map(String::as_str)
        .collect();
    if !unmapped.is_empty() {
        draft.note(format!(
            "`models.json`: {} top-level field(s) not migrated: {}",
            unmapped.len(),
            name_list(&unmapped)
        ));
    }
}

/// One `models.json` header value. Radian's `model.headers` holds plain strings, so only a
/// literal, non-secret value can be carried over.
fn map_header(provider: &str, header: &str, value: &Json, draft: &mut Draft) {
    let Some(text) = value.as_str() else {
        draft.note(format!(
            "`models.providers.{provider}.headers.{header}`: not a string, skipped"
        ));
        return;
    };
    match classify(text) {
        SecretShape::Literal => {
            if super::looks_like_secret(text) {
                draft.note(format!(
                    "`models.providers.{provider}.headers.{header}`: the value looks like a \
                     secret, so it is not copied (its value is never shown)"
                ));
            } else {
                draft.headers.push((header.to_owned(), text.to_owned()));
            }
        }
        _ => draft.note(format!(
            "`models.providers.{provider}.headers.{header}`: Radian's `model.headers` cannot \
             interpolate the environment or run a command; skipped"
        )),
    }
}

/// The shape of a Pi value that may hold a credential.
enum SecretShape<'a> {
    /// Exactly `$NAME` or `${NAME}`.
    Env(&'a str),
    /// A leading `!command`.
    Command,
    /// `$NAME` inside a longer string.
    Interpolated,
    /// Everything else.
    Literal,
}

fn classify(text: &str) -> SecretShape<'_> {
    if text.starts_with('!') {
        return SecretShape::Command;
    }
    if let Some(name) = env_name(text) {
        return SecretShape::Env(name);
    }
    if contains_interpolation(text) {
        return SecretShape::Interpolated;
    }
    SecretShape::Literal
}

fn env_name(text: &str) -> Option<&str> {
    let inner = if let Some(rest) = text.strip_prefix("${") {
        rest.strip_suffix('}')?
    } else {
        text.strip_prefix('$')?
    };
    (!inner.is_empty() && inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then_some(inner)
}

fn contains_interpolation(text: &str) -> bool {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            match chars.peek() {
                Some('{') => return true,
                Some(next) if next.is_ascii_alphanumeric() || *next == '_' => return true,
                _ => {}
            }
        }
    }
    false
}

/// Reports a Pi `apiKey` as a note: the key is **never** copied into the draft (ADR 0026 D2
/// removed `model.api_key`; the key belongs in `auth.json`). A literal value is never shown.
fn secret_ref(value: &Json, provider: &str, draft: &mut Draft) {
    let auth_hint = |env: &str| {
        format!(
            "put it in `~/.radian/credentials/auth.json` as \
             {{\"{provider}\": {{\"type\": \"api_key\", \"key\": \"${env}\"}}}}"
        )
    };
    let Some(text) = value.as_str() else {
        draft.note(format!(
            "`models.providers.{provider}.apiKey`: not a string, skipped"
        ));
        return;
    };
    match classify(text) {
        SecretShape::Env(name) => draft.note(format!(
            "`models.providers.{provider}.apiKey`: credentials are not migrated; {}",
            auth_hint(name)
        )),
        SecretShape::Command => draft.note(format!(
            "`models.providers.{provider}.apiKey`: a `!command` value is not supported and was \
             not executed; {}",
            auth_hint(&suggested_env(provider))
        )),
        SecretShape::Interpolated => draft.note(format!(
            "`models.providers.{provider}.apiKey`: the value interpolates a variable inside a \
             longer string and was not migrated; {}",
            auth_hint(&suggested_env(provider))
        )),
        SecretShape::Literal => draft.note(format!(
            "`models.providers.{provider}.apiKey`: a literal value was not migrated (its value \
             is never shown); {}",
            auth_hint(&suggested_env(provider))
        )),
    }
}

/// The environment variable a literal key should move to, e.g. `openai` → `OPENAI_API_KEY`.
fn suggested_env(provider: &str) -> String {
    let sanitized: String = provider
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "PI_API_KEY".to_owned()
    } else {
        format!("{sanitized}_API_KEY")
    }
}

/// Whether a URL carries credentials (`scheme://user:pass@host`). Such a value is a secret and
/// is never migrated.
fn url_has_credentials(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    authority.contains('@')
}

// ---------- keybindings.json ----------

/// Pi action id → Radian action name. Only actions with the same meaning are listed; every
/// other Pi action is reported instead.
const PI_ACTIONS: &[(&str, &str)] = &[
    ("tui.input.submit", "send"),
    ("tui.input.newLine", "newline"),
    ("tui.input.tab", "complete"),
    ("tui.editor.cursorLeft", "cursor_left"),
    ("tui.editor.cursorRight", "cursor_right"),
    ("tui.editor.cursorWordLeft", "word_left"),
    ("tui.editor.cursorWordRight", "word_right"),
    ("tui.editor.cursorLineStart", "line_start"),
    ("tui.editor.cursorLineEnd", "line_end"),
    ("tui.editor.historyPrevious", "history_prev"),
    ("tui.editor.historyNext", "history_next"),
    ("tui.editor.deleteCharBackward", "delete_before"),
    ("tui.editor.deleteCharForward", "delete_under"),
    ("tui.editor.deleteWordBackward", "delete_word"),
    ("tui.editor.deleteToLineStart", "delete_to_line_start"),
    ("tui.editor.deleteToLineEnd", "delete_to_line_end"),
    ("tui.altScreen.pageUp", "scroll_page_up"),
    ("tui.altScreen.pageDown", "scroll_page_down"),
    ("tui.altScreen.lineUp", "scroll_line_up"),
    ("tui.altScreen.lineDown", "scroll_line_down"),
    ("tui.altScreen.top", "scroll_top"),
    ("tui.altScreen.bottom", "scroll_bottom"),
    ("tui.altScreen.search", "find"),
    ("tui.altScreen.searchNext", "find_next"),
    ("tui.altScreen.searchPrevious", "find_prev"),
    ("tui.altScreen.searchClose", "find_close"),
];

/// A snapshot of `radian-tui`'s built-in transcript/input bindings. `radian-tui`'s `keys.rs`
/// stays the authority; this copy exists only so the draft does not bind a key that is
/// already a **different** action's default (the TUI refuses to start on that conflict). A
/// drift here can only lose a warning — it can never cause a wrong write.
const RADIAN_MAIN_DEFAULTS: &[(&str, &[&str])] = &[
    ("send", &["enter"]),
    ("newline", &["ctrl-j", "alt-enter", "shift-enter"]),
    ("cursor_left", &["left"]),
    ("cursor_right", &["right"]),
    ("word_left", &["ctrl-left", "alt-left"]),
    ("word_right", &["ctrl-right", "alt-right"]),
    ("line_start", &["home", "ctrl-a"]),
    ("line_end", &["end", "ctrl-e"]),
    ("history_prev", &["up"]),
    ("history_next", &["down"]),
    ("delete_before", &["backspace"]),
    ("delete_under", &["delete"]),
    ("delete_word", &["ctrl-w"]),
    ("delete_to_line_start", &["ctrl-u"]),
    ("delete_to_line_end", &["ctrl-k"]),
    ("scroll_page_up", &["pageup"]),
    ("scroll_page_down", &["pagedown"]),
    ("scroll_line_up", &["shift-up"]),
    ("scroll_line_down", &["shift-down"]),
    ("scroll_top", &["ctrl-home"]),
    ("scroll_bottom", &["ctrl-end"]),
    ("escape", &["esc"]),
    ("cancel", &["ctrl-c"]),
    ("complete", &["tab"]),
    ("find", &["ctrl-f"]),
];

/// The find box is a layer of its own (its keys are looked up first), so a key may be a
/// default in both layers.
const RADIAN_FIND_DEFAULTS: &[(&str, &[&str])] = &[
    ("find_prev", &["enter", "ctrl-f"]),
    ("find_next", &["shift-enter", "ctrl-g"]),
    ("find_close", &["esc"]),
];

fn map_keybindings(keybindings: Option<&Json>, draft: &mut Draft) {
    let Some(keybindings) = keybindings else {
        return;
    };
    let Some(table) = keybindings.as_object() else {
        draft.note("`keybindings.json`: the top level is not an object, skipped");
        return;
    };
    let mut emitted: Vec<(String, Vec<String>)> = Vec::new();
    let mut used: HashMap<String, String> = HashMap::new();
    let mut unmapped: Vec<&str> = Vec::new();
    for (action, value) in table {
        let Some(radian) = pi_action(action) else {
            unmapped.push(action);
            continue;
        };
        let keys: Vec<&str> = match value {
            Json::String(text) => vec![text.as_str()],
            Json::Array(items) => items.iter().filter_map(Json::as_str).collect(),
            _ => {
                draft.note(format!(
                    "`keybindings.{action}`: not a key or a list of keys, skipped"
                ));
                continue;
            }
        };
        for key in keys {
            let Some(translated) = radian_key(key) else {
                draft.note(format!(
                    "`keybindings.{action} = {key:?}`: Radian cannot express that key; skipped"
                ));
                continue;
            };
            if let Some(other) = radian_default_action(radian, &translated)
                && other != radian
            {
                draft.note(format!(
                    "`keybindings.{action} = {key:?}`: `{translated}` is already Radian's \
                     default for `{other}`, so binding it too would make the TUI refuse to \
                     start; skipped"
                ));
                continue;
            }
            if let Some(other) = used.get(&translated)
                && other != radian
            {
                draft.note(format!(
                    "`keybindings.{action} = {key:?}`: `{translated}` is already bound to \
                     `{other}` in this draft; skipped"
                ));
                continue;
            }
            used.insert(translated.clone(), radian.to_owned());
            match emitted.iter_mut().find(|(name, _)| name == radian) {
                Some((_, keys)) => {
                    if !keys.contains(&translated) {
                        keys.push(translated);
                    }
                }
                None => emitted.push((radian.to_owned(), vec![translated])),
            }
        }
    }
    if !unmapped.is_empty() {
        draft.note(format!(
            "`keybindings.json`: {} action(s) have no Radian equivalent: {}",
            unmapped.len(),
            name_list(&unmapped)
        ));
    }
    if !emitted.is_empty() {
        draft.note(
            "`tui.keybindings` was translated from Pi's key syntax; review it, because the TUI \
             refuses to start when one key is bound to two actions",
        );
    }
    draft.keybindings = emitted;
}

fn pi_action(name: &str) -> Option<&'static str> {
    PI_ACTIONS
        .iter()
        .find(|(pi, _)| *pi == name)
        .map(|(_, radian)| *radian)
}

/// The action a key is the built-in default of, within `action`'s layer.
fn radian_default_action<'a>(action: &str, key: &'a str) -> Option<&'a str> {
    let layer = if is_find_action(action) {
        RADIAN_FIND_DEFAULTS
    } else {
        RADIAN_MAIN_DEFAULTS
    };
    layer
        .iter()
        .find(|(_, keys)| keys.contains(&key))
        .map(|(action, _)| *action)
}

fn is_find_action(action: &str) -> bool {
    matches!(action, "find_prev" | "find_next" | "find_close")
}

/// Translates Pi's `modifier+key` into Radian's `[ctrl-][alt-][shift-]key`. `None` when
/// Radian cannot express the key (function keys, `super`, …).
fn radian_key(text: &str) -> Option<String> {
    let mut parts: Vec<&str> = text.split('+').collect();
    let key = parts.pop()?;
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    for modifier in parts {
        match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            // `super` (and `cmd`) need a terminal protocol Radian does not use.
            _ => return None,
        }
    }
    let lower = key.to_ascii_lowercase();
    let code = match lower.as_str() {
        "escape" | "esc" => "esc",
        "enter" | "return" => "enter",
        "tab" => "tab",
        "space" => "space",
        "backspace" => "backspace",
        "delete" => "delete",
        "insert" => "insert",
        "home" => "home",
        "end" => "end",
        "pageup" => "pageup",
        "pagedown" => "pagedown",
        "up" => "up",
        "down" => "down",
        "left" => "left",
        "right" => "right",
        other if other.chars().count() == 1 => other,
        _ => return None,
    };
    let mut out = String::new();
    if ctrl {
        out.push_str("ctrl-");
    }
    if alt {
        out.push_str("alt-");
    }
    if shift {
        out.push_str("shift-");
    }
    out.push_str(code);
    Some(out)
}

/// `a`, `b`, `c` — capped, so a long list stays one readable line.
fn name_list(names: &[&str]) -> String {
    const CAP: usize = 8;
    let shown: Vec<String> = names
        .iter()
        .take(CAP)
        .map(|name| format!("`{name}`"))
        .collect();
    let mut text = shown.join(", ");
    if names.len() > CAP {
        text.push_str(&format!(", and {} more", names.len() - CAP));
    }
    text
}

// ---------- the draft ----------

#[derive(Default)]
struct Draft {
    provider: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    headers: Vec<(String, String)>,
    auto_compact: Option<bool>,
    theme: Option<String>,
    keybindings: Vec<(String, Vec<String>)>,
    notes: Vec<String>,
}

impl Draft {
    fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }

    /// The draft as TOML. Built through the `toml` serializer so every value is escaped
    /// correctly, and only keys the reader implements are emitted.
    fn render(&self) -> String {
        let mut root = toml::map::Map::new();
        root.insert(
            "schema_version".into(),
            Toml::Integer(super::SUPPORTED_SCHEMA_VERSION as i64),
        );
        let mut model = toml::map::Map::new();
        if let Some(provider) = &self.provider {
            model.insert("provider".into(), Toml::String(provider.clone()));
        }
        if let Some(url) = &self.base_url {
            model.insert("base_url".into(), Toml::String(url.clone()));
        }
        if let Some(name) = &self.model {
            model.insert("model".into(), Toml::String(name.clone()));
        }
        if !self.headers.is_empty() {
            let mut headers = toml::map::Map::new();
            for (name, value) in &self.headers {
                headers.insert(name.clone(), Toml::String(value.clone()));
            }
            model.insert("headers".into(), Toml::Table(headers));
        }
        if !model.is_empty() {
            root.insert("model".into(), Toml::Table(model));
        }
        if let Some(auto) = self.auto_compact {
            let mut compaction = toml::map::Map::new();
            compaction.insert("auto".into(), Toml::Boolean(auto));
            root.insert("compaction".into(), Toml::Table(compaction));
        }
        if let Some(theme) = &self.theme {
            let mut tui = match root.remove("tui") {
                Some(Toml::Table(table)) => table,
                _ => toml::map::Map::new(),
            };
            tui.insert("theme".into(), Toml::String(theme.clone()));
            root.insert("tui".into(), Toml::Table(tui));
        }
        if !self.keybindings.is_empty() {
            let mut tui = match root.remove("tui") {
                Some(Toml::Table(table)) => table,
                _ => toml::map::Map::new(),
            };
            let mut bindings = toml::map::Map::new();
            for (action, keys) in &self.keybindings {
                bindings.insert(
                    action.clone(),
                    Toml::Array(keys.iter().cloned().map(Toml::String).collect()),
                );
            }
            tui.insert("keybindings".into(), Toml::Table(bindings));
            root.insert("tui".into(), Toml::Table(tui));
        }
        let body = toml::to_string(&Toml::Table(root)).expect("the draft is serializable");
        format!(
            "# A Radian configuration drafted from a Pi agent directory by\n\
             # `radian config import-pi`. Radian does not promise compatibility with Pi's file\n\
             # format: review this file, and read the notes the command printed on stderr for\n\
             # everything that could not be mapped.\n\
             {body}"
        )
    }
}
