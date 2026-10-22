//! The separate checkout for one gate stage (ADR 0048 decision 10).
//!
//! `<repository>/.radian/arc/gates/<stage>/` is a detached linked worktree of the main
//! checkout. Each stage keeps its own `target/` between runs; trees of different stages
//! never share build state.

use std::path::{Path, PathBuf};

use super::config::Stage;
use super::error::GateError;
use super::git;

/// The `gates/` directory under the repository's `arc` directory.
#[must_use]
pub fn gates_dir(repository: &Path) -> PathBuf {
    repository.join(".radian").join("arc").join("gates")
}

/// One stage's checkout.
#[derive(Debug, Clone)]
pub struct GateCheckout {
    repository: PathBuf,
    stage: Stage,
    remote: String,
}

impl GateCheckout {
    /// The checkout of `stage` in `repository`, fetching from `remote`.
    ///
    /// `remote` is the name of the repository's remote (the `[repository] remote` of
    /// `config.toml`); the gate never assumes a name.
    #[must_use]
    pub fn new(repository: impl Into<PathBuf>, stage: Stage, remote: impl Into<String>) -> Self {
        Self {
            repository: repository.into(),
            stage,
            remote: remote.into(),
        }
    }

    /// The checkout path, `<repository>/.radian/arc/gates/<stage>/`.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        gates_dir(&self.repository).join(self.stage.as_str())
    }

    /// The stage this checkout belongs to.
    #[must_use]
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// The repository root this checkout belongs to.
    #[must_use]
    pub fn repository(&self) -> &Path {
        &self.repository
    }

    /// The remote the checkout fetches from.
    #[must_use]
    pub fn remote(&self) -> &str {
        &self.remote
    }

    /// Prepares the checkout for `sha` and returns the full commit id.
    ///
    /// It fetches `remote` first, then creates the linked worktree when it is missing and
    /// otherwise detaches it at `sha`, resets it hard and cleans untracked and ignored
    /// files. The clean keeps `target/`, so a stage reuses its build directory between runs.
    /// A leftover empty directory at the checkout path (what a failed `worktree add` can
    /// leave inside `gates/`) is removed and the worktree is added again.
    ///
    /// # Errors
    ///
    /// The fetch failed, `sha` is not a commit after the fetch, the checkout path is
    /// occupied by something that is not a linked worktree and is not an empty leftover
    /// directory, or a git command failed.
    pub fn prepare(&self, sha: &str) -> Result<String, GateError> {
        git::fetch_remote(&self.repository, &self.remote)?;
        let commit = git::resolve_commit(&self.repository, sha)?;
        let path = self.path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| GateError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let path_arg = self.path_string()?;

        let mut create = false;
        if path.exists() {
            if path.join(".git").exists() {
                git::ok(&path, &["checkout", "--detach", "--force", commit.as_str()])?;
            } else if self.is_empty_leftover(&path) {
                // Only an empty directory directly inside `gates/` is ours to clear: it is
                // what a failed `worktree add` leaves, and it holds nothing of the caller's.
                std::fs::remove_dir(&path).map_err(|source| GateError::Io {
                    path: path.clone(),
                    source,
                })?;
                create = true;
            } else {
                return Err(GateError::CheckoutOccupied(path));
            }
        } else {
            create = true;
        }

        if create {
            // A directory that is gone but still registered would make `worktree add` fail.
            git::ok(&self.repository, &["worktree", "prune"])?;
            git::ok(
                &self.repository,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    path_arg.as_str(),
                    commit.as_str(),
                ],
            )?;
        }

        git::ok(&path, &["reset", "--hard", commit.as_str()])?;
        git::ok(&path, &["clean", "-ffdx", "-e", "target"])?;
        Ok(commit)
    }

    /// Whether `path` is an empty directory directly inside `gates/`.
    fn is_empty_leftover(&self, path: &Path) -> bool {
        let gates = gates_dir(&self.repository);
        path.is_dir()
            && path.parent() == Some(gates.as_path())
            && std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
    }

    fn path_string(&self) -> Result<String, GateError> {
        let path = self.path();
        path.to_str()
            .map(str::to_string)
            .ok_or(GateError::NonUtf8Path(path))
    }
}
