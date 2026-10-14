//! The contributor mechanism (ADR 0027 C3): compiled-in data producers that **publish data,
//! never draw**.
//!
//! A [`StatusContributor`] answers with [`Contribution`]s (text + a theme role). Each runs on
//! its own tokio task that awaits [`ContributorRunner::tick`], writes the shared
//! [`ContributionStore`], and wakes the UI through the host's callback. The UI never waits for
//! a contributor: it reads a snapshot on the next frame. A failed refresh keeps the last value,
//! marks it stale (the TUI draws it dim), and only the **first** failure of a run produces a
//! notice.

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
    /// and on failure keeps the values and marks them stale. Only the first failure of a
    /// consecutive run yields a notice.
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
                    notice: (self.failures == 1)
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
    contributor: Arc<dyn StatusContributor>,
    store: Arc<ContributionStore>,
    on_tick: Arc<dyn Fn(Tick) + Send + Sync>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
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

/// The built-in `git_branch` / `worktree` contributor (ADR 0027 C2). Every git call runs in
/// the blocking pool, never on the draw path. The period is 5 s: a branch change is a human
/// action, and the refresh is three `git` invocations.
pub struct GitContributor {
    dir: PathBuf,
}

impl GitContributor {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn git(&self, args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }

    /// The values for `git_branch` and `worktree`, as blocking work.
    fn read(&self) -> (Option<String>, Option<String>) {
        let branch = self.git(&["rev-parse", "--abbrev-ref", "HEAD"]);
        let branch = branch.filter(|name| name != "HEAD");
        // A linked worktree has a `--git-dir` different from the `--git-common-dir`; its name
        // is the worktree directory's basename.
        let git_dir = self.git(&["rev-parse", "--absolute-git-dir"]);
        let common = self.git(&["rev-parse", "--git-common-dir"]);
        let worktree = match (git_dir, common) {
            (Some(dir), Some(common)) if Path::new(&dir) != Path::new(&common) => self
                .dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            _ => None,
        };
        (branch, worktree)
    }
}

impl StatusContributor for GitContributor {
    fn id(&self) -> &'static str {
        "git"
    }

    fn items(&self) -> &'static [&'static str] {
        &["git_branch", "worktree"]
    }

    fn period(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn refresh(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Contribution>, String>> + Send + '_>> {
        Box::pin(async move {
            let dir = self.dir.clone();
            let (branch, worktree) =
                tokio::task::spawn_blocking(move || GitContributor::new(dir).read())
                    .await
                    .map_err(|error| format!("the git refresh task failed: {error}"))?;
            // Not a repository: `git_branch` is empty (the footer hides it), and `worktree`
            // too. An empty contribution is the honest "no data".
            let segments = |value: Option<String>| {
                value.map_or_else(Vec::new, |text| {
                    vec![Segment::new(text, SegmentRole::Normal)]
                })
            };
            Ok(vec![
                Contribution {
                    item: "git_branch".to_owned(),
                    segments: segments(branch),
                    stale: false,
                },
                Contribution {
                    item: "worktree".to_owned(),
                    // `worktree` shows a label (the accent) plus the name.
                    segments: worktree.map_or_else(Vec::new, |text| {
                        vec![
                            Segment::new("worktree ", SegmentRole::Accent),
                            Segment::new(text, SegmentRole::Normal),
                        ]
                    }),
                    stale: false,
                },
            ])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

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
            Err("boom".to_string()),
            Err("boom again".to_string()),
            Ok(value("two")),
        ]);
        let mut runner = ContributorRunner::new(contributor, Arc::clone(&store));
        runner.tick().await;
        let tick = runner.tick().await;
        assert!(tick.changed, "becoming stale is a redraw");
        assert!(tick.notice.as_deref().unwrap_or("").contains("boom"));
        let held = store.get("thing").unwrap();
        assert_eq!(held.segments[0].text, "one", "the last value is kept");
        assert!(held.stale);
        let tick = runner.tick().await;
        assert!(
            tick.notice.is_none(),
            "a consecutive failure does not repeat the notice"
        );
        assert_eq!(runner.failures(), 2);
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
}
