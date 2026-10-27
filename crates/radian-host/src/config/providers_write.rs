//! The probe write-back (ADR 0026 D10): after a named-provider request succeeds on a protocol
//! the model list or `kind` did not name, radian remembers it as one exact
//! `[[providers.<name>.models]]` rule.
//!
//! The rule goes through the shared [`edit_preserving`] writer, as `model_write.rs` and
//! `theme_write.rs` do, so every other byte of the file survives. It is appended **after** the
//! existing rules, so an earlier user rule still wins. A rule with the same `match` already in
//! the file is left alone: the writer re-reads the file, and two processes saving the same rule
//! therefore end with one rule.

use std::path::{Path, PathBuf};

use toml_edit::{ArrayOfTables, Item, Table, Value, value};

use super::preserve::{PreserveError, edit_preserving};
use crate::provider::ModelRuleDecl;
use crate::provider::ProviderDecl;

/// What the write-back did. Every case leaves exactly one rule for `match`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderRuleOutcome {
    /// The rule was appended (the file was created when absent).
    Written(PathBuf),
    /// The rule was already there; the file was not changed.
    AlreadyPresent(PathBuf),
}

impl ProviderRuleOutcome {
    /// The file the rule lives in.
    pub fn path(&self) -> &Path {
        match self {
            Self::Written(path) | Self::AlreadyPresent(path) => path,
        }
    }
}

/// Why the rule write-back did not happen. Every variant leaves the file untouched.
#[derive(Debug, thiserror::Error)]
pub enum ProviderWriteError {
    /// The file was read read-only (a `schema_version` newer than this build, ADR 0008 §4):
    /// the probe's route applies for this session but the file is never written.
    #[error(
        "{path}: the configuration file is newer than the supported v{supported}; \
         the protocol applies to this session only and the file is not written"
    )]
    ReadOnly { path: PathBuf, supported: u64 },
    #[error("{path}: cannot read the configuration: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: cannot parse the configuration: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("writing {path} would change more than `providers.<name>.models`; nothing was written")]
    Verify { path: PathBuf },
    #[error("{path}: cannot write the configuration: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl From<PreserveError> for ProviderWriteError {
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

/// Whether `config` already carries a rule for exactly `model` under `provider`.
pub fn has_rule(config: &super::Config, provider: &str, model: &str) -> bool {
    config
        .providers
        .get(provider)
        .is_some_and(|decl| decl.models.iter().any(|rule| rule.pattern == model))
}

/// Appends `[[providers.<name>.models]] match = "<model>" kind = "<kind>"` after the provider's
/// existing rules (the table and the file are created when absent).
///
/// `read_only` is the reading process's verdict (ADR 0008 §4); it alone refuses the write. The
/// file is re-read here, so a rule another process already wrote is not written twice. The
/// post-write verification compares against a fresh parse of the file's own text, so no other
/// caller state is needed.
pub fn write_protocol_rule(
    path: &Path,
    read_only: bool,
    provider: &str,
    model: &str,
    kind: &str,
) -> Result<ProviderRuleOutcome, ProviderWriteError> {
    if already_present(path, provider, model)? {
        return Ok(ProviderRuleOutcome::AlreadyPresent(path.to_path_buf()));
    }
    edit_preserving(
        path,
        read_only,
        "`providers.<name>.models`",
        |doc| append_rule(doc, path, provider, model, kind),
        |expected| {
            expected
                .providers
                .entry(provider.to_owned())
                .or_insert_with(ProviderDecl::default)
                .models
                .push(ModelRuleDecl {
                    pattern: model.to_owned(),
                    kind: kind.to_owned(),
                });
        },
    )
    .map_err(ProviderWriteError::from)
    .map(ProviderRuleOutcome::Written)
}

/// The fresh-file check: a missing file has no rule; an unreadable or unparsable file is left
/// for [`edit_preserving`] to report with its own error.
fn already_present(path: &Path, provider: &str, model: &str) -> Result<bool, ProviderWriteError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(ProviderWriteError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let config = super::parse(path, &text).map_err(|error| ProviderWriteError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    Ok(has_rule(&config, provider, model))
}

/// The edit itself: append the rule to the provider's `models` array, whether it is written as
/// `[[providers.<name>.models]]` tables or as an inline array.
fn append_rule(
    doc: &mut toml_edit::DocumentMut,
    path: &Path,
    provider: &str,
    model: &str,
    kind: &str,
) -> Result<(), PreserveError> {
    let verify = || PreserveError::Verify {
        path: path.to_path_buf(),
        keys: "`providers.<name>.models`",
    };
    let providers = doc
        .entry("providers")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(verify)?;
    let entry = providers
        .entry(provider)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(verify)?;
    // A file the same process just wrote (or a race after the pre-check) already has the rule:
    // leave the document as it is and let the verify pass.
    let models = entry.entry("models").or_insert_with(|| {
        let mut tables = ArrayOfTables::new();
        tables.push(rule_table(model, kind));
        Item::ArrayOfTables(tables)
    });
    match models {
        Item::ArrayOfTables(tables) => {
            if !tables
                .iter()
                .any(|table| table.get("match").and_then(|item| item.as_str()) == Some(model))
            {
                tables.push(rule_table(model, kind));
            }
        }
        Item::Value(Value::Array(array)) => {
            let present = array.iter().any(|value| {
                value
                    .as_inline_table()
                    .and_then(|table| table.get("match"))
                    .and_then(|value| value.as_str())
                    == Some(model)
            });
            if !present {
                let mut table = toml_edit::InlineTable::new();
                table.insert("match", Value::from(model));
                table.insert("kind", Value::from(kind));
                array.push(Value::InlineTable(table));
            }
        }
        _ => return Err(verify()),
    }
    Ok(())
}

fn rule_table(model: &str, kind: &str) -> Table {
    let mut table = Table::new();
    table["match"] = value(model);
    table["kind"] = value(kind);
    table
}

#[cfg(test)]
mod tests {
    use super::super::{SUPPORTED_SCHEMA_VERSION, parse};
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "radian-providers-write-{}-{label}-{n}",
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

    /// Only the new rule is added: comments, other tables and an earlier rule survive, and the
    /// new rule comes last so the earlier one still wins.
    #[test]
    fn the_rule_is_appended_after_the_existing_rules() {
        let scratch = Scratch::new("append");
        let path = scratch.file("config.toml");
        let original = "\
# my configuration\n\
schema_version = 1\n\
\n\
[[providers.magpie.models]]\n\
match = \"claude/*\"\n\
kind = \"anthropic\"\n\
\n\
[tui]\n\
theme = \"dark\"\n\
\n\
# trailing comment\n";
        std::fs::write(&path, original).unwrap();
        let outcome = write_protocol_rule(
            &path,
            false,
            "magpie",
            "codex/gpt-5.6-sol",
            "openai-responses",
        )
        .unwrap();
        assert_eq!(outcome, ProviderRuleOutcome::Written(path.clone()));
        let after = std::fs::read_to_string(&path).unwrap();
        let parsed = parse(&path, &after).unwrap();
        let rules = &parsed.providers["magpie"].models;
        assert_eq!(rules.len(), 2, "{rules:?}");
        assert_eq!(rules[0].pattern, "claude/*");
        assert_eq!(rules[1].pattern, "codex/gpt-5.6-sol");
        assert_eq!(rules[1].kind, "openai-responses");
        assert!(after.contains("# trailing comment"), "{after}");
    }

    /// A missing file is created with `schema_version` and the rules-only table.
    #[test]
    fn a_missing_file_is_created_with_the_rules_only_table() {
        let scratch = Scratch::new("create");
        let path = scratch.file("config.toml");
        write_protocol_rule(&path, false, "magpie", "claude/opus", "anthropic").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        let parsed = parse(&path, &after).unwrap();
        assert_eq!(parsed.schema_version, Some(SUPPORTED_SCHEMA_VERSION));
        assert!(!parsed.providers["magpie"].is_full(), "{after}");
        assert_eq!(
            parsed.providers["magpie"].models,
            vec![ModelRuleDecl {
                pattern: "claude/opus".to_owned(),
                kind: "anthropic".to_owned(),
            }],
            "{after}"
        );
    }

    /// The same `match` is never written twice, and the file stays byte-identical.
    #[test]
    fn an_existing_match_is_left_alone() {
        let scratch = Scratch::new("idempotent");
        let path = scratch.file("config.toml");
        let original = "schema_version = 1\n\
                        [[providers.magpie.models]]\n\
                        match = \"claude/opus\"\n\
                        kind = \"anthropic\"\n";
        std::fs::write(&path, original).unwrap();
        let outcome =
            write_protocol_rule(&path, false, "magpie", "claude/opus", "openai-compatible")
                .unwrap();
        assert_eq!(outcome, ProviderRuleOutcome::AlreadyPresent(path.clone()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// A read-only config is never written.
    #[test]
    fn a_read_only_config_is_refused() {
        let scratch = Scratch::new("readonly");
        let path = scratch.file("config.toml");
        let original = "schema_version = 99\n";
        std::fs::write(&path, original).unwrap();
        let error = write_protocol_rule(&path, true, "magpie", "m", "anthropic").unwrap_err();
        assert!(
            matches!(error, ProviderWriteError::ReadOnly { .. }),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// An inline `models` array gains an inline rule too, so the file stays valid.
    #[test]
    fn an_inline_models_array_gains_an_inline_rule() {
        let scratch = Scratch::new("inline");
        let path = scratch.file("config.toml");
        let original = "schema_version = 1\n\
                        [providers.magpie]\n\
                        kind = \"openai-compatible\"\n\
                        base_url = \"https://x/v1\"\n\
                        models = [{ match = \"a/*\", kind = \"anthropic\" }]\n";
        std::fs::write(&path, original).unwrap();
        write_protocol_rule(&path, false, "magpie", "b/model", "openai-responses").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        let parsed = parse(&path, &after).unwrap();
        assert_eq!(parsed.providers["magpie"].models.len(), 2, "{after}");
        assert_eq!(parsed.providers["magpie"].models[1].pattern, "b/model");
    }
}
