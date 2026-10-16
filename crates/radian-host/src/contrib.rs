//! The contributor mechanism (ADR 0027 C3): compiled-in data producers that **publish data,
//! never draw**.
//!
//! A [`StatusContributor`] answers with [`Contribution`]s (text + a theme role). Each runs on
//! its own tokio task that awaits [`ContributorRunner::tick`], writes the shared
//! [`ContributionStore`], and wakes the UI through the host's callback. The UI never waits for
//! a contributor: it reads a snapshot on the next frame. A failed refresh keeps the last value,
//! marks it stale (the TUI draws it dim). A run of failures produces one notice, at the
//! [`NOTICE_AFTER`]th in a row: a single network blip (a laptop waking up) only dims the value.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// A theme role a contribution asks for. Colors come from the theme (ADR 0029); the host never
/// names a color, and a segment never carries an escape sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentRole {
    /// The ordinary text level.
    Normal,
    /// The weak level (a stale value is drawn dim).
    Dim,
    /// A warning.
    Warn,
    /// A success.
    Ok,
    /// An error.
    Error,
    /// The accent (a label or name).
    Accent,
}

/// One styled piece of a contribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub role: SegmentRole,
}

impl Segment {
    pub fn new(text: impl Into<String>, role: SegmentRole) -> Self {
        Self {
            text: text.into(),
            role,
        }
    }
}

/// One contributor's answer for one item. `stale` is set by the runner when a refresh failed:
/// the segments are the last good value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contribution {
    pub item: String,
    pub segments: Vec<Segment>,
    pub stale: bool,
}

/// The shared contribution store: producers write, the UI reads a snapshot. The dirty/revision
/// protocol mirrors [`crate::SessionProjection`] (ADR 0023 U2): write the value, bump the
/// revision, then set `dirty` with `Release`; the UI clears the flag **before** reading.
#[derive(Debug, Default)]
pub struct ContributionStore {
    items: RwLock<BTreeMap<String, Contribution>>,
    revision: AtomicU64,
    dirty: AtomicBool,
}

impl ContributionStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Replaces one item's contribution, clearing `stale`. Returns whether the **value**
    /// changed: an identical value writes nothing and does not wake the UI.
    pub fn write(&self, item: &str, segments: Vec<Segment>) -> bool {
        let mut items = self.items.write().unwrap_or_else(|e| e.into_inner());
        if let Some(current) = items.get(item)
            && !current.stale
            && current.segments == segments
        {
            return false;
        }
        items.insert(
            item.to_owned(),
            Contribution {
                item: item.to_owned(),
                segments,
                stale: false,
            },
        );
        drop(items);
        self.bump();
        true
    }

    /// Marks one item stale, keeping its last segments. Returns whether the value changed: a
    /// no-op when it is already stale or has no value.
    pub fn mark_stale(&self, item: &str) -> bool {
        let mut items = self.items.write().unwrap_or_else(|e| e.into_inner());
        let Some(current) = items.get_mut(item) else {
            return false;
        };
        if current.stale {
            return false;
        }
        current.stale = true;
        // A stale mark is a visual change: the UI redraws it dim.
        drop(items);
        self.bump();
        true
    }

    /// Removes one item's contribution (ADR 0027): when the current model switches
    /// away from a provider its row hides. Returns whether anything changed.
    pub fn remove(&self, item: &str) -> bool {
        let mut items = self.items.write().unwrap_or_else(|e| e.into_inner());
        if items.remove(item).is_none() {
            return false;
        }
        drop(items);
        self.bump();
        true
    }

    /// One item's contribution.
    pub fn get(&self, item: &str) -> Option<Contribution> {
        self.items
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(item)
            .cloned()
    }

    /// Every contribution, in item-id order.
    pub fn snapshot(&self) -> Vec<Contribution> {
        self.items
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Clears the dirty flag and reports whether it was set. The UI calls this **before**
    /// reading a snapshot.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Acquire)
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    fn bump(&self) -> bool {
        self.revision.fetch_add(1, Ordering::Relaxed);
        !self.dirty.swap(true, Ordering::Release)
    }
}

/// A compiled-in data producer (ADR 0027 C3). `refresh` returns one [`Contribution`] per item
/// it provides; it never touches the terminal, and the runner awaits it off the UI thread.
pub trait StatusContributor: Send + Sync {
    /// The contributor's stable id (for notices).
    fn id(&self) -> &'static str;
    /// The item ids it provides (ADR 0027 C2 names them in a footer row or a widget).
    fn items(&self) -> &'static [&'static str];
    /// How often the runner refreshes it.
    fn period(&self) -> Duration;
    /// One refresh. `Err` keeps the last value and marks it stale.
    fn refresh(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Contribution>, String>> + Send + '_>>;
}

/// What one tick did, for the host's callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tick {
    /// The store changed: the UI must redraw (one dirty notification).
    pub changed: bool,
    /// The first failure of a run, as a one-line notice.
    pub notice: Option<String>,
}

/// Drives one contributor: a manual [`ContributorRunner::tick`] for tests, the periodic loop in
/// production.
pub struct ContributorRunner {
    contributor: Arc<dyn StatusContributor>,
    store: Arc<ContributionStore>,
    failures: u32,
}

/// How many refreshes in a row must fail before the failure is a notice. Before that the
/// value is only drawn stale.
pub const NOTICE_AFTER: u32 = 3;

impl ContributorRunner {
    pub fn new(contributor: Arc<dyn StatusContributor>, store: Arc<ContributionStore>) -> Self {
        Self {
            contributor,
            store,
            failures: 0,
        }
    }

    pub fn period(&self) -> Duration {
        self.contributor.period()
    }

    /// One refresh: writes every contribution on success (an unchanged value writes nothing),
    /// and on failure keeps the values and marks them stale. Only the [`NOTICE_AFTER`]th
    /// failure of a consecutive run yields a notice.
    pub async fn tick(&mut self) -> Tick {
        match self.contributor.refresh().await {
            Ok(contributions) => {
                self.failures = 0;
                let mut changed = false;
                for contribution in contributions {
                    changed |= self.store.write(&contribution.item, contribution.segments);
                }
                Tick {
                    changed,
                    notice: None,
                }
            }
            Err(error) => {
                self.failures += 1;
                let mut changed = false;
                for item in self.contributor.items() {
                    changed |= self.store.mark_stale(item);
                }
                Tick {
                    changed,
                    notice: (self.failures == NOTICE_AFTER)
                        .then(|| format!("{}: {error}", self.contributor.id())),
                }
            }
        }
    }

    /// How many refreshes failed in a row.
    pub fn failures(&self) -> u32 {
        self.failures
    }
}

/// Runs a contributor on its period, calling `on_tick` after each refresh. A failed refresh
/// keeps the task alive; the store keeps the last value. The task is dropped when the runtime
/// shuts down.
pub fn spawn(
    handle: &tokio::runtime::Handle,
    contributor: Arc<dyn StatusContributor>,
    store: Arc<ContributionStore>,
    on_tick: Arc<dyn Fn(Tick) + Send + Sync>,
) -> tokio::task::JoinHandle<()> {
    handle.spawn(async move {
        let mut runner = ContributorRunner::new(contributor, store);
        loop {
            let tick = runner.tick().await;
            if tick.changed || tick.notice.is_some() {
                on_tick(tick);
            }
            tokio::time::sleep(runner.period()).await;
        }
    })
}

/// Like [`spawn`], but the contributor is **gated** (ADR 0027): while `active` is
/// false the loop removes its items (so the row hides) and refreshes nothing — no network; the
/// moment it becomes true it refreshes once, then every period. `active` is a watch channel, so
/// a change wakes a sleeping loop at once.
pub fn spawn_gated(
    handle: &tokio::runtime::Handle,
    contributor: Arc<dyn StatusContributor>,
    store: Arc<ContributionStore>,
    on_tick: Arc<dyn Fn(Tick) + Send + Sync>,
    mut active: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let items = contributor.items();
    handle.spawn(async move {
        let mut runner = ContributorRunner::new(contributor, Arc::clone(&store));
        loop {
            if !*active.borrow() {
                // Inactive: hide the row and wait for a change (no refresh, no network).
                let changed = items.iter().any(|item| store.remove(item));
                if changed {
                    on_tick(Tick {
                        changed: true,
                        notice: None,
                    });
                }
                if active.changed().await.is_err() {
                    break;
                }
                continue;
            }
            let tick = runner.tick().await;
            if tick.changed || tick.notice.is_some() {
                on_tick(tick);
            }
            tokio::select! {
                _ = tokio::time::sleep(runner.period()) => {}
                result = active.changed() => {
                    if result.is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// The built-in `git_branch` / `worktree` contributor (ADR 0027 C2). Every git call runs in
/// the blocking pool, never on the draw path. The period is 5 s: a branch change is a human
/// action, and the refresh is three `git` invocations.
pub struct GitContributor {
    dir: PathBuf,
}

/// One blocking git read (ADR 0027 C3 and revision 1). Each field is `None` when git has no
/// answer (not a repository, or the command failed); the refresh then publishes nothing for it.
struct GitFacts {
    branch: Option<String>,
    worktree: Option<String>,
    changes: Option<(usize, usize)>,
    repo: Option<String>,
}

/// The command for **every** background git read (ADR 0027 C3). `--no-optional-locks` (and
/// `GIT_OPTIONAL_LOCKS=0` for older git) keeps a background `git status` from refreshing the
/// index, which would briefly take `.git/index.lock` and collide with the user's own
/// `git add`/`commit` in another terminal ("Another git process seems to be running"). This is
/// the standard editor/shell-prompt practice. ADR 0039 decision 3: the shared host no-filter
/// overrides, so the footer refresh never runs a repository filter command; without them the
/// read is skipped, not run unguarded.
fn background_git(dir: &Path) -> Option<Command> {
    let mut command = radian_coding::host_git::host_git_command(dir).ok()?;
    command
        .arg("--no-optional-locks")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    Some(command)
}

impl GitContributor {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Like `git`, but keeps an empty successful output as `Some("")`: a clean tree is a real
    /// answer (zero changes), not a missing one.
    fn git_raw(&self, args: &[&str]) -> Option<String> {
        let output = background_git(&self.dir)?.args(args).output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn git(&self, args: &[&str]) -> Option<String> {
        let output = background_git(&self.dir)?.args(args).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }

    /// The values for `git_branch`, `worktree`, the change counts and the repo name, as
    /// blocking work.
    fn read(&self) -> GitFacts {
        let branch = self.git(&["rev-parse", "--abbrev-ref", "HEAD"]);
        let branch = branch.filter(|name| name != "HEAD");
        // A linked worktree has a `--git-dir` different from the `--git-common-dir`; its name
        // is the worktree directory's basename. Both come from the same resolution, so they
        // are comparable even when one is relative (the main checkout's `.git`).
        let git_dir = self.git(&["rev-parse", "--git-dir"]);
        let common = self.git(&["rev-parse", "--git-common-dir"]);
        let worktree = match (git_dir, common) {
            (Some(dir), Some(common)) if dir != common => self
                .dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            _ => None,
        };
        // ADR 0027 C3 / revision 1: the change counts (modified tracked, untracked) are the
        // data slots the numbers row fills in slice 2b. `git status --porcelain` in the
        // background task, never at draw time.
        // ADR 0027: `{repo}` is the repository root's directory name. In a linked
        // worktree `--git-common-dir` points at the main repo's `.git`, so its parent is the
        // shared root; in a plain checkout it is the relative `.git`, whose root is `self.dir`.
        let repo = self
            .git(&["rev-parse", "--git-common-dir"])
            .and_then(|common| {
                let path = Path::new(&common);
                let root = if path.is_absolute() {
                    path.parent().map(Path::to_path_buf)
                } else {
                    Some(self.dir.clone())
                };
                root.and_then(|root| {
                    root.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
            });
        let changes = self.git_raw(&["status", "--porcelain"]).map(|status| {
            let mut modified = 0usize;
            let mut untracked = 0usize;
            for line in status.lines() {
                if line.starts_with("??") {
                    untracked += 1;
                } else if !line.trim().is_empty() {
                    modified += 1;
                }
            }
            (modified, untracked)
        });
        GitFacts {
            branch,
            worktree,
            changes,
            repo,
        }
    }
}

impl StatusContributor for GitContributor {
    fn id(&self) -> &'static str {
        "git"
    }

    fn items(&self) -> &'static [&'static str] {
        &[
            "git_branch",
            "worktree",
            "git_root",
            "git_modified",
            "git_untracked",
        ]
    }

    fn period(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn refresh(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Contribution>, String>> + Send + '_>> {
        Box::pin(async move {
            let dir = self.dir.clone();
            let GitFacts {
                branch,
                worktree,
                changes,
                repo,
            } = tokio::task::spawn_blocking(move || GitContributor::new(dir).read())
                .await
                .map_err(|error| format!("the git refresh task failed: {error}"))?;
            // Not a repository: no value. An item with no value is not written at all, so the
            // footer hides it and an empty value never counts as a change.
            let mut contributions = Vec::new();
            if let Some(branch) = branch {
                contributions.push(Contribution {
                    item: "git_branch".to_owned(),
                    segments: vec![Segment::new(branch, SegmentRole::Normal)],
                    stale: false,
                });
            }
            if let Some(name) = worktree {
                // The raw worktree name: the footer item adds its own label and the `cwd`
                // item's `{worktree}` placeholder uses it as-is (ADR 0027).
                contributions.push(Contribution {
                    item: "worktree".to_owned(),
                    segments: vec![Segment::new(name, SegmentRole::Normal)],
                    stale: false,
                });
            }
            if let Some(repo) = repo {
                contributions.push(Contribution {
                    item: "git_root".to_owned(),
                    segments: vec![Segment::new(repo, SegmentRole::Normal)],
                    stale: false,
                });
            }
            if let Some((modified, untracked)) = changes {
                contributions.push(Contribution {
                    item: "git_modified".to_owned(),
                    segments: vec![Segment::new(modified.to_string(), SegmentRole::Normal)],
                    stale: false,
                });
                contributions.push(Contribution {
                    item: "git_untracked".to_owned(),
                    segments: vec![Segment::new(untracked.to_string(), SegmentRole::Normal)],
                    stale: false,
                });
            }
            Ok(contributions)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::sync::atomic::AtomicUsize;

    /// ADR 0027 C3: every background git read carries `--no-optional-locks` **and**
    /// `GIT_OPTIONAL_LOCKS=0`, so the periodic `git status` never takes `.git/index.lock` and
    /// collides with the user's own `git add`/`commit` in another terminal. Both are asserted:
    /// the flag is newer git's door, the variable is the one older git reads.
    #[test]
    fn the_background_git_disables_optional_locks() {
        let command = background_git(Path::new("/tmp")).expect("the command");
        let args: Vec<&OsStr> = command.get_args().collect();
        assert!(
            args.contains(&OsStr::new("--no-optional-locks")),
            "the flag must be on every background git call: {args:?}"
        );
        // ADR 0039 decision 3: the host no-filter overrides travel with it too.
        assert!(
            args.contains(&OsStr::new("core.attributesFile=/dev/null")),
            "the no-filter overrides must be on a background git call: {args:?}"
        );
        let locks = command
            .get_envs()
            .find(|(key, _)| *key == "GIT_OPTIONAL_LOCKS")
            .and_then(|(_, value)| value);
        assert_eq!(locks, Some(OsStr::new("0")));
    }

    /// A scripted contributor: each refresh returns the next outcome.
    struct Scripted {
        outcomes: MutexVec<Result<Vec<Contribution>, String>>,
        calls: AtomicUsize,
    }

    use std::sync::Mutex;
    type MutexVec<T> = Mutex<Vec<T>>;

    impl Scripted {
        fn new(outcomes: Vec<Result<Vec<Contribution>, String>>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: Mutex::new(outcomes),
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl StatusContributor for Scripted {
        fn id(&self) -> &'static str {
            "scripted"
        }
        fn items(&self) -> &'static [&'static str] {
            &["thing"]
        }
        fn period(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn refresh(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Contribution>, String>> + Send + '_>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.outcomes.lock().unwrap().remove(0)
            })
        }
    }

    fn value(text: &str) -> Vec<Contribution> {
        vec![Contribution {
            item: "thing".to_owned(),
            segments: vec![Segment::new(text, SegmentRole::Normal)],
            stale: false,
        }]
    }

    #[tokio::test]
    async fn a_refresh_writes_the_store_and_an_unchanged_value_is_not_dirty() {
        let store = ContributionStore::new();
        let contributor = Scripted::new(vec![Ok(value("one")), Ok(value("one")), Ok(value("two"))]);
        let mut runner = ContributorRunner::new(contributor, Arc::clone(&store));
        let tick = runner.tick().await;
        assert!(tick.changed, "the first value is a change");
        assert_eq!(store.get("thing").unwrap().segments[0].text, "one");
        assert!(store.take_dirty(), "the store woke the UI");
        let tick = runner.tick().await;
        assert!(!tick.changed, "an identical value is not a change");
        assert!(!store.take_dirty(), "and does not wake the UI");
        let tick = runner.tick().await;
        assert!(tick.changed);
        assert_eq!(store.get("thing").unwrap().segments[0].text, "two");
    }

    #[tokio::test]
    async fn a_failure_keeps_the_value_marks_it_stale_and_notices_once() {
        let store = ContributionStore::new();
        let contributor = Scripted::new(vec![
            Ok(value("one")),
            Err("boom 1".to_string()),
            Err("boom 2".to_string()),
            Err("boom 3".to_string()),
            Err("boom 4".to_string()),
            Ok(value("two")),
        ]);
        let mut runner = ContributorRunner::new(contributor, Arc::clone(&store));
        runner.tick().await;
        let tick = runner.tick().await;
        assert!(tick.changed, "becoming stale is a redraw");
        assert!(tick.notice.is_none(), "one failure only dims the value");
        let held = store.get("thing").unwrap();
        assert_eq!(held.segments[0].text, "one", "the last value is kept");
        assert!(held.stale);
        assert!(
            runner.tick().await.notice.is_none(),
            "two failures still only dim it"
        );
        let tick = runner.tick().await;
        assert!(
            tick.notice.as_deref().unwrap_or("").contains("boom 3"),
            "the third failure in a row is a notice with its error: {:?}",
            tick.notice
        );
        let tick = runner.tick().await;
        assert!(
            tick.notice.is_none(),
            "a consecutive failure does not repeat the notice"
        );
        assert_eq!(runner.failures(), 4);
        let tick = runner.tick().await;
        assert!(tick.changed);
        assert!(!store.get("thing").unwrap().stale, "a success clears stale");
        assert_eq!(runner.failures(), 0);
    }

    /// The store's protocol: the value and revision are visible to a reader that clears the
    /// dirty flag first (ADR 0023 U2's wake-up order).
    #[tokio::test]
    async fn the_dirty_flag_is_cleared_before_reading() {
        let store = ContributionStore::new();
        store.write("thing", vec![Segment::new("x", SegmentRole::Normal)]);
        assert!(store.take_dirty());
        assert_eq!(store.revision(), 1);
        store.write("thing", vec![Segment::new("x", SegmentRole::Normal)]);
        assert!(!store.take_dirty(), "a repeat write is not a change");
    }
    /// ADR 0039 decision 3: the footer's background `git status` carries the shared no-filter
    /// overrides, so a stat-dirty file with a filter attribute never runs the clean filter in the
    /// host.
    #[test]
    fn the_git_contributor_never_runs_a_repository_filter() {
        let dir =
            std::env::temp_dir().join(format!("radian-contrib-gitfilter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(args)
                    .status()
                    .expect("git")
                    .success(),
                "git {args:?}"
            );
        };
        run(&["init", "-q", "-b", "feature"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        run(&["add", "tracked.txt"]);
        run(&["commit", "-q", "-m", "init"]);

        let marker = std::env::temp_dir().join(format!(
            "radian-contrib-gitfilter-marker-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let script = dir.join("evil-clean.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        run(&["config", "filter.evil.clean", script.to_str().unwrap()]);
        std::fs::write(dir.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        // Stat-dirty against the index, same size (git then hashes the content and runs the
        // clean filter).
        std::fs::write(dir.join("tracked.txt"), "two\n").unwrap();

        let facts = GitContributor::new(dir.clone()).read();
        assert_eq!(facts.branch.as_deref(), Some("feature"));
        assert!(
            !marker.exists(),
            "the footer git read must not run a repository filter"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&marker);
    }
}
