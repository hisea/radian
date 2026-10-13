//! Where a host writes local diagnostics (ADR 0013 decision 4).
//!
//! Diagnostics live in the **user cache** directory — `…/radian/diagnostics/` — not in the
//! session directory: a cache may be cleaned by the system, while the session journal is the
//! authoritative record and must never be treated as disposable. The directory is kept
//! owner-only, files older than [`RETENTION`] are removed, and the total is capped at
//! [`MAX_TOTAL_BYTES`] by dropping the oldest files first. Cleanup runs at startup and again
//! before each write.
//!
//! Nothing here ever touches the session directory, and a failed write or cleanup is a
//! diagnostic in itself — it is reported and the run continues (ADR 0013 decision 4).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Diagnostics older than this are removed.
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// The total size the diagnostics directory may keep.
pub const MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;

/// The suffix of the panic log, the only diagnostic file kind today.
const PANIC_LOG_SUFFIX: &str = ".panic.log";

/// Which platform's cache-directory rule to apply. The paths follow the conventions the
/// session directory already uses (`agent-cli`/`radian-tui`'s `session_dir`), so no `dirs`-style
/// dependency is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// `~/Library/Caches/radian/diagnostics`.
    MacOs,
    /// `$XDG_CACHE_HOME/radian/diagnostics`, or `~/.cache/radian/diagnostics`.
    Linux,
}

/// The diagnostics directory for `platform`, given the resolved `HOME` and `XDG_CACHE_HOME`.
///
/// Pure so both rules are testable on one machine. On Linux an `XDG_CACHE_HOME` that is not
/// absolute is ignored (freedesktop 0.8: a relative value is invalid), exactly like
/// `session_dir` ignores a relative data dir.
pub fn diagnostics_dir_for(
    platform: Platform,
    home: &Path,
    xdg_cache_home: Option<&Path>,
) -> PathBuf {
    match platform {
        Platform::MacOs => home.join("Library/Caches/radian/diagnostics"),
        Platform::Linux => {
            let base = xdg_cache_home
                .filter(|path| path.is_absolute())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.join(".cache"));
            base.join("radian/diagnostics")
        }
    }
}

/// The diagnostics directory for this process, or `None` when there is no `HOME`.
pub fn diagnostics_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)?;
    let platform = if cfg!(target_os = "macos") {
        Platform::MacOs
    } else {
        Platform::Linux
    };
    let xdg = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    Some(diagnostics_dir_for(platform, &home, xdg.as_deref()))
}

/// The file name for one session's panic log: `<session-id>-<unix-millis>.panic.log`. The
/// session id identifies the conversation; the timestamp makes it unique and sortable.
pub fn panic_log_name(session_id: &str, unix_millis: u128) -> String {
    format!("{session_id}-{unix_millis}{PANIC_LOG_SUFFIX}")
}

/// Milliseconds since the Unix epoch, for [`panic_log_name`].
pub fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// True for a file name this program wrote: `<session-id>-<digits>.panic.log`. Cleanup uses
/// this so a user's own file in the directory is never removed.
pub fn is_diagnostic_file(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(PANIC_LOG_SUFFIX) else {
        return false;
    };
    let Some((_, millis)) = stem.rsplit_once('-') else {
        return false;
    };
    !millis.is_empty() && millis.bytes().all(|byte| byte.is_ascii_digit())
}

/// What one cleanup did. Failures are named, not returned as an error: cleanup must not stop
/// the run (ADR 0013 decision 4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanReport {
    pub removed: u64,
    pub freed_bytes: u64,
    /// One ready-to-print line per file that could not be inspected or removed.
    pub failures: Vec<String>,
}

/// Removes diagnostics older than [`RETENTION`], then the oldest files until the total is at
/// most [`MAX_TOTAL_BYTES`]. Only files directly in `dir` whose name is
/// [`is_diagnostic_file`] are considered; a missing directory is the normal first run.
pub fn clean(dir: &Path) -> CleanReport {
    clean_with(dir, RETENTION, MAX_TOTAL_BYTES)
}

/// [`clean`] with explicit limits, so the retention and size rules are testable without
/// creating 50 MiB of files.
pub fn clean_with(dir: &Path, retention: Duration, max_total: u64) -> CleanReport {
    let mut report = CleanReport::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // Not existing yet is normal: there is nothing to clean.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return report,
        Err(error) => {
            report.failures.push(format!(
                "[diagnostics] cannot read {}: {error}",
                dir.display()
            ));
            return report;
        }
    };

    let now = SystemTime::now();
    let mut kept: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.failures.push(format!(
                    "[diagnostics] cannot read an entry in {}: {error}",
                    dir.display()
                ));
                continue;
            }
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_diagnostic_file(&name) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                report.failures.push(format!(
                    "[diagnostics] cannot stat {}: {error}",
                    entry.path().display()
                ));
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        let expired = now
            .duration_since(modified)
            .map(|age| age > retention)
            .unwrap_or(false);
        if expired {
            remove(&entry.path(), metadata.len(), &mut report);
        } else {
            kept.push((entry.path(), modified, metadata.len()));
        }
    }

    // The size cap drops the oldest first: the newest diagnostics are the useful ones.
    let mut total: u64 = kept.iter().map(|(_, _, size)| *size).sum();
    if total > max_total {
        kept.sort_by_key(|(_, modified, _)| *modified);
        for (path, _, size) in kept {
            if total <= max_total {
                break;
            }
            remove(&path, size, &mut report);
            total = total.saturating_sub(size);
        }
    }
    report
}

fn remove(path: &Path, size: u64, report: &mut CleanReport) {
    match std::fs::remove_file(path) {
        Ok(()) => {
            report.removed += 1;
            report.freed_bytes += size;
        }
        Err(error) => report.failures.push(format!(
            "[diagnostics] cannot remove {}: {error}",
            path.display()
        )),
    }
}

/// Appends one line to the diagnostic at `path`: creates the parent directory owner-only
/// (ADR 0013), cleans the directory first, and opens the file owner-only. The caller reports a
/// failure; it never stops the run.
pub fn append(path: &Path, line: &str) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the diagnostic path has no directory",
        )
    })?;
    radian_session::ensure_private_dir(dir)?;
    let _ = clean(dir);
    let mut file = radian_session::open_private_append(path)?;
    file.write_all(line.as_bytes())?;
    file.flush()
}

/// Cleanup at startup: resolve the directory, clean it, and print one line per failure.
/// Called once per run; a missing directory or a failure never stops the run.
pub fn clean_startup() {
    let Some(dir) = diagnostics_dir() else {
        return;
    };
    for failure in clean(&dir).failures {
        eprintln!("{failure}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "radian-host-diagnostics-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(name_dir: &Path, name: &str, bytes: usize) -> PathBuf {
        let path = name_dir.join(name);
        std::fs::write(&path, vec![b'x'; bytes]).unwrap();
        path
    }

    fn set_age(path: &Path, age: Duration) {
        let when = SystemTime::now() - age;
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_two_path_rules_follow_the_platform_conventions() {
        let home = Path::new("/home/u");
        assert_eq!(
            diagnostics_dir_for(Platform::MacOs, home, None),
            PathBuf::from("/home/u/Library/Caches/radian/diagnostics")
        );
        // Linux: an absolute XDG_CACHE_HOME wins.
        assert_eq!(
            diagnostics_dir_for(Platform::Linux, home, Some(Path::new("/xdg/cache"))),
            PathBuf::from("/xdg/cache/radian/diagnostics")
        );
        // Linux: unset falls back to ~/.cache.
        assert_eq!(
            diagnostics_dir_for(Platform::Linux, home, None),
            PathBuf::from("/home/u/.cache/radian/diagnostics")
        );
        // Linux: a relative XDG_CACHE_HOME is invalid and ignored.
        assert_eq!(
            diagnostics_dir_for(Platform::Linux, home, Some(Path::new("relative/cache"))),
            PathBuf::from("/home/u/.cache/radian/diagnostics")
        );
    }

    #[test]
    fn only_our_own_file_names_are_diagnostics() {
        assert!(is_diagnostic_file("abc-1700000000000.panic.log"));
        assert!(is_diagnostic_file("a-b-c-42.panic.log"));
        assert!(!is_diagnostic_file("abc.panic.log"), "no timestamp");
        assert!(!is_diagnostic_file("abc-12.panic.log.bak"));
        assert!(!is_diagnostic_file("notes.txt"));
        assert!(!is_diagnostic_file("important.json"));
        assert!(!is_diagnostic_file("abc-xyz.panic.log"), "not a timestamp");
        assert_eq!(
            panic_log_name("sess-1", 1_700_000_000_000),
            "sess-1-1700000000000.panic.log"
        );
    }

    #[test]
    fn a_file_older_than_the_retention_is_removed_and_a_new_one_kept() {
        let dir = scratch("retention");
        let old = write(&dir, "s-1000.panic.log", 10);
        let new = write(&dir, "s-2000.panic.log", 10);
        set_age(&old, Duration::from_secs(8 * 24 * 60 * 60));

        let report = clean(&dir);
        assert_eq!(report.removed, 1, "{report:?}");
        assert!(report.failures.is_empty(), "{report:?}");
        assert!(!old.exists(), "the 8-day-old file must be removed");
        assert!(new.exists(), "the fresh file must stay");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_oversized_directory_drops_the_oldest_first_until_under_the_limit() {
        let dir = scratch("size-cap");
        let oldest = write(&dir, "s-1000.panic.log", 40);
        let middle = write(&dir, "s-2000.panic.log", 40);
        let newest = write(&dir, "s-3000.panic.log", 40);
        set_age(&oldest, Duration::from_secs(300));
        set_age(&middle, Duration::from_secs(200));
        set_age(&newest, Duration::from_secs(100));

        // 120 bytes total, cap 100: the oldest must go, the newest must stay.
        let report = clean_with(&dir, Duration::from_secs(7 * 24 * 60 * 60), 100);
        assert_eq!(report.removed, 1, "{report:?}");
        assert!(!oldest.exists(), "the oldest must be dropped first");
        assert!(middle.exists() && newest.exists(), "{report:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_leaves_files_that_are_not_ours() {
        let dir = scratch("foreign");
        let ours = write(&dir, "s-1000.panic.log", 40);
        let foreign = write(&dir, "notes.txt", 40);
        let foreign_old = write(&dir, "important.json", 40);
        set_age(&ours, Duration::from_secs(8 * 24 * 60 * 60));
        set_age(&foreign_old, Duration::from_secs(8 * 24 * 60 * 60));
        // A subdirectory must never be touched either.
        let sub = dir.join("subdir");
        std::fs::create_dir_all(&sub).unwrap();

        let report = clean(&dir);
        assert_eq!(report.removed, 1, "{report:?}");
        assert!(!ours.exists());
        assert!(foreign.exists(), "a foreign file must not be removed");
        assert!(foreign_old.exists(), "a foreign file must not be removed");
        assert!(sub.exists(), "a subdirectory must not be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn append_creates_the_directory_and_file_owner_only() {
        let dir = scratch("modes");
        let nested = dir.join("radian/diagnostics");
        let path = nested.join("s-1000.panic.log");
        append(&path, "hello\n").unwrap();
        assert_eq!(mode(&nested), 0o700, "the directory must be 0700");
        assert_eq!(mode(&path), 0o600, "the file must be 0600");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        // A second append adds a line, not a new file.
        append(&path, "world\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nworld\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_also_cleans_the_directory() {
        let dir = scratch("append-clean");
        let old = write(&dir, "s-1000.panic.log", 10);
        set_age(&old, Duration::from_secs(8 * 24 * 60 * 60));
        let path = dir.join("s-2000.panic.log");
        append(&path, "line\n").unwrap();
        assert!(!old.exists(), "the write must also clean");
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_into_an_unusable_directory_is_an_error_not_a_panic() {
        let dir = scratch("unwritable");
        // The diagnostics path exists but is a regular file: the write cannot succeed, and it
        // must be a returned error the caller can report, never a panic.
        let blocked = dir.join("diagnostics");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let error = append(&blocked.join("s-1000.panic.log"), "line\n").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_directory_cleans_cleanly() {
        let dir = scratch("missing");
        let missing = dir.join("does-not-exist");
        let report = clean(&missing);
        assert_eq!(report, CleanReport::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
