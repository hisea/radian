//! The read-only views the arc TUI renders: the remote main, a task's attempts and their
//! verdicts, and a task's own change.
//!
//! Nothing here writes, takes a lock or fetches. A caller runs these on a worker thread: the
//! TUI must never wait on git for a frame, and a diff can be large. The functions return one
//! value each and name the file or commit they could not read.

use std::path::Path;

use super::gate::{Stage, Verdict};
use super::store::attempt_ops;
use super::store::model::TaskState;
use super::store::{Error, Store};

/// One file of a task's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    /// The repository-relative path, as git prints it after `--numstat`.
    pub path: String,
    /// Added lines (`0` for a binary file).
    pub added: usize,
    /// Removed lines (`0` for a binary file).
    pub removed: usize,
    /// The file's unified diff (`git diff --unified=3 <range> -- <path>`), without the
    /// `diff --git` preamble.
    pub text: String,
}

/// A task's change: every changed file with its counts and its unified diff.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskDiff {
    pub files: Vec<DiffFile>,
}

/// One stage's verdict in one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptVerdict {
    /// `commands`, `mutation`, `review`, `ci`, `human` or `integration`.
    pub stage: &'static str,
    pub passed: bool,
    /// The failure reason: `command_failed`, `timeout`, `conflict`, an owner's note.
    pub reason: Option<String>,
}

/// One attempt and the verdicts recorded for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptView {
    /// The attempt number: the `<n>` in `<task>-<n>`.
    pub attempt: u32,
    /// Every verdict file of the attempt, in stage order.
    pub verdicts: Vec<AttemptVerdict>,
}

/// The stages in the order a row names them: the profile's list, then `integration`, which
/// always runs at merge.
const STAGE_ORDER: [Stage; 6] = [
    Stage::Commands,
    Stage::Mutation,
    Stage::Review,
    Stage::Ci,
    Stage::Human,
    Stage::Integration,
];

/// The short (8-character) sha of the configured remote's main branch. `None` when the
/// remote-tracking ref does not exist; the header then shows `—`.
///
/// No fetch: `origin/main` is what the last fetch left, and a read view never reaches the
/// network.
///
/// # Errors
///
/// The configuration cannot be read.
pub fn remote_main_short(store: &Store) -> Result<Option<String>, Error> {
    let (remote, main_branch) = store.repository_names()?;
    let reference = format!("refs/remotes/{remote}/{main_branch}");
    let output = crate::git::run(
        store.repo(),
        &["rev-parse", "--verify", "--quiet", &reference],
    )
    .map_err(|error| Error::Failed(error.to_string()))?;
    if !output.success() {
        return Ok(None);
    }
    let sha = output.stdout.trim();
    Ok((!sha.is_empty()).then(|| sha.chars().take(8).collect()))
}

/// Every attempt of `task` that has a run directory, with the verdicts of that attempt.
///
/// The latest attempt is the highest number, not the task's `attempt` field: a rework may be
/// claimed before the task file is read. A reverify's `.iN` integration re-runs are not
/// attempts; only `<task>-<n>` directories count.
///
/// # Errors
///
/// A directory or verdict file that exists but cannot be read or decoded.
pub fn task_attempts(store: &Store, task: &str) -> Result<Vec<AttemptView>, Error> {
    let root = store.root().join("runs").join(task).join("attempts");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(Error::Failed(format!(
                "cannot read {}: {error}",
                root.display()
            )));
        }
    };
    let mut attempts = Vec::new();
    for entry in entries.flatten() {
        let Some(attempt) = attempt_number(task, &entry.file_name().to_string_lossy()) else {
            continue;
        };
        let dir = entry.path().join("verdicts");
        let mut verdicts = Vec::new();
        let files = match std::fs::read_dir(&dir) {
            Ok(files) => files,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                attempts.push(AttemptView { attempt, verdicts });
                continue;
            }
            Err(error) => {
                return Err(Error::Failed(format!(
                    "cannot read {}: {error}",
                    dir.display()
                )));
            }
        };
        for file in files.flatten() {
            if file.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let verdict = Verdict::read(&file.path())
                .map_err(|error| Error::Failed(format!("cannot read a verdict: {error}")))?;
            verdicts.push(AttemptVerdict {
                stage: verdict.stage.as_str(),
                passed: verdict.is_pass(),
                reason: verdict.reason.clone(),
            });
        }
        verdicts.sort_by_key(|verdict| {
            STAGE_ORDER
                .iter()
                .position(|stage| stage.as_str() == verdict.stage)
                .unwrap_or(STAGE_ORDER.len())
        });
        attempts.push(AttemptView { attempt, verdicts });
    }
    attempts.sort_by_key(|attempt| attempt.attempt);
    Ok(attempts)
}

/// The change a task is responsible for.
///
/// - A **merged** task: the first-parent diff of its merge commit, the integration verdict's
///   `candidate_sha` (`<candidate>^1..<candidate>`). When the candidate was a fast-forward,
///   that is the task's own commit against its parent. The task's file list is what the merge
///   put on main, not what main has gained since.
/// - Any other task: `base_sha..head_sha` of the selected attempt, that attempt's own
///   `AttemptFinished` record when it has one, else the task's current fields.
///
/// The diff is empty when the task has no commit yet (no base or head, or both equal).
///
/// # Errors
///
/// The task cannot be read, or git cannot produce the diff.
pub fn task_diff(store: &Store, task: &str, attempt: Option<u32>) -> Result<TaskDiff, Error> {
    let item = store.load_task(task)?.item;
    let range = match item.state {
        TaskState::Done => match merged_candidate(store, task)? {
            Some(candidate) => Some((format!("{candidate}^1"), candidate)),
            None => attempt_range(store, &item.id, attempt, &item.base_sha, &item.head_sha),
        },
        _ => attempt_range(store, &item.id, attempt, &item.base_sha, &item.head_sha),
    };
    let Some((base, head)) = range else {
        return Ok(TaskDiff::default());
    };
    if base == head {
        return Ok(TaskDiff::default());
    }
    diff_range(store.repo(), &base, &head)
}

/// The base and head a diff range uses for one attempt: the attempt's own record when it
/// wrote one, the task's current fields otherwise.
fn attempt_range(
    store: &Store,
    task: &str,
    attempt: Option<u32>,
    base: &Option<String>,
    head: &Option<String>,
) -> Option<(String, String)> {
    let recorded = attempt
        .and_then(|attempt| attempt_ops::read_finish(store, task, attempt))
        .and_then(|finish| Some((finish.base_sha?, finish.head_sha?)));
    let (base, head) = match recorded {
        Some(recorded) => recorded,
        None => match (base.clone(), head.clone()) {
            (Some(base), Some(head)) => (base, head),
            _ => return None,
        },
    };
    let usable = |sha: &str| !sha.is_empty();
    (usable(&base) && usable(&head)).then_some((base, head))
}

/// The newest passing `integration` verdict's `candidate_sha`, across every attempt of the
/// task. A merged task's merge commit is exactly this candidate (ADR 0048 decision 10).
fn merged_candidate(store: &Store, task: &str) -> Result<Option<String>, Error> {
    let root = store.root().join("runs").join(task).join("attempts");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(_) => return Ok(None),
    };
    let mut best: Option<(String, String)> = None;
    for entry in entries.flatten() {
        let verdicts = entry.path().join("verdicts");
        let Ok(files) = std::fs::read_dir(&verdicts) else {
            continue;
        };
        for file in files.flatten() {
            if file.file_name().to_string_lossy() != "integration.json" {
                continue;
            }
            let Ok(verdict) = Verdict::read(&file.path()) else {
                continue;
            };
            if !verdict.is_pass() {
                continue;
            }
            let Some(candidate) = verdict.candidate_sha.clone() else {
                continue;
            };
            if candidate.is_empty() {
                continue;
            }
            let ended = verdict.ended_at.clone();
            if best.as_ref().is_none_or(|(at, _)| *at <= ended) {
                best = Some((ended, candidate));
            }
        }
    }
    Ok(best.map(|(_, candidate)| candidate))
}

/// Runs the two git commands of a diff: `--numstat -z` for the paths and counts, then one
/// `git diff` per file for the text (a path is never split out of a `--unified` header, so a
/// name with a space or a non-ASCII byte needs no quoting rules).
fn diff_range(repo: &Path, base: &str, head: &str) -> Result<TaskDiff, Error> {
    let range = format!("{base}..{head}");
    let numstat = match git(repo, &["diff", "--numstat", "-z", &range]) {
        Ok(text) => text,
        // A base that is not a commit (a task that recorded nothing usable): no diff rather
        // than a screen full of an error.
        Err(_) => return Ok(TaskDiff::default()),
    };
    let mut files = Vec::new();
    let mut rename_from: Option<usize> = None;
    for record in numstat.split('\0').filter(|record| !record.is_empty()) {
        match record.split_once('\t') {
            Some((added, rest)) => {
                let Some((removed, path)) = rest.split_once('\t') else {
                    continue;
                };
                let count = |text: &str| text.parse::<usize>().unwrap_or(0);
                files.push(DiffFile {
                    path: path.to_owned(),
                    added: count(added),
                    removed: count(removed),
                    text: String::new(),
                });
                rename_from = Some(files.len() - 1);
            }
            // A rename's new path follows its old one as a bare field.
            None => {
                if let Some(index) = rename_from {
                    files[index].path = record.to_owned();
                }
            }
        }
    }
    for file in &mut files {
        file.text = git(
            repo,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "-M",
                "--unified=3",
                &range,
                "--",
                &file.path,
            ],
        )
        .unwrap_or_default();
    }
    Ok(TaskDiff { files })
}

fn git(repo: &Path, args: &[&str]) -> Result<String, Error> {
    crate::git::ok(repo, args).map_err(Error::Failed)
}

/// The attempt number in an attempt directory name, `<task>-<n>`; `None` for an integration
/// re-run (`<task>-<n>.iK`) or anything else.
fn attempt_number(task: &str, name: &str) -> Option<u32> {
    let rest = name.strip_prefix(task)?.strip_prefix('-')?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attempt_directory_names_its_number_only_for_a_real_attempt() {
        let task = "TK0001-work";
        assert_eq!(attempt_number(task, "TK0001-work-1"), Some(1));
        assert_eq!(attempt_number(task, "TK0001-work-12"), Some(12));
        assert_eq!(attempt_number(task, "TK0001-work-1.i2"), None);
        assert_eq!(attempt_number(task, "TK0001-work"), None);
        assert_eq!(attempt_number("TK0002-other", "TK0001-work-1"), None);
        assert_eq!(attempt_number(task, "TK0001-work-x"), None);
    }
}
