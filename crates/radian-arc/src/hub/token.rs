//! `$RADIAN_HOME/hub/token`: the bearer token every hub request must carry (ADR 0048
//! decision 16).
//!
//! The file is 0600 inside the hub directory the sandbox cannot read (ADR 0020), and holds 32
//! random bytes as 64 lowercase hex characters. [`ensure`] creates it on the first start (or
//! for `radian hub pair`) when it is missing, and returns the value it leaves there.
//! [`rotate`] writes a new one through a temporary file and an atomic rename, so a reader sees
//! either the old token or the new one, never a partial file.
//!
//! A running hub reads the file at start and re-reads it whenever its mtime changed, one
//! `stat` per request, so [`rotate`] takes effect on the next request without a restart.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::hub::registry::TOKEN_FILE;
use crate::store::Error;

/// The token's length in bytes.
pub const TOKEN_BYTES: usize = 32;
/// The token file's length in characters: two hex digits per byte.
pub const TOKEN_CHARS: usize = TOKEN_BYTES * 2;

/// The token file inside the hub directory.
pub fn path(dir: &Path) -> PathBuf {
    dir.join(TOKEN_FILE)
}

/// The token the hub must accept, creating one when the file is missing or does not hold a
/// token. This is the first thing `radian hub` and `radian hub pair` do.
///
/// # Errors
///
/// The hub directory or the token file cannot be created or read.
pub fn ensure(dir: &Path) -> Result<String, Error> {
    if let Ok(text) = fs::read_to_string(path(dir)) {
        let token = text.trim().to_owned();
        if valid(&token) {
            return Ok(token);
        }
    }
    write_new(dir)
}

/// Writes a new token and returns it. `radian hub token rotate` calls this; the old token
/// stops working on the next request.
///
/// # Errors
///
/// The OS random source fails, or the hub directory or the token file cannot be written.
pub fn rotate(dir: &Path) -> Result<String, Error> {
    write_new(dir)
}

/// Whether `text` is exactly 32 bytes as 64 lowercase hex characters.
pub fn valid(text: &str) -> bool {
    text.len() == TOKEN_CHARS
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 32 random bytes from the OS, as 64 lowercase hex characters.
fn generate() -> Result<String, Error> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| Error::Failed(format!("cannot read the OS random source: {error}")))?;
    let mut token = String::with_capacity(TOKEN_CHARS);
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    Ok(token)
}

/// Writes a fresh token through a temporary file and an atomic rename, 0600.
fn write_new(dir: &Path) -> Result<String, Error> {
    radian_host::ensure_private_dir(dir)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", dir.display())))?;
    let token = generate()?;
    let target = path(dir);
    let temp = dir.join(format!(".{TOKEN_FILE}.tmp.{}", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        // `mode` is subject to the umask; set it explicitly so the token is 0600 even under a
        // permissive umask.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &target)?;
        File::open(dir)?.sync_all()?;
        Ok(())
    };
    if let Err(error) = write() {
        let _ = fs::remove_file(&temp);
        return Err(Error::Failed(format!(
            "cannot write {}: {error}",
            target.display()
        )));
    }
    Ok(token)
}
