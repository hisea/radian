//! The remembered permission tier per project (ADR 0038 decision 3).
//!
//! `/access` can remember the tier for one project. The record lives under `~/.radian/`, in
//! the user's own directory, keyed by the ADR 0021 **project identity** (the git common dir,
//! so a repository's worktrees share one decision). It is **never** written into the
//! repository: otherwise a cloned repository could ship a line that opens the sandbox.
//!
//! The store is `~/.radian/access/access.json` (`$RADIAN_HOME/access/access.json`), a JSON
//! object `{ "<identity key>": "safe" | "workspace" | "full" }`, sorted keys, written
//! atomically `0600` in a `0700` directory — the same shape and error handling as the trust
//! store (ADR 0031 T2), which this module mirrors:
//!
//! - A missing file is an empty store.
//! - A malformed file (not an object, a value that is not one of the three tier names) is
//!   **rejected whole** and warned about, never partially adopted.
//! - A corrupt or unreadable file is an error on write: it is never silently replaced with an
//!   empty store, which would destroy every other project's decision.
//!
//! Nothing else in the repository can set this: it is written only by the user's `/access`
//! choice, and read at startup to pick the tier.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use radian_permissions::Preset;

use crate::home::access_dir_for;

/// The record: ADR 0021 project identity key → tier name.
pub type Store = BTreeMap<String, String>;

/// The file name inside `access/`.
pub const FILE_NAME: &str = "access.json";

/// The tier name a [`Preset`] is stored under (the user-visible `safe`/`workspace`/`full`).
pub fn tier_name(tier: Preset) -> &'static str {
    tier.as_str()
}

/// Parses a stored tier name. Only the three canonical names are accepted: an unknown value
/// rejects the whole file (fail-closed), so a typo cannot silently change the tier.
pub fn parse_tier(name: &str) -> Option<Preset> {
    match name {
        "safe" => Some(Preset::Safe),
        "workspace" => Some(Preset::WorkspaceWrite),
        "full" => Some(Preset::FullAccess),
        _ => None,
    }
}

/// `~/.radian/access/` for this process, created `0700` (ADR 0020). `None` without a home.
///
/// **Sandbox-denied** like `trust/` (ADR 0038 decision 3): it is the user's map of which
/// projects run unsandboxed, so a workspace's own code must not read it.
pub fn access_dir() -> Option<PathBuf> {
    Some(crate::home::private(access_dir_for(
        &crate::home::radian_home()?,
    )))
}

/// `~/.radian/access/access.json` for this process.
pub fn access_file() -> Option<PathBuf> {
    Some(access_dir()?.join(FILE_NAME))
}

/// The identity key for `workspace`: the ADR 0021 project identity (the git common dir, so a
/// repository's worktrees share one decision; the canonical workspace otherwise).
pub fn key_for(workspace: &Path) -> String {
    crate::memory::project_identity_key(workspace)
        .to_string_lossy()
        .into_owned()
}

/// Reads the record at `path`. `Err` is the one-line warning for a **whole-file** rejection; a
/// missing file is `Ok(empty)`.
pub fn read_at(path: &Path) -> Result<Store, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Store::new()),
        Err(error) => return Err(format!("{}: cannot read: {error}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("{}: not valid JSON: {error}", path.display()))?;
    let serde_json::Value::Object(map) = value else {
        return Err(format!(
            "{}: the access store must be a JSON object of identity → tier",
            path.display()
        ));
    };
    let mut store = Store::new();
    for (key, value) in map {
        let Some(name) = value.as_str() else {
            return Err(format!(
                "{}: the value for {key:?} must be one of safe, workspace, full",
                path.display()
            ));
        };
        if parse_tier(name).is_none() {
            return Err(format!(
                "{}: the value for {key:?} must be one of safe, workspace, full, not {name:?}",
                path.display()
            ));
        }
        store.insert(key, name.to_owned());
    }
    Ok(store)
}

/// Reads this process's record. `Err` is the warning for a whole-file rejection; the caller
/// treats it as an empty store and warns (fail-closed).
pub fn read() -> Result<Store, String> {
    match access_file() {
        Some(path) => read_at(&path),
        None => Ok(Store::new()),
    }
}

/// Writes the record at `path` atomically: sorted keys, `0600`, `0700` directory.
pub fn write_at(path: &Path, store: &Store) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        radian_session::ensure_private_dir(dir)?;
    }
    let sorted: serde_json::Map<String, serde_json::Value> = store
        .iter()
        .map(|(key, tier)| (key.clone(), serde_json::Value::String(tier.clone())))
        .collect();
    let mut bytes = serde_json::to_vec_pretty(&serde_json::Value::Object(sorted))
        .unwrap_or_else(|_| b"{}".to_vec());
    bytes.push(b'\n');
    crate::atomic_write::write_atomic(path, &bytes, Some(0o600))
}

/// The remembered tier for `key`, or `None`. A corrupt record is an error.
pub fn get_at(path: &Path, key: &str) -> Result<Option<Preset>, String> {
    Ok(read_at(path)?.get(key).and_then(|name| parse_tier(name)))
}

/// Sets `workspace`'s remembered tier at `path`. The identity key is computed here. A corrupt
/// record is an error: it is never silently replaced with an empty store.
pub fn set_at(path: &Path, workspace: &Path, tier: Preset) -> Result<(), String> {
    let key = key_for(workspace);
    let mut store = read_at(path)?;
    store.insert(key, tier_name(tier).to_owned());
    write_at(path, &store).map_err(|error| format!("{}: cannot write: {error}", path.display()))
}

/// Removes `workspace`'s remembered tier at `path`. Removing a key that is not there is not an
/// error and writes nothing. A corrupt record is an error, like [`set_at`].
pub fn forget_at(path: &Path, workspace: &Path) -> Result<(), String> {
    let key = key_for(workspace);
    let mut store = read_at(path)?;
    if store.remove(&key).is_none() {
        return Ok(());
    }
    write_at(path, &store).map_err(|error| format!("{}: cannot write: {error}", path.display()))
}

/// The remembered tier for `workspace` in this process's store. `Err` is the whole-file
/// rejection warning; the caller treats it as "no remembered tier" and warns (fail-closed).
pub fn remembered_for(workspace: &Path) -> Result<Option<Preset>, String> {
    match access_file() {
        Some(path) => get_at(&path, &key_for(workspace)),
        None => Ok(None),
    }
}

/// Remembers `tier` for `workspace` in this process's store. `Err` when there is no home or
/// the record is corrupt/unwritable.
pub fn remember_for(workspace: &Path, tier: Preset) -> Result<(), String> {
    let path = access_file()
        .ok_or_else(|| "no RADIAN_HOME or HOME, so the tier cannot be remembered".to_owned())?;
    set_at(&path, workspace, tier)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-access-test-{}-{label}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_file_is_an_empty_store() {
        let dir = scratch("missing");
        assert_eq!(read_at(&dir.join("access.json")).unwrap(), Store::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bad file is rejected whole, never partly adopted (the trust store's rule).
    #[test]
    fn a_bad_file_is_rejected_whole_not_partly_adopted() {
        let dir = scratch("bad");
        let path = dir.join("access.json");
        for bad in [
            "not json",
            "[1, 2]",
            r#"{"a": 1}"#,
            r#"{"a": "safe", "b": "yolo"}"#,
            r#"{"a": "workspace-write"}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(read_at(&path).is_err(), "{bad:?} must be rejected");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The record round-trips sorted and private, keyed by the project identity.
    #[test]
    fn the_record_round_trips_sorted_and_private() {
        let dir = scratch("round-trip");
        // A fresh parent, so `write_at` creates it 0700 (an existing 0755 dir is only warned
        // about, never chmod'd, which is the repository's rule).
        let parent = dir.join("access");
        let path = parent.join("access.json");
        let mut store = Store::new();
        store.insert("/z".to_owned(), "full".to_owned());
        store.insert("/a".to_owned(), "safe".to_owned());
        write_at(&path, &store).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let a = text.find("/a").unwrap();
        let z = text.find("/z").unwrap();
        assert!(a < z, "sorted keys: {text}");
        assert_eq!(read_at(&path).unwrap(), store);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The identity key makes two worktrees of one repository share a decision, and a
    /// different repository not.
    #[test]
    fn the_key_is_the_project_identity() {
        let root = scratch("identity");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let other = root.join("other");
        std::fs::create_dir_all(other.join(".git")).unwrap();
        let key = key_for(&repo);
        assert_eq!(key, key_for(&repo));
        assert_ne!(key, key_for(&other));
        // The identity is the `.git` common dir, not the workspace path.
        assert!(key.ends_with("/repo/.git"), "{key}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A corrupt record is an error on write and is **not overwritten**.
    #[test]
    fn a_corrupt_record_is_not_overwritten() {
        let dir = scratch("corrupt");
        let path = dir.join("access.json");
        std::fs::write(&path, b"{not json").unwrap();
        let workspace = dir.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        let error = set_at(&path, &workspace, Preset::FullAccess).unwrap_err();
        assert!(error.contains("not valid JSON"), "{error}");
        let error = forget_at(&path, &workspace).unwrap_err();
        assert!(error.contains("not valid JSON"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), b"{not json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_get_and_forget_round_trip() {
        let dir = scratch("set-get");
        let path = dir.join("access.json");
        let workspace = dir.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();

        assert_eq!(get_at(&path, &key_for(&workspace)).unwrap(), None);
        set_at(&path, &workspace, Preset::FullAccess).unwrap();
        assert_eq!(
            get_at(&path, &key_for(&workspace)).unwrap(),
            Some(Preset::FullAccess)
        );
        set_at(&path, &workspace, Preset::Safe).unwrap();
        assert_eq!(
            get_at(&path, &key_for(&workspace)).unwrap(),
            Some(Preset::Safe)
        );
        forget_at(&path, &workspace).unwrap();
        assert_eq!(get_at(&path, &key_for(&workspace)).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
