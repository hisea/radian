//! The `~/.radian/` layout (ADR 0020): one root for config, cache, sessions, memory and
//! themes (ADR 0029 decision 1).
//!
//! `RADIAN_HOME` overrides the whole root (tests, special deployments); otherwise it is
//! `$HOME/.radian`. There are no platform-specific branches any more (no `Library/...`, no
//! `XDG_*`). Directories are created owner-only (0700) on first use (ADR 0013's permission
//! rule). `--session-dir` and `RADIAN_SESSION_DIR` still override **only** the sessions
//! directory, and the hosts resolve that; it is the actual sessions directory that the
//! sandbox denies read on (ADR 0020 decision 3).

use std::path::{Path, PathBuf};

/// The environment variable that overrides the whole root.
pub const RADIAN_HOME_ENV: &str = "RADIAN_HOME";

/// The root for `radian_home`/`home`. An **absolute** `RADIAN_HOME` wins; otherwise
/// `$HOME/.radian`. Pure, so both rules are testable without touching the environment.
pub fn radian_home_for(radian_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(root) = radian_home.filter(|path| path.is_absolute()) {
        return Some(root.to_path_buf());
    }
    home.filter(|path| path.is_absolute())
        .map(|home| home.join(".radian"))
}

/// The user's home directory (`HOME`), the base a `~` in `[skills] extra_paths` expands to
/// (ADR 0025 slice 4). `None` when `HOME` is unset or empty; a `~` is then left unexpanded and
/// reported as a missing directory, never guessed.
pub fn user_home() -> Option<PathBuf> {
    env_path("HOME")
}

/// The root for this process, or `None` when neither `RADIAN_HOME` nor `HOME` is usable.
pub fn radian_home() -> Option<PathBuf> {
    let radian_home = env_path(RADIAN_HOME_ENV);
    let home = env_path("HOME");
    radian_home_for(radian_home.as_deref(), home.as_deref())
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn config_dir_for(root: &Path) -> PathBuf {
    root.join("config")
}
pub fn cache_dir_for(root: &Path) -> PathBuf {
    root.join("cache")
}
pub fn sessions_dir_for(root: &Path) -> PathBuf {
    root.join("sessions")
}
pub fn memory_dir_for(root: &Path) -> PathBuf {
    root.join("memory")
}
pub fn credentials_dir_for(root: &Path) -> PathBuf {
    root.join("credentials")
}
pub fn themes_dir_for(root: &Path) -> PathBuf {
    root.join("themes")
}
pub fn skills_dir_for(root: &Path) -> PathBuf {
    root.join("skills")
}
pub fn diagnostics_dir_for(root: &Path) -> PathBuf {
    cache_dir_for(root).join("diagnostics")
}
pub fn catalog_cache_for(root: &Path) -> PathBuf {
    cache_dir_for(root).join("catalog").join("models.json")
}

/// Creates `path` (and its parents) owner-only (0700) and returns it. A failure is not fatal
/// here: the caller that writes reports it, and the path is still returned so it can be named.
fn private(path: PathBuf) -> PathBuf {
    let _ = radian_session::ensure_private_dir(&path);
    path
}

/// The config directory for this process, created 0700. Sandbox-readable (ADR 0020).
pub fn config_dir() -> Option<PathBuf> {
    Some(private(config_dir_for(&radian_home()?)))
}

/// The cache root for this process, created 0700. Sandbox-readable (ADR 0020).
pub fn cache_dir() -> Option<PathBuf> {
    Some(private(cache_dir_for(&radian_home()?)))
}

/// The sessions directory for this process, created 0700. **Sandbox-denied** (ADR 0020).
pub fn sessions_dir() -> Option<PathBuf> {
    Some(private(sessions_dir_for(&radian_home()?)))
}

/// The memory directory for this process, created 0700. **Sandbox-denied** (ADR 0020).
pub fn memory_dir() -> Option<PathBuf> {
    Some(private(memory_dir_for(&radian_home()?)))
}

/// The credentials directory for this process, created 0700. **Sandbox-denied** (ADR 0026
/// D4): it holds `auth.json`, which the sandbox must not read.
pub fn credentials_dir() -> Option<PathBuf> {
    Some(private(credentials_dir_for(&radian_home()?)))
}

/// The themes directory for this process, created 0700. Sandbox-readable (ADR 0020): theme
/// files are user UI data, like the config (ADR 0029 decision 1: `~/.radian/themes/<name>.json`).
pub fn themes_dir() -> Option<PathBuf> {
    Some(private(themes_dir_for(&radian_home()?)))
}

/// The skills directory for this process, created 0700. **Sandbox-readable** (ADR 0020's
/// revision for ADR 0025 ruling 1): a skill's body is read by the model and its scripts are run
/// by shell inside the sandbox, so the directory is not on the deny-read list.
pub fn skills_dir() -> Option<PathBuf> {
    Some(private(skills_dir_for(&radian_home()?)))
}

/// The diagnostics directory for this process, created 0700. Sandbox-readable (ADR 0020).
pub fn diagnostics_dir() -> Option<PathBuf> {
    Some(private(diagnostics_dir_for(&radian_home()?)))
}

/// The model catalog cache file. The writer creates its parent; this only names it.
pub fn catalog_cache() -> Option<PathBuf> {
    Some(catalog_cache_for(&radian_home()?))
}

/// Creates `dir` (and its parents) owner-only (0700), warning once if an existing one is
/// group/world-accessible. The hosts call this for the **actual** sessions directory (which
/// `--session-dir` may have moved) before denying it, so Linux's bwrap can mask it (ADR 0020).
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    radian_session::ensure_private_dir(dir)
}

/// The pre-ADR-0020 session directories, in the order a hint should name them. Kept only for
/// the one-line migration hint; nothing reads or writes them.
pub fn legacy_sessions_dirs_for(home: &Path, xdg_data_home: Option<&Path>) -> Vec<PathBuf> {
    let data = xdg_data_home
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".local/share"));
    vec![
        home.join("Library/Application Support/radian/sessions"),
        data.join("radian").join("sessions"),
    ]
}

fn has_jsonl(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(".jsonl"))
            })
        })
        .unwrap_or(false)
}

/// A one-line, copy-pasteable `mv` hint when an old sessions directory has `.jsonl` files and
/// the new `sessions/` has none. Returns `None` otherwise. **Never moves anything** (ADR 0020
/// decision 5): moving a user's data is the user's decision.
pub fn legacy_sessions_hint_for(
    new_sessions: &Path,
    home: &Path,
    xdg_data_home: Option<&Path>,
) -> Option<String> {
    if has_jsonl(new_sessions) {
        return None;
    }
    let old = legacy_sessions_dirs_for(home, xdg_data_home)
        .into_iter()
        .find(|dir| has_jsonl(dir))?;
    Some(format!(
        "[radian] sessions found in the old location {}; Radian now uses {}. Move them with: \
         mv '{}'/* '{}'/",
        old.display(),
        new_sessions.display(),
        old.display(),
        new_sessions.display()
    ))
}

/// [`legacy_sessions_hint_for`] for this process, or `None` when there is nothing to say.
pub fn legacy_sessions_hint() -> Option<String> {
    let home = env_path("HOME")?;
    let xdg_data_home = env_path("XDG_DATA_HOME");
    let new_sessions = sessions_dir()?;
    legacy_sessions_hint_for(&new_sessions, &home, xdg_data_home.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radian_home_prefers_an_absolute_override_then_home() {
        let home = Path::new("/home/u");
        assert_eq!(
            radian_home_for(Some(Path::new("/custom/radian")), Some(home)),
            Some(PathBuf::from("/custom/radian"))
        );
        assert_eq!(
            radian_home_for(None, Some(home)),
            Some(PathBuf::from("/home/u/.radian"))
        );
        // A relative override is ignored (like a relative XDG value was).
        assert_eq!(
            radian_home_for(Some(Path::new("relative")), Some(home)),
            Some(PathBuf::from("/home/u/.radian"))
        );
        assert_eq!(radian_home_for(None, None), None);
    }

    #[test]
    fn the_subdirectory_names_follow_the_layout() {
        let root = Path::new("/home/u/.radian");
        assert_eq!(
            config_dir_for(root),
            PathBuf::from("/home/u/.radian/config")
        );
        assert_eq!(cache_dir_for(root), PathBuf::from("/home/u/.radian/cache"));
        assert_eq!(
            sessions_dir_for(root),
            PathBuf::from("/home/u/.radian/sessions")
        );
        assert_eq!(
            memory_dir_for(root),
            PathBuf::from("/home/u/.radian/memory")
        );
        assert_eq!(
            credentials_dir_for(root),
            PathBuf::from("/home/u/.radian/credentials")
        );
        assert_eq!(
            themes_dir_for(root),
            PathBuf::from("/home/u/.radian/themes")
        );
        assert_eq!(
            diagnostics_dir_for(root),
            PathBuf::from("/home/u/.radian/cache/diagnostics")
        );
        assert_eq!(
            catalog_cache_for(root),
            PathBuf::from("/home/u/.radian/cache/catalog/models.json")
        );
    }

    fn scratch(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("radian-home-test-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_legacy_hint_is_one_line_and_only_when_the_new_dir_has_no_jsonl() {
        let root = scratch("legacy");
        let home = root.join("home");
        let old = home.join("Library/Application Support/radian/sessions");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("s.jsonl"), "{}\n").unwrap();
        let new = home.join(".radian/sessions");
        std::fs::create_dir_all(&new).unwrap();

        let hint = legacy_sessions_hint_for(&new, &home, None).expect("a hint");
        assert!(!hint.contains('\n'), "one line: {hint:?}");
        assert!(hint.contains("mv '"), "{hint}");
        assert!(hint.contains(old.to_str().unwrap()), "{hint}");
        assert!(hint.contains(new.to_str().unwrap()), "{hint}");
        // Nothing was moved.
        assert!(old.join("s.jsonl").exists());

        // Once the new dir has a .jsonl, the hint stops.
        std::fs::write(new.join("n.jsonl"), "{}\n").unwrap();
        assert!(legacy_sessions_hint_for(&new, &home, None).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_legacy_data_means_no_hint() {
        let root = scratch("no-legacy");
        let home = root.join("home");
        let new = home.join(".radian/sessions");
        std::fs::create_dir_all(&new).unwrap();
        assert!(legacy_sessions_hint_for(&new, &home, None).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
