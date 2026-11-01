//! The hub's status: what it schedules, what it only reads, and the machine-wide count of
//! tasks in `doing` (ADR 0048 decision 16).
//!
//! The HTTP task serves this structure; `radian hub` prints it as one line per repository when
//! it starts and on every change. The rendering is here, in one place, so both see the same
//! text.

use std::path::PathBuf;

/// What one registered repository's loop is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoState {
    /// The loop has not tried the lock yet.
    Starting,
    /// The hub holds the repository's `runner.lock` and schedules it.
    Scheduled,
    /// Another process holds the lock. The value is the pid it wrote, when it wrote one.
    ReadOnly(Option<u32>),
    /// The loop stopped after [`crate::run::REPEATED_ERROR_LIMIT`] identical pass errors.
    Stopped,
}

impl RepoState {
    /// The word the status line uses.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            RepoState::Starting => "starting".to_owned(),
            RepoState::Scheduled => "scheduled".to_owned(),
            RepoState::ReadOnly(Some(pid)) => format!("read-only (pid {pid})"),
            RepoState::ReadOnly(None) => "read-only (holder unknown)".to_owned(),
            RepoState::Stopped => "stopped (repeated error)".to_owned(),
        }
    }
}

/// One registered repository and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoStatus {
    pub name: String,
    pub path: PathBuf,
    pub state: RepoState,
}

/// The hub's whole status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HubStatus {
    /// The machine-wide cap (`[hub] concurrency`).
    pub cap: u64,
    /// Tasks in `doing` across every registered repository, at the moment it was read.
    pub doing: u64,
    /// Every registered repository, in registry order.
    pub repos: Vec<RepoStatus>,
}

impl HubStatus {
    /// The text `radian hub` prints: one count line, then one line per repository.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = format!("hub: {} in doing, cap {}\n", self.doing, self.cap);
        for repo in &self.repos {
            text.push_str(&format!(
                "{}  {}  {}\n",
                repo.name,
                repo.path.display(),
                repo.state.describe()
            ));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_names_a_read_only_holder_and_its_pid() {
        let status = HubStatus {
            cap: 4,
            doing: 1,
            repos: vec![
                RepoStatus {
                    name: "one".to_owned(),
                    path: PathBuf::from("/tmp/one"),
                    state: RepoState::Scheduled,
                },
                RepoStatus {
                    name: "two".to_owned(),
                    path: PathBuf::from("/tmp/two"),
                    state: RepoState::ReadOnly(Some(4242)),
                },
                RepoStatus {
                    name: "three".to_owned(),
                    path: PathBuf::from("/tmp/three"),
                    state: RepoState::ReadOnly(None),
                },
            ],
        };
        let text = status.render();
        assert!(text.contains("hub: 1 in doing, cap 4"), "{text}");
        assert!(text.contains("one  /tmp/one  scheduled"), "{text}");
        assert!(
            text.contains("two  /tmp/two  read-only (pid 4242)"),
            "{text}"
        );
        assert!(
            text.contains("three  /tmp/three  read-only (holder unknown)"),
            "{text}"
        );
    }
}
