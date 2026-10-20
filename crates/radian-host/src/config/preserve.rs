//! The format-preserving edit both config write-backs share (ADR 0008 §5.4).
//!
//! `/theme` and the first-launch model choice both change a few keys through `toml_edit` so
//! every other byte — comments, blank lines, key order, unknown fields — is preserved. The
//! result is re-read with the normal reader and refused when anything but the intended keys
//! differs. The write goes through the shared atomic helper (a unique temp file + rename,
//! the original file's permissions), and a symbolic link is resolved first so a dotfiles
//! setup keeps its link.

use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;

use super::{Config, SUPPORTED_SCHEMA_VERSION, parse};

/// Why a preserving edit did not happen. Every variant leaves the file untouched.
#[derive(Debug, thiserror::Error)]
pub enum PreserveError {
    /// The file was read read-only (a `schema_version` newer than this build, ADR 0008 §5.2).
    #[error(
        "{path}: the configuration file is newer than the supported v{supported}; \
         applies to this session only and the file is not written"
    )]
    ReadOnly { path: PathBuf, supported: u64 },
    #[error("{path}: cannot read the configuration: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: cannot parse the configuration: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("writing {path} would change more than {keys}; nothing was written")]
    Verify { path: PathBuf, keys: &'static str },
    #[error("{path}: cannot write the configuration: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Edits the file at `path` (created when absent) through `toml_edit`.
///
/// `keys` names the edited keys for the verify error (e.g. "`tui.theme`"). `edit` mutates
/// the document (a fresh one with `schema_version` when the file is absent). `expect`
/// mutates the baseline `Config` the same way, so the post-write parse comparison only
/// allows the intended change. Returns the path on success.
pub fn edit_preserving(
    path: &Path,
    read_only: bool,
    keys: &'static str,
    edit: impl FnOnce(&mut DocumentMut) -> Result<(), PreserveError>,
    expect: impl FnOnce(&mut Config),
) -> Result<PathBuf, PreserveError> {
    if read_only {
        return Err(PreserveError::ReadOnly {
            path: path.to_path_buf(),
            supported: SUPPORTED_SCHEMA_VERSION,
        });
    }
    let original = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(PreserveError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut doc = match &original {
        Some(text) => text
            .parse::<DocumentMut>()
            .map_err(|error| PreserveError::Parse {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?,
        None => DocumentMut::new(),
    };
    if original.is_none() {
        doc["schema_version"] = toml_edit::value(SUPPORTED_SCHEMA_VERSION as i64);
    }
    edit(&mut doc)?;
    let new_text = doc.to_string();

    let mut expected = match &original {
        Some(text) => parse(path, text).map_err(|error| PreserveError::Parse {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?,
        None => Config {
            source: Some(path.to_path_buf()),
            schema_version: Some(SUPPORTED_SCHEMA_VERSION),
            ..Config::default()
        },
    };
    let after = parse(path, &new_text).map_err(|error| PreserveError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    expect(&mut expected);
    expected.source = after.source.clone();
    if expected != after {
        return Err(PreserveError::Verify {
            path: path.to_path_buf(),
            keys,
        });
    }

    let target = resolve_target(path);
    let mut bytes = new_text.into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    crate::atomic_write::write_atomic(&target, &bytes, None).map_err(|source| {
        PreserveError::Write {
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
