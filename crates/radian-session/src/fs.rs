//! Owner-only filesystem helpers (ADR 0013).
//!
//! A session is potentially sensitive data: the JSONL can carry a model-echoed secret in a
//! `provider_payload` (ADR 0006 §11). So a session directory and the files in it are created
//! with owner-only permissions, and an **existing** path that is accessible to group/other is
//! reported once on open — never silently changed. Changing permissions the user set on
//! purpose is a surprise; telling them is not.

use std::io;
use std::path::Path;

/// Owner-only mode for a session directory (Unix): `rwx------`.
#[cfg(unix)]
pub const PRIVATE_DIR_MODE: u32 = 0o700;
/// Owner-only mode for a session file (Unix): `rw-------`.
#[cfg(unix)]
pub const PRIVATE_FILE_MODE: u32 = 0o600;

/// Creates `dir` and its parents with owner-only permissions if it does not exist. If it
/// already exists and is group/world-accessible, prints one warning line to stderr and leaves
/// its permissions alone.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if dir.exists() {
        warn_if_wide(dir);
        return Ok(());
    }
    create_dir_mode(dir)
}

#[cfg(unix)]
fn create_dir_mode(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    // `recursive` applies the mode to every directory it creates, so no level is ever
    // group/world-readable, even for a moment.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIR_MODE)
        .create(dir)
}

#[cfg(not(unix))]
fn create_dir_mode(dir: &Path) -> io::Result<()> {
    // No Unix mode to set; the platform's default ACLs apply.
    std::fs::create_dir_all(dir)
}

/// Creates `path` with owner-only permissions if it does not exist, and warns (without
/// changing anything) if it does and is group/world-accessible. Used before a third party
/// (SQLite) opens the file itself.
pub fn ensure_private_file(path: &Path) -> io::Result<()> {
    if path.exists() {
        warn_if_wide(path);
        return Ok(());
    }
    create_file_mode(path)
}

#[cfg(unix)]
fn create_file_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(path)
    {
        Ok(_) => Ok(()),
        // Another process created it between the check and here: it is not ours to warn
        // about, and the caller is about to open it anyway.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn create_file_mode(path: &Path) -> io::Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Opens `path` for appending, creating it with owner-only permissions if it does not exist.
pub fn open_private_append(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(PRIVATE_FILE_MODE);
    }
    options.open(path)
}

/// The warning line for `path` when it exists and is group/world-accessible, else `None`.
///
/// Split out from [`warn_if_wide`] so the wording is testable without capturing stderr.
#[cfg(unix)]
pub fn wide_permissions_warning(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).ok()?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 == 0 {
        return None;
    }
    let wanted = if metadata.is_dir() { "700" } else { "600" };
    Some(format!(
        "[privacy] {} is accessible to other users (mode {mode:04o}); session data there may \
         be readable by them. Consider: chmod {wanted} {}",
        path.display(),
        path.display()
    ))
}

/// No group/world bits to reason about off Unix (the platform's ACLs are not modelled here).
#[cfg(not(unix))]
pub fn wide_permissions_warning(_path: &Path) -> Option<String> {
    None
}

/// Prints [`wide_permissions_warning`] to stderr if there is one. Never changes permissions.
pub fn warn_if_wide(path: &Path) {
    let Some(warning) = wide_permissions_warning(path) else {
        return;
    };
    // Warn about a given path **once per process**. A host that warns at startup (on the
    // normal screen) must not repeat the same line from a lazy call later: inside a
    // full-screen UI that write lands in the alternate screen, where the renderer's diff
    // skips the cells it thinks are unchanged and the text stays there for good
    // (dogfooding: `/sessions` printed this while the picker was open).
    static WARNED: std::sync::Mutex<Option<std::collections::HashSet<std::path::PathBuf>>> =
        std::sync::Mutex::new(None);
    {
        let mut warned = WARNED.lock().unwrap_or_else(|poison| poison.into_inner());
        if !warned
            .get_or_insert_with(std::collections::HashSet::new)
            .insert(path.to_path_buf())
        {
            return;
        }
    }
    eprintln!("{warning}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("radian-session-fs-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[cfg(unix)]
    #[test]
    fn a_new_directory_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("new-dir");
        ensure_private_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_wide_directory_is_reported_and_not_changed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("wide-dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(wide_permissions_warning(&dir).is_some());
        ensure_private_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o755,
            "an existing directory's permissions must not be changed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_owner_only_directory_is_not_reported() {
        let dir = scratch("private-dir");
        ensure_private_dir(&dir).unwrap();
        assert!(wide_permissions_warning(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
