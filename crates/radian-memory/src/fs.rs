//! Owner-only directory and file creation, matching `radian-session`'s rules (ADR 0013/0021):
//! the memory directory is `0700` and its files are `0600`.

use std::io;
use std::path::Path;

/// Owner-only mode for the memory directory (Unix).
#[cfg(unix)]
pub(crate) const PRIVATE_DIR_MODE: u32 = 0o700;
/// Owner-only mode for a memory file (Unix).
#[cfg(unix)]
pub(crate) const PRIVATE_FILE_MODE: u32 = 0o600;

/// Creates `dir` and its parents with owner-only permissions if missing. An existing directory
/// is left alone, including one another process created between the check and the create.
pub(crate) fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    create_or_accept(dir, create_dir_mode(dir))
}

/// Accepts a create that lost a race: the path is a directory now, so the call succeeded.
fn create_or_accept(dir: &Path, created: io::Result<()>) -> io::Result<()> {
    match created {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn create_dir_mode(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIR_MODE)
        .create(dir)
}

#[cfg(not(unix))]
fn create_dir_mode(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Creates `path` (and its parents) with owner-only permissions for the file.
pub(crate) fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(PRIVATE_FILE_MODE);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-memory-fs-{}-{label}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_directory_a_racing_caller_created_is_not_an_error() {
        let base = temp_dir("race");
        let dir = base.join("scope");
        std::fs::create_dir(&dir).unwrap();
        // The loser of the race saw the directory missing, then its create failed with
        // `AlreadyExists` because the winner got there first.
        create_or_accept(&dir, Err(io::Error::from(ErrorKind::AlreadyExists))).unwrap();
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_existing_file_with_the_directory_name_is_still_an_error() {
        let base = temp_dir("file");
        let path = base.join("scope");
        std::fs::write(&path, b"not a directory").unwrap();
        let error =
            create_or_accept(&path, Err(io::Error::from(ErrorKind::AlreadyExists))).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_existing_directory_is_left_alone() {
        let base = temp_dir("exists");
        let dir = base.join("scope");
        std::fs::create_dir(&dir).unwrap();
        ensure_private_dir(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }
}
