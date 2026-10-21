//! The first-launch model write-back (ADR 0008 §5).
//!
//! When a session starts with no configured model and the user picks one with `/model`,
//! the choice is written back as `[model] provider` and `[model] model`, preserving
//! everything else byte-for-byte (the same rules as `/theme`, including the read-only
//! refusal). A later `/model` switch in a session that already had a configured model does
//! not write the file (ADR 0026 D5: `/model` only advances the journal's `ConfigChanged`).

use std::path::{Path, PathBuf};

use toml_edit::{Item, Table, value};

use super::preserve::{PreserveError, edit_preserving};

/// Why the model write-back did not happen. Every variant leaves the file untouched.
#[derive(Debug, thiserror::Error)]
pub enum ModelWriteError {
    /// The file was read read-only (a `schema_version` newer than this build, ADR 0008 §4):
    /// the model applies for this session but the file is never written.
    #[error(
        "{path}: the configuration file is newer than the supported v{supported}; \
         the model applies to this session only and the file is not written"
    )]
    ReadOnly { path: PathBuf, supported: u64 },
    #[error("{path}: cannot read the configuration: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: cannot parse the configuration: {message}")]
    Parse { path: PathBuf, message: String },
    #[error(
        "writing {path} would change more than `[model] provider`/`model`; nothing was written"
    )]
    Verify { path: PathBuf },
    #[error("{path}: cannot write the configuration: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl From<PreserveError> for ModelWriteError {
    fn from(error: PreserveError) -> Self {
        match error {
            PreserveError::ReadOnly { path, supported } => Self::ReadOnly { path, supported },
            PreserveError::Read { path, source } => Self::Read { path, source },
            PreserveError::Parse { path, message } => Self::Parse { path, message },
            PreserveError::Verify { path, .. } => Self::Verify { path },
            PreserveError::Write { path, source } => Self::Write { path, source },
        }
    }
}

/// Writes `[model] provider` and `[model] model` into the configuration at `path` (created
/// when absent, `[model]` created when absent).
///
/// `read_only` is the reading process's verdict (ADR 0008 §4); it alone refuses the write.
/// Returns the path on success.
pub fn write_model_route(
    path: &Path,
    read_only: bool,
    provider: &str,
    model: &str,
) -> Result<PathBuf, ModelWriteError> {
    edit_preserving(
        path,
        read_only,
        "`[model] provider`/`model`",
        |doc| {
            let table = doc
                .entry("model")
                .or_insert(Item::Table(Table::new()))
                .as_table_mut()
                .ok_or_else(|| PreserveError::Verify {
                    path: path.to_path_buf(),
                    keys: "`[model] provider`/`model`",
                })?;
            table["provider"] = value(provider);
            table["model"] = value(model);
            Ok(())
        },
        |expected| {
            expected.model.provider = Some(provider.to_owned());
            expected.model.model = Some(model.to_owned());
        },
    )
    .map_err(ModelWriteError::from)
}

#[cfg(test)]
mod tests {
    use super::super::parse;
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "radian-model-write-{}-{label}-{n}",
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

    /// Only the two keys change; comments, other tables and unknown fields survive.
    #[test]
    fn write_model_changes_only_the_two_keys() {
        let scratch = Scratch::new("fidelity");
        let path = scratch.file("config.toml");
        let original = "\
# my configuration\n\
schema_version = 1\n\
\n\
[model]\n\
provider = \"openai-compatible\"\n\
model = \"old\"\n\
future_option = true   # kept\n\
\n\
[tui]\n\
theme = \"dark\"\n\
\n\
# trailing comment\n";
        std::fs::write(&path, original).unwrap();
        write_model_route(&path, false, "anthropic", "new-model").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("provider = \"anthropic\""), "{after}");
        assert!(after.contains("model = \"new-model\""), "{after}");
        assert!(after.contains("# kept"), "{after}");
        assert!(after.contains("future_option = true"), "{after}");
        assert!(after.contains("theme = \"dark\""), "{after}");
        assert!(after.contains("# trailing comment"), "{after}");
        let parsed = parse(&path, &after).unwrap();
        assert_eq!(parsed.model.provider.as_deref(), Some("anthropic"));
        assert_eq!(parsed.model.model.as_deref(), Some("new-model"));
    }

    /// A missing file is created with `schema_version` and `[model]`.
    #[test]
    fn write_model_creates_a_missing_file_and_table() {
        let scratch = Scratch::new("create");
        let path = scratch.file("config.toml");
        write_model_route(&path, false, "opencode-go", "m-1").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        let parsed = parse(&path, &after).unwrap();
        assert_eq!(
            parsed.schema_version,
            Some(super::super::SUPPORTED_SCHEMA_VERSION)
        );
        assert_eq!(parsed.model.provider.as_deref(), Some("opencode-go"));
        assert_eq!(parsed.model.model.as_deref(), Some("m-1"));
    }

    /// A file without `[model]` gains only the table.
    #[test]
    fn write_model_creates_a_missing_table() {
        let scratch = Scratch::new("table");
        let path = scratch.file("config.toml");
        std::fs::write(&path, "schema_version = 1\n[tui]\ntheme = \"dark\"\n").unwrap();
        write_model_route(&path, false, "anthropic", "m-2").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("theme = \"dark\""), "{after}");
        let parsed = parse(&path, &after).unwrap();
        assert_eq!(parsed.model.provider.as_deref(), Some("anthropic"));
        assert_eq!(parsed.model.model.as_deref(), Some("m-2"));
    }

    /// A read-only config is never written.
    #[test]
    fn write_model_refuses_a_read_only_config() {
        let scratch = Scratch::new("readonly");
        let path = scratch.file("config.toml");
        let original = "schema_version = 99\n[model]\nprovider = \"a\"\nmodel = \"b\"\n";
        std::fs::write(&path, original).unwrap();
        let error = write_model_route(&path, true, "x", "y").unwrap_err();
        assert!(matches!(error, ModelWriteError::ReadOnly { .. }), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "a refused write leaves the file untouched"
        );
    }
}
