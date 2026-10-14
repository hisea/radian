//! The `/theme` write-back (ADR 0008 revision, ).
//!
//! The first place a program writes the configuration file. It changes **only** `tui.theme`,
//! through `toml_edit`, so every other byte — comments, blank lines, key order, unknown
//! fields (ADR 0008 §5.4) — is preserved. The result is re-read with the normal reader and
//! the write is refused (nothing is written) when anything but `tui.theme` differs. The write
//! is atomic (a unique temp file in the same directory + rename) and keeps the original
//! file's permissions.

use std::io::Write;
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

    write_atomic(path, new_text.as_bytes())?;
    Ok(path.to_path_buf())
}

/// A unique temp file in the same directory, the original's permissions, then rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ThemeWriteError> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let result = (|| -> std::io::Result<()> {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        if !bytes.ends_with(b"\n") {
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        // Preserve the original file's permissions; a fresh file keeps the umask default.
        if let Ok(metadata) = std::fs::metadata(path) {
            std::fs::set_permissions(&tmp, metadata.permissions())?;
        }
        std::fs::rename(&tmp, path)
    })();
    if let Err(source) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(ThemeWriteError::Write {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
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
