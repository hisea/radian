//! arc's storage: the files under `<repository>/.radian/arc/`, the locks, the ledger, and
//! the transactions that change them (ADR 0048 decision 5).
//!
//! The rules this module keeps:
//!
//! - arc is the only writer of a plan or a task file, and every change is one transaction
//!   under `state.lock`: take the lock, check the ledger, write a temporary file, fsync,
//!   rename, fsync the directory, update the ledger the same way, release.
//! - the ledger holds, per file, the `state_version` arc last wrote and the SHA-256 of the
//!   bytes it wrote. A file that does not match its ledger entry changed outside arc.
//! - a copy of each file as arc last wrote it is kept next to its ledger entry
//!   (`ledger/plans/<id>.last.md`), so `adopt` can refuse an outside change to a field that
//!   only arc may set.
//!
//! Everything else arc stores (`runs/`, `attempts/`, `gates/`, `worktrees/`) is created by
//! [`Store::init`] and written by the gate (`crate::gate`) and the worker wrapper
//! (`crate::work`).

pub mod attempt_ops;
pub mod chain;
pub mod cli;
pub mod config;
mod frontmatter;
mod git;
pub mod model;
pub mod plan_ops;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use config::STAGE_NAMES;
pub use model::{
    AUTO_LEVELS, DEFAULT_PRIORITY, PLAN_PRIORITIES, PLAN_STATUSES, TASK_PHASES, TASK_STATES,
};

/// The directory arc keeps for one repository, relative to its root.
pub const ARC_DIR: &str = ".radian/arc";
/// The file `init` writes once.
pub const CONFIG_FILE: &str = "config.toml";
/// The repository's standing worker instructions (ADR 0048 decision 7): every brief the
/// wrapper composes ends with this file's content when it exists and is not empty. `init`
/// writes the default once and never overwrites it.
pub const WORKER_BRIEF_FILE: &str = "worker-brief.md";
/// The default `worker-brief.md` `init` writes. It lives here, in one place, and the manual
/// quotes it verbatim (a drift test in `crates/radian-arc/tests/` compares them).
pub const WORKER_BRIEF_DEFAULT: &str = "\
Work only on the current branch.\n\
Commit everything that belongs to the change, and end with `git status --short` printing nothing.\n\
Put scratch files only under `.radian/`.\n\
Never change `.radian/arc/`.\n\
While iterating, run only the tests being changed, and run each named crate in full once at the \
end; never the whole workspace.\n\
In a `MUTATIONS.md`, add a new section at the end of the file, with one blank line before its \
`##` heading.\n\
End with a short report: the commits, the tests you ran, the decisions you made, and any open \
questions.";
/// The short lock every state change takes.
pub const STATE_LOCK: &str = "state.lock";
/// The lock a scheduling loop holds for its lifetime.
pub const RUNNER_LOCK: &str = "runner.lock";
/// How long a state change waits for `state.lock` before it gives up.
pub const LOCK_WAIT: Duration = Duration::from_secs(5);
/// The largest id number: ids are four digits.
pub const MAX_ID_NUMBER: u32 = 9999;

/// arc's exit codes (ADR 0048 decision 1), as one closed list. The manual's table is
/// checked against this.
pub const EXIT_CODES: &[(u8, &str)] = &[
    (0, "success"),
    (1, "an error"),
    (2, "a usage error"),
    (3, "the state does not allow the operation"),
    (4, "a lock is held by another process"),
];

/// The exit codes `radian arc merge` itself returns: success, a git or file error, a
/// refused state (the branch moved, the digest changed, or three integration runs did not
/// settle), and a held `state.lock`. `2` is clap's usage error before the command runs.
/// The manual's merge exit table is checked against this list.
pub const MERGE_EXIT_CODES: &[Exit] = &[Exit::Ok, Exit::Error, Exit::State, Exit::Locked];

/// One exit code, by meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// 0: the operation succeeded.
    Ok,
    /// 1: an error.
    Error,
    /// 2: a usage error.
    Usage,
    /// 3: the state does not allow the operation (including a file changed outside arc).
    State,
    /// 4: a lock is held by another process.
    Locked,
}

impl Exit {
    /// The process exit code.
    pub fn code(self) -> u8 {
        match self {
            Exit::Ok => 0,
            Exit::Error => 1,
            Exit::Usage => 2,
            Exit::State => 3,
            Exit::Locked => 4,
        }
    }
}

/// Why an arc operation did not succeed. Each variant maps to one exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// arc is not set up for this repository.
    NotInitialized,
    /// The file on disk is not the one arc last wrote.
    ChangedOutsideArc(String),
    /// The state does not allow the operation.
    Refused(String),
    /// The argument itself is wrong (a slug, an id, a stage name).
    Usage(String),
    /// Another process holds the lock.
    Locked(&'static str),
    /// Anything else, already a one-line message.
    Failed(String),
}

impl Error {
    /// The exit code this failure reports.
    pub fn exit(&self) -> Exit {
        match self {
            Error::NotInitialized | Error::ChangedOutsideArc(_) | Error::Refused(_) => Exit::State,
            Error::Usage(_) => Exit::Usage,
            Error::Locked(_) => Exit::Locked,
            Error::Failed(_) => Exit::Error,
        }
    }

    /// The one line the user sees (without the `radian arc:` prefix).
    pub fn message(&self) -> String {
        match self {
            Error::NotInitialized => {
                "arc is not set up for this repository; run `radian arc init`".to_owned()
            }
            Error::ChangedOutsideArc(id) => format!(
                "`{id}` changed outside arc; run `radian arc adopt {id}` to accept the change"
            ),
            Error::Refused(message) | Error::Usage(message) | Error::Failed(message) => {
                message.clone()
            }
            Error::Locked(lock) => format!("{lock} is locked by another process"),
        }
    }
}

/// Which of the two stored objects a path or an id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Plan,
    Task,
}

impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::Plan => "plans",
            Kind::Task => "tasks",
        }
    }

    fn ledger_dir(self) -> &'static str {
        match self {
            Kind::Plan => "ledger/plans",
            Kind::Task => "ledger/tasks",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Kind::Plan => "plan",
            Kind::Task => "task",
        }
    }

    /// The prefix a new id of this kind starts with, from `[ids]`.
    fn prefix(self, config: &config::Config) -> &str {
        match self {
            Kind::Plan => &config.ids.plan_prefix,
            Kind::Task => &config.ids.task_prefix,
        }
    }
}

/// One stored file as it is on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded<T> {
    pub item: T,
    /// The bytes that were read (the file is never rewritten from this by a reader).
    pub raw: String,
    /// Whether the bytes differ from what the ledger recorded.
    pub changed_outside_arc: bool,
}

/// A plan with its tasks, in the plan's display order.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanView {
    pub plan: Loaded<model::PlanFile>,
    pub tasks: Vec<Loaded<model::TaskFile>>,
}

impl PlanView {
    /// The plan's counted cost: every task's `cost_usd`, the number the plan budget compares.
    /// Read when the plan is read; it is never stored in the plan file.
    pub fn cost_usd(&self) -> f64 {
        self.tasks
            .iter()
            .map(|task| task.item.cost_usd.unwrap_or(0.0))
            .sum()
    }

    /// The plan's total cost: every task's running total (its `cost_usd` when its file has no
    /// `total_cost_usd`). Read when the plan is read; it is never stored in the plan file.
    pub fn total_cost_usd(&self) -> f64 {
        self.tasks
            .iter()
            .map(|task| task.item.running_total())
            .sum()
    }

    /// Whether the plan is finished (ADR 0048 decision 3): it has at least one task, every
    /// task is `done.merged` or `failed.cancelled`, and at least one is `done.merged`. A plan
    /// with no tasks is never finished. This is the one finished rule: `radian arc list` and
    /// the arc TUI's `done` view share it, and the `✓` plan mark reads it.
    pub fn is_finished(&self) -> bool {
        !self.tasks.is_empty()
            && self
                .tasks
                .iter()
                .all(|task| task.item.is_merged() || task.item.is_cancelled())
            && self
                .tasks
                .iter()
                .any(|task| task.item.state == model::TaskState::Done)
    }

    /// Whether the plan's own tasks say it was cancelled: it has at least one task and every
    /// task is `failed.cancelled`. The plan file's `status` is not rewritten.
    pub fn is_cancelled_by_tasks(&self) -> bool {
        !self.tasks.is_empty() && self.tasks.iter().all(|task| task.item.is_cancelled())
    }

    /// Whether the plan is cancelled: its stored `status` is `cancelled`, or its tasks are all
    /// `failed.cancelled` ([`PlanView::is_cancelled_by_tasks`]). The one cancelled rule `list`,
    /// its `--json` form, the arc TUI's views and the plan mark share.
    pub fn is_cancelled(&self) -> bool {
        self.plan.item.is_cancelled() || self.is_cancelled_by_tasks()
    }

    /// The plan's progress as `(merged, total)`: the tasks that are `done.merged`, and the
    /// tasks that are not `failed.cancelled`. A cancelled task is not work left to do, so it
    /// is not in the total; a plan whose tasks are all cancelled reads `0/0`.
    pub fn progress(&self) -> (usize, usize) {
        let merged = self
            .tasks
            .iter()
            .filter(|task| task.item.is_merged())
            .count();
        let total = self
            .tasks
            .iter()
            .filter(|task| !task.item.is_cancelled())
            .count();
        (merged, total)
    }
}

/// What `show <id>` found. The task variant is the larger one (a task file has the attempt
/// fields); `show` returns one at a time, so the difference costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Plan(PlanView),
    Task(Loaded<model::TaskFile>),
}

/// One requirement check `init` ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    /// The detail on success, or the warning line on failure.
    pub detail: Result<String, String>,
}

impl Check {
    /// Whether the check passed.
    pub fn passed(&self) -> bool {
        self.detail.is_ok()
    }
}

/// What one `init` did.
#[derive(Debug, Clone, PartialEq)]
pub struct InitReport {
    pub repository: PathBuf,
    pub remote: String,
    pub remote_url: String,
    pub main_branch: String,
    pub config_path: PathBuf,
    pub config_created: bool,
    /// The standing worker instructions file `init` writes when it is missing.
    pub worker_brief_path: PathBuf,
    pub worker_brief_created: bool,
    pub exclude_path: PathBuf,
    /// The lines this run appended to the exclude file (empty on a second run).
    pub exclude_added: Vec<String>,
    pub checks: Vec<Check>,
}

/// What one `adopt` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptReport {
    pub id: String,
    /// `"plan"` or `"task"`.
    pub kind: &'static str,
    /// Whether the recorded content changed (`false`: the file already matched the ledger).
    pub changed: bool,
}

/// The repository's arc store. Cheap to clone; every operation reads the files it needs.
#[derive(Debug, Clone)]
pub struct Store {
    repo: PathBuf,
    root: PathBuf,
}

impl Store {
    /// Finds the store for the repository that contains `dir`. Refuses, naming `radian arc
    /// init`, when the repository has no `config.toml` yet; a config that does not parse
    /// also stops it, so a typo in the file is never silently ignored.
    pub fn open(dir: &Path) -> Result<Self, Error> {
        let repo = git::repository_root(dir).map_err(Error::Failed)?;
        let root = repo.join(ARC_DIR);
        if !root.join(CONFIG_FILE).is_file() {
            return Err(Error::NotInitialized);
        }
        let store = Self { repo, root };
        store.config()?;
        Ok(store)
    }

    /// The store at an explicit repository root, without the setup check.
    pub fn at(repo: impl Into<PathBuf>) -> Self {
        let repo = repo.into();
        let root = repo.join(ARC_DIR);
        Self { repo, root }
    }

    /// The repository root.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// The `.radian/arc/` directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The parsed `config.toml`.
    pub fn config(&self) -> Result<config::Config, Error> {
        config::load(&self.root.join(CONFIG_FILE)).map_err(Error::Failed)
    }

    /// The `[repository] remote` and `main_branch` from `config.toml`: the names the gate
    /// fetches from and the wrapper pushes to when a caller has no explicit value.
    pub fn repository_names(&self) -> Result<(String, String), Error> {
        let config = self.config()?;
        Ok((config.repository.remote, config.repository.main_branch))
    }

    fn path(&self, kind: Kind, id: &str) -> PathBuf {
        self.root.join(kind.dir()).join(format!("{id}.md"))
    }

    fn ledger_path(&self, kind: Kind, id: &str) -> PathBuf {
        self.root.join(kind.ledger_dir()).join(format!("{id}.json"))
    }

    fn last_path(&self, kind: Kind, id: &str) -> PathBuf {
        self.root
            .join(kind.ledger_dir())
            .join(format!("{id}.last.md"))
    }

    // ---- setup ------------------------------------------------------------------------

    /// Sets up arc for the repository that contains `dir` (ADR 0048 decision 15): creates the
    /// layout, writes `config.toml` when it is missing, adds the exclude lines once, and
    /// runs the requirement checks.
    pub fn init(dir: &Path, track: bool) -> Result<InitReport, Error> {
        let repository = git::repository_root(dir).map_err(Error::Failed)?;
        let remote = "origin".to_owned();
        let remote_url = git::remote_url(&repository, &remote).map_err(|_| {
            Error::Failed(format!(
                "the repository at {} has no `{remote}` remote; arc reads the main branch \
                 from it and pushes task branches to it",
                repository.display()
            ))
        })?;
        let main_branch = git::main_branch(&repository, &remote);
        let root = repository.join(ARC_DIR);
        for sub in [
            "plans",
            "tasks",
            "ledger/plans",
            "ledger/tasks",
            "runs",
            "attempts",
            "gates",
            "worktrees",
        ] {
            let path = root.join(sub);
            fs::create_dir_all(&path).map_err(|error| {
                Error::Failed(format!("cannot create {}: {error}", path.display()))
            })?;
        }
        let config_path = root.join(CONFIG_FILE);
        let config_created = if config_path.is_file() {
            false
        } else {
            write_atomic(&config_path, config::default_text(&main_branch).as_bytes())?;
            true
        };
        // The standing worker instructions (ADR 0048 decision 7). A file the owner edited is
        // never overwritten: `init` only fills in the generic default.
        let worker_brief_path = root.join(WORKER_BRIEF_FILE);
        let worker_brief_created = if worker_brief_path.is_file() {
            false
        } else {
            write_atomic(&worker_brief_path, WORKER_BRIEF_DEFAULT.as_bytes())?;
            true
        };
        let exclude_path = git::exclude_path(&repository).map_err(Error::Failed)?;
        let exclude_added = add_exclude_lines(&exclude_path, track)?;
        let checks = vec![
            check_credential(),
            check_remote(&repository, &remote),
            check_push(&repository, &remote, &main_branch),
            check_sandbox(&repository),
        ];
        Ok(InitReport {
            repository,
            remote,
            remote_url,
            main_branch,
            config_path,
            config_created,
            worker_brief_path,
            worker_brief_created,
            exclude_path,
            exclude_added,
            checks,
        })
    }

    // ---- locks ------------------------------------------------------------------------

    /// Takes `state.lock`, waiting up to [`LOCK_WAIT`] for another process to release it.
    pub fn lock_state(&self) -> Result<StateLock, Error> {
        let path = self.root.join(STATE_LOCK);
        let file = open_lock_file(&path)?;
        wait_exclusive(&file, &path)?;
        Ok(StateLock { file })
    }

    /// Checks whether another process holds `state.lock`, without creating the lock file.
    ///
    /// A command that only reads (a confirm's `--dry-run`) calls this: a missing lock file
    /// means no process holds the lock, and the check leaves the store byte for byte as it
    /// was instead of writing an empty `state.lock`.
    pub fn check_state_lock(&self) -> Result<(), Error> {
        let path = self.root.join(STATE_LOCK);
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(Error::Failed(format!(
                    "cannot open {}: {error}",
                    path.display()
                )));
            }
        };
        wait_exclusive(&file, &path)?;
        let _ = FileExt::unlock(&file);
        Ok(())
    }

    /// Tries `runner.lock` without waiting: `Some` when this process now holds it, `None`
    /// when a scheduling loop already does. Dropping the guard releases it. The holder writes
    /// its pid into the file, which a viewer's header reads; the flock is the lock itself.
    pub fn try_runner_lock(&self) -> Result<Option<RunnerLock>, Error> {
        let path = self.root.join(RUNNER_LOCK);
        let mut file = open_lock_file(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                record_runner_pid(&mut file);
                Ok(Some(RunnerLock { file }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(Error::Failed(format!(
                "cannot lock {}: {error}",
                path.display()
            ))),
        }
    }

    /// The pid the holder of `runner.lock` recorded, when it did. It is only meaningful while
    /// the lock is held: the probe that reads it failed first. A lock file from an older arc,
    /// or one whose pid could not be written, reads as `None`.
    #[must_use]
    pub fn runner_lock_pid(&self) -> Option<u32> {
        fs::read_to_string(self.root.join(RUNNER_LOCK))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    // ---- reading ----------------------------------------------------------------------

    /// Reads one plan and marks whether it changed outside arc.
    pub fn load_plan(&self, id: &str) -> Result<Loaded<model::PlanFile>, Error> {
        check_id(id)?;
        self.load(Kind::Plan, id, model::PlanFile::from_text)
    }

    /// Reads one task and marks whether it changed outside arc.
    pub fn load_task(&self, id: &str) -> Result<Loaded<model::TaskFile>, Error> {
        check_id(id)?;
        self.load(Kind::Task, id, model::TaskFile::from_text)
    }

    fn load<T>(
        &self,
        kind: Kind,
        id: &str,
        parse: fn(&str) -> Result<T, String>,
    ) -> Result<Loaded<T>, Error> {
        let path = self.path(kind, id);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::Failed(format!("no {} `{id}`", kind.noun())));
            }
            Err(error) => {
                return Err(Error::Failed(format!(
                    "cannot read {}: {error}",
                    path.display()
                )));
            }
        };
        let item = parse(&raw)
            .map_err(|message| Error::Failed(format!("{}: {message}", path.display())))?;
        let changed_outside_arc = !self.matches_ledger(kind, id, raw.as_bytes());
        Ok(Loaded {
            item,
            raw,
            changed_outside_arc,
        })
    }

    fn matches_ledger(&self, kind: Kind, id: &str, bytes: &[u8]) -> bool {
        self.read_ledger(kind, id)
            .is_some_and(|ledger| ledger.sha256 == sha256_hex(bytes))
    }

    fn read_ledger(&self, kind: Kind, id: &str) -> Option<Ledger> {
        let text = fs::read_to_string(self.ledger_path(kind, id)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Every plan, each with its tasks in display order.
    pub fn plans(&self) -> Result<Vec<PlanView>, Error> {
        let mut views = Vec::new();
        for (id, _) in self.ids(Kind::Plan)? {
            views.push(self.plan_view(&id)?);
        }
        Ok(views)
    }

    /// One plan with its tasks.
    pub fn plan_view(&self, id: &str) -> Result<PlanView, Error> {
        let plan = self.load_plan(id)?;
        let mut tasks = Vec::new();
        for task_id in &plan.item.tasks {
            // A task file a person deleted is left out: the plan stays readable, and the
            // missing file is visible in `plans/`.
            if self.path(Kind::Task, task_id).is_file() {
                tasks.push(self.load_task(task_id)?);
            }
        }
        Ok(PlanView { plan, tasks })
    }

    /// Whether the plan is cancelled ([`PlanView::is_cancelled`]): its stored `status` is
    /// `cancelled`, or its tasks are all `failed.cancelled`. Every command that changes a plan
    /// or one of its tasks asks this one question first, because a cancelled plan is final
    /// (ADR 0048 decisions 3 and 9).
    pub fn plan_is_cancelled(&self, id: &str) -> Result<bool, Error> {
        Ok(self.plan_view(id)?.is_cancelled())
    }

    /// Refuses the plan when it is cancelled ([`Store::plan_is_cancelled`]), naming its
    /// status: the refusal every command that would change a plan or one of its tasks makes
    /// before it writes anything (ADR 0048 decisions 3 and 9).
    pub fn refuse_cancelled(&self, id: &str) -> Result<(), Error> {
        let view = self.plan_view(id)?;
        if view.is_cancelled() {
            return Err(cancelled_plan(&view.plan.item.id));
        }
        Ok(())
    }

    /// `show <id>`: the plan or the task the id names. An id that names both is refused.
    pub fn show(&self, id: &str) -> Result<Item, Error> {
        check_id(id)?;
        match self.resolve_kind(id)? {
            Kind::Plan => Ok(Item::Plan(self.plan_view(id)?)),
            Kind::Task => Ok(Item::Task(self.load_task(id)?)),
        }
    }

    /// Whether a plan file with this id exists.
    pub fn plan_exists(&self, id: &str) -> bool {
        check_id(id).is_ok() && self.path(Kind::Plan, id).is_file()
    }

    /// Whether a task file with this id exists.
    pub fn task_exists(&self, id: &str) -> bool {
        check_id(id).is_ok() && self.path(Kind::Task, id).is_file()
    }

    /// Every stored task's id, sorted: the retry release and the reconcile walk them.
    pub fn task_ids(&self) -> Result<Vec<String>, Error> {
        Ok(self
            .ids(Kind::Task)?
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    }

    // ---- writing ----------------------------------------------------------------------

    /// Appends one task to a plan, in one transaction: the task file, its ledger entry, and
    /// the plan's `tasks` list. `radian arc add task` uses this: a confirmed plan gets no task.
    #[allow(clippy::too_many_arguments)]
    pub fn add_task(
        &self,
        plan_id: &str,
        slug: &str,
        title: &str,
        after: Vec<String>,
        allow_noop: bool,
        extra_stages: Vec<String>,
        brief: &str,
    ) -> Result<String, Error> {
        self.add_task_in(
            plan_id,
            slug,
            title,
            after,
            allow_noop,
            extra_stages,
            brief,
            false,
        )
    }

    /// The lead's MCP `add_task` and arc's review follow-up (ADR 0048 decisions 3 and 11): a
    /// draft plan gets a `todo.ready` task, exactly as `radian arc add task` makes it, and a
    /// confirmed plan gets a `todo.proposed` task, which waits for the owner's `confirm`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_task_proposed(
        &self,
        plan_id: &str,
        slug: &str,
        title: &str,
        after: Vec<String>,
        allow_noop: bool,
        extra_stages: Vec<String>,
        brief: &str,
    ) -> Result<String, Error> {
        self.add_task_in(
            plan_id,
            slug,
            title,
            after,
            allow_noop,
            extra_stages,
            brief,
            true,
        )
    }

    /// [`Store::add_task`], with `allow_confirmed` naming whether a confirmed plan takes the
    /// task as `todo.proposed` (`true`) or refuses it (`false`).
    #[allow(clippy::too_many_arguments)]
    fn add_task_in(
        &self,
        plan_id: &str,
        slug: &str,
        title: &str,
        after: Vec<String>,
        allow_noop: bool,
        extra_stages: Vec<String>,
        brief: &str,
        allow_confirmed: bool,
    ) -> Result<String, Error> {
        check_new(slug, title)?;
        check_id(plan_id)?;
        for stage in &extra_stages {
            if !STAGE_NAMES.contains(&stage.as_str()) {
                return Err(Error::Usage(format!(
                    "`{stage}` is not a gate stage; stages are {}",
                    STAGE_NAMES.join(", ")
                )));
            }
        }
        let _lock = self.lock_state()?;
        let plan = self.load_plan(plan_id)?;
        require_unchanged(plan_id, plan.changed_outside_arc)?;
        // The tasks decide too: a plan whose tasks are all `failed.cancelled` is cancelled, so
        // a task added here would bring a final plan back to life (ADR 0048 decisions 3 and 9).
        self.refuse_cancelled(plan_id)?;
        let proposed = if plan.item.status == "draft" {
            false
        } else if plan.item.status == "confirmed" && allow_confirmed {
            true
        } else {
            return Err(Error::Refused(format!(
                "plan `{plan_id}` is `{}`; a confirmed plan gets no new tasks",
                plan.item.status
            )));
        };
        for dependency in &after {
            check_id(dependency)?;
            let dependency_path = self.path(Kind::Task, dependency);
            let allowed = if dependency_path.is_file() {
                let task = self.load_task(dependency)?;
                task.item.plan == plan_id || task.item.state == model::TaskState::Done
            } else {
                false
            };
            if !allowed {
                return Err(Error::Refused(format!(
                    "`{dependency}` is neither a task of plan `{plan_id}` nor a done task"
                )));
            }
        }
        let id = self.next_id(Kind::Task, slug)?;
        let mut task = model::TaskFile::new(
            id.clone(),
            plan_id.to_owned(),
            title.trim().to_owned(),
            after,
            allow_noop,
            extra_stages,
            brief,
        );
        if proposed {
            task.phase = model::PROPOSED_PHASE.to_owned();
        }
        // The task file comes first: a crash between the two writes leaves an unlisted task
        // file, which is visible, instead of a plan that names a file that does not exist.
        self.write_item(Kind::Task, &id, &task.to_text(), 1)?;
        let mut plan = plan.item;
        plan.tasks.push(id.clone());
        let version = self.next_version(Kind::Plan, plan_id);
        self.write_item(Kind::Plan, plan_id, &plan.to_text(), version)?;
        Ok(id)
    }

    /// Creates a plan in one transaction.
    pub fn add_plan(&self, slug: &str, title: &str, request: &str) -> Result<String, Error> {
        check_new(slug, title)?;
        let _lock = self.lock_state()?;
        let id = self.next_id(Kind::Plan, slug)?;
        let plan = model::PlanFile::new(id.clone(), title.trim().to_owned(), request);
        self.write_item(Kind::Plan, &id, &plan.to_text(), 1)?;
        Ok(id)
    }

    /// Runs one state change on a task under `state.lock`. This is the entry every later
    /// slice uses: claim, reconcile, verify, merge. Returns the new `state_version`.
    pub fn change_task(
        &self,
        id: &str,
        change: impl FnOnce(&mut model::TaskFile) -> Result<(), Error>,
    ) -> Result<u64, Error> {
        let _lock = self.lock_state()?;
        let loaded = self.load_task(id)?;
        require_unchanged(id, loaded.changed_outside_arc)?;
        let mut item = loaded.item;
        change(&mut item)?;
        let version = self.next_version(Kind::Task, id);
        self.write_item(Kind::Task, id, &item.to_text(), version)?;
        Ok(version)
    }

    /// Runs one state change on a plan under `state.lock`.
    ///
    /// This writes a plan in any state. Every command that changes a plan uses
    /// [`Store::change_open_plan`] instead, which refuses a cancelled plan first; this one is
    /// left for the cancel itself, which writes the final status, for `pause`, which only sets
    /// `auto` to `off` on a plan that may already be final, and for arc's own `auto` write when
    /// a budget or the automatic rework limit pauses a plan.
    pub fn change_plan(
        &self,
        id: &str,
        change: impl FnOnce(&mut model::PlanFile) -> Result<(), Error>,
    ) -> Result<u64, Error> {
        let _lock = self.lock_state()?;
        let loaded = self.load_plan(id)?;
        require_unchanged(id, loaded.changed_outside_arc)?;
        let mut item = loaded.item;
        change(&mut item)?;
        let version = self.next_version(Kind::Plan, id);
        self.write_item(Kind::Plan, id, &item.to_text(), version)?;
        Ok(version)
    }

    /// [`Store::change_plan`] for a plan a command may change: a plan that is cancelled
    /// ([`Store::plan_is_cancelled`], its stored `status` or its tasks) is refused under the
    /// same lock, before `change` runs, because a cancelled plan is final (ADR 0048 decisions
    /// 3 and 9).
    pub fn change_open_plan(
        &self,
        id: &str,
        change: impl FnOnce(&mut model::PlanFile) -> Result<(), Error>,
    ) -> Result<u64, Error> {
        let _lock = self.lock_state()?;
        let view = self.plan_view(id)?;
        require_unchanged(id, view.plan.changed_outside_arc)?;
        if view.is_cancelled() {
            return Err(cancelled_plan(&view.plan.item.id));
        }
        let mut item = view.plan.item;
        change(&mut item)?;
        let version = self.next_version(Kind::Plan, id);
        self.write_item(Kind::Plan, id, &item.to_text(), version)?;
        Ok(version)
    }

    /// Records the file's current content as arc's, resolving an outside change. Refuses a
    /// change to a field only arc may set, naming the fields.
    pub fn adopt(&self, id: &str) -> Result<AdoptReport, Error> {
        check_id(id)?;
        let _lock = self.lock_state()?;
        let kind = self.resolve_kind(id)?;
        let path = self.path(kind, id);
        let raw = fs::read_to_string(&path)
            .map_err(|error| Error::Failed(format!("cannot read {}: {error}", path.display())))?;
        let bytes = raw.as_bytes();
        let ledger = self.read_ledger(kind, id);
        if ledger
            .as_ref()
            .is_some_and(|ledger| ledger.sha256 == sha256_hex(bytes))
        {
            return Ok(AdoptReport {
                id: id.to_owned(),
                kind: kind.noun(),
                changed: false,
            });
        }
        let last_path = self.last_path(kind, id);
        let last = fs::read_to_string(&last_path).map_err(|_| {
            Error::Refused(format!(
                "no version of `{id}` that arc wrote is recorded under {}; arc cannot tell \
                 which fields changed outside it",
                last_path.display()
            ))
        })?;
        let touched = protected_changes(kind, &last, &raw)?;
        if !touched.is_empty() {
            return Err(Error::Refused(format!(
                "`{id}` changed outside arc in {}; only the owner may change those fields",
                touched.join(", ")
            )));
        }
        let version = ledger.map(|ledger| ledger.state_version + 1).unwrap_or(1);
        self.write_ledger(kind, id, version, &sha256_hex(bytes))?;
        write_atomic(&last_path, bytes)?;
        Ok(AdoptReport {
            id: id.to_owned(),
            kind: kind.noun(),
            changed: true,
        })
    }

    /// Which kind `id` names. An id that names both a plan and a task is refused (exit 3,
    /// naming both files): an id names one item, and no command picks one of the two.
    pub(crate) fn resolve_kind(&self, id: &str) -> Result<Kind, Error> {
        check_id(id)?;
        let plan = self.path(Kind::Plan, id);
        let task = self.path(Kind::Task, id);
        match (plan.is_file(), task.is_file()) {
            (true, true) => Err(Error::Refused(format!(
                "`{id}` names both a plan and a task: {} and {}",
                plan.display(),
                task.display()
            ))),
            (true, false) => Ok(Kind::Plan),
            (false, true) => Ok(Kind::Task),
            (false, false) => Err(Error::Failed(format!("no plan or task `{id}`"))),
        }
    }

    /// The next free four-digit number for `kind` and `slug`, with that kind's configured
    /// prefix. The caller holds `state.lock`, so two processes never take the same number.
    fn next_id(&self, kind: Kind, slug: &str) -> Result<String, Error> {
        let config = self.config()?;
        let number = self.next_number(kind)?;
        Ok(model::format_id(kind.prefix(&config), number, slug))
    }

    /// The next free four-digit number of `kind`: the highest number any item of that kind
    /// (a file or a ledger entry, whatever prefix it carries), plus one. Plans and tasks have
    /// separate sequences.
    fn next_number(&self, kind: Kind) -> Result<u32, Error> {
        let highest = self
            .ids(kind)?
            .into_iter()
            .filter_map(|(id, _)| model::id_number(&id))
            .max()
            .unwrap_or(0);
        let number = highest + 1;
        if number > MAX_ID_NUMBER {
            return Err(Error::Failed(format!(
                "no free four-digit {} number left",
                kind.noun()
            )));
        }
        Ok(number)
    }

    /// Every stored id of `kind`, from the files and their ledger entries, sorted.
    fn ids(&self, kind: Kind) -> Result<Vec<(String, PathBuf)>, Error> {
        let mut ids: BTreeMap<String, PathBuf> = BTreeMap::new();
        for dir in [kind.dir(), kind.ledger_dir()] {
            let path = self.root.join(dir);
            let Ok(entries) = fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let stem = name
                    .strip_suffix(".md")
                    .or_else(|| name.strip_suffix(".json"));
                if let Some(stem) = stem
                    && model::valid_id(stem)
                {
                    ids.entry(stem.to_owned()).or_insert_with(|| entry.path());
                }
            }
        }
        Ok(ids.into_iter().collect())
    }

    fn next_version(&self, kind: Kind, id: &str) -> u64 {
        self.read_ledger(kind, id)
            .map(|ledger| ledger.state_version + 1)
            .unwrap_or(1)
    }

    /// Writes the file, its ledger entry, and the copy `adopt` compares against.
    fn write_item(&self, kind: Kind, id: &str, text: &str, version: u64) -> Result<(), Error> {
        let bytes = text.as_bytes();
        write_atomic(&self.path(kind, id), bytes)?;
        self.write_ledger(kind, id, version, &sha256_hex(bytes))?;
        write_atomic(&self.last_path(kind, id), bytes)?;
        Ok(())
    }

    fn write_ledger(
        &self,
        kind: Kind,
        id: &str,
        state_version: u64,
        sha256: &str,
    ) -> Result<(), Error> {
        let ledger = Ledger {
            state_version,
            sha256: sha256.to_owned(),
        };
        let mut text = serde_json::to_string_pretty(&ledger)
            .map_err(|error| Error::Failed(format!("cannot encode the ledger: {error}")))?;
        text.push('\n');
        write_atomic(&self.ledger_path(kind, id), text.as_bytes())
    }
}

/// The ledger entry for one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ledger {
    state_version: u64,
    sha256: String,
}

/// The `state.lock` guard: the lock is released when this drops.
#[derive(Debug)]
pub struct StateLock {
    file: File,
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// The `runner.lock` guard: the lock is released when this drops.
#[derive(Debug)]
pub struct RunnerLock {
    file: File,
}

impl Drop for RunnerLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Waits up to [`LOCK_WAIT`] for `file`'s exclusive flock, in the one loop every lock takes.
fn wait_exclusive(file: &File, path: &Path) -> Result<(), Error> {
    let start = Instant::now();
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if start.elapsed() >= LOCK_WAIT {
                    return Err(Error::Locked(STATE_LOCK));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(Error::Failed(format!(
                    "cannot lock {}: {error}",
                    path.display()
                )));
            }
        }
    }
}

/// Opens (creating) a lock file. The flock is the lock; `runner.lock` also carries the holder's
/// pid for a viewer's header, and no other lock file carries state.
fn open_lock_file(path: &Path) -> Result<File, Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Error::Failed(format!("cannot create {}: {error}", parent.display()))
        })?;
    }
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|error| Error::Failed(format!("cannot open {}: {error}", path.display())))
}

/// Writes this process's pid into `runner.lock`, for a viewer that cannot take the lock. A
/// failure is ignored: the lock is the flock, and the pid only feeds a header line.
fn record_runner_pid(file: &mut File) {
    use std::io::{Seek, SeekFrom, Write};
    if file.set_len(0).is_err() || file.seek(SeekFrom::Start(0)).is_err() {
        return;
    }
    let _ = file.write_all(std::process::id().to_string().as_bytes());
    let _ = file.sync_data();
}

/// Writes `bytes` to `path` the way every arc write happens: a temporary file in the same
/// directory, fsync, rename over the target, fsync the parent directory.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| Error::Failed(format!("{} has no directory", path.display())))?;
    fs::create_dir_all(parent)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", parent.display())))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let sequence = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = parent.join(format!(".{name}.tmp.{}.{sequence}", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
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

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn check_id(id: &str) -> Result<(), Error> {
    if model::valid_id(id) {
        return Ok(());
    }
    Err(Error::Usage(format!(
        "`{id}` is not a plan or task id (a 1 to 4 letter prefix, four digits, a dash, and a slug)"
    )))
}

fn check_new(slug: &str, title: &str) -> Result<(), Error> {
    if !model::valid_slug(slug) {
        return Err(Error::Usage(format!(
            "`{slug}` is not a slug: 1 to {} characters from [a-z0-9-], with no leading or \
             trailing -",
            model::SLUG_MAX_CHARS
        )));
    }
    if title.trim().is_empty() {
        return Err(Error::Usage("the title is empty".to_owned()));
    }
    Ok(())
}

fn require_unchanged(id: &str, changed: bool) -> Result<(), Error> {
    if changed {
        return Err(Error::ChangedOutsideArc(id.to_owned()));
    }
    Ok(())
}

/// The refusal every command that changes a plan uses when the plan is `cancelled` (ADR 0048
/// decisions 3 and 9): a cancelled plan is final. The message names the status.
pub(crate) fn cancelled_plan(id: &str) -> Error {
    Error::Refused(format!(
        "plan `{id}` is `cancelled`; a cancelled plan is final"
    ))
}

/// The fields `adopt` refuses to accept from an outside change, for the fields that differ
/// between the last version arc wrote and the current file.
fn protected_changes(kind: Kind, last: &str, current: &str) -> Result<Vec<&'static str>, Error> {
    let mut touched = Vec::new();
    match kind {
        Kind::Plan => {
            let last = model::PlanFile::from_text(last).map_err(|message| {
                Error::Failed(format!(
                    "the recorded plan version does not parse: {message}"
                ))
            })?;
            let current = model::PlanFile::from_text(current).map_err(|message| {
                Error::Failed(format!("the plan file does not parse: {message}"))
            })?;
            // `status`, `auto` and `priority` are a plan's state: the owner confirms, sets
            // the auto level and sets the priority, so an outside edit of any is refused.
            if last.status != current.status {
                touched.push("status");
            }
            if last.auto != current.auto {
                touched.push("auto");
            }
            if last.priority() != current.priority() {
                touched.push("priority");
            }
        }
        Kind::Task => {
            let last = model::TaskFile::from_text(last).map_err(|message| {
                Error::Failed(format!(
                    "the recorded task version does not parse: {message}"
                ))
            })?;
            let current = model::TaskFile::from_text(current).map_err(|message| {
                Error::Failed(format!("the task file does not parse: {message}"))
            })?;
            if last.state != current.state {
                touched.push("state");
            }
            if last.phase != current.phase {
                touched.push("phase");
            }
            if last.attempt != current.attempt {
                touched.push("attempt");
            }
            if last.attempt_id != current.attempt_id {
                touched.push("attempt_id");
            }
            if last.token != current.token {
                touched.push("token");
            }
            if last.cost_usd != current.cost_usd {
                touched.push("cost_usd");
            }
            if last.total_cost_usd != current.total_cost_usd {
                touched.push("total_cost_usd");
            }
            if last.auto_rework != current.auto_rework {
                touched.push("auto_rework");
            }
            if last.retry_count != current.retry_count {
                touched.push("retry_count");
            }
            if last.retry_at != current.retry_at {
                touched.push("retry_at");
            }
            if last.base_sha != current.base_sha {
                touched.push("base_sha");
            }
            if last.head_sha != current.head_sha {
                touched.push("head_sha");
            }
            if last.verified_sha != current.verified_sha {
                touched.push("verified_sha");
            }
            if last.verdict_set_digest != current.verdict_set_digest {
                touched.push("verdict_set_digest");
            }
        }
    }
    Ok(touched)
}

/// The `.git/info/exclude` lines arc adds, in the order `init` writes them.
pub fn exclude_lines(track: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if !track {
        lines.push("/.radian/arc/config.toml".to_owned());
        lines.push("/.radian/arc/plans/".to_owned());
        lines.push("/.radian/arc/tasks/".to_owned());
    }
    lines.extend(
        [
            "/.radian/arc/ledger/",
            "/.radian/arc/runs/",
            "/.radian/arc/attempts/",
            "/.radian/arc/gates/",
            "/.radian/arc/state.lock",
            "/.radian/arc/runner.lock",
            "/.radian/arc/worktrees/",
            "/.radian/arc/worker-brief.md",
        ]
        .map(str::to_owned),
    );
    lines
}

/// The comment that explains the block, written once.
const EXCLUDE_HEADING: &str = "# arc (radian): local state, never written to git by arc";

/// Appends the lines that are not there yet. Returns the lines this run added.
fn add_exclude_lines(path: &Path, track: bool) -> Result<Vec<String>, Error> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let mut present: Vec<String> = existing
        .lines()
        .map(|line| line.trim_end().to_owned())
        .collect();
    let mut added = Vec::new();
    for line in std::iter::once(EXCLUDE_HEADING.to_owned()).chain(exclude_lines(track)) {
        if present.iter().any(|present| present == &line) {
            continue;
        }
        present.push(line.clone());
        added.push(line);
    }
    if added.is_empty() {
        return Ok(added);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Error::Failed(format!("cannot create {}: {error}", parent.display()))
        })?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| Error::Failed(format!("cannot open {}: {error}", path.display())))?;
    let mut text = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        text.push('\n');
    }
    for line in &added {
        text.push_str(line);
        text.push('\n');
    }
    file.write_all(text.as_bytes())
        .map_err(|error| Error::Failed(format!("cannot write {}: {error}", path.display())))?;
    file.sync_all()
        .map_err(|error| Error::Failed(format!("cannot sync {}: {error}", path.display())))?;
    Ok(added)
}

fn check_credential() -> Check {
    let detail = match radian_host::list_auth() {
        Ok(entries) if entries.is_empty() => {
            Err("no stored provider credential; run `radian login`".to_owned())
        }
        Ok(entries) => Ok(format!("{} stored", entries.len())),
        Err(error) => Err(format!("cannot read the credential store: {error}")),
    };
    Check {
        name: "provider credential".to_owned(),
        detail,
    }
}

fn check_remote(repository: &Path, remote: &str) -> Check {
    Check {
        name: format!("`{remote}` reachable"),
        detail: git::ls_remote(repository, remote).map(|()| "reachable".to_owned()),
    }
}

fn check_push(repository: &Path, remote: &str, main_branch: &str) -> Check {
    Check {
        name: format!("push to `{remote}` `{main_branch}`"),
        detail: git::push_dry_run(repository, remote, main_branch).map(|()| "accepted".to_owned()),
    }
}

fn check_sandbox(repository: &Path) -> Check {
    let detail =
        match radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::WorkspaceWrite, repository)
        {
            Ok(sandbox) => Ok(format!("{} backend", sandbox.backend_name())),
            Err(error) => Err(error.to_string()),
        };
    Check {
        name: "workspace sandbox".to_owned(),
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exit_codes_are_one_closed_list() {
        let codes: Vec<u8> = EXIT_CODES.iter().map(|(code, _)| *code).collect();
        assert_eq!(codes, vec![0, 1, 2, 3, 4]);
        assert_eq!(Exit::Ok.code(), 0);
        assert_eq!(Exit::Error.code(), 1);
        assert_eq!(Exit::Usage.code(), 2);
        assert_eq!(Exit::State.code(), 3);
        assert_eq!(Exit::Locked.code(), 4);
    }

    /// `radian arc merge` returns four of the five codes: it never returns a usage error
    /// itself (clap refuses a bad flag before `merge` runs). The manual's merge table is
    /// checked against exactly this list.
    #[test]
    fn the_merge_exit_codes_are_the_commands_own() {
        let codes: Vec<u8> = MERGE_EXIT_CODES.iter().map(|exit| exit.code()).collect();
        assert_eq!(codes, vec![0, 1, 3, 4]);
        for exit in MERGE_EXIT_CODES {
            assert!(
                EXIT_CODES.iter().any(|(code, _)| *code == exit.code()),
                "merge's {} is not one of arc's exit codes",
                exit.code()
            );
        }
    }

    #[test]
    fn the_exclude_lines_are_the_layout_once_each() {
        let untracked = exclude_lines(false);
        assert!(untracked.contains(&"/.radian/arc/config.toml".to_owned()));
        assert!(untracked.contains(&"/.radian/arc/plans/".to_owned()));
        assert!(untracked.contains(&"/.radian/arc/tasks/".to_owned()));
        assert!(untracked.contains(&"/.radian/arc/gates/".to_owned()));
        // The wrapper writes `attempts/<task>/{base_sha,last_head}`: the directory line
        // covers both, so `git status` stays clean after a run.
        assert!(untracked.contains(&"/.radian/arc/attempts/".to_owned()));
        let mut sorted = untracked.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), untracked.len(), "one line per path");
        let tracked = exclude_lines(true);
        assert!(!tracked.contains(&"/.radian/arc/config.toml".to_owned()));
        assert!(!tracked.contains(&"/.radian/arc/plans/".to_owned()));
        assert!(!tracked.contains(&"/.radian/arc/tasks/".to_owned()));
        assert!(tracked.contains(&"/.radian/arc/ledger/".to_owned()));
        assert!(tracked.contains(&"/.radian/arc/worktrees/".to_owned()));
    }

    #[test]
    fn an_unknown_id_is_a_usage_error_not_a_failed_lookup() {
        assert_eq!(check_id("../etc").unwrap_err().exit(), Exit::Usage);
        assert!(check_id("PL0001-a").is_ok());
        assert!(check_id("TK0001-a").is_ok());
        assert_eq!(
            check_id("0001-a").unwrap_err().exit(),
            Exit::Usage,
            "an id without a prefix is malformed"
        );
    }

    #[test]
    fn the_sha256_hex_is_lowercase_and_64_characters() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
