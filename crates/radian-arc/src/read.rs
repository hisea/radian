//! The read-only views the arc TUI renders: the remote main, a task's attempts and their
//! verdicts, and a task's own change.
//!
//! Nothing here writes, takes a lock or fetches. A caller runs these on a worker thread: the
//! TUI must never wait on git for a frame, and a diff can be large. The functions return one
//! value each and name the file or commit they could not read.

use std::path::{Path, PathBuf};

use super::gate::{Stage, Verdict};
use super::store::attempt_ops;
use super::store::model::{TaskFile, TaskState};
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
    /// The file has content the running attempt has not committed yet (a modified or
    /// untracked file in the worker's worktree). Always `false` for a committed range.
    pub uncommitted: bool,
}

/// Where a task's diff comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffSource {
    /// A committed range (`base_sha..head_sha`, or a merged task's candidate).
    #[default]
    Range,
    /// The running attempt's worktree against its `base_sha`, uncommitted files included
    /// (ADR 0048 decision 13).
    Worktree,
}

/// A task's change: every changed file with its counts and its unified diff.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskDiff {
    pub files: Vec<DiffFile>,
    /// The range the files came from.
    pub source: DiffSource,
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

/// How one stage of the merge page stands (ADR 0048 §6, §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageMark {
    /// A verdict for the task's commit passed.
    Passed,
    /// A verdict for the task's commit failed.
    Failed,
    /// A gate is running and this is the stage it is on.
    Running,
    /// The stage is in the profile and has no verdict yet.
    NotRun,
    /// The profile does not list the stage.
    NotInProfile,
}

/// One command a `commands` or `integration` stage ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandView {
    /// The command line as the profile wrote it.
    pub line: String,
    /// Its wall-clock duration.
    pub duration_ms: u64,
    /// Its log file, when the record names one that exists.
    pub log: Option<PathBuf>,
    /// Whether it exited zero (a signal-killed command did not).
    pub passed: bool,
}

/// One row of the merge page: one stage of the task's profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageView {
    /// `commands`, `mutation`, `review`, `ci`, `human` or `integration`.
    pub stage: &'static str,
    pub mark: StageMark,
    /// Why the stage failed.
    pub reason: Option<String>,
    /// The short commit the stage ran on (the task's commit, never the candidate).
    pub on: Option<String>,
    /// The short candidate commit, when the stage ran on a merge of the task onto main.
    pub candidate: Option<String>,
    /// The short main the candidate was built on, when there is one.
    pub main: Option<String>,
    /// The commands the stage ran, in order.
    pub commands: Vec<CommandView>,
    /// The review stage's sessions.
    pub sessions: usize,
    /// The stage's wall-clock duration.
    pub duration_ms: Option<u64>,
    /// The `human` stage's approver: `auto`, or `owner`.
    pub approver: Option<String>,
}

/// The `review` stage's findings for the task's commit (ADR 0048 decision 10).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewSummaryView {
    pub summary: String,
    pub blocking: Vec<String>,
    pub suggestions: Vec<String>,
    pub verified: Vec<String>,
}

/// One commit of the task's own change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitView {
    /// The short sha.
    pub sha: String,
    pub subject: String,
}

/// Everything the merge page draws for one task: the stage rows, the review's findings and
/// the task's commits. Read-only, like the rest of this module; git runs for the commits only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeView {
    pub task: String,
    pub title: String,
    /// The attempt the rows belong to.
    pub attempt: u32,
    /// The short `verified_sha`.
    pub verified: Option<String>,
    /// The short `head_sha`.
    pub head: Option<String>,
    /// `head_sha` is no longer `verified_sha`: every mark is weak and the page says so.
    pub moved: bool,
    /// One row per stage, in the fixed order.
    pub stages: Vec<StageView>,
    /// Whether the profile or the task's `extra_stages` list the `review` stage.
    pub review_stage: bool,
    /// The review verdict's findings, when the stage ran and reported.
    pub review: Option<ReviewSummaryView>,
    /// The task's own commits, newest first.
    pub commits: Vec<CommitView>,
    /// Why the commit list could not be read.
    pub commits_error: Option<String>,
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
/// - A **running** task (`doing.running`): the worker's worktree against the attempt's
///   `base_sha`, uncommitted and untracked files included, so the change is visible while the
///   attempt runs (ADR 0048 decision 13). The attempt's base is the one the wrapper recorded in
///   `attempts/<task>/base_sha`, else the task's `base_sha`; without a worktree or a base the
///   committed range below is used.
/// - A **merged** task: its passing integration verdict's candidate (ADR 0048 decision 10).
///   A merge candidate's first-parent diff (`<candidate>^1..<candidate>`) is the change the
///   merge put on main, not what main has gained since. When the candidate is the task head
///   itself (main had not moved, so main was fast-forwarded to it), that range would be only
///   the head's last commit: the task's own change is `base_sha..head_sha` instead, exactly
///   the range the review prompt uses.
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
    if item.state == TaskState::Doing
        && item.phase == "running"
        && let Some(diff) = live_worktree_diff(store, &item)?
    {
        return Ok(diff);
    }
    let range = match item.state {
        TaskState::Done => match merged_candidate(store, task)? {
            Some(verdict) => merged_range(&verdict).or_else(|| {
                attempt_range(store, &item.id, attempt, &item.base_sha, &item.head_sha)
            }),
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
    diff_range(store.repo(), &base, &head).map(|mut diff| {
        diff.source = DiffSource::Range;
        diff
    })
}

/// The running attempt's change: the worker's worktree against the attempt's `base_sha`, with
/// modified and untracked files included. `None` when the attempt has no worktree yet or no
/// base to compare with.
fn live_worktree_diff(store: &Store, task: &TaskFile) -> Result<Option<TaskDiff>, Error> {
    let worktree = store.root().join("worktrees").join(&task.id);
    if !worktree.join(".git").exists() {
        return Ok(None);
    }
    let recorded = std::fs::read_to_string(
        store
            .root()
            .join("attempts")
            .join(&task.id)
            .join("base_sha"),
    )
    .ok()
    .map(|text| text.trim().to_owned())
    .filter(|sha| !sha.is_empty());
    let Some(base) = recorded.or_else(|| task.base_sha.clone().filter(|sha| !sha.is_empty()))
    else {
        return Ok(None);
    };
    let Some(numstat) = git_raw(&worktree, &["diff", "--numstat", "-z", &base]) else {
        // A base that is not a commit (a worktree made outside the wrapper): no diff rather
        // than a screen full of an error.
        return Ok(None);
    };
    // The paths with worktree changes (modified, staged or untracked), so a file that only
    // exists in the working tree is marked. `--porcelain` keeps a leading space for an
    // unstaged change, so its output is read raw.
    let dirty = git_raw(
        &worktree,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
    )
    .map(|text| porcelain_paths(&text))
    .unwrap_or_default();
    let mut files = numstat_entries(&numstat)
        .into_iter()
        .map(|(path, added, removed)| DiffFile {
            uncommitted: dirty.iter().any(|dirty| dirty == &path),
            text: git_raw(
                &worktree,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "-M",
                    "--unified=3",
                    &base,
                    "--",
                    &path,
                ],
            )
            .unwrap_or_default(),
            path,
            added,
            removed,
        })
        .collect::<Vec<_>>();
    // Untracked files are not in `git diff <base>`: they are new content of the attempt.
    let untracked = git_raw(
        &worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .unwrap_or_default();
    for path in untracked.split('\0').filter(|path| !path.is_empty()) {
        if files.iter().any(|file| file.path == path) {
            continue;
        }
        let text = no_index_diff(&worktree, path);
        let (added, removed) = count_diff_lines(&text);
        files.push(DiffFile {
            path: path.to_owned(),
            added,
            removed,
            text,
            uncommitted: true,
        });
    }
    Ok(Some(TaskDiff {
        files,
        source: DiffSource::Worktree,
    }))
}

/// The paths `git status --porcelain -z` names, in either field of a rename or copy.
fn porcelain_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut records = text.split('\0').filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 4 {
            continue;
        }
        let status = &record[..2];
        paths.push(record[3..].to_owned());
        if (status.starts_with('R') || status.starts_with('C'))
            && let Some(original) = records.next()
        {
            paths.push(original.to_owned());
        }
    }
    paths
}

/// A `git diff --no-index` of a new file against `/dev/null`: git exits `1` for a difference,
/// so the text is taken whatever the status is.
fn no_index_diff(worktree: &Path, path: &str) -> String {
    crate::git::run(
        worktree,
        &[
            "diff",
            "--no-index",
            "--no-color",
            "--no-ext-diff",
            "--unified=3",
            "--",
            "/dev/null",
            path,
        ],
    )
    .map(|output| output.stdout)
    .unwrap_or_default()
}

/// The added and removed lines of a unified diff, `+++`/`---` headers left out.
fn count_diff_lines(text: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in text.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
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

/// The newest passing `integration` verdict, across every attempt of the task. A merged
/// task's merge commit is exactly its `candidate_sha` (ADR 0048 decision 10).
fn merged_candidate(store: &Store, task: &str) -> Result<Option<Verdict>, Error> {
    let root = store.root().join("runs").join(task).join("attempts");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(_) => return Ok(None),
    };
    let mut best: Option<(String, Verdict)> = None;
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
                best = Some((ended, verdict));
            }
        }
    }
    Ok(best.map(|(_, verdict)| verdict))
}

/// The range of a merged task's own change, from its passing integration verdict.
///
/// A merge candidate's first-parent diff is the task's change against the main it was built
/// on. A candidate that is the task head was a fast-forward of main: the branch may hold
/// several commits, and only `base_sha..head_sha` covers all of them.
fn merged_range(verdict: &Verdict) -> Option<(String, String)> {
    let candidate = verdict
        .candidate_sha
        .clone()
        .filter(|sha| !sha.is_empty())?;
    if candidate == verdict.sha
        && let Some(base) = verdict.base_sha.clone().filter(|sha| !sha.is_empty())
    {
        return Some((base, candidate));
    }
    Some((format!("{candidate}^1"), candidate))
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
    for (path, added, removed) in numstat_entries(&numstat) {
        let text = git(
            repo,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "-M",
                "--unified=3",
                &range,
                "--",
                &path,
            ],
        )
        .unwrap_or_default();
        files.push(DiffFile {
            path,
            added,
            removed,
            text,
            uncommitted: false,
        });
    }
    Ok(TaskDiff {
        files,
        source: DiffSource::Range,
    })
}

/// The `--numstat -z` records as `(path, added, removed)`. A rename's new path follows its old
/// one as a bare field. A binary file's `-` counts read as `0`.
fn numstat_entries(text: &str) -> Vec<(String, usize, usize)> {
    let mut files: Vec<(String, usize, usize)> = Vec::new();
    let mut rename_from: Option<usize> = None;
    for record in text.split('\0').filter(|record| !record.is_empty()) {
        match record.split_once('\t') {
            Some((added, rest)) => {
                let Some((removed, path)) = rest.split_once('\t') else {
                    continue;
                };
                let count = |text: &str| text.parse::<usize>().unwrap_or(0);
                files.push((path.to_owned(), count(added), count(removed)));
                rename_from = Some(files.len() - 1);
            }
            // A rename's new path follows its old one as a bare field.
            None => {
                if let Some(index) = rename_from {
                    files[index].0 = record.to_owned();
                }
            }
        }
    }
    files
}

fn git(repo: &Path, args: &[&str]) -> Result<String, Error> {
    crate::git::ok(repo, args).map_err(Error::Failed)
}

/// A git command whose output is read as it is: a `-z` listing or `--porcelain` line starts
/// with a space that trimming would eat. `None` when the command fails.
fn git_raw(dir: &Path, args: &[&str]) -> Option<String> {
    let output = crate::git::run(dir, args).ok()?;
    output.success().then_some(output.stdout)
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

// ---- the merge page ------------------------------------------------------------------

/// The stages in the order the merge page draws them (ADR 0048 decision 10).
const MERGE_STAGES: [Stage; 6] = [
    Stage::Commands,
    Stage::Mutation,
    Stage::Review,
    Stage::Ci,
    Stage::Human,
    Stage::Integration,
];

/// Everything the merge page draws for `task` (ADR 0048 §6 and §10).
///
/// The rows are the profile's stages and the task's `extra_stages` in the fixed order, plus
/// `integration`, which always runs at merge. A stage neither lists is `NotInProfile`; one
/// that lists it with no verdict for the task's commit is `NotRun`, except the stage a
/// running gate is on, which is `Running` (`gate_running` is the caller's lock probe);
/// `human` never runs in a gate, so it stays `NotRun` until the owner approves. `head_sha` no
/// longer `verified_sha` sets [`MergeView::moved`]: a verdict counts only for its sha, so
/// every mark is weak, no row keeps its details and the review's findings are not shown.
///
/// The review's findings are the newest `review` verdict for the task's commit. The commits
/// are the task's own change: merged, `base_sha..head_sha`; otherwise the merge base of the
/// remote main and the head, falling back to `base_sha`. Git runs for the commits only.
///
/// # Errors
///
/// The task cannot be read, or a verdict file exists but cannot be read or decoded.
pub fn merge_view(store: &Store, task: &str, gate_running: bool) -> Result<MergeView, Error> {
    let item = store.load_task(task)?.item;
    let head = item.head_sha.clone().filter(|sha| !sha.is_empty());
    let verified = item.verified_sha.clone().filter(|sha| !sha.is_empty());
    let moved = match (&head, &verified) {
        (Some(head), Some(verified)) => head != verified,
        _ => false,
    };
    let target = verified.clone().or_else(|| head.clone());
    let verdicts = all_verdicts(store, task)?;
    let label = format!("{}-{}", item.id, item.attempt);

    let config = store.config()?;
    let profile = config.gate.selected_profile().cloned();
    // The effective stages are the profile's plus the task's `extra_stages` (every name was
    // checked when the task file was read).
    let extra: Vec<Stage> = item
        .extra_stages
        .iter()
        .filter_map(|name| {
            crate::gate::LISTED_STAGES
                .iter()
                .copied()
                .find(|stage| stage.as_str() == name)
        })
        .collect();
    let in_profile = |stage: Stage| {
        stage == Stage::Integration
            || extra.contains(&stage)
            || profile
                .as_ref()
                .is_some_and(|profile| profile.stages.iter().any(|spec| spec.stage == stage))
    };
    let review_stage = in_profile(Stage::Review);

    let mut stages = Vec::new();
    let mut stopped = false;
    let mut running_marked = false;
    for stage in MERGE_STAGES {
        if !in_profile(stage) {
            stages.push(StageView {
                stage: stage.as_str(),
                mark: StageMark::NotInProfile,
                reason: None,
                on: None,
                candidate: None,
                main: None,
                commands: Vec::new(),
                sessions: 0,
                duration_ms: None,
                approver: None,
            });
            continue;
        }
        let verdict = stage_verdict(&verdicts, &label, stage, target.as_deref());
        let mark = match &verdict {
            Some(_) if moved => {
                stopped = true;
                StageMark::NotRun
            }
            Some(verdict) => {
                if !verdict.is_pass() {
                    stopped = true;
                }
                if verdict.is_pass() {
                    StageMark::Passed
                } else {
                    StageMark::Failed
                }
            }
            None if !moved
                && !stopped
                && gate_running
                && !running_marked
                && stage != Stage::Human =>
            {
                running_marked = true;
                stopped = true;
                StageMark::Running
            }
            None => {
                stopped = true;
                StageMark::NotRun
            }
        };
        // A verdict counts only for its sha: after a move no row keeps its details.
        let verdict = if moved { None } else { verdict };
        stages.push(stage_view(stage, mark, verdict));
    }

    // The review's findings are part of the merge page only when the profile has the stage,
    // and only for the verified commit: a moved branch's review is not this commit's.
    let review = if review_stage && !moved {
        verdicts
            .iter()
            .filter(|(_, verdict)| verdict.stage == Stage::Review)
            .filter(|(_, verdict)| match &target {
                Some(sha) => &verdict.sha == sha,
                None => true,
            })
            .max_by(|(_, one), (_, other)| one.ended_at.cmp(&other.ended_at))
            .and_then(|(_, verdict)| verdict.review.as_ref())
            .map(|report| ReviewSummaryView {
                summary: report.summary.clone(),
                blocking: report.blocking.iter().map(ToString::to_string).collect(),
                suggestions: report.suggestions.iter().map(ToString::to_string).collect(),
                verified: report.verified.clone(),
            })
    } else {
        None
    };

    let (commits, commits_error) = match task_commits(store, &item) {
        Ok(commits) => (commits, None),
        Err(error) => (Vec::new(), Some(error)),
    };

    Ok(MergeView {
        task: item.id.clone(),
        title: item.title.clone(),
        attempt: item.attempt,
        verified: verified.as_deref().map(short),
        head: head.as_deref().map(short),
        moved,
        stages,
        review_stage,
        review,
        commits,
        commits_error,
    })
}

/// One stage row from its verdict (already filtered to the task's commit and, after a move,
/// dropped so no detail survives).
fn stage_view(stage: Stage, mark: StageMark, verdict: Option<&Verdict>) -> StageView {
    let Some(verdict) = verdict else {
        return StageView {
            stage: stage.as_str(),
            mark,
            reason: None,
            on: None,
            candidate: None,
            main: None,
            commands: Vec::new(),
            sessions: 0,
            duration_ms: None,
            approver: None,
        };
    };
    let on = (stage != Stage::Human).then(|| short(&verdict.sha));
    let candidate = verdict
        .candidate_sha
        .as_deref()
        .filter(|candidate| !candidate.is_empty() && *candidate != verdict.sha)
        .map(short);
    let main = verdict
        .main_sha
        .as_deref()
        .or(verdict.base_sha.as_deref())
        .filter(|sha| !sha.is_empty())
        .map(short);
    let commands = verdict
        .commands
        .iter()
        .map(|record| CommandView {
            line: record.line.clone(),
            duration_ms: record.duration_ms,
            log: Some(PathBuf::from(&record.log)).filter(|path| path.is_file()),
            passed: record.exit_code == Some(0),
        })
        .collect();
    let sessions = verdict.review_session_ids.len();
    let approver = (stage == Stage::Human).then(|| {
        verdict
            .approver
            .clone()
            .unwrap_or_else(|| "owner".to_owned())
    });
    StageView {
        stage: stage.as_str(),
        mark,
        reason: verdict.reason.clone(),
        on,
        candidate,
        main,
        commands,
        sessions,
        duration_ms: (stage != Stage::Human)
            .then(|| duration_ms(&verdict.started_at, &verdict.ended_at))
            .flatten(),
        approver,
    }
}

/// The verdict of `stage` for the task's commit in `label` (the current attempt), or the
/// newest integration verdict across every attempt (integration labels grow `.iK`).
fn stage_verdict<'a>(
    verdicts: &'a [(String, Verdict)],
    label: &str,
    stage: Stage,
    target: Option<&str>,
) -> Option<&'a Verdict> {
    let matches = |(verdict_label, verdict): &(String, Verdict)| {
        verdict.stage == stage
            && match stage {
                Stage::Integration => true,
                _ => verdict_label == label,
            }
            && target.is_none_or(|sha| verdict.sha == sha)
    };
    verdicts
        .iter()
        .filter(|entry| matches(entry))
        .max_by(|(_, one), (_, other)| one.ended_at.cmp(&other.ended_at))
        .map(|(_, verdict)| verdict)
}

/// Every verdict file under `runs/<task>/attempts/*/verdicts/`, with the attempt label its
/// directory names.
fn all_verdicts(store: &Store, task: &str) -> Result<Vec<(String, Verdict)>, Error> {
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
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let label = entry.file_name().to_string_lossy().into_owned();
        let dir = entry.path().join("verdicts");
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            if file.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let verdict = Verdict::read(&file.path())
                .map_err(|error| Error::Failed(format!("cannot read a verdict: {error}")))?;
            out.push((label.clone(), verdict));
        }
    }
    Ok(out)
}

/// The task's own commits, newest first: `base_sha..head_sha` for a merged task; otherwise
/// the merge base of the remote main and the head, falling back to `base_sha`.
fn task_commits(store: &Store, task: &TaskFile) -> Result<Vec<CommitView>, String> {
    let Some(head) = task.head_sha.clone().filter(|sha| !sha.is_empty()) else {
        return Ok(Vec::new());
    };
    let base = if task.state == TaskState::Done {
        task.base_sha.clone()
    } else {
        remote_main_base(store, &head).or_else(|| task.base_sha.clone())
    };
    let range = match base.filter(|sha| !sha.is_empty() && *sha != head) {
        Some(base) => format!("{base}..{head}"),
        None => head.clone(),
    };
    let text = crate::git::ok(
        store.repo(),
        &["log", "--format=%H%x1f%s", "--max-count=100", &range],
    )?;
    Ok(text
        .lines()
        .filter_map(|line| line.split_once('\u{1f}'))
        .map(|(sha, subject)| CommitView {
            sha: short(sha),
            subject: subject.to_owned(),
        })
        .collect())
}

/// The merge base of the configured remote's main and `head`, when both exist.
fn remote_main_base(store: &Store, head: &str) -> Option<String> {
    let (remote, main_branch) = store.repository_names().ok()?;
    let reference = format!("refs/remotes/{remote}/{main_branch}");
    crate::git::ok(store.repo(), &["merge-base", &reference, head])
        .ok()
        .map(|sha| sha.trim().to_owned())
        .filter(|sha| !sha.is_empty())
}

/// The 8-character sha the pages print.
fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// The wall-clock duration between two RFC 3339 stamps, in milliseconds.
fn duration_ms(started_at: &str, ended_at: &str) -> Option<u64> {
    let start = chrono::DateTime::parse_from_rfc3339(started_at).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(ended_at).ok()?;
    let millis = end.signed_duration_since(start).num_milliseconds();
    u64::try_from(millis).ok()
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
