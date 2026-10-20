//! The project-level declaration (ADR 0043 decision 2): `<workspace>/.radian/config.toml`.
//!
//! A project may say that it wants the sandbox, and nothing else. The only key read is
//! `[permissions] profile = "workspace"`; the tier is a **request to narrow** and is
//! never allowed to widen (a cloned repository must not be able to open the sandbox). Writing
//! `full` there is a configuration error: it is warned about and ignored. Any other key is
//! skipped and named; `[sandbox]` keys (`allow_env`, `allow_unix_sockets`) are refused by name
//! because the user-level configuration owns them (ADR 0034).
//!
//! The file also counts as a project resource for the trust gate (ADR 0043 decision 1), but
//! the narrowing below applies whether or not the project is trusted: narrowing cannot hurt.

use std::path::{Path, PathBuf};

use radian_permissions::Preset;

/// The file's name inside `<workspace>/.radian/`.
pub const FILE_NAME: &str = "config.toml";

/// `<workspace>/.radian/config.toml`.
pub fn path(workspace: &Path) -> PathBuf {
    workspace.join(".radian").join(FILE_NAME)
}

/// What a project config asks for, and every reason it could not be used in full.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectConfig {
    /// The tier the project asks for, after the `full`-is-refused rule. `None` when the file
    /// is absent, unreadable, malformed, or asks for nothing usable.
    pub tier: Option<Preset>,
    /// One line per ignored key or refused value. The host prints these at startup.
    pub warnings: Vec<String>,
}

/// Reads `<workspace>/.radian/config.toml`. Never an error: a problem is a warning and the
/// tier stays `None`, so a repository cannot break startup or widen the tier.
pub fn read(workspace: &Path) -> ProjectConfig {
    read_at(&path(workspace))
}

/// [`read`] at an explicit path (the tests use this).
pub fn read_at(path: &Path) -> ProjectConfig {
    let mut out = ProjectConfig::default();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return out,
        Err(error) => {
            out.warnings
                .push(format!("{}: cannot read: {error}", path.display()));
            return out;
        }
    };
    let doc: toml::Value = match toml::from_str(&text) {
        Ok(doc) => doc,
        Err(error) => {
            out.warnings
                .push(format!("{}: not valid TOML: {error}", path.display()));
            return out;
        }
    };
    let Some(table) = doc.as_table() else {
        out.warnings
            .push(format!("{}: the top level is not a table", path.display()));
        return out;
    };
    for (key, value) in table {
        match key.as_str() {
            "permissions" => match value.as_table() {
                Some(permissions) => {
                    for (name, value) in permissions {
                        match (name.as_str(), value) {
                            ("profile", toml::Value::String(profile)) => {
                                apply_profile(path, profile, &mut out)
                            }
                            ("profile", _) => out.warnings.push(format!(
                                "{}: permissions.profile must be a string; ignored",
                                path.display()
                            )),
                            (other, _) => out.warnings.push(format!(
                                "{}: ignoring permissions.{other} (a project config sets only \
                                 permissions.profile)",
                                path.display()
                            )),
                        }
                    }
                }
                None => out.warnings.push(format!(
                    "{}: permissions must be a table; ignored",
                    path.display()
                )),
            },
            // ADR 0034: the socket and environment whitelists belong to
            // the user-level configuration. A project must not set them.
            "sandbox" => match value.as_table() {
                Some(sandbox) => {
                    for name in sandbox.keys() {
                        out.warnings.push(format!(
                            "{}: sandbox.{name} is not allowed in a project config \
                             (ADR 0034); ignored",
                            path.display()
                        ));
                    }
                }
                None => out.warnings.push(format!(
                    "{}: sandbox must be a table; ignored",
                    path.display()
                )),
            },
            other => out.warnings.push(format!(
                "{}: ignoring {other} (a project config sets only permissions.profile)",
                path.display()
            )),
        }
    }
    out
}

fn apply_profile(path: &Path, profile: &str, out: &mut ProjectConfig) {
    match profile {
        "workspace" | "workspace-write" => out.tier = Some(Preset::WorkspaceWrite),
        // A project may only narrow. `full` is what the user runs by default, so accepting it
        // would be a no-op at best and a widening at worst.
        "full" | "full-access" => out.warnings.push(format!(
            "{}: permissions.profile = \"{profile}\" cannot widen the tier from a project; \
             ignored",
            path.display()
        )),
        other => out.warnings.push(format!(
            "{}: permissions.profile = {other:?} is not one of workspace, full; ignored",
            path.display()
        )),
    }
}

/// The narrower of two tiers (`workspace` < `full`).
pub fn narrow(a: Preset, b: Preset) -> Preset {
    if rank(a) <= rank(b) { a } else { b }
}

/// The tier in force: `chosen`, narrowed by the project's request when there is one.
pub fn effective(chosen: Preset, declared: Option<Preset>) -> Preset {
    declared.map_or(chosen, |declared| narrow(chosen, declared))
}

fn rank(tier: Preset) -> u8 {
    match tier {
        Preset::WorkspaceWrite => 0,
        Preset::FullAccess => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!(
                "radian-project-test-{label}-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn write(&self, text: &str) -> PathBuf {
            let path = path(&self.root);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_missing_file_asks_for_nothing() {
        let scratch = Scratch::new("missing");
        assert_eq!(read(&scratch.root), ProjectConfig::default());
    }

    #[test]
    fn workspace_narrows() {
        let scratch = Scratch::new("tier");
        scratch.write("[permissions]\nprofile = \"workspace\"\n");
        let read = read(&scratch.root);
        assert_eq!(read.tier, Some(Preset::WorkspaceWrite));
        assert!(read.warnings.is_empty(), "{:?}", read.warnings);
    }

    /// The removed tier name is unknown: it warns and asks for nothing, like any typo.
    #[test]
    fn safe_in_a_project_config_is_unknown_and_asks_for_nothing() {
        let scratch = Scratch::new("safe");
        scratch.write("[permissions]\nprofile = \"safe\"\n");
        let read = read(&scratch.root);
        assert_eq!(read.tier, None);
        assert_eq!(read.warnings.len(), 1, "{:?}", read.warnings);
        assert!(
            read.warnings[0].contains("not one of workspace, full"),
            "{:?}",
            read.warnings
        );
    }

    /// The mutation: accepting `full` from the project. The tier must stay `None`, and the
    /// refusal must be named.
    #[test]
    fn full_in_a_project_config_is_refused_with_a_warning() {
        let scratch = Scratch::new("full");
        scratch.write("[permissions]\nprofile = \"full\"\n");
        let read = read(&scratch.root);
        assert_eq!(read.tier, None);
        assert_eq!(read.warnings.len(), 1, "{:?}", read.warnings);
        assert!(
            read.warnings[0].contains("cannot widen"),
            "{:?}",
            read.warnings
        );
    }

    /// Other keys are skipped and named, never adopted; `[sandbox]` is refused by name
    /// (ADR 0034).    #[test]
    fn other_keys_are_named_and_not_used() {
        let scratch = Scratch::new("keys");
        scratch.write(
            "schema_version = 1\n[permissions]\nprofile = \"workspace\"\nfuture = 1\n\
             [sandbox]\nallow_env = [\"FOO\"]\nallow_unix_sockets = [\"~/.x\"]\n\
             [model]\nmodel = \"m\"\n",
        );
        let read = read(&scratch.root);
        assert_eq!(read.tier, Some(Preset::WorkspaceWrite));
        for expected in [
            "permissions.future",
            "sandbox.allow_env",
            "sandbox.allow_unix_sockets",
            "ignoring schema_version",
            "ignoring model",
        ] {
            assert!(
                read.warnings
                    .iter()
                    .any(|warning| warning.contains(expected)),
                "{expected} is not named: {:?}",
                read.warnings
            );
        }
    }

    /// A project `[mcp]` starts nothing: user MCP servers come only from the user's
    /// `config.toml` `[mcp]` and `--mcp-config`, never from the project file. The keys
    /// are still named (the generic project rule), in both TOML spellings.
    #[test]
    fn a_project_mcp_section_starts_nothing() {
        let scratch = Scratch::new("mcp");
        scratch.write(
            "[[mcp.servers]]\nid = \"evil\"\ncommand = \"/tmp/evil\"\n\
              [mcp.mcpServers.other]\ncommand = \"/tmp/other\"\n",
        );
        let read = read(&scratch.root);
        assert_eq!(read.tier, None, "no tier was asked for: {read:?}");
        assert!(
            read.warnings.iter().any(|warning| warning.contains("ignoring mcp")),
            "the project [mcp] must be named and ignored: {:?}",
            read.warnings
        );
    }

    /// A malformed file cannot widen and cannot break startup.
    #[test]
    fn a_malformed_file_warns_and_asks_for_nothing() {
        let scratch = Scratch::new("malformed");
        scratch.write("not toml [");
        let read = read(&scratch.root);
        assert_eq!(read.tier, None);
        assert_eq!(read.warnings.len(), 1, "{:?}", read.warnings);
        assert!(
            read.warnings[0].contains("not valid TOML"),
            "{:?}",
            read.warnings
        );
    }

    /// The narrowing rule (`workspace` < `full`), both directions. Taking the wider of
    /// the two is the mutation this catches.
    #[test]
    fn the_narrower_tier_wins() {
        assert_eq!(
            effective(Preset::FullAccess, Some(Preset::WorkspaceWrite)),
            Preset::WorkspaceWrite
        );
        // A project request wider than the chosen tier does not widen it.
        assert_eq!(
            effective(Preset::WorkspaceWrite, Some(Preset::FullAccess)),
            Preset::WorkspaceWrite
        );
        assert_eq!(effective(Preset::FullAccess, None), Preset::FullAccess);
        assert_eq!(
            effective(Preset::WorkspaceWrite, None),
            Preset::WorkspaceWrite
        );
    }
}
