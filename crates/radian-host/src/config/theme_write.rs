//! The `/theme` write-back (ADR 0008 revision, ).
//!
//! The first place a program writes the configuration file. It changes **only** `tui.theme`,
//! through `toml_edit`, so every other byte — comments, blank lines, key order, unknown
//! fields (ADR 0008 §5.4) — is preserved. The result is re-read with the normal reader and
//! the write is refused (nothing is written) when anything but `tui.theme` differs. The write
//! goes through the shared [`crate::atomic_write`] helper (a unique temp file + rename, the
//! original file's permissions), and a symbolic link is resolved first so a dotfiles setup
//! keeps its link.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, value};

use super::{Config, SUPPORTED_SCHEMA_VERSION, parse};

/// Why the write-back did not happen. Every variant leaves the file untouched.
#[derive(Debug, thiserror::Error)]
pub enum ThemeWriteError {
    /// The file was read read-only (a `schema_version` newer than this build, ADR 0008 §5.2):
    /// the theme applies for this session but the file is never written.
    #[error(
        "{path}: the configuration file is newer than the supported v{supported}; \
         the theme applies to this session only and the file is not written"
    )]
    ReadOnly { path: PathBuf, supported: u64 },
    #[error("{path}: cannot read the configuration: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: cannot parse the configuration: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("writing {path} would change more than `tui.theme`; nothing was written")]
    Verify { path: PathBuf },
    #[error("{path}: cannot write the configuration: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Writes `tui.theme = theme` into the configuration at `path` (created when absent).
///
/// `read_only` is the reading process's verdict (ADR 0008 §5.2: a `schema_version` newer
/// than this build); it alone refuses the write. The post-write verification compares
/// against a fresh parse of the file's own text, so no other caller state is needed.
/// Returns the path on success.
pub fn write_theme(path: &Path, read_only: bool, theme: &str) -> Result<PathBuf, ThemeWriteError> {
    if read_only {
        return Err(ThemeWriteError::ReadOnly {
            path: path.to_path_buf(),
            supported: SUPPORTED_SCHEMA_VERSION,
        });
    }
    let original = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(ThemeWriteError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    // The document to edit: the file's own bytes, or a fresh one.
    let mut doc = match &original {
        Some(text) => text
            .parse::<DocumentMut>()
            .map_err(|error| ThemeWriteError::Parse {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?,
        None => DocumentMut::new(),
    };
    if original.is_none() {
        doc["schema_version"] = value(SUPPORTED_SCHEMA_VERSION as i64);
    }
    let tui = doc
        .entry("tui")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| ThemeWriteError::Verify {
            path: path.to_path_buf(),
        })?;
    tui["theme"] = value(theme);
    let new_text = doc.to_string();

    // Verify with the normal reader: only `tui.theme` may differ. The baseline is the file's
    // own parse (or the default for a fresh file), so the comparison does not depend on the
    // caller's `current` being built by this same reader.
    let mut expected = match &original {
        Some(text) => parse(path, text).map_err(|error| ThemeWriteError::Parse {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?,
        None => Config {
            source: Some(path.to_path_buf()),
            schema_version: Some(SUPPORTED_SCHEMA_VERSION),
            ..Config::default()
        },
    };
    let after = parse(path, &new_text).map_err(|error| ThemeWriteError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    expected.tui.theme = Some(theme.to_string());
    expected.source = after.source.clone();
    if expected != after {
        return Err(ThemeWriteError::Verify {
            path: path.to_path_buf(),
        });
    }

    // Resolve a symbolic link: a dotfiles setup links `config.toml` into a repository, and a
    // rename onto the link's path would replace the link with a plain file.
    // The write goes to the link's target; the notice keeps naming the path the user knows.
    let target = resolve_target(path);
    let mut bytes = new_text.into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    crate::atomic_write::write_atomic(&target, &bytes, None).map_err(|source| {
        ThemeWriteError::Write {
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(path.to_path_buf())
}

/// The real file a write targets: the symbolic link is resolved (all of it, through
/// `canonicalize`), and when the file does not exist yet the nearest existing ancestor is
/// resolved so a symlinked parent directory still lands in the repository.
fn resolve_target(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            match std::fs::canonicalize(parent) {
                Ok(real_parent) => real_parent.join(name),
                Err(_) => path.to_path_buf(),
            }
        }
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique scratch directory per test, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "radian-theme-write-{}-{label}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The format-preservation promise (ADR 0008 §5.4): comments, blank lines, key order and
    /// unknown fields survive; only the `theme` line changes.
    #[test]
    fn write_theme_changes_only_the_theme_line() {
        let scratch = Scratch::new("fidelity");
        let path = scratch.file("config.toml");
        let original = "\
# my configuration\n\
schema_version = 1\n\
\n\
[model]\n\
provider = \"opencode-go\"   # the fast one\n\
future_option = true\n\
\n\
[tui]\n\
theme = \"dark\"\n\
mouse = false\n\
\n\
# trailing comment\n\
[tui.colors]\n\
user = \"#0af\"\n";
        std::fs::write(&path, original).unwrap();
        write_theme(&path, false, "light").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        let expected = original.replace("theme = \"dark\"", "theme = \"light\"");
        assert_eq!(after, expected, "only the theme line may change");
    }

    /// A missing file is created with `schema_version` and `[tui] theme`.
    #[test]
    fn write_theme_creates_a_missing_file() {
        let scratch = Scratch::new("create");
        let path = scratch.file("config.toml");
        write_theme(&path, false, "mono").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        let parsed = parse(&path, &after).unwrap();
        assert_eq!(parsed.schema_version, Some(SUPPORTED_SCHEMA_VERSION));
        assert_eq!(parsed.tui.theme.as_deref(), Some("mono"));
    }

    /// A config with an existing `[tui]` table gains only the key (or replaces it).
    #[test]
    fn write_theme_replaces_an_existing_key_in_place() {
        let scratch = Scratch::new("replace");
        let path = scratch.file("config.toml");
        std::fs::write(&path, "schema_version = 1\n[tui]\nmouse = false\n").unwrap();
        write_theme(&path, false, "light").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("mouse = false"), "{after}");
        assert!(after.contains("theme = \"light\""), "{after}");
    }

    /// A read-only config (a newer `schema_version`) is never written.
    #[test]
    fn write_theme_refuses_a_read_only_config() {
        let scratch = Scratch::new("readonly");
        let path = scratch.file("config.toml");
        let original = "schema_version = 99\n[tui]\ntheme = \"dark\"\n";
        std::fs::write(&path, original).unwrap();
        let error = write_theme(&path, true, "light").unwrap_err();
        assert!(matches!(error, ThemeWriteError::ReadOnly { .. }), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "a refused write leaves the file untouched"
        );
    }

    /// The original file's permissions survive the atomic replace.
    #[cfg(unix)]
    #[test]
    fn write_theme_keeps_the_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new("mode");
        let path = scratch.file("config.toml");
        std::fs::write(&path, "schema_version = 1\n[tui]\ntheme = \"dark\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_theme(&path, false, "light").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640, "the mode is preserved");
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "radian-theme-review-{}-{label}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// U2: two writes that overlap in one process must not fight over a temp file (the old
    /// `tmp-<pid>` name let the later `remove_file` delete the earlier writer's file) and must
    /// leave nothing behind.
    #[test]
    fn concurrent_writes_do_not_error_and_leave_no_temp_files() {
        let scratch = Scratch::new("concurrent");
        let path = scratch.file("config.toml");
        std::fs::write(&path, "schema_version = 1\n[tui]\ntheme = \"dark\"\n").unwrap();
        std::thread::scope(|scope| {
            for name in ["light", "mono", "dark", "light", "mono", "dark"] {
                let path = path.clone();
                scope.spawn(move || {
                    write_theme(&path, false, name).expect("a concurrent write must not fail");
                });
            }
        });
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("theme = \""), "{text}");
        let leftovers: Vec<String> = std::fs::read_dir(&scratch.0)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    /// U3: a dotfiles setup links `config.toml` into a repository. The write must reach the
    /// target and leave the link a link.
    #[cfg(unix)]
    #[test]
    fn write_theme_keeps_a_symlink_and_writes_its_target() {
        let scratch = Scratch::new("symlink");
        let dotfiles = scratch.file("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join("config.toml");
        std::fs::write(&target, "schema_version = 1\n[tui]\ntheme = \"dark\"\n").unwrap();
        let link = scratch.file("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_theme(&link, false, "light").unwrap();

        let metadata = std::fs::symlink_metadata(&link).unwrap();
        assert!(
            metadata.file_type().is_symlink(),
            "the link must survive the write"
        );
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("theme = \"light\""), "{text}");
    }

    /// U3, the other shape: the link is on the **directory**. Writing a fresh file under a
    /// linked parent must land in the real directory too.
    #[cfg(unix)]
    #[test]
    fn write_theme_through_a_symlinked_directory() {
        let scratch = Scratch::new("symlink-dir");
        let real = scratch.file("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = scratch.file("linked");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let path = link.join("config.toml");

        write_theme(&path, false, "mono").unwrap();

        let written = real.join("config.toml");
        assert!(written.exists(), "the file landed in the real directory");
        let text = std::fs::read_to_string(&written).unwrap();
        assert!(text.contains("theme = \"mono\""), "{text}");
    }
}
