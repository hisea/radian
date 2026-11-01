//! The hub's scheduling loop against real temp repositories and a temp `RADIAN_HOME`
//! (ADR 0048 decision 16): one loop per registered repository, each on its own OS thread, the
//! registry re-read every interval, `hub.lock` per machine, the machine-wide concurrency cap,
//! fault isolation, SIGTERM, the rebuilt-binary restart, and `hub add`/`remove` while it runs.

mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::*;
use radian_arc::hub::HubLock;
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, plan_ops};

fn setup(repo: &Repo) -> Store {
    Store::init(&repo.clone, false).expect("arc init");
    Store::open(&repo.clone).expect("the store opens")
}

/// A confirmed plan at auto `run` with one `todo.ready` task.
fn ready_task(store: &Store, slug: &str) -> String {
    let plan = store
        .add_plan(slug, "A plan", "the request\n")
        .expect("add a plan");
    let task = store
        .add_task(
            &plan,
            slug,
            "A task",
            Vec::new(),
            false,
            Vec::new(),
            "the brief\n",
        )
        .expect("add a task");
    plan_ops::confirm(store, &plan).expect("confirm");
    plan_ops::set_auto(store, &plan, "run").expect("auto run");
    task
}

fn tune(repo: &Repo, from: &str, to: &str) {
    let path = repo.arc().join("config.toml");
    let text = fs::read_to_string(&path).expect("config.toml");
    assert!(text.contains(from), "config.toml has `{from}`:\n{text}");
    fs::write(&path, text.replacen(from, to, 1)).expect("write config.toml");
}

/// The attempt fails quickly once it starts; the test observes `doing` and does not wait for a
/// worker that needs a model.
fn command_worker(repo: &Repo) {
    tune(
        repo,
        "# command = [\"codex\", \"exec\"]",
        "command = [\"true\"]",
    );
}

/// Holds the claim in `doing` for `secs`, so a test can observe scheduling without a model.
fn slow_before_attempt(repo: &Repo, secs: u32) {
    tune(
        repo,
        "before_attempt = [\"merge-main\"]",
        &format!("before_attempt = [{{ run = \"sleep {secs}\" }}]"),
    );
}

fn state(store: &Store, task: &str) -> (String, String) {
    let task = store.load_task(task).expect("the task reads").item;
    (task.state.as_str().to_owned(), task.phase)
}

fn is_doing(store: &Store, task: &str) -> bool {
    state(store, task).0 == "doing"
}

fn doing_count(store: &Store) -> usize {
    store
        .task_ids()
        .expect("the task ids")
        .into_iter()
        .filter(|id| {
            store
                .load_task(id)
                .is_ok_and(|loaded| loaded.item.state == TaskState::Doing)
        })
        .count()
}

fn loop_log(repo: &Repo) -> String {
    fs::read_to_string(repo.arc().join("loop.log")).unwrap_or_default()
}

fn wait_until(limit: Duration, what: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen in {limit:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether `runner.lock` can be taken: true when no process holds it.
fn runner_lock_free(repo: &Repo) -> bool {
    use fs2::FileExt;
    let path = repo.arc().join("runner.lock");
    let file = match fs::OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
        Err(error) => panic!("cannot open {}: {error}", path.display()),
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&file);
            true
        }
        Err(_) => false,
    }
}

/// A temp `RADIAN_HOME` with a registry and the hub configuration a test wants.
struct HubHome {
    dir: TempDir,
    interval: u64,
}

impl HubHome {
    fn new(label: &str, interval: u64, cap: u64) -> Self {
        let dir = TempDir::new(label);
        let config = dir.path().join("config");
        fs::create_dir_all(&config).expect("the config directory");
        let mut text =
            format!("schema_version = 1\n[control]\nenabled = false\n[hub]\nconcurrency = {cap}\n");
        if interval >= radian_arc::run::MIN_INTERVAL_SECS {
            text.push_str(&format!("interval_secs = {interval}\n"));
        }
        fs::write(config.join("config.toml"), text).expect("the home config");
        fs::create_dir_all(dir.path().join("sessions")).expect("the session directory");
        Self { dir, interval }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn hub_dir(&self) -> PathBuf {
        self.path().join("hub")
    }

    /// Registers `repos` (name, path) in the registry, rewriting the whole file.
    fn register(&self, repos: &[(&str, &Path)]) {
        fs::create_dir_all(self.hub_dir()).expect("the hub directory");
        let mut text = String::new();
        for (name, path) in repos {
            let canonical = fs::canonicalize(path).expect("the repository canonicalizes");
            text.push_str(&format!(
                "[[repos]]\nname = \"{name}\"\npath = \"{}\"\n\n",
                canonical.display()
            ));
        }
        let path = self.hub_dir().join("repos.toml");
        let temp = self
            .hub_dir()
            .join(format!(".repos.toml.tmp.{}", std::process::id()));
        fs::write(&temp, text).expect("the temporary registry");
        fs::rename(&temp, &path).expect("the registry rename");
    }

    fn command(&self, program: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(self.path())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.path())
            .env("RADIAN_HOME", self.path())
            .env("RADIAN_SESSION_DIR", self.path().join("sessions"))
            .env("RADIAN_CATALOG_URL", fast_catalog_url())
            .stdin(Stdio::null());
        if self.interval < radian_arc::run::MIN_INTERVAL_SECS {
            command.env("RADIAN_HUB_INTERVAL_SECS", self.interval.to_string());
        }
        command
    }

    fn spawn(&self) -> HubRun {
        let program = radian_bin();
        let mut command = self.command(&program, &["hub"]);
        command.stdout(Stdio::piped()).stderr(Stdio::null());
        HubRun::new(command.spawn().expect("radian hub starts"))
    }

    fn spawn_from(&self, program: &Path) -> HubRun {
        let mut command = self.command(program, &["hub"]);
        command.stdout(Stdio::piped()).stderr(Stdio::null());
        HubRun::new(command.spawn().expect("the copied radian hub starts"))
    }

    /// One `radian` command in this home, waited for.
    fn cli(&self, args: &[&str]) -> Run {
        let output = self
            .command(&radian_bin(), args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("radian runs");
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// One `radian` command in this home, killed when it outlives `limit`. `None` is a
    /// command that did not exit: a second hub that runs instead of refusing.
    fn run_within(&self, args: &[&str], limit: Duration) -> Option<Run> {
        let mut child = self
            .command(&radian_bin(), args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("radian runs");
        let deadline = Instant::now() + limit;
        loop {
            let status = child.try_wait().expect("the child's status");
            if let Some(status) = status {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    use std::io::Read as _;
                    let _ = pipe.read_to_string(&mut stdout);
                }
                if let Some(mut pipe) = child.stderr.take() {
                    use std::io::Read as _;
                    let _ = pipe.read_to_string(&mut stderr);
                }
                return Some(Run {
                    code: status.code().unwrap_or(-1),
                    stdout,
                    stderr,
                });
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// A running hub with its stdout read line by line on its own thread.
struct HubRun {
    child: Child,
    rx: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl HubRun {
    fn new(mut child: Child) -> Self {
        let stdout = child.stdout.take().expect("the hub's stdout is piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            rx,
            seen: Vec::new(),
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn next_line(&mut self, timeout: Duration) -> Option<String> {
        let line = self.rx.recv_timeout(timeout).ok()?;
        self.seen.push(line.clone());
        Some(line)
    }

    /// The next stdout line containing `needle`, within `timeout`.
    fn wait_line_where(
        &mut self,
        timeout: Duration,
        mut predicate: impl FnMut(&str) -> bool,
    ) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let line = self.next_line(left)?;
            if predicate(&line) {
                return Some(line);
            }
        }
    }

    fn term(&mut self) -> bool {
        Command::new("kill")
            .args(["-TERM", &self.id().to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("the hub's status") {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for HubRun {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn status_for(line: &str, name: &str, word: &str) -> bool {
    line.starts_with(&format!("{name}  ")) && line.contains(word)
}

/// Edge 1: two registered repositories, each with one `todo.ready` task: both reach `doing`
/// within one interval, and each repository's own `loop.log` has its own lines.
#[test]
fn two_repositories_start_their_tasks_in_one_interval() {
    let a = Repo::new("hub-two-a");
    let b = Repo::new("hub-two-b");
    let sa = setup(&a);
    let sb = setup(&b);
    let ta = ready_task(&sa, "a");
    let tb = ready_task(&sb, "b");
    for repo in [&a, &b] {
        command_worker(repo);
        slow_before_attempt(repo, 3);
    }
    let home = HubHome::new("hub-two", 1, 4);
    home.register(&[("a", &a.clone), ("b", &b.clone)]);
    let mut hub = home.spawn();

    wait_until(Duration::from_secs(10), "both tasks in doing", || {
        is_doing(&sa, &ta) && is_doing(&sb, &tb)
    });
    wait_until(Duration::from_secs(10), "both loop.log start lines", || {
        loop_log(&a).contains(&format!("{ta}-1 started -> doing.running"))
            && loop_log(&b).contains(&format!("{tb}-1 started -> doing.running"))
    });
    let log_a = loop_log(&a);
    let log_b = loop_log(&b);
    assert!(
        log_a.contains(&format!("{ta}-1 started -> doing.running")),
        "{log_a}"
    );
    assert!(
        log_b.contains(&format!("{tb}-1 started -> doing.running")),
        "{log_b}"
    );
    assert!(
        !log_a.contains(&format!("{tb}-1 started")),
        "the lines crossed repositories: {log_a}"
    );
    assert!(
        !log_b.contains(&format!("{ta}-1 started")),
        "the lines crossed repositories: {log_b}"
    );

    assert!(hub.term(), "SIGTERM reaches the hub");
    assert_eq!(
        hub.wait_exit(Duration::from_secs(10))
            .and_then(|s| s.code()),
        Some(0)
    );
}

/// Edge 2: a repository whose `runner.lock` a test process holds is read-only with that pid;
/// after the holder exits, the next interval takes the lock and schedules it.
#[test]
fn a_held_runner_lock_is_read_only_until_the_holder_leaves() {
    let repo = Repo::new("hub-held");
    let store = setup(&repo);
    let task = ready_task(&store, "held");
    command_worker(&repo);
    slow_before_attempt(&repo, 2);
    let home = HubHome::new("hub-held-home", 1, 4);
    home.register(&[("repo", &repo.clone)]);

    let held = store
        .try_runner_lock()
        .expect("take runner.lock")
        .expect("runner.lock is free");
    let pid = std::process::id();
    let mut hub = home.spawn();
    let line = hub
        .wait_line_where(Duration::from_secs(10), |line| {
            status_for(line, "repo", &format!("read-only (pid {pid})"))
        })
        .unwrap_or_else(|| panic!("no read-only line naming {pid}: {:?}", hub.seen));
    assert!(line.contains("read-only"), "{line}");
    assert!(
        !is_doing(&store, &task),
        "a read-only repository was scheduled"
    );

    drop(held);
    wait_until(Duration::from_secs(10), "the hub takes runner.lock", || {
        !runner_lock_free(&repo)
    });
    wait_until(Duration::from_secs(10), "the task in doing", || {
        is_doing(&store, &task)
    });
    assert!(hub.term());
    assert_eq!(
        hub.wait_exit(Duration::from_secs(10))
            .and_then(|s| s.code()),
        Some(0)
    );
}

/// Edge 3: `[hub] concurrency = 1` with a ready task in each of two repositories: only one
/// task is in `doing` at a time, and the second starts when the first leaves `doing`.
#[test]
fn the_machine_cap_holds_one_task_at_a_time() {
    let a = Repo::new("hub-cap-a");
    let b = Repo::new("hub-cap-b");
    let sa = setup(&a);
    let sb = setup(&b);
    let ta = ready_task(&sa, "a");
    let tb = ready_task(&sb, "b");
    for repo in [&a, &b] {
        command_worker(repo);
        slow_before_attempt(repo, 3);
    }
    let home = HubHome::new("hub-cap", 1, 1);
    home.register(&[("a", &a.clone)]);
    let mut hub = home.spawn();
    wait_until(Duration::from_secs(10), "A's task in doing", || {
        is_doing(&sa, &ta)
    });

    // B joins while A's task is doing: the machine-wide count is at the cap, so B waits.
    home.register(&[("a", &a.clone), ("b", &b.clone)]);
    hub.wait_line_where(Duration::from_secs(10), |line| line.starts_with("b  "))
        .expect("B's status line");
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        !is_doing(&sb, &tb),
        "the machine-wide cap let a second task start"
    );

    // A's attempt ends (its before_attempt sleep), it leaves doing, and B starts.
    wait_until(Duration::from_secs(15), "A to leave doing", || {
        !is_doing(&sa, &ta)
    });
    wait_until(Duration::from_secs(15), "B's task in doing", || {
        is_doing(&sb, &tb)
    });

    assert!(hub.term());
    assert_eq!(
        hub.wait_exit(Duration::from_secs(10))
            .and_then(|s| s.code()),
        Some(0)
    );
}

/// Edge 4: a repository whose `[limits] concurrency` is 1 keeps one task in `doing` even when
/// the hub cap is 4.
#[test]
fn the_repository_limit_holds_one_task_even_under_a_large_hub_cap() {
    let repo = Repo::new("hub-repo-limit");
    let store = setup(&repo);
    let one = ready_task(&store, "one");
    let two = ready_task(&store, "two");
    command_worker(&repo);
    slow_before_attempt(&repo, 3);
    let home = HubHome::new("hub-repo-limit-home", 1, 4);
    home.register(&[("repo", &repo.clone)]);
    let mut hub = home.spawn();

    wait_until(Duration::from_secs(10), "one task in doing", || {
        doing_count(&store) == 1
    });
    std::thread::sleep(Duration::from_millis(2000));
    assert_eq!(
        doing_count(&store),
        1,
        "the repository's own limit let a second task start"
    );
    assert!(is_doing(&store, &one) || is_doing(&store, &two));

    assert!(hub.term());
    assert_eq!(
        hub.wait_exit(Duration::from_secs(10))
            .and_then(|s| s.code()),
        Some(0)
    );
}

/// Edge 5: a pass that fails in one repository writes its error to that repository's
/// `loop.log`; the other repository keeps scheduling.
#[test]
fn a_failed_pass_does_not_stop_the_other_repository() {
    let a = Repo::new("hub-fault-a");
    let b = Repo::new("hub-fault-b");
    let sa = setup(&a);
    let sb = setup(&b);
    let ta = ready_task(&sa, "a");
    let b1 = ready_task(&sb, "one");
    let b2 = ready_task(&sb, "two");
    for repo in [&a, &b] {
        command_worker(repo);
        slow_before_attempt(repo, 2);
    }
    let home = HubHome::new("hub-fault", 1, 4);
    home.register(&[("a", &a.clone), ("b", &b.clone)]);
    let mut hub = home.spawn();
    wait_until(Duration::from_secs(10), "both loops schedule", || {
        is_doing(&sa, &ta) && is_doing(&sb, &b1)
    });

    // A's config stops parsing while its loop runs; every later pass fails before the gate.
    tune(&a, "cancel_grace_secs = 10", "cancel_grace_secs = \"one\"");
    wait_until(
        Duration::from_secs(15),
        "A's loop.log carries the error",
        || loop_log(&a).contains("cancel_grace_secs"),
    );
    // B keeps scheduling: its first attempt ends and it claims the other ready task.
    wait_until(Duration::from_secs(20), "B's second task in doing", || {
        is_doing(&sb, &b2)
    });
    assert!(
        !runner_lock_free(&b),
        "B's loop released runner.lock after A's pass failed"
    );
    assert!(loop_log(&a).contains("radian arc: "), "{}", loop_log(&a));

    assert!(hub.term());
    assert_eq!(
        hub.wait_exit(Duration::from_secs(10))
            .and_then(|s| s.code()),
        Some(0)
    );
}

/// Edge 6: SIGTERM to the hub releases every repository's lock and `hub.lock`, and the
/// process exits `0`.
#[test]
fn sigterm_releases_every_lock_and_exits_zero() {
    let a = Repo::new("hub-term-a");
    let b = Repo::new("hub-term-b");
    for repo in [&a, &b] {
        setup(repo);
    }
    let home = HubHome::new("hub-term", 1, 4);
    home.register(&[("a", &a.clone), ("b", &b.clone)]);
    let mut hub = home.spawn();
    wait_until(Duration::from_secs(10), "both locks held", || {
        !runner_lock_free(&a) && !runner_lock_free(&b)
    });
    assert!(
        HubLock::holder(&home.hub_dir()).is_some(),
        "the hub holds hub.lock"
    );

    assert!(hub.term(), "SIGTERM reaches the hub");
    let status = hub
        .wait_exit(Duration::from_secs(10))
        .expect("the hub exits on SIGTERM");
    assert_eq!(status.code(), Some(0));
    assert!(runner_lock_free(&a), "A's runner.lock stayed held");
    assert!(runner_lock_free(&b), "B's runner.lock stayed held");
    assert_eq!(
        HubLock::probe(&home.hub_dir()),
        None,
        "hub.lock stayed held"
    );
}

/// Edge 7: a rebuilt binary makes the hub `exec` once after the passes in flight end, and
/// every lock is released before it (the new process takes them again, or its own `hub.lock`
/// attempt would fail).
#[test]
fn a_rebuilt_binary_restarts_the_hub_once() {
    let repo = Repo::new("hub-reexec");
    setup(&repo);
    let home = HubHome::new("hub-reexec-home", 5, 4);
    home.register(&[("repo", &repo.clone)]);

    let dir = TempDir::new("hub-reexec-bin");
    let program = dir.path().join("radian");
    fs::copy(radian_bin(), &program).expect("copy the radian binary");
    let mut hub = home.spawn_from(&program);
    wait_until(Duration::from_secs(10), "the hub's first pass", || {
        loop_log(&repo).contains("no transition")
    });
    let pid = hub.id();
    assert!(!runner_lock_free(&repo), "the hub holds runner.lock");

    let replacement = dir.path().join("radian.new");
    fs::copy(radian_bin(), &replacement).expect("copy the new radian binary");
    fs::rename(&replacement, &program).expect("replace the running binary");

    wait_until(Duration::from_secs(30), "the restart line", || {
        loop_log(&repo).contains(radian_arc::run::RESTART_TEXT)
    });
    wait_until(
        Duration::from_secs(30),
        "the new hub took both locks",
        || {
            !runner_lock_free(&repo)
                && HubLock::holder(&home.hub_dir()) == Some(pid)
                && Store::at(&repo.clone).runner_lock_pid() == Some(pid)
        },
    );
    assert_eq!(
        loop_log(&repo)
            .matches(radian_arc::run::RESTART_TEXT)
            .count(),
        1,
        "the hub restarted more than once: {}",
        loop_log(&repo)
    );

    assert!(hub.term());
    assert_eq!(
        hub.wait_exit(Duration::from_secs(10))
            .and_then(|s| s.code()),
        Some(0)
    );
}

/// Edge 8: `hub add` of a third repository while the hub runs schedules it within two
/// intervals; `hub remove` releases a repository's lock within two intervals.
#[test]
fn add_and_remove_reach_a_running_hub() {
    let a = Repo::new("hub-add-a");
    let b = Repo::new("hub-add-b");
    let sa = setup(&a);
    let sb = setup(&b);
    let ta = ready_task(&sa, "a");
    let tb = ready_task(&sb, "b");
    for repo in [&a, &b] {
        command_worker(repo);
        slow_before_attempt(repo, 2);
    }
    let home = HubHome::new("hub-add-home", 1, 4);
    home.register(&[("a", &a.clone)]);
    let mut hub = home.spawn();
    wait_until(Duration::from_secs(10), "A scheduled", || {
        is_doing(&sa, &ta)
    });

    let added = home.cli(&["hub", "add", b.clone.to_str().unwrap(), "--name", "b"]);
    assert_eq!(added.code, 0, "{added:?}");
    wait_until(Duration::from_secs(10), "B's lock and task", || {
        !runner_lock_free(&b) && is_doing(&sb, &tb)
    });

    let removed = home.cli(&["hub", "remove", "a"]);
    assert_eq!(removed.code, 0, "{removed:?}");
    wait_until(Duration::from_secs(10), "A's lock release", || {
        runner_lock_free(&a)
    });

    assert!(hub.term());
    assert_eq!(
        hub.wait_exit(Duration::from_secs(10))
            .and_then(|s| s.code()),
        Some(0)
    );
}

/// Edge 9: `radian arc serve` exits `2` with one stderr line naming `radian hub`, and takes
/// no lock.
#[test]
fn arc_serve_is_removed_and_points_at_the_hub() {
    let repo = Repo::new("hub-serve-removed");
    setup(&repo);
    for args in [
        vec!["arc", "serve"],
        vec!["arc", "serve", "--interval", "5"],
    ] {
        let run = repo.radian(&args);
        assert_eq!(run.code, 2, "{args:?}: {run:?}");
        assert_eq!(
            run.stderr.trim_end(),
            "radian arc: `serve` is now `radian hub`",
            "{args:?}: {run:?}"
        );
        assert!(run.stdout.is_empty(), "{args:?}: {run:?}");
        assert!(
            runner_lock_free(&repo),
            "the refused command took runner.lock"
        );
    }
}

/// Edge 10: a second `radian hub` exits `1` naming the holder's pid and touches no
/// repository lock.
#[test]
fn a_second_hub_exits_one_and_names_the_holder() {
    let a = Repo::new("hub-second-a");
    let b = Repo::new("hub-second-b");
    for repo in [&a, &b] {
        setup(repo);
    }
    let home = HubHome::new("hub-second", 1, 4);
    home.register(&[("a", &a.clone), ("b", &b.clone)]);
    let mut hub = home.spawn();
    wait_until(Duration::from_secs(10), "both locks held", || {
        !runner_lock_free(&a) && !runner_lock_free(&b)
    });
    let holder = HubLock::holder(&home.hub_dir()).expect("the hub wrote its pid");
    let a_pid = Store::at(&a.clone).runner_lock_pid();
    let b_pid = Store::at(&b.clone).runner_lock_pid();
    let second = home
        .run_within(&["hub"], Duration::from_secs(10))
        .expect("the second hub exits instead of running");
    assert_eq!(second.code, 1, "{second:?}");
    assert!(
        second.stderr.contains(&format!("pid {holder}")),
        "the refusal does not name the holder: {second:?}"
    );
    assert!(second.stdout.is_empty(), "{second:?}");
    assert_eq!(Store::at(&a.clone).runner_lock_pid(), a_pid);
    assert_eq!(Store::at(&b.clone).runner_lock_pid(), b_pid);

    assert!(hub.term());
    assert_eq!(
        hub.wait_exit(Duration::from_secs(10))
            .and_then(|s| s.code()),
        Some(0)
    );
}

/// Edge 11: one repository's `before_attempt` sleeps longer than the interval; the other
/// repository's passes keep their cadence during the sleep (the loops are OS threads, not
/// async tasks on one runtime).
#[test]
fn a_slow_pass_does_not_delay_another_repository() {
    let slow = Repo::new("hub-thread-slow");
    let quick = Repo::new("hub-thread-quick");
    let s_slow = setup(&slow);
    let s_quick = setup(&quick);
    let slow_task = ready_task(&s_slow, "slow");
    let _first = ready_task(&s_quick, "one");
    let second = ready_task(&s_quick, "two");
    command_worker(&slow);
    slow_before_attempt(&slow, 5);
    command_worker(&quick);
    let home = HubHome::new("hub-thread", 1, 4);
    home.register(&[("slow", &slow.clone), ("quick", &quick.clone)]);
    let mut hub = home.spawn();

    let started = Instant::now();
    wait_until(Duration::from_secs(10), "the slow pass's claim", || {
        is_doing(&s_slow, &slow_task)
    });
    // The quick repository starts its first task, finishes it, and starts the second while
    // the slow repository's wrapper is still in `before_attempt`.
    wait_until(
        Duration::from_secs(4),
        "the quick repository's second task",
        || is_doing(&s_quick, &second),
    );
    assert!(
        is_doing(&s_slow, &slow_task),
        "the slow pass ended before the quick repository advanced"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the quick repository waited for the slow pass"
    );

    assert!(hub.term());
    assert_eq!(
        hub.wait_exit(Duration::from_secs(10))
            .and_then(|s| s.code()),
        Some(0)
    );
}
