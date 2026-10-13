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
/// is left alone.
pub(crate) fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    create_dir_mode(dir)
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
