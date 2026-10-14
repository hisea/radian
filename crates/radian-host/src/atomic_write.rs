//! The one atomic file write for Radian's own files.
//!
//! A writer that replaces a file a reader may look at mid-write uses this: a temp file in the
//! same directory, a rename, and a cleanup on failure. The temp name is unique per write —
//! pid + a process-local counter + nanoseconds — because two overlapping writes in **one**
//! process would otherwise share the name, and the later `remove_file` would delete the
//! earlier writer's in-flight file. (`auth.json` had exactly that bug; `/theme`'s write-back
//! copied it.)
//!
//! The temp file is created `create_new` (so a leftover is never silently reused) and its
//! mode is the caller's (`new_mode`) or the target's existing permissions.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Atomically replaces `path` with `bytes`.
///
/// `new_mode` (unix) is the mode a **fresh** file gets; `None` preserves the existing file's
/// permissions (and a fresh file keeps the umask default). On any failure the temp file is
/// removed and `path` is untouched.
pub fn write_atomic(path: &Path, bytes: &[u8], new_mode: Option<u32>) -> std::io::Result<()> {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    // `with_extension` replaces the extension (`config.toml` → `config.tmp-…`), which is what
    // the auth store has always done; the name is diagnostic only.
    let tmp = path.with_extension(format!("tmp-{}-{seq}-{nanos}", std::process::id()));
    // A leftover under the old pid-only name can only be a crashed write from an older Radian
    // (this scheme never generates it), so removing it cannot race a concurrent writer.
    let legacy = path.with_extension(format!("tmp-{}", std::process::id()));
    if legacy != tmp {
        let _ = std::fs::remove_file(&legacy);
    }
    let _ = std::fs::remove_file(&tmp);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(mode) = new_mode {
            options.mode(mode);
        }
    }
    let result = (|| -> std::io::Result<()> {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if new_mode.is_none()
            && let Ok(metadata) = std::fs::metadata(path)
        {
            std::fs::set_permissions(&tmp, metadata.permissions())?;
        }
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
