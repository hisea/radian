//! The credential store (ADR 0026 D1/D2): `~/.radian/credentials/auth.json`, **format
//! identical to pi's `auth.json`** so a file can be copied either way.
//!
//! The file is a flat JSON object keyed by provider id. An entry is either
//! `{"type":"api_key","key":string,"env":{string:string}}` or an OAuth entry
//! (`{"type":"oauth","access":…,"refresh":…,"expires":…}`). This build **uses** only
//! `api_key`; `oauth` and unknown entries are kept verbatim on write so nothing the user
//! logged into in pi is lost.
//!
//! Rules (ADR 0026 D1):
//!
//! * a missing file is an empty store;
//! * a file wider than `0600` is **refused**, never chmodded;
//! * `key` is a template: `$NAME` / `${NAME}` **anywhere** in the string, `$$` for a literal
//!   `$`, `$!` for a literal `!` (pi's `resolveConfigValue`, verified
//!   `@earendil-works/pi-coding-agent/dist/core/resolve-config-value.js:21-64`, read
//!   2026-10-13); the entry's `env` object is the first lookup source, then the process
//!   environment;
//! * a `key` starting with `!` is a command and is **refused without being run** (it would
//!   execute outside the sandbox);
//! * writes are atomic (temp file + rename), `0600`, in a `0700` directory;
//! * **concurrent writes are not locked**: two `/login` runs read-modify-write without a lock,
//!   so the later write wins and may drop the earlier provider's entry. Accepted for the first
//!   release (a lock is a later slice if it is needed). Each write uses its own uniquely named
//!   temp file (`tmp-<pid>-<counter>-<nanos>`), so concurrent writers in one process no longer
//!   **delete each other's** temp file — the last-writer-wins rule stays, but the loser is not
//!   left with an I/O error;
//! * the key is never printed: [`ResolvedKey`]'s `Debug` redacts it (ADR 0013).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::home::credentials_dir;

/// The file name inside the credentials directory.
pub const AUTH_FILE_NAME: &str = "auth.json";

/// The `env` key the generic `openai-compatible` provider uses for its base URL (ADR 0026 D1:
/// a Radian-defined name pi ignores).
pub const RADIAN_BASE_URL: &str = "RADIAN_BASE_URL";

/// Why the credential store could not be read or written.
#[derive(Debug)]
pub enum AuthError {
    /// Neither `RADIAN_HOME` nor `HOME` is usable, so there is no credentials directory.
    NoHome,
    /// The file could not be read or written.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file is group/world-accessible. It is refused, never silently chmodded.
    WidePermissions { path: PathBuf, mode: u32 },
    /// The file is not a JSON object.
    Corrupt { path: PathBuf, message: String },
    /// A `$NAME` / `${NAME}` reference names a variable that is not set.
    MissingEnv { provider_id: String, name: String },
    /// `/logout <provider> <name>` named a key the entry does not have.
    UnknownKey {
        provider_id: String,
        name: String,
        known: Vec<String>,
    },
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHome => write!(
                f,
                "no credential store: set RADIAN_HOME or HOME to a directory"
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::WidePermissions { path, mode } => write!(
                f,
                "{} is accessible to other users (mode {mode:04o}); refusing to read it. Fix: \
                 chmod 600 {}",
                path.display(),
                path.display()
            ),
            Self::Corrupt { path, message } => {
                write!(f, "{}: not a valid auth.json: {message}", path.display())
            }
            Self::MissingEnv { provider_id, name } => write!(
                f,
                "`{provider_id}` key references ${name}, which is not set in the environment"
            ),
            Self::UnknownKey {
                provider_id,
                name,
                known,
            } => write!(
                f,
                "`{provider_id}` has no key named `{name}`; known: {}",
                if known.is_empty() {
                    "(none)".to_owned()
                } else {
                    known.join(", ")
                }
            ),
        }
    }
}

impl std::error::Error for AuthError {}

/// A usable credential for one provider. `Debug` never prints the key (ADR 0013).
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedKey {
    pub provider_id: String,
    pub key: String,
    /// The entry's `env` object, with `$ENV` references already resolved. The generic
    /// `openai-compatible` provider reads [`RADIAN_BASE_URL`] from here.
    pub env: BTreeMap<String, String>,
}

/// One key of an `api_key` entry, in file order (ADR 0026 D1). `Debug` never prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct NamedKey {
    /// `Some("first")` for the entry's `key`, `Some(name)` for a named `keys[]` item, and
    /// `None` for an unnamed item (shown by its masked form).
    pub name: Option<String>,
    pub key: String,
    /// The 0-based position in the entry's `keys` array, or `None` for the top-level `key`.
    /// `/logout` needs it because a refused item does not appear among the usable keys.
    pub item: Option<usize>,
}

impl NamedKey {
    /// The name a display uses: the stored name, or the key's masked form.
    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| mask_key(&self.key))
    }
}

impl fmt::Debug for NamedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NamedKey")
            .field("name", &self.display_name())
            .field("key", &"<redacted>")
            .finish()
    }
}

/// Every key of one `api_key` entry, in file order (ADR 0026 D1).
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedKeys {
    pub provider_id: String,
    /// The entry's `key` first (named `first`), then each usable `keys[]` item.
    pub keys: Vec<NamedKey>,
    /// The entry's `env` object, raw (never interpolated).
    pub env: BTreeMap<String, String>,
    /// One line per `keys[]` item refused because its value is a `!command` (or has no
    /// string `key`). A refused item never removes the other keys.
    pub notices: Vec<String>,
}

impl fmt::Debug for ResolvedKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedKeys")
            .field("provider_id", &self.provider_id)
            .field("keys", &self.keys)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("notices", &self.notices)
            .finish()
    }
}

/// One key as `/logout` lists it: the display name and the masked value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChoice {
    pub name: String,
    pub masked: String,
}

impl fmt::Debug for ResolvedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedKey")
            .field("provider_id", &self.provider_id)
            .field("key", &"<redacted>")
            // The values may carry a credential too (a Cloudflare account id, a base URL with
            // userinfo); only the names are shown.
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// One entry in [`AuthStore::list`], for `/model`'s "logged-in providers" view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthEntry {
    pub provider_id: String,
    /// The entry's `type` (`api_key`, `oauth`, or an unknown string).
    pub kind: String,
    /// True for an `api_key` entry whose key resolves (not `!`, its env vars are set), or
    /// (ADR 0026, `chatgpt` only) an `oauth` entry. See [`AuthStore::is_usable`].
    pub usable: bool,
}

/// An `oauth` entry's three reserved fields (ADR 0026 D1): `{"type":"oauth","access",
/// "refresh","expires"}`. The ChatGPT workspace id is **not** one of them (ADR 0026
/// decision 3): it is decoded from `access` each time a caller needs it, never stored.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthEntry {
    pub access: String,
    pub refresh: String,
    /// Unix epoch milliseconds (pi's `auth.json` convention for this field).
    pub expires_at_millis: u64,
}

impl fmt::Debug for OAuthEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthEntry")
            .field("access", &"<redacted>")
            .field("refresh", &"<redacted>")
            .field("expires_at_millis", &self.expires_at_millis)
            .finish()
    }
}

/// The parsed `auth.json`, kept as raw JSON so unknown entries survive a write.
pub struct AuthStore {
    path: PathBuf,
    entries: Map<String, Value>,
}

impl AuthStore {
    /// Opens the store at the process's `~/.radian/credentials/auth.json` (creating the
    /// directory `0700`). A missing file is an empty store.
    pub fn open() -> Result<Self, AuthError> {
        let path = auth_path().ok_or(AuthError::NoHome)?;
        Self::open_at(path)
    }

    /// Opens the store at an explicit path (tests use a temporary `RADIAN_HOME`).
    pub fn open_at(path: PathBuf) -> Result<Self, AuthError> {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                check_permissions(&path)?;
                let value: Value =
                    serde_json::from_str(&text).map_err(|error| AuthError::Corrupt {
                        path: path.clone(),
                        message: error.to_string(),
                    })?;
                let Value::Object(entries) = value else {
                    return Err(AuthError::Corrupt {
                        path: path.clone(),
                        message: "the top level must be a JSON object".into(),
                    });
                };
                Ok(Self { path, entries })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                path,
                entries: Map::new(),
            }),
            Err(source) => Err(AuthError::Io { path, source }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every key for `provider_id`, in file order, or `None` when there is no usable
    /// `api_key` entry (missing, `oauth`, unknown type, or no `key` at all). [`Self::note`]
    /// explains a `None` that is a refusal rather than an absence.
    ///
    /// An entry's `key` is the first key (named `first`); each `keys[]` item follows it in
    /// order. A `keys[]` item whose value starts with `!` is refused without being run and
    /// contributes one notice; the other keys stay usable (ADR 0026 D1).
    pub fn resolve_keys(&self, provider_id: &str) -> Result<Option<ResolvedKeys>, AuthError> {
        let Some(entry) = self.entries.get(provider_id).and_then(Value::as_object) else {
            return Ok(None);
        };
        if entry.get("type").and_then(Value::as_str) != Some("api_key") {
            return Ok(None);
        }
        let Some(key) = entry.get("key").and_then(Value::as_str) else {
            return Ok(None);
        };
        // The entry's `env` object is the first lookup source, then the process environment
        // (pi: `resolveConfigValue(key, credential.env)`). Its values are copied **raw**: pi
        // does not interpolate them (only the key is resolved).
        let mut env = BTreeMap::new();
        if let Some(values) = entry.get("env").and_then(Value::as_object) {
            for (name, value) in values {
                if let Some(value) = value.as_str() {
                    env.insert(name.clone(), value.to_owned());
                }
            }
        }
        let mut keys = Vec::new();
        let mut notices = Vec::new();
        if key.starts_with('!') {
            // A command: refused, and deliberately never run (ADR 0026 D1).
            notices.push(refusal(provider_id, "its `key`"));
        } else {
            keys.push(NamedKey {
                name: Some("first".to_owned()),
                key: resolve_template(provider_id, key, &env)?,
                item: None,
            });
        }
        if let Some(items) = entry.get("keys").and_then(Value::as_array) {
            for (index, item) in items.iter().enumerate() {
                let label = format!("keys[{}]", index + 1);
                let Some(object) = item.as_object() else {
                    notices.push(format!("`{provider_id}` {label} is not an object; skipped"));
                    continue;
                };
                let name = object
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty());
                let shown =
                    name.map_or_else(|| label.clone(), |name| format!("{label} \"{name}\""));
                let Some(key) = object.get("key").and_then(Value::as_str) else {
                    notices.push(format!("`{provider_id}` {shown} has no `key`; skipped"));
                    continue;
                };
                if key.starts_with('!') {
                    notices.push(refusal(provider_id, &shown));
                    continue;
                }
                keys.push(NamedKey {
                    name: name.map(str::to_owned),
                    key: resolve_template(provider_id, key, &env)?,
                    item: Some(index),
                });
            }
        }
        Ok(Some(ResolvedKeys {
            provider_id: provider_id.to_owned(),
            keys,
            env,
            notices,
        }))
    }

    /// The usable first key for `provider_id`, or `None` when there is no usable `api_key`
    /// entry. [`Self::resolve_keys`] carries every key.
    pub fn resolve_key(&self, provider_id: &str) -> Result<Option<ResolvedKey>, AuthError> {
        let Some(resolved) = self.resolve_keys(provider_id)? else {
            return Ok(None);
        };
        Ok(resolved.keys.into_iter().next().map(|first| ResolvedKey {
            provider_id: provider_id.to_owned(),
            key: first.key,
            env: resolved.env,
        }))
    }

    /// The entry's keys as `/logout` lists them: the display name and the masked value.
    /// Empty when the entry is not an `api_key` entry or has no key. The values are **not**
    /// resolved: a name and a mask need no `$ENV` lookup, so a missing variable never hides a
    /// key the user wants to remove.
    pub fn key_choices(&self, provider_id: &str) -> Result<Vec<KeyChoice>, AuthError> {
        Ok(self
            .entries
            .get(provider_id)
            .and_then(Value::as_object)
            .map(key_slots)
            .unwrap_or_default()
            .into_iter()
            .map(|slot| KeyChoice {
                name: slot.name,
                masked: slot.masked,
            })
            .collect())
    }

    /// A one-line explanation when `resolve_key` returned `None` because the entry is
    /// **refused** (a `!command` key, or an `oauth` entry this build cannot use), or when a
    /// `keys[]` item was refused. `None` for a missing entry or a usable one.
    pub fn note(&self, provider_id: &str) -> Option<String> {
        let entry = self.entries.get(provider_id).and_then(Value::as_object)?;
        match entry.get("type").and_then(Value::as_str) {
            Some("api_key") => {
                let notices = self
                    .resolve_keys(provider_id)
                    .ok()
                    .flatten()
                    .map(|resolved| resolved.notices)
                    .unwrap_or_default();
                if !notices.is_empty() {
                    return Some(notices.join(" "));
                }
                let key = entry.get("key").and_then(Value::as_str)?;
                key.starts_with('!')
                    .then(|| refusal(provider_id, "its `key`"))
            }
            Some("oauth") => Some(format!(
                "`{provider_id}` is an OAuth entry; this build supports API keys only. Run \
                 `/login {provider_id}`."
            )),
            _ => None,
        }
    }

    /// A named provider's declaration from its entry's `env` (ADR 0026 D10:
    /// `RADIAN_BASE_URL`, `RADIAN_KIND`), **even when the key itself is unusable** — the
    /// declaration is what tells the host which endpoint and protocol the name names. `None`
    /// when there is no entry or no `env` object.
    pub fn entry_env(&self, provider_id: &str) -> Option<BTreeMap<String, String>> {
        let values = self
            .entries
            .get(provider_id)
            .and_then(Value::as_object)?
            .get("env")?
            .as_object()?;
        let mut env = BTreeMap::new();
        for (name, value) in values {
            if let Some(value) = value.as_str() {
                env.insert(name.clone(), value.to_owned());
            }
        }
        Some(env)
    }

    /// Adds or replaces the `api_key` entry for `provider_id`, merging into any existing
    /// object so its unknown fields survive. Other entries are untouched.
    pub fn upsert_api_key(
        &mut self,
        provider_id: &str,
        key: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<(), AuthError> {
        let mut entry = self
            .entries
            .get(provider_id)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        entry.insert("type".into(), json!("api_key"));
        entry.insert("key".into(), json!(key));
        set_env(&mut entry, env);
        self.entries
            .insert(provider_id.to_owned(), Value::Object(entry));
        self.write()
    }

    /// Appends a further key to the `api_key` entry for `provider_id` (ADR 0026 D1): the
    /// entry's `key` stays the first key, and the new value joins `keys` in order. `name`
    /// is optional; an unnamed item shows as its masked form. Merges into any existing
    /// object so its unknown fields survive; other entries are untouched.
    pub fn add_api_key(
        &mut self,
        provider_id: &str,
        key: &str,
        name: Option<&str>,
        env: &BTreeMap<String, String>,
    ) -> Result<(), AuthError> {
        let mut entry = self
            .entries
            .get(provider_id)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        entry.insert("type".into(), json!("api_key"));
        if entry.get("key").and_then(Value::as_str).is_none() {
            // No first key yet: the new key becomes it. The file keeps a `key`, so pi reads
            // it; the shape has no name slot on the first key, so a name is not stored.
            entry.insert("key".into(), json!(key));
        } else {
            let mut item = Map::new();
            if let Some(name) = name.filter(|name| !name.is_empty()) {
                item.insert("name".into(), json!(name));
            }
            item.insert("key".into(), json!(key));
            let mut keys = entry
                .get("keys")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            keys.push(Value::Object(item));
            entry.insert("keys".into(), Value::Array(keys));
        }
        set_env(&mut entry, env);
        self.entries
            .insert(provider_id.to_owned(), Value::Object(entry));
        self.write()
    }

    /// Removes one key from the entry, or the whole entry when `name` is `None` (the `/logout`
    /// `all` choice). Removing the first key promotes the next one to `key`, so the file keeps
    /// a `key` and pi keeps reading it (ADR 0026 D1). The last key removes the entry.
    /// An unknown name is refused, naming the known ones.
    pub fn remove_key(&mut self, provider_id: &str, name: Option<&str>) -> Result<(), AuthError> {
        let Some(name) = name else {
            return self.remove(provider_id);
        };
        let Some(entry) = self.entries.get(provider_id).and_then(Value::as_object) else {
            return Err(AuthError::UnknownKey {
                provider_id: provider_id.to_owned(),
                name: name.to_owned(),
                known: Vec::new(),
            });
        };
        if entry.get("type").and_then(Value::as_str) != Some("api_key") {
            return Err(AuthError::UnknownKey {
                provider_id: provider_id.to_owned(),
                name: name.to_owned(),
                known: Vec::new(),
            });
        }
        let slots = key_slots(entry);
        let Some(slot) = slots.iter().find(|slot| slot.name == name) else {
            return Err(AuthError::UnknownKey {
                provider_id: provider_id.to_owned(),
                name: name.to_owned(),
                known: slots.into_iter().map(|slot| slot.name).collect(),
            });
        };
        if slots.len() <= 1 {
            // The one key is gone: the entry has no credential left.
            return self.remove(provider_id);
        }
        let item = slot.item;
        let mut entry = entry.clone();
        let mut keys = entry
            .get("keys")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        match item {
            // The top-level `key`: promote the next usable `keys[]` item to `key` and drop it
            // from the list, so the file keeps a `key` and pi keeps reading it.
            None => {
                let next = slots
                    .iter()
                    .find(|slot| slot.item.is_some())
                    .and_then(|slot| slot.item);
                if let Some(next) = next
                    && let Some(key) = keys
                        .get(next)
                        .and_then(Value::as_object)
                        .and_then(|item| item.get("key"))
                        .and_then(Value::as_str)
                {
                    entry.insert("key".into(), json!(key));
                    keys.remove(next);
                }
            }
            // A `keys[]` item: remove exactly that array element, refused items included in
            // the numbering.
            Some(item) => {
                keys.remove(item.min(keys.len().saturating_sub(1)));
            }
        }
        if keys.is_empty() {
            entry.remove("keys");
        } else {
            entry.insert("keys".into(), Value::Array(keys));
        }
        self.entries
            .insert(provider_id.to_owned(), Value::Object(entry));
        self.write()
    }

    /// The `oauth` entry for `provider_id`, or `None` when there is no entry, it is some other
    /// type, or it is missing one of the three reserved fields.
    pub fn oauth_entry(&self, provider_id: &str) -> Option<OAuthEntry> {
        let entry = self.entries.get(provider_id)?.as_object()?;
        if entry.get("type").and_then(Value::as_str) != Some("oauth") {
            return None;
        }
        Some(OAuthEntry {
            access: entry.get("access")?.as_str()?.to_owned(),
            refresh: entry.get("refresh")?.as_str()?.to_owned(),
            expires_at_millis: entry.get("expires")?.as_u64()?,
        })
    }

    /// Adds or replaces the `oauth` entry for `provider_id`, merging into any existing object
    /// so an unknown field (one a future Radian, or pi, added) survives — the same rule
    /// [`Self::upsert_api_key`] follows. Other entries are untouched.
    pub fn upsert_oauth(&mut self, provider_id: &str, entry: &OAuthEntry) -> Result<(), AuthError> {
        let mut object = self
            .entries
            .get(provider_id)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        object.insert("type".into(), json!("oauth"));
        object.insert("access".into(), json!(entry.access));
        object.insert("refresh".into(), json!(entry.refresh));
        object.insert("expires".into(), json!(entry.expires_at_millis));
        self.entries
            .insert(provider_id.to_owned(), Value::Object(object));
        self.write()
    }

    /// Removes the entry for `provider_id` (the `/logout` operation), leaving every other
    /// entry byte-for-byte. Writing an empty object is fine; the file is not deleted.
    pub fn remove(&mut self, provider_id: &str) -> Result<(), AuthError> {
        self.entries.remove(provider_id);
        self.write()
    }

    /// Every entry: id, type, and whether it is usable in this build.
    /// Every usable key **and every `oauth` entry's access/refresh token** in this store,
    /// sorted and deduplicated (ADR 0013 T4; ADR 0026: an OAuth token
    /// must be redacted exactly like an API key — never printed, journaled, or exported). The
    /// free [`resolved_keys`] is this for the process store.
    pub fn resolved_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        for entry in self.list() {
            if let Ok(Some(resolved)) = self.resolve_keys(&entry.provider_id) {
                for key in resolved.keys {
                    if !key.key.is_empty() {
                        keys.push(key.key);
                    }
                }
            }
            if let Some(oauth) = self.oauth_entry(&entry.provider_id) {
                if !oauth.access.is_empty() {
                    keys.push(oauth.access);
                }
                if !oauth.refresh.is_empty() {
                    keys.push(oauth.refresh);
                }
            }
        }
        keys.sort();
        keys.dedup();
        keys
    }

    pub fn list(&self) -> Vec<AuthEntry> {
        self.entries
            .iter()
            .map(|(provider_id, value)| {
                let kind = value
                    .as_object()
                    .and_then(|object| object.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                AuthEntry {
                    provider_id: provider_id.clone(),
                    kind,
                    usable: self.is_usable(provider_id),
                }
            })
            .collect()
    }

    /// Whether `provider_id` has **any** login a host can actually use: an `api_key` entry
    /// that resolves, or (ADR 0026, scoped to `chatgpt` only) an `oauth` entry.
    /// These are different shapes — `resolve_key` only ever returns the former, since an
    /// oauth entry carries no single static key, only an access/refresh pair a host refreshes
    /// itself — so a caller that only wants "is this provider logged in" (not the key itself)
    /// uses this instead.
    ///
    /// The oauth exception is scoped to `chatgpt` by id, not "any provider with an oauth
    /// entry": a stray oauth entry under, say, `anthropic` (copied from pi, or a future
    /// import) must not make that provider look logged-in when nothing in this build knows
    /// how to use an oauth credential there.
    pub fn is_usable(&self, provider_id: &str) -> bool {
        self.resolve_keys(provider_id)
            .ok()
            .flatten()
            .is_some_and(|resolved| !resolved.keys.is_empty())
            || (provider_id == crate::oauth_chatgpt::PROVIDER_ID
                && self.oauth_entry(provider_id).is_some())
    }

    /// Atomically writes the store: a temp file in the same directory, `0600`, then rename.
    /// The directory is created `0700`.
    fn write(&self) -> Result<(), AuthError> {
        let dir = self.path.parent().ok_or_else(|| AuthError::Io {
            path: self.path.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the auth path has no directory",
            ),
        })?;
        radian_session::ensure_private_dir(dir).map_err(|source| AuthError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let text = serde_json::to_string_pretty(&Value::Object(self.entries.clone())).map_err(
            |error| AuthError::Corrupt {
                path: self.path.clone(),
                message: error.to_string(),
            },
        )?;
        // The shared atomic write owns the unique temp name, `create_new`, the `0600` mode
        // and the cleanup (`/theme` copied this code).
        let mut bytes = text.into_bytes();
        bytes.push(b'\n');
        crate::atomic_write::write_atomic(
            &self.path,
            &bytes,
            Some(radian_session::PRIVATE_FILE_MODE),
        )
        .map_err(|source| AuthError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

/// The process's `auth.json` path, or `None` when there is no `RADIAN_HOME`/`HOME`.
pub fn auth_path() -> Option<PathBuf> {
    Some(credentials_dir()?.join(AUTH_FILE_NAME))
}

/// The masked form a key is shown by when it has no name: the first three characters, an
/// ellipsis, and the last four (`sk-first` → `sk-…irst`). A value too short to leave anything
/// hidden is only an ellipsis.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() >= 8 {
        let prefix: String = chars[..3].iter().collect();
        let suffix: String = chars[chars.len() - 4..].iter().collect();
        format!("{prefix}…{suffix}")
    } else {
        "…".to_owned()
    }
}

/// One key as `/logout` lists it, with its position in the entry's `keys` array (`None` for
/// the top-level `key`). Values stay raw: a name and a mask need no `$ENV` lookup.
struct KeySlot {
    name: String,
    masked: String,
    item: Option<usize>,
}

/// The display slots of an `api_key` entry, in file order. A `!command` value is skipped, like
/// [`AuthStore::resolve_keys`]; the top-level `key` is first.
fn key_slots(entry: &Map<String, Value>) -> Vec<KeySlot> {
    if entry.get("type").and_then(Value::as_str) != Some("api_key") {
        return Vec::new();
    }
    let mut slots = Vec::new();
    if let Some(key) = entry.get("key").and_then(Value::as_str)
        && !key.starts_with('!')
    {
        slots.push(KeySlot {
            name: "first".to_owned(),
            masked: mask_key(key),
            item: None,
        });
    }
    if let Some(keys) = entry.get("keys").and_then(Value::as_array) {
        for (index, item) in keys.iter().enumerate() {
            let Some(object) = item.as_object() else {
                continue;
            };
            let Some(key) = object.get("key").and_then(Value::as_str) else {
                continue;
            };
            if key.starts_with('!') {
                continue;
            }
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| mask_key(key));
            slots.push(KeySlot {
                name,
                masked: mask_key(key),
                item: Some(index),
            });
        }
    }
    slots
}

/// One refused `!command` key's notice. The command is never run (ADR 0026 D1).
fn refusal(provider_id: &str, label: &str) -> String {
    format!(
        "`{provider_id}` {label} uses a `!command` key; Radian does not run commands from \
         auth.json, so that key is skipped. Run `/login {provider_id}` to store a key instead."
    )
}

/// Writes an entry's `env` object, or removes it when empty.
fn set_env(entry: &mut Map<String, Value>, env: &BTreeMap<String, String>) {
    if env.is_empty() {
        entry.remove("env");
    } else {
        entry.insert(
            "env".into(),
            Value::Object(env.iter().map(|(k, v)| (k.clone(), json!(v))).collect()),
        );
    }
}

/// [`AuthStore::resolve_key`] for the process's store.
pub fn resolve_key(provider_id: &str) -> Result<Option<ResolvedKey>, AuthError> {
    AuthStore::open()?.resolve_key(provider_id)
}

/// [`AuthStore::resolve_keys`] for the process's store.
pub fn resolve_keys(provider_id: &str) -> Result<Option<ResolvedKeys>, AuthError> {
    AuthStore::open()?.resolve_keys(provider_id)
}

/// [`AuthStore::key_choices`] for the process's store.
pub fn key_choices(provider_id: &str) -> Result<Vec<KeyChoice>, AuthError> {
    AuthStore::open()?.key_choices(provider_id)
}

/// [`AuthStore::note`] for the process's store.
pub fn resolve_note(provider_id: &str) -> Result<Option<String>, AuthError> {
    Ok(AuthStore::open()?.note(provider_id))
}

/// [`AuthStore::upsert_api_key`] for the process's store.
pub fn upsert_api_key(
    provider_id: &str,
    key: &str,
    env: &BTreeMap<String, String>,
) -> Result<(), AuthError> {
    AuthStore::open()?.upsert_api_key(provider_id, key, env)
}

/// [`AuthStore::add_api_key`] for the process's store.
pub fn add_api_key(
    provider_id: &str,
    key: &str,
    name: Option<&str>,
    env: &BTreeMap<String, String>,
) -> Result<(), AuthError> {
    AuthStore::open()?.add_api_key(provider_id, key, name, env)
}

/// [`AuthStore::remove_key`] for the process's store.
pub fn remove_key(provider_id: &str, name: Option<&str>) -> Result<(), AuthError> {
    AuthStore::open()?.remove_key(provider_id, name)
}

/// [`AuthStore::remove`] for the process's store.
pub fn remove(provider_id: &str) -> Result<(), AuthError> {
    AuthStore::open()?.remove(provider_id)
}

/// [`AuthStore::list`] for the process's store.
pub fn list() -> Result<Vec<AuthEntry>, AuthError> {
    Ok(AuthStore::open()?.list())
}

/// [`AuthStore::is_usable`] for the process's store. A read failure (no home, a corrupt or
/// too-wide file) is **not** a login — a host deciding whether to refuse startup must see the
/// same "no" a missing entry gives, not silently treat a broken store as logged in.
pub fn has_login(provider_id: &str) -> bool {
    AuthStore::open().is_ok_and(|store| store.is_usable(provider_id))
}

/// Every usable key in this process's `auth.json`, sorted and deduplicated (ADR 0013 T4).
///
/// A redaction list must carry **every** login, not only the provider this process uses: a
/// session can mention a key that belongs to another provider (a model echo, a message the
/// user pasted), and an export or a panic log must not write it out. An entry that cannot be
/// resolved (a missing `$ENV`, a malformed value) contributes nothing — there is no key to
/// redact. `$ENV` interpolation happens here, so the list holds the values that would be sent.
pub fn resolved_keys() -> Vec<String> {
    AuthStore::open()
        .map(|store| store.resolved_keys())
        .unwrap_or_default()
}

/// Refuses a file wider than `0600`. On non-Unix there are no mode bits to check.
fn check_permissions(path: &Path) -> Result<(), AuthError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(path).map_err(|source| AuthError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(AuthError::WidePermissions {
                path: path.to_path_buf(),
                mode,
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Resolves pi's config-value template: `$NAME` / `${NAME}` **anywhere** in the string, with
/// `$$` for a literal `$` and `$!` for a literal `!`. Verified against pi's
/// `resolveConfigValue` (`@earendil-works/pi-coding-agent/dist/core/resolve-config-value.js:21-64`,
/// read 2026-10-13): it scans the whole string and builds literal + env parts, so
/// `prefix-$VAR` and `${A}${B}` both interpolate. A `$` that does not start a valid name is a
/// literal. A reference looks in the entry's `env` object first, then the process environment;
/// a missing variable is an error.
fn resolve_template(
    provider_id: &str,
    value: &str,
    env: &BTreeMap<String, String>,
) -> Result<String, AuthError> {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '$' {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        match chars.get(index + 1) {
            // `$$` and `$!` escape a literal.
            Some('$') => {
                out.push('$');
                index += 2;
            }
            Some('!') => {
                out.push('!');
                index += 2;
            }
            Some('{') => {
                if let Some(offset) = chars[index + 2..].iter().position(|&c| c == '}') {
                    let end = index + 2 + offset;
                    let name: String = chars[index + 2..end].iter().collect();
                    if is_env_name(&name) {
                        out.push_str(&lookup_env(provider_id, &name, env)?);
                    } else {
                        // Not a valid name: the whole `${…}` is a literal.
                        out.extend(&chars[index..=end]);
                    }
                    index = end + 1;
                } else {
                    out.push('$');
                    index += 1;
                }
            }
            Some(&next) if is_env_start(next) => {
                let mut end = index + 1;
                while end < chars.len() && is_env_continue(chars[end]) {
                    end += 1;
                }
                let name: String = chars[index + 1..end].iter().collect();
                out.push_str(&lookup_env(provider_id, &name, env)?);
                index = end;
            }
            _ => {
                out.push('$');
                index += 1;
            }
        }
    }
    Ok(out)
}

/// The value of `$name`: the entry's `env` object first (an empty value falls through, like
/// pi's `env?.[name] || process.env[name]`), then the process environment. A missing variable
/// is an error.
fn lookup_env(
    provider_id: &str,
    name: &str,
    env: &BTreeMap<String, String>,
) -> Result<String, AuthError> {
    if let Some(value) = env.get(name).filter(|value| !value.is_empty()) {
        return Ok(value.clone());
    }
    std::env::var(name).map_err(|_| AuthError::MissingEnv {
        provider_id: provider_id.to_owned(),
        name: name.to_owned(),
    })
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if is_env_start(first) => chars.all(is_env_continue),
        _ => false,
    }
}

fn is_env_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn is_env_continue(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A temp `RADIAN_HOME`-style root; the store lives at `<root>/credentials/auth.json`.
    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!(
                "radian-auth-test-{label}-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn path(&self) -> PathBuf {
            self.root.join("credentials").join(AUTH_FILE_NAME)
        }

        fn open(&self) -> Result<AuthStore, AuthError> {
            AuthStore::open_at(self.path())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn write_auth(scratch: &Scratch, text: &str) {
        let path = scratch.path();
        let dir = path.parent().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .unwrap();
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(&path, text).unwrap();
        // A real auth.json is 0600; the wide-permission test widens it on purpose.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    /// ADR 0013 T4: the redaction list covers **every** resolvable login, so an export or a
    /// panic log cannot leak a key that belongs to a provider other than the current route.
    #[test]
    fn resolved_keys_carries_every_login_sorted_and_deduplicated() {
        let scratch = Scratch::new("resolved-keys");
        write_auth(
            &scratch,
            r#"{
                "anthropic": {"type": "api_key", "key": "sk-a"},
                "opencode-go": {"type": "api_key", "key": "k-b"},
                "unused": {"type": "oauth", "access": "not-a-key"}
            }"#,
        );
        let store = scratch.open().unwrap();
        assert_eq!(
            store.resolved_keys(),
            vec!["k-b".to_owned(), "sk-a".to_owned()]
        );
        // An entry that cannot be resolved (a missing `$ENV`) contributes nothing.
        write_auth(
            &scratch,
            r#"{"anthropic": {"type": "api_key", "key": "$NO_SUCH_VAR_FOR_THE_TEST"}}"#,
        );
        let store = scratch.open().unwrap();
        assert!(store.resolved_keys().is_empty());
    }

    #[test]
    fn a_missing_file_is_an_empty_store() {
        let scratch = Scratch::new("missing");
        let store = scratch.open().unwrap();
        assert!(store.list().is_empty());
        assert_eq!(store.resolve_key("anthropic").unwrap(), None);
    }

    #[test]
    fn a_pi_shaped_file_reads_verbatim() {
        let scratch = Scratch::new("pi");
        write_auth(
            &scratch,
            r#"{
  "opencode-go": {"type": "api_key", "key": "sk-abc"},
  "openai-compatible": {"type": "api_key", "key": "sk-def", "env": {"RADIAN_BASE_URL": "https://example.com/v1"}},
  "anthropic": {"type": "oauth", "access": "a", "refresh": "r", "expires": 123}
}"#,
        );
        let store = scratch.open().unwrap();
        let key = store.resolve_key("opencode-go").unwrap().unwrap();
        assert_eq!(key.key, "sk-abc");
        let generic = store.resolve_key("openai-compatible").unwrap().unwrap();
        assert_eq!(
            generic.env.get(RADIAN_BASE_URL).unwrap(),
            "https://example.com/v1"
        );
        // OAuth is kept but not usable.
        assert_eq!(store.resolve_key("anthropic").unwrap(), None);
        assert!(store.note("anthropic").unwrap().contains("OAuth"));
        let listed = store.list();
        assert_eq!(listed.len(), 3);
        assert!(
            listed
                .iter()
                .any(|e| e.provider_id == "anthropic" && e.kind == "oauth" && !e.usable)
        );
    }

    /// Parameterized over the group/other bits: every mode with a group or other bit is
    /// refused, and only owner-only modes are accepted. This catches a check that only looks
    /// at `0o007` (a group-readable `0640` would slip through).
    #[test]
    fn a_wide_permission_file_is_refused_with_a_fix_command() {
        let scratch = Scratch::new("wide");
        write_auth(&scratch, r#"{"p": {"type": "api_key", "key": "k"}}"#);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for mode in [0o640, 0o660, 0o604, 0o644, 0o666, 0o607, 0o770] {
                std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(mode))
                    .unwrap();
                let error = match scratch.open() {
                    Err(error) => error,
                    Ok(_) => panic!("mode {mode:04o} must be refused"),
                };
                let text = error.to_string();
                assert!(text.contains("chmod 600"), "{mode:04o}: {text}");
                assert!(
                    matches!(error, AuthError::WidePermissions { .. }),
                    "{mode:04o}: {error}"
                );
            }
            for mode in [0o600, 0o400] {
                std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(mode))
                    .unwrap();
                let store = scratch
                    .open()
                    .unwrap_or_else(|error| panic!("mode {mode:04o} must be accepted: {error}"));
                assert_eq!(store.resolve_key("p").unwrap().unwrap().key, "k");
            }
        }
    }

    /// A leftover temp file must not loosen the new file's mode: the writer removes it and
    /// creates exclusively, so `mode(0o600)` applies to a fresh inode.
    /// Two threads writing **different providers** 50 times each, barrier-synchronised: every
    /// write succeeds (no `NotFound` from a temp file another writer removed), the final file
    /// is valid JSON with mode `0600`, and no temp file is left behind. The last-writer-wins
    /// rule itself is accepted (module docs); what this pins is that the writes never destroy
    /// each other's temp file.
    #[test]
    fn concurrent_writes_in_one_process_do_not_delete_each_other_s_temps() {
        use std::sync::{Arc, Barrier};
        let scratch = Scratch::new("concurrent");
        let path = scratch.path();
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for thread in 0..2u32 {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let mut store = AuthStore::open_at(path).expect("open");
                barrier.wait();
                for n in 0..50u32 {
                    let provider = format!("p{thread}-{n}");
                    store
                        .upsert_api_key(&provider, "sk-test", &BTreeMap::new())
                        .unwrap_or_else(|error| {
                            panic!("write {provider} failed: {error}");
                        });
                }
            }));
        }
        for handle in handles {
            handle.join().expect("no writer panicked");
        }

        // The final file is valid JSON (a torn rename would not parse) …
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert!(value.as_object().is_some_and(|map| !map.is_empty()));
        // … mode 0600 …
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{mode:04o}");
        }
        // … and no temp file stayed behind (neither this scheme's unique names nor the old
        // exact-name leftover).
        let dir = path.parent().unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("auth.json.tmp-"))
            })
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "temp leftovers: {leftovers:?}");
    }

    #[test]
    fn a_leftover_temp_file_does_not_loosen_the_permissions() {
        let scratch = Scratch::new("tmp-leftover");
        let tmp = scratch
            .path()
            .with_extension(format!("tmp-{}", std::process::id()));
        std::fs::create_dir_all(tmp.parent().unwrap()).unwrap();
        std::fs::write(&tmp, "leftover").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let mut store = scratch.open().unwrap();
        store.upsert_api_key("p", "k", &BTreeMap::new()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(scratch.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{mode:04o}");
        }
        assert!(
            !tmp.exists(),
            "the leftover temp file is replaced by the rename"
        );
    }

    #[test]
    fn a_command_key_is_refused_and_never_run() {
        let scratch = Scratch::new("command");
        let marker = scratch.root.join("ran");
        write_auth(
            &scratch,
            &format!(
                r#"{{"evil": {{"type": "api_key", "key": "!touch {}"}}}}"#,
                marker.display()
            ),
        );
        let store = scratch.open().unwrap();
        assert_eq!(store.resolve_key("evil").unwrap(), None);
        assert!(store.note("evil").unwrap().contains("!command"));
        assert!(!marker.exists(), "the command must never run");
    }

    #[test]
    fn an_env_reference_resolves_and_a_missing_one_is_an_error() {
        let scratch = Scratch::new("env");
        write_auth(
            &scratch,
            r#"{"p": {"type": "api_key", "key": "$RADIAN_AUTH_TEST_KEY"}}"#,
        );
        let store = scratch.open().unwrap();
        // SAFETY: single-threaded test process state; restored below.
        unsafe { std::env::set_var("RADIAN_AUTH_TEST_KEY", "from-env") };
        assert_eq!(store.resolve_key("p").unwrap().unwrap().key, "from-env");
        unsafe { std::env::remove_var("RADIAN_AUTH_TEST_KEY") };
        let error = store.resolve_key("p").unwrap_err();
        assert!(matches!(error, AuthError::MissingEnv { .. }), "{error}");

        // `${NAME}` too.
        write_auth(
            &scratch,
            r#"{"p": {"type": "api_key", "key": "${RADIAN_AUTH_TEST_KEY}"}}"#,
        );
        let store = scratch.open().unwrap();
        unsafe { std::env::set_var("RADIAN_AUTH_TEST_KEY", "braced") };
        assert_eq!(store.resolve_key("p").unwrap().unwrap().key, "braced");
        unsafe { std::env::remove_var("RADIAN_AUTH_TEST_KEY") };
    }

    /// pi's `resolveConfigValue` interpolates anywhere in the string (verified
    /// `resolve-config-value.js:21-64`), so `prefix-$VAR` and `${A}${B}` resolve; `$$` and `$!`
    /// escape; `${not a name}` is a literal.
    #[test]
    fn a_template_interpolates_in_the_middle_and_escapes() {
        let scratch = Scratch::new("template");
        write_auth(
            &scratch,
            r#"{
                "p": {"type": "api_key", "key": "prefix-$RADIAN_AUTH_TEST_A-suffix"},
                "q": {"type": "api_key", "key": "${RADIAN_AUTH_TEST_A}${B}", "env": {"B": "bee"}},
                "r": {"type": "api_key", "key": "$$literal-$!bang"},
                "t": {"type": "api_key", "key": "${not a name}"},
                "u": {"type": "api_key", "key": "$RADIAN_AUTH_UNSET_VAR"}
            }"#,
        );
        let store = scratch.open().unwrap();
        // SAFETY: single-threaded test process state; restored below.
        unsafe { std::env::set_var("RADIAN_AUTH_TEST_A", "aye") };
        assert_eq!(
            store.resolve_key("p").unwrap().unwrap().key,
            "prefix-aye-suffix"
        );
        // `${A}${B}`: A from the process env, B from the entry's own `env` object.
        assert_eq!(store.resolve_key("q").unwrap().unwrap().key, "ayebee");
        // `$$` and `$!` are literals; no lookup happens.
        assert_eq!(
            store.resolve_key("r").unwrap().unwrap().key,
            "$literal-!bang"
        );
        // `${not a name}` is not a valid name, so the whole reference is a literal.
        assert_eq!(
            store.resolve_key("t").unwrap().unwrap().key,
            "${not a name}"
        );
        unsafe { std::env::remove_var("RADIAN_AUTH_TEST_A") };
        // A missing variable fails.
        let error = store.resolve_key("u").unwrap_err();
        assert!(matches!(error, AuthError::MissingEnv { .. }), "{error}");
    }

    #[test]
    fn a_write_preserves_unknown_and_oauth_entries_and_is_atomic() {
        let scratch = Scratch::new("preserve");
        let before = r#"{
  "anthropic": {"type": "oauth", "access": "a", "refresh": "r", "expires": 123, "future": {"x": 1}},
  "future-provider": {"type": "something-new", "value": [1, 2, 3]},
  "opencode-go": {"type": "api_key", "key": "old", "extra": "keep-me"}
}"#;
        write_auth(&scratch, before);
        let mut store = scratch.open().unwrap();
        let mut env = BTreeMap::new();
        env.insert(RADIAN_BASE_URL.to_owned(), "https://new/v1".to_owned());
        store
            .upsert_api_key("openai-compatible", "sk-new", &env)
            .unwrap();

        let after = std::fs::read_to_string(scratch.path()).unwrap();
        let after: Value = serde_json::from_str(&after).unwrap();
        // The unknown/oauth entries are value-identical (key order is normalized by
        // `serde_json`; ADR 0007 J3's accepted tradeoff).
        let before: Value = serde_json::from_str(before).unwrap();
        assert_eq!(after["anthropic"], before["anthropic"]);
        assert_eq!(after["future-provider"], before["future-provider"]);
        // The target entry kept its unknown field and got the new key.
        assert_eq!(after["opencode-go"]["extra"], "keep-me");
        assert_eq!(after["opencode-go"]["key"], "old");
        assert_eq!(after["openai-compatible"]["key"], "sk-new");
        assert_eq!(
            after["openai-compatible"]["env"][RADIAN_BASE_URL],
            "https://new/v1"
        );

        // The file is 0600 and the directory 0700.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::metadata(scratch.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600, "{file_mode:04o}");
            let dir_mode = std::fs::metadata(scratch.path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700, "{dir_mode:04o}");
        }
        // No temp file is left behind.
        let leftovers: Vec<String> = std::fs::read_dir(scratch.path().parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != AUTH_FILE_NAME)
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn remove_only_drops_that_entry() {
        let scratch = Scratch::new("remove");
        write_auth(
            &scratch,
            r#"{"a": {"type": "api_key", "key": "ka"}, "b": {"type": "api_key", "key": "kb"}}"#,
        );
        let mut store = scratch.open().unwrap();
        store.remove("a").unwrap();
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(scratch.path()).unwrap()).unwrap();
        assert!(after.get("a").is_none());
        assert_eq!(after["b"]["key"], "kb");
    }

    /// ADR 0026 slice 3: a `chatgpt` oauth entry is usable (unlike slice ①, which
    /// only stored it); the same oauth *type* under a different provider id is still not —
    /// the exception is scoped to the one id that knows how to use it.
    #[test]
    fn chatgpt_oauth_is_usable_but_the_same_shape_elsewhere_is_not() {
        let scratch = Scratch::new("oauth-usable");
        write_auth(
            &scratch,
            r#"{
                "chatgpt": {"type": "oauth", "access": "a", "refresh": "r", "expires": 1},
                "anthropic": {"type": "oauth", "access": "a", "refresh": "r", "expires": 1}
            }"#,
        );
        let store = scratch.open().unwrap();
        assert!(store.is_usable("chatgpt"));
        assert!(!store.is_usable("anthropic"));
        assert!(!store.is_usable("no-such-entry"));
        let listed = store.list();
        assert!(
            listed
                .iter()
                .any(|e| e.provider_id == "chatgpt" && e.kind == "oauth" && e.usable)
        );
        assert!(
            listed
                .iter()
                .any(|e| e.provider_id == "anthropic" && e.kind == "oauth" && !e.usable)
        );
    }

    #[test]
    fn oauth_entry_round_trips_and_preserves_unknown_fields() {
        let scratch = Scratch::new("oauth-roundtrip");
        write_auth(
            &scratch,
            r#"{"chatgpt": {"type": "oauth", "access": "old-a", "refresh": "old-r", "expires": 1, "future": "keep-me"}}"#,
        );
        let mut store = scratch.open().unwrap();
        assert_eq!(
            store.oauth_entry("chatgpt").unwrap(),
            OAuthEntry {
                access: "old-a".into(),
                refresh: "old-r".into(),
                expires_at_millis: 1,
            }
        );
        store
            .upsert_oauth(
                "chatgpt",
                &OAuthEntry {
                    access: "new-a".into(),
                    refresh: "new-r".into(),
                    expires_at_millis: 999,
                },
            )
            .unwrap();
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(scratch.path()).unwrap()).unwrap();
        assert_eq!(after["chatgpt"]["access"], "new-a");
        assert_eq!(after["chatgpt"]["refresh"], "new-r");
        assert_eq!(after["chatgpt"]["expires"], 999);
        assert_eq!(after["chatgpt"]["future"], "keep-me");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(scratch.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{mode:04o}");
        }
    }

    /// ADR 0026: an OAuth access/refresh token is redacted exactly like
    /// an API key.
    #[test]
    fn resolved_keys_also_carries_oauth_access_and_refresh_tokens() {
        let scratch = Scratch::new("oauth-redact");
        write_auth(
            &scratch,
            r#"{
                "anthropic": {"type": "api_key", "key": "sk-a"},
                "chatgpt": {"type": "oauth", "access": "at-secret", "refresh": "rt-secret", "expires": 1}
            }"#,
        );
        let store = scratch.open().unwrap();
        let keys = store.resolved_keys();
        assert!(keys.contains(&"sk-a".to_owned()));
        assert!(keys.contains(&"at-secret".to_owned()));
        assert!(keys.contains(&"rt-secret".to_owned()));
    }

    /// ADR 0026's acceptance item "tokens never in a log/journal/export": this is
    /// the host-side half of that guarantee — `resolved_keys()` feeds the exact redactor
    /// `radian-session`'s journal/export/panic-log paths use, so a scan of redacted text
    /// proves the chain end to end rather than only that the list contains the token.
    #[test]
    fn oauth_tokens_are_redacted_from_journal_and_export_shaped_text() {
        let scratch = Scratch::new("oauth-redact-e2e");
        write_auth(
            &scratch,
            r#"{"chatgpt": {"type": "oauth", "access": "at-super-secret-123", "refresh": "rt-super-secret-456", "expires": 1}}"#,
        );
        let store = scratch.open().unwrap();
        let secrets = store.resolved_keys();
        let journal_line =
            r#"{"event":"ModelRequest","headers":{"authorization":"Bearer at-super-secret-123"}}"#
                .to_owned();
        let export_text = "the model saw refresh token rt-super-secret-456 in a tool result";
        let panic_text = "thread panicked: at-super-secret-123 could not be parsed";
        for text in [journal_line.as_str(), export_text, panic_text] {
            let redacted = radian_session::redact_text(text, &secrets);
            assert!(
                !redacted.contains("at-super-secret-123")
                    && !redacted.contains("rt-super-secret-456"),
                "{redacted}"
            );
        }
    }

    /// ADR 0026 D1: `keys[]` holds further keys in order. The first key is `first`; a named
    /// item keeps its name; an unnamed item is shown by its masked form; `$ENV` resolves in an
    /// item exactly as in `key`. Every key enters the redaction list.
    #[test]
    fn a_keys_list_resolves_every_key_in_order_with_names() {
        let scratch = Scratch::new("keys");
        write_auth(
            &scratch,
            r#"{"p": {"type": "api_key", "key": "sk-first", "keys": [
                {"name": "team", "key": "sk-second"},
                {"key": "$RADIAN_AUTH_TEST_KEYS_ENV"}
            ]}}"#,
        );
        let store = scratch.open().unwrap();
        // SAFETY: single-threaded test process state; restored below.
        unsafe { std::env::set_var("RADIAN_AUTH_TEST_KEYS_ENV", "sk-from-env") };
        let resolved = store.resolve_keys("p").unwrap().unwrap();
        assert_eq!(
            resolved
                .keys
                .iter()
                .map(|key| (key.display_name(), key.key.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("first".to_owned(), "sk-first"),
                ("team".to_owned(), "sk-second"),
                ("sk-…-env".to_owned(), "sk-from-env"),
            ]
        );
        assert!(resolved.notices.is_empty(), "{:?}", resolved.notices);
        // The first key is the one `resolve_key`/pi see.
        assert_eq!(store.resolve_key("p").unwrap().unwrap().key, "sk-first");
        let redacted = store.resolved_keys();
        for key in ["sk-first", "sk-second", "sk-from-env"] {
            assert!(redacted.contains(&key.to_owned()), "{key}: {redacted:?}");
        }
        unsafe { std::env::remove_var("RADIAN_AUTH_TEST_KEYS_ENV") };
    }

    /// A `!command` item is refused without being run, with one notice naming it; the other
    /// keys stay usable (ADR 0026 D1).
    #[test]
    fn a_command_item_is_refused_with_a_notice_and_the_others_stay_usable() {
        let scratch = Scratch::new("keys-command");
        let marker = scratch.root.join("ran");
        write_auth(
            &scratch,
            &format!(
                r#"{{"p": {{"type": "api_key", "key": "sk-first", "keys": [
                    {{"name": "bad", "key": "!touch {}"}},
                    {{"name": "team", "key": "sk-second"}}
                ]}}}}"#,
                marker.display()
            ),
        );
        let store = scratch.open().unwrap();
        let resolved = store.resolve_keys("p").unwrap().unwrap();
        assert_eq!(
            resolved
                .keys
                .iter()
                .map(|key| key.display_name())
                .collect::<Vec<_>>(),
            vec!["first".to_owned(), "team".to_owned()]
        );
        assert_eq!(resolved.notices.len(), 1, "{:?}", resolved.notices);
        assert!(
            resolved.notices[0].contains("bad"),
            "{:?}",
            resolved.notices
        );
        assert!(
            resolved.notices[0].contains("!command"),
            "{:?}",
            resolved.notices
        );
        assert!(!marker.exists(), "the command must never run");
        assert_eq!(store.key_choices("p").unwrap().len(), 2);
    }

    /// An `oauth` entry's `keys` list is ignored: only `api_key` entries have keys (D1).
    #[test]
    fn an_oauth_entrys_keys_are_ignored() {
        let scratch = Scratch::new("oauth-keys");
        write_auth(
            &scratch,
            r#"{"chatgpt": {"type": "oauth", "access": "a", "refresh": "r", "expires": 1,
                            "keys": [{"name": "team", "key": "sk-ignored"}]}}"#,
        );
        let store = scratch.open().unwrap();
        assert_eq!(store.resolve_keys("chatgpt").unwrap(), None);
        assert!(!store.resolved_keys().contains(&"sk-ignored".to_owned()));
    }

    /// `add_api_key` appends in order; removing the first key promotes the next to `key`;
    /// removing the last key removes the entry; `all` removes every key; an unknown name is
    /// refused naming the known ones (ADR 0026 D1/D3).
    #[test]
    fn adding_and_removing_keys_keeps_key_as_the_first() {
        let scratch = Scratch::new("keys-add-remove");
        write_auth(&scratch, r#"{"p": {"type": "api_key", "key": "sk-first"}}"#);
        let mut store = scratch.open().unwrap();
        let env = BTreeMap::new();
        store
            .add_api_key("p", "sk-second", Some("team"), &env)
            .unwrap();
        store.add_api_key("p", "sk-third", None, &env).unwrap();
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(scratch.path()).unwrap()).unwrap();
        assert_eq!(after["p"]["key"], "sk-first");
        assert_eq!(after["p"]["keys"][0]["name"], "team");
        assert_eq!(after["p"]["keys"][0]["key"], "sk-second");
        assert!(after["p"]["keys"][1].get("name").is_none());
        assert_eq!(
            store
                .key_choices("p")
                .unwrap()
                .iter()
                .map(|key| key.name.clone())
                .collect::<Vec<_>>(),
            vec![
                "first".to_owned(),
                "team".to_owned(),
                crate::auth::mask_key("sk-third"),
            ]
        );

        // Removing the first key promotes the next to `key`; the old name is gone with the
        // slot, and the file still has a `key` for pi.
        store.remove_key("p", Some("first")).unwrap();
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(scratch.path()).unwrap()).unwrap();
        assert_eq!(after["p"]["key"], "sk-second");
        assert_eq!(after["p"]["keys"].as_array().unwrap().len(), 1);
        assert_eq!(store.resolve_key("p").unwrap().unwrap().key, "sk-second");

        // An unknown name is refused, naming what is known. The promoted key is `first`
        // (its old `keys[]` name went with the slot) and the unnamed item shows masked.
        let error = store.remove_key("p", Some("no-such-key")).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("no-such-key"), "{text}");
        assert!(text.contains("first"), "{text}");
        assert!(text.contains(&crate::auth::mask_key("sk-third")), "{text}");

        // Removing the last key removes the entry; `all` does the same when several remain.
        store
            .remove_key("p", Some(&crate::auth::mask_key("sk-third")))
            .unwrap();
        assert_eq!(store.resolve_key("p").unwrap().unwrap().key, "sk-second");
        store.remove_key("p", Some("first")).unwrap();
        assert!(store.resolve_key("p").unwrap().is_none());
        write_auth(
            &scratch,
            r#"{"p": {"type": "api_key", "key": "a", "keys": [{"name": "b", "key": "b"}]}}"#,
        );
        let mut store = scratch.open().unwrap();
        store.remove_key("p", None).unwrap();
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(scratch.path()).unwrap()).unwrap();
        assert!(after.get("p").is_none());
    }

    #[test]
    fn debug_never_prints_the_key() {
        let key = ResolvedKey {
            provider_id: "p".into(),
            key: "sk-super-secret".into(),
            env: BTreeMap::from([(RADIAN_BASE_URL.to_owned(), "https://x".to_owned())]),
        };
        let text = format!("{key:?}");
        assert!(!text.contains("sk-super-secret"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(
            !text.contains("https://x"),
            "env values are hidden too: {text}"
        );
    }
}
