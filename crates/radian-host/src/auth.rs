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
use std::io::Write;
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
    /// True only for an `api_key` entry whose key resolves (not `!`, its env vars are set).
    pub usable: bool,
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

    /// The usable key for `provider_id`, or `None` when there is no usable `api_key` entry
    /// (missing, `oauth`, unknown type, or a `!command` key). [`Self::note`] explains a
    /// `None` that is a refusal rather than an absence.
    pub fn resolve_key(&self, provider_id: &str) -> Result<Option<ResolvedKey>, AuthError> {
        let Some(entry) = self.entries.get(provider_id).and_then(Value::as_object) else {
            return Ok(None);
        };
        if entry.get("type").and_then(Value::as_str) != Some("api_key") {
            return Ok(None);
        }
        let Some(key) = entry.get("key").and_then(Value::as_str) else {
            return Ok(None);
        };
        if key.starts_with('!') {
            // A command: refused, and deliberately never run (ADR 0026 D1).
            return Ok(None);
        }
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
        let key = resolve_template(provider_id, key, &env)?;
        Ok(Some(ResolvedKey {
            provider_id: provider_id.to_owned(),
            key,
            env,
        }))
    }

    /// A one-line explanation when `resolve_key` returned `None` because the entry is
    /// **refused** (a `!command` key, or an `oauth` entry this build cannot use). `None` for
    /// a missing entry or a usable one.
    pub fn note(&self, provider_id: &str) -> Option<String> {
        let entry = self.entries.get(provider_id).and_then(Value::as_object)?;
        match entry.get("type").and_then(Value::as_str) {
            Some("api_key") => {
                let key = entry.get("key").and_then(Value::as_str)?;
                key.starts_with('!').then(|| {
                    format!(
                        "`{provider_id}` uses a `!command` key; Radian does not run commands \
                         from auth.json. Run `/login {provider_id}` to store a key instead."
                    )
                })
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
        if env.is_empty() {
            entry.remove("env");
        } else {
            entry.insert(
                "env".into(),
                Value::Object(env.iter().map(|(k, v)| (k.clone(), json!(v))).collect()),
            );
        }
        self.entries
            .insert(provider_id.to_owned(), Value::Object(entry));
        self.write()
    }

    /// Removes the entry for `provider_id` (the `/logout` operation), leaving every other
    /// entry byte-for-byte. Writing an empty object is fine; the file is not deleted.
    pub fn remove(&mut self, provider_id: &str) -> Result<(), AuthError> {
        self.entries.remove(provider_id);
        self.write()
    }

    /// Every entry: id, type, and whether it is usable in this build.
    /// Every usable key in this store, sorted and deduplicated (ADR 0013 T4). The free
    /// [`resolved_keys`] is this for the process store.
    pub fn resolved_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        for entry in self.list() {
            if let Ok(Some(key)) = self.resolve_key(&entry.provider_id)
                && !key.key.is_empty()
            {
                keys.push(key.key);
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
                let usable = self.resolve_key(provider_id).ok().flatten().is_some();
                AuthEntry {
                    provider_id: provider_id.clone(),
                    kind,
                    usable,
                }
            })
            .collect()
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
        // A unique temp name per write: pid + a process-local counter + nanoseconds. Two
        // concurrent writes in one process therefore never share a temp file (the old
        // `tmp-<pid>` scheme let the later `remove_file` delete the earlier writer's file).
        // No randomness dependency: the counter alone is unique within the process, the
        // timestamp disambiguates leftovers across restarts.
        static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let tmp = self
            .path
            .with_extension(format!("tmp-{}-{seq}-{nanos}", std::process::id()));
        // Clean up a leftover under the **old exact** name (`auth.json.tmp-<pid>`): it can only
        // be a crashed write from an older Radian, because this scheme never generates that
        // name, so it can never be a concurrent writer's in-flight file.
        let legacy = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        if legacy != tmp {
            let _ = std::fs::remove_file(&legacy);
        }
        // `create_new` still guards the (now unique) name: `mode(0o600)` only applies to a
        // fresh inode, so a reused file would keep its old (possibly wide) mode.
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(radian_session::PRIVATE_FILE_MODE);
            }
            let mut file = options.open(&tmp).map_err(|source| AuthError::Io {
                path: tmp.clone(),
                source,
            })?;
            file.write_all(text.as_bytes())
                .and_then(|()| file.write_all(b"\n"))
                .and_then(|()| file.sync_all())
                .map_err(|source| AuthError::Io {
                    path: tmp.clone(),
                    source,
                })?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|source| AuthError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

/// The process's `auth.json` path, or `None` when there is no `RADIAN_HOME`/`HOME`.
pub fn auth_path() -> Option<PathBuf> {
    Some(credentials_dir()?.join(AUTH_FILE_NAME))
}

/// [`AuthStore::resolve_key`] for the process's store.
pub fn resolve_key(provider_id: &str) -> Result<Option<ResolvedKey>, AuthError> {
    AuthStore::open()?.resolve_key(provider_id)
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

/// [`AuthStore::remove`] for the process's store.
pub fn remove(provider_id: &str) -> Result<(), AuthError> {
    AuthStore::open()?.remove(provider_id)
}

/// [`AuthStore::list`] for the process's store.
pub fn list() -> Result<Vec<AuthEntry>, AuthError> {
    Ok(AuthStore::open()?.list())
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
