//! `$RADIAN_HOME/hub/repos.toml`: the repositories a hub manages.
//!
//! The file is a TOML document of `[[repos]]` tables with a `name` and a `path`. A missing
//! file is an empty registry. A file that does not parse, a name outside its rule, a path that
//! is not absolute or cannot be resolved, a path with no `.radian/arc/`, and a duplicate name
//! or path are errors naming the file (with the line where there is one) or the path.
//!
//! Every write creates the hub directory 0700, writes a temporary file 0600 in it, fsyncs,
//! renames it over the registry, and fsyncs the directory. A reader therefore sees either the
//! old or the new whole document, which is what lets a running hub re-read the file while
//! `radian hub add` and `remove` change it.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::Spanned;

use crate::store::{ARC_DIR, Error, Store};

/// The registry file inside the hub directory.
pub const REGISTRY_FILE: &str = "repos.toml";
/// The bearer token the HTTP API requires (the HTTP task).
pub const TOKEN_FILE: &str = "token";
/// The flock a running hub holds for its lifetime (the loop task).
pub const HUB_LOCK: &str = "hub.lock";

/// The longest repository name, in characters.
pub const NAME_MAX_CHARS: usize = 48;

/// A repository name: 1 to 48 characters from `[a-z0-9-]`.
pub fn valid_name(name: &str) -> bool {
    let count = name.chars().count();
    (1..=NAME_MAX_CHARS).contains(&count)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// One registered repository: its name and its absolute, canonical path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoEntry {
    pub name: String,
    pub path: PathBuf,
}

/// The file as written. `deny_unknown_fields` keeps a typo in the registry visible.
#[derive(Serialize)]
struct Document<'a> {
    repos: &'a [RepoEntry],
}

/// The file as read: each entry keeps its span, so a bad name or path can name its line.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReposFile {
    #[serde(default)]
    repos: Vec<Spanned<RawRepo>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepo {
    name: Spanned<String>,
    path: Spanned<PathBuf>,
}

/// The registry in one hub directory. Cheap to construct; every operation re-reads the file,
/// because a `radian hub add` in another process may have changed it.
#[derive(Debug, Clone)]
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    /// The registry of this process: `$RADIAN_HOME/hub/`. The directory is created on the
    /// first write, not here, so a read of a missing registry leaves nothing behind.
    ///
    /// # Errors
    ///
    /// Neither `RADIAN_HOME` nor `HOME` is usable.
    pub fn open() -> Result<Self, Error> {
        let home = radian_host::radian_home().ok_or_else(|| {
            Error::Failed(
                "cannot find the radian home directory (set RADIAN_HOME or HOME)".to_owned(),
            )
        })?;
        Ok(Self::at(radian_host::hub_dir_for(&home)))
    }

    /// The registry rooted at an explicit hub directory (tests, an alternate home).
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The hub directory this registry lives in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The `repos.toml` path.
    pub fn file(&self) -> PathBuf {
        self.dir.join(REGISTRY_FILE)
    }

    /// The registered repositories, in file order.
    pub fn repos(&self) -> Result<Vec<RepoEntry>, Error> {
        self.read()
    }

    /// The registered repositories, in file order. The name `hub list` uses.
    pub fn list(&self) -> Result<Vec<RepoEntry>, Error> {
        self.repos()
    }

    /// Registers `path`, under `name` or the directory's base name. The path is
    /// canonicalized first; a duplicate name or a duplicate path (a symlink included) is
    /// refused, and so is a directory that is not an arc repository.
    pub fn add(&self, path: &Path, name: Option<&str>) -> Result<RepoEntry, Error> {
        let canonical = fs::canonicalize(path).map_err(|error| {
            Error::Failed(format!("cannot register `{}`: {error}", path.display()))
        })?;
        if !canonical.join(ARC_DIR).is_dir() {
            return Err(Error::Failed(format!(
                "`{}` is not an arc repository (no {ARC_DIR}/)",
                canonical.display()
            )));
        }
        let name = match name {
            Some(name) => {
                if !valid_name(name) {
                    return Err(Error::Usage(format!(
                        "`{name}` is not a repository name (1 to {NAME_MAX_CHARS} characters of \
                         a-z, 0-9 or -)"
                    )));
                }
                name.to_owned()
            }
            None => {
                let base = canonical
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        Error::Usage(format!(
                            "`{}` has no directory name; pass --name",
                            canonical.display()
                        ))
                    })?;
                if !valid_name(base) {
                    return Err(Error::Usage(format!(
                        "the directory name `{base}` is not a repository name (1 to \
                         {NAME_MAX_CHARS} characters of a-z, 0-9 or -); pass --name"
                    )));
                }
                base.to_owned()
            }
        };

        let mut entries = self.read()?;
        if let Some(existing) = entries.iter().find(|entry| entry.path == canonical) {
            return Err(Error::Failed(format!(
                "`{}` is already registered as `{}`",
                canonical.display(),
                existing.name
            )));
        }
        if entries.iter().any(|entry| entry.name == name) {
            return Err(Error::Failed(format!(
                "a repository named `{name}` is already registered"
            )));
        }
        let entry = RepoEntry {
            name,
            path: canonical,
        };
        entries.push(entry.clone());
        self.write(&entries)?;
        Ok(entry)
    }

    /// Removes the repository named `name` and returns it. An unknown name is refused.
    pub fn remove(&self, name: &str) -> Result<RepoEntry, Error> {
        let mut entries = self.read()?;
        let index = entries
            .iter()
            .position(|entry| entry.name == name)
            .ok_or_else(|| Error::Failed(format!("no repository named `{name}` is registered")))?;
        let removed = entries.remove(index);
        self.write(&entries)?;
        Ok(removed)
    }

    /// Opens the arc [`Store`] of the repository named `name`, the way the arc CLI opens the
    /// current repository's.
    pub fn store_for(&self, name: &str) -> Result<Store, Error> {
        let entry = self
            .repos()?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| Error::Failed(format!("no repository named `{name}` is registered")))?;
        Store::open(&entry.path)
    }

    /// Reads and validates the file. A missing file is empty.
    fn read(&self) -> Result<Vec<RepoEntry>, Error> {
        let path = self.file();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(Error::Failed(format!(
                    "cannot read {}: {error}",
                    path.display()
                )));
            }
        };
        let file: ReposFile = toml::from_str(&text).map_err(|error| {
            let location = error
                .span()
                .map(|span| format!(":{}", line_of(&text, span.start)))
                .unwrap_or_default();
            Error::Failed(format!("{}{location}: {error}", path.display()))
        })?;

        let mut entries: Vec<RepoEntry> = Vec::with_capacity(file.repos.len());
        for repo in &file.repos {
            let raw_repo = repo.get_ref();
            let name = raw_repo.name.get_ref().clone();
            let name_line = line_of(&text, raw_repo.name.span().start);
            if !valid_name(&name) {
                return Err(Error::Failed(format!(
                    "{}:{name_line}: repository name `{name}` is not 1 to {NAME_MAX_CHARS} \
                     characters of a-z, 0-9 or -",
                    path.display()
                )));
            }
            let raw = raw_repo.path.get_ref();
            let path_line = line_of(&text, raw_repo.path.span().start);
            if !raw.is_absolute() {
                return Err(Error::Failed(format!(
                    "{}:{path_line}: repository path `{}` is not absolute",
                    path.display(),
                    raw.display()
                )));
            }
            let canonical = fs::canonicalize(raw).map_err(|error| {
                Error::Failed(format!(
                    "{}:{path_line}: repository path `{}` cannot be resolved: {error}",
                    path.display(),
                    raw.display()
                ))
            })?;
            if !canonical.join(ARC_DIR).is_dir() {
                return Err(Error::Failed(format!(
                    "{}:{path_line}: `{}` is not an arc repository (no {ARC_DIR}/)",
                    path.display(),
                    canonical.display()
                )));
            }
            if entries.iter().any(|entry| entry.name == name) {
                return Err(Error::Failed(format!(
                    "{}:{name_line}: repository name `{name}` is registered twice",
                    path.display()
                )));
            }
            if entries.iter().any(|entry| entry.path == canonical) {
                return Err(Error::Failed(format!(
                    "{}:{path_line}: repository path `{}` is registered twice",
                    path.display(),
                    canonical.display()
                )));
            }
            entries.push(RepoEntry {
                name,
                path: canonical,
            });
        }
        Ok(entries)
    }

    /// Writes the whole file through a temporary file and an atomic rename.
    fn write(&self, entries: &[RepoEntry]) -> Result<(), Error> {
        radian_host::ensure_private_dir(&self.dir).map_err(|error| {
            Error::Failed(format!("cannot create {}: {error}", self.dir.display()))
        })?;
        let text = if entries.is_empty() {
            String::new()
        } else {
            toml::to_string(&Document { repos: entries }).map_err(|error| {
                Error::Failed(format!("cannot render the repository registry: {error}"))
            })?
        };
        write_atomic(&self.file(), text.as_bytes())
    }
}

/// The 1-based line of a byte offset in `text`. The span always falls on a character
/// boundary of the parsed document, but counting bytes keeps a defensive offset safe too.
fn line_of(text: &str, offset: usize) -> usize {
    let end = offset.min(text.len());
    text.as_bytes()[..end]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

/// Writes `bytes` to `path` the way every registry write happens: a temporary file in the
/// same directory (0600), fsync, rename over the target, fsync the parent directory.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| Error::Failed(format!("{} has no directory", path.display())))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| REGISTRY_FILE.to_owned());
    let sequence = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = parent.join(format!(".{name}.tmp.{}.{sequence}", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        // `mode` is subject to the umask; set the mode explicitly so the registry is 0600
        // even under a permissive umask.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    };
    if let Err(error) = write() {
        let _ = fs::remove_file(&temp);
        return Err(Error::Failed(format!(
            "cannot write {}: {error}",
            path.display()
        )));
    }
    Ok(())
}
