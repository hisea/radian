//! The store against a real git repository: `init`, the exclude lines, the ids, the
//! transactions, the ledger, outside changes, and `adopt`.
//!
//! Every test builds a throwaway repository with a throwaway bare `origin` under the
//! system temp directory, and removes it at the end.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use radian_arc::store::model::{self, TaskFile};
use radian_arc::store::{self, Error, Exit, Store, config};

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-arc-store-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the temp dir is creatable");
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn repo(&self) -> PathBuf {
        self.dir.join("repo")
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.path(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the parent is creatable");
        }
        fs::write(&path, text).expect("the file is writable");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "arc test")
        .env("GIT_AUTHOR_EMAIL", "arc@example.com")
        .env("GIT_COMMITTER_NAME", "arc test")
        .env("GIT_COMMITTER_EMAIL", "arc@example.com")
        .args(["-c", "core.hooksPath="])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repository with one commit on `main`, pushed to a local bare `origin`.
fn fixture(label: &str) -> Scratch {
    let scratch = Scratch::new(label);
    git(&scratch.dir, &["init", "-q", "--bare", "origin.git"]);
    git(
        &scratch.path("origin.git"),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
    git(&scratch.dir, &["init", "-q", "repo"]);
    git(
        &scratch.repo(),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
    scratch.write("repo/README.md", "hello\n");
    git(&scratch.repo(), &["add", "README.md"]);
    git(&scratch.repo(), &["commit", "-q", "-m", "first"]);
    let origin = scratch.path("origin.git");
    git(
        &scratch.repo(),
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&scratch.repo(), &["push", "-q", "-u", "origin", "main"]);
    git(&scratch.repo(), &["remote", "set-head", "origin", "main"]);
    scratch
}

fn initialized(scratch: &Scratch) -> Store {
    Store::init(&scratch.repo(), false).expect("init succeeds");
    Store::at(scratch.repo())
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("the file reads")
}

#[test]
fn init_creates_the_layout_and_writes_the_config() {
    let scratch = fixture("init");
    let report = Store::init(&scratch.repo(), false).expect("init succeeds");

    assert_eq!(report.repository, fs::canonicalize(scratch.repo()).unwrap());
    assert_eq!(report.remote, "origin");
    assert_eq!(report.main_branch, "main");
    assert!(report.config_created);
    assert!(!report.exclude_added.is_empty());
    assert_eq!(report.checks.len(), 4);
    for name in [
        "provider credential",
        "`origin` reachable",
        "push to `origin` `main`",
        "workspace sandbox",
    ] {
        assert!(
            report.checks.iter().any(|check| check.name == name),
            "missing check {name}"
        );
    }
    // A local bare remote is reachable and accepts a dry-run push; the sandbox probe is a
    // real one, so its outcome is whatever this machine allows.
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "`origin` reachable" && check.passed())
    );

    let root = scratch.repo().join(".radian/arc");
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
        assert!(root.join(sub).is_dir(), "{sub} is created");
    }
    let config = config::load(&root.join("config.toml")).expect("the config parses");
    assert_eq!(config.repository.main_branch, "main");
    assert_eq!(config.repository.remote, "origin");
    assert!(
        report.remote_url.ends_with("origin.git"),
        "{}",
        report.remote_url
    );
}

#[test]
fn a_second_init_keeps_the_config_and_adds_no_exclude_line_twice() {
    let scratch = fixture("init-twice");
    Store::init(&scratch.repo(), false).expect("init succeeds");
    let config_path = scratch.repo().join(".radian/arc/config.toml");
    fs::write(
        &config_path,
        "# a hand-edited config\n".to_owned() + &read(&config_path),
    )
    .expect("the config is writable");
    let before = read(&config_path);

    let report = Store::init(&scratch.repo(), false).expect("the second init succeeds");
    assert!(!report.config_created);
    assert!(
        report.exclude_added.is_empty(),
        "{:?}",
        report.exclude_added
    );
    assert_eq!(read(&config_path), before, "the config file is untouched");

    let exclude = read(&report.exclude_path);
    for line in store::exclude_lines(false) {
        assert_eq!(
            exclude.lines().filter(|present| *present == line).count(),
            1,
            "`{line}` appears exactly once in:\n{exclude}"
        );
    }
}

#[test]
fn the_track_variant_keeps_config_plans_and_tasks_visible_to_git() {
    let scratch = fixture("init-track");
    let report = Store::init(&scratch.repo(), true).expect("init succeeds");
    let exclude = read(&report.exclude_path);
    for line in store::exclude_lines(true) {
        assert!(exclude.contains(&line), "`{line}` is excluded");
    }
    for line in [
        "/.radian/arc/config.toml",
        "/.radian/arc/plans/",
        "/.radian/arc/tasks/",
    ] {
        assert!(!exclude.contains(line), "`{line}` stays tracked");
    }
    assert!(exclude.contains("/.radian/arc/gates/"), "gates are local");
}

#[test]
fn without_setup_every_open_names_init_and_exits_three() {
    let scratch = fixture("no-setup");
    let error = Store::open(&scratch.repo()).expect_err("no config yet");
    assert_eq!(error, Error::NotInitialized);
    assert_eq!(error.exit(), Exit::State);
    assert!(
        error.message().contains("radian arc init"),
        "{}",
        error.message()
    );
}

#[test]
fn an_unknown_config_key_stops_the_store() {
    let scratch = fixture("bad-config");
    initialized(&scratch);
    let config_path = scratch.repo().join(".radian/arc/config.toml");
    let text = read(&config_path).replace("[limits]\n", "[limits]\nbudget = 3\n");
    fs::write(&config_path, text).expect("the config is writable");
    let error = Store::open(&scratch.repo()).expect_err("an unknown key fails");
    assert_eq!(error.exit(), Exit::Error);
    assert!(error.message().contains("budget"), "{}", error.message());
}

#[test]
fn a_slug_outside_the_rules_is_a_usage_error() {
    let scratch = fixture("slugs");
    let store = initialized(&scratch);
    for slug in ["", "Upper", "a_b", "-a", "a-", &"a".repeat(49)] {
        let error = store
            .add_plan(slug, "title", "request")
            .expect_err("a bad slug is refused");
        assert_eq!(error.exit(), Exit::Usage, "{slug}");
    }
    let error = store
        .add_plan("ok", "  ", "request")
        .expect_err("an empty title is refused");
    assert_eq!(error.exit(), Exit::Usage);
    assert!(store.add_plan("ok", "title", "request").is_ok());
}

#[test]
fn the_number_is_the_next_free_one_of_that_kind() {
    let scratch = fixture("numbers");
    let store = initialized(&scratch);
    let first = store.add_plan("a", "A", "request").unwrap();
    let second = store.add_plan("b", "B", "request").unwrap();
    assert_eq!(first, "PL0001-a");
    assert_eq!(second, "PL0002-b");
    let task = store
        .add_task(&first, "t", "T", Vec::new(), false, Vec::new(), "brief")
        .unwrap();
    assert_eq!(
        task, "TK0001-t",
        "a task counts in its own sequence, not above the plans"
    );
    // A plan file removed by hand frees its number only if the ledger entry is gone too.
    fs::remove_file(scratch.repo().join(".radian/arc/plans/PL0002-b.md")).unwrap();
    let third = store.add_plan("c", "C", "request").unwrap();
    assert_eq!(
        third, "PL0003-c",
        "the ledger keeps PL0002 reserved; the task's TK0001 does not count"
    );
}

/// Plans and tasks count in separate sequences, so the same number and slug occur once in
/// each kind and the prefix tells them apart.
#[test]
fn plans_and_tasks_count_in_separate_sequences() {
    let scratch = fixture("separate-numbers");
    let store = initialized(&scratch);
    let plan = store.add_plan("x", "X", "request").unwrap();
    assert_eq!(plan, "PL0001-x");
    let task = store
        .add_task(&plan, "x", "T", Vec::new(), false, Vec::new(), "brief")
        .unwrap();
    assert_eq!(task, "TK0001-x", "the first task is TK0001");
    let second = store.add_plan("x", "X", "request").unwrap();
    assert_eq!(
        second, "PL0002-x",
        "the next plan continues the plan sequence"
    );
}

/// A custom `[ids]` section gives the new ids its prefixes.
#[test]
fn the_configured_prefixes_name_new_ids() {
    let scratch = fixture("custom-prefixes");
    let store = initialized(&scratch);
    let config_path = scratch.repo().join(".radian/arc/config.toml");
    let text = read(&config_path)
        .replace("plan_prefix = \"PL\"", "plan_prefix = \"P\"")
        .replace("task_prefix = \"TK\"", "task_prefix = \"T\"");
    fs::write(&config_path, text).unwrap();
    let plan = store.add_plan("x", "X", "request").unwrap();
    assert_eq!(plan, "P0001-x");
    let task = store
        .add_task(&plan, "y", "Y", Vec::new(), false, Vec::new(), "brief")
        .unwrap();
    assert_eq!(task, "T0001-y");
}

/// An item made under an earlier prefix is still read and adopted, and the next number of
/// its kind continues above it.
#[test]
fn an_old_prefix_still_works_and_its_number_still_counts() {
    let scratch = fixture("old-prefix");
    let store = initialized(&scratch);
    let plan = store.add_plan("x", "X", "request").unwrap();
    let task = store
        .add_task(&plan, "t", "T", Vec::new(), false, Vec::new(), "brief")
        .unwrap();
    let config_path = scratch.repo().join(".radian/arc/config.toml");
    let text = read(&config_path)
        .replace("plan_prefix = \"PL\"", "plan_prefix = \"Q\"")
        .replace("task_prefix = \"TK\"", "task_prefix = \"T\"");
    fs::write(&config_path, text).unwrap();

    // The old plan and task still show, and a hand edit to the plan is adoptable under its
    // old id.
    assert!(matches!(
        store.show(&plan).expect("an old-prefix plan still reads"),
        store::Item::Plan(_)
    ));
    assert!(matches!(
        store.show(&task).expect("an old-prefix task still reads"),
        store::Item::Task(_)
    ));
    let path = scratch
        .repo()
        .join(".radian/arc/plans")
        .join(format!("{plan}.md"));
    fs::write(&path, read(&path).replace("request", "a better request")).unwrap();
    store.adopt(&plan).expect("the old id is adopted");

    // Q0001 is taken by the old plan: the next plan is Q0002, not Q0001. The same for the
    // task: T0001 is taken, so the next task is T0002.
    let next_plan = store.add_plan("y", "Y", "request").unwrap();
    assert_eq!(next_plan, "Q0002-y");
    let next_task = store
        .add_task(&plan, "y", "Y", Vec::new(), false, Vec::new(), "brief")
        .unwrap();
    assert_eq!(next_task, "T0002-y");
}

/// An id without a prefix is malformed: exit 2, and nothing is read or written.
#[test]
fn an_id_without_a_prefix_is_a_usage_error() {
    let scratch = fixture("no-prefix");
    let store = initialized(&scratch);
    let plan = store.add_plan("x", "X", "request").unwrap();
    for id in ["0001-x", "0001", "-/x"] {
        let error = store.show(id).expect_err("a malformed id is refused");
        assert_eq!(error.exit(), Exit::Usage, "{id}: {}", error.message());
    }
    let error = store
        .add_task("0001-x", "t", "T", Vec::new(), false, Vec::new(), "brief")
        .expect_err("a malformed plan id is refused");
    assert_eq!(error.exit(), Exit::Usage);
    assert!(store.show(&plan).is_ok());
}

/// An id a person made name both a plan and a task: every command that takes either refuses
/// it (exit 3, naming both files) and changes neither file.
#[test]
fn an_id_that_names_both_a_plan_and_a_task_is_refused() {
    let scratch = fixture("ambiguous-id");
    let store = initialized(&scratch);
    let id = store.add_plan("x", "X", "request").unwrap();
    assert_eq!(id, "PL0001-x");
    // A person writes a task file with the plan's id.
    let task = TaskFile::new(
        id.clone(),
        id.clone(),
        "T".to_owned(),
        Vec::new(),
        Some(false),
        Vec::new(),
        "brief",
    );
    scratch.write(&format!("repo/.radian/arc/tasks/{id}.md"), &task.to_text());
    let plan_path = scratch
        .repo()
        .join(".radian/arc/plans")
        .join(format!("{id}.md"));
    let task_path = scratch
        .repo()
        .join(".radian/arc/tasks")
        .join(format!("{id}.md"));
    let plan_before = read(&plan_path);
    let task_before = read(&task_path);

    let show = store.show(&id).expect_err("show refuses an ambiguous id");
    assert_eq!(show.exit(), Exit::State, "{}", show.message());
    for path in [&plan_path, &task_path] {
        assert!(
            show.message().contains(&path.display().to_string()),
            "{} names {}",
            show.message(),
            path.display()
        );
    }

    let adopt = store.adopt(&id).expect_err("adopt refuses an ambiguous id");
    assert_eq!(adopt.exit(), Exit::State, "{}", adopt.message());

    let reset = store::attempt_ops::reset_budget(&store, &id)
        .expect_err("reset-budget refuses an ambiguous id");
    assert_eq!(reset.exit(), Exit::State, "{}", reset.message());

    assert_eq!(read(&plan_path), plan_before, "the plan file is unchanged");
    assert_eq!(read(&task_path), task_before, "the task file is unchanged");
}

#[test]
fn concurrent_writers_never_take_the_same_number() {
    let scratch = fixture("concurrent");
    initialized(&scratch);
    let repo = scratch.repo();
    let mut handles = Vec::new();
    for n in 0..4 {
        let repo = repo.clone();
        handles.push(std::thread::spawn(move || {
            let store = Store::at(repo);
            store
                .add_plan("same", "title", &format!("request {n}"))
                .expect("the plan is added")
        }));
    }
    let mut ids: Vec<String> = handles
        .into_iter()
        .map(|handle| handle.join().expect("the thread finishes"))
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 4, "four distinct ids: {ids:?}");
    // The same slug in every writer: only the number can tell them apart.
    let numbers: Vec<String> = ids
        .iter()
        .map(|id| id.split_once('-').expect("an id").0.to_owned())
        .collect();
    assert_eq!(
        numbers,
        vec!["PL0001", "PL0002", "PL0003", "PL0004"],
        "the numbers are sequential"
    );
    let store = Store::at(scratch.repo());
    for id in &ids {
        let loaded = store.load_plan(id).expect("the plan reads");
        assert!(!loaded.changed_outside_arc, "{id} matches its ledger");
        let ledger = read(
            &scratch
                .repo()
                .join(".radian/arc/ledger/plans")
                .join(format!("{id}.json")),
        );
        let ledger: serde_json::Value = serde_json::from_str(&ledger).expect("the ledger parses");
        assert_eq!(ledger["state_version"], 1);
        assert_eq!(
            ledger["sha256"].as_str().expect("a hash").len(),
            64,
            "{ledger}"
        );
    }
}

#[test]
fn a_hand_edited_brief_is_shown_as_changed_and_blocks_a_change() {
    let scratch = fixture("outside");
    let store = initialized(&scratch);
    let plan = store.add_plan("p", "P", "request").unwrap();
    let task = store
        .add_task(&plan, "t", "T", Vec::new(), false, Vec::new(), "the brief")
        .unwrap();
    let task_path = scratch
        .repo()
        .join(".radian/arc/tasks")
        .join(format!("{task}.md"));
    let text = read(&task_path);
    fs::write(&task_path, text.replace("the brief", "a different brief")).unwrap();

    let loaded = store.load_task(&task).expect("the task reads");
    assert!(loaded.changed_outside_arc, "the edit is seen");
    let view = store.plan_view(&plan).expect("the plan reads");
    assert!(view.tasks[0].changed_outside_arc);

    let error = store
        .change_task(&task, |task| {
            task.attempt += 1;
            Ok(())
        })
        .expect_err("a changed file is not changed further");
    assert_eq!(error.exit(), Exit::State);
    assert!(error.message().contains("changed outside arc"));

    // The plan itself is untouched, so a new task on it still works.
    store
        .add_task(&plan, "u", "U", Vec::new(), false, Vec::new(), "brief")
        .expect("the plan did not change outside arc");
}

#[test]
fn adopt_accepts_a_brief_edit_and_refuses_a_state_edit() {
    let scratch = fixture("adopt");
    let store = initialized(&scratch);
    let plan = store.add_plan("p", "P", "request").unwrap();
    let task = store
        .add_task(&plan, "t", "T", Vec::new(), false, Vec::new(), "the brief")
        .unwrap();
    let task_path = scratch
        .repo()
        .join(".radian/arc/tasks")
        .join(format!("{task}.md"));

    // A brief edit is arc's to accept.
    let text = read(&task_path);
    fs::write(&task_path, text.replace("the brief", "a better brief")).unwrap();
    let report = store.adopt(&task).expect("a brief edit is adoptable");
    assert!(report.changed);
    assert_eq!(report.kind, "task");
    assert!(!store.load_task(&task).unwrap().changed_outside_arc);
    assert!(
        !store
            .adopt(&task)
            .expect("a second adopt is a no-op")
            .changed
    );

    // A state edit is not.
    let text = read(&task_path);
    let edited = text
        .replace("state: todo", "state: doing")
        .replace("phase: ready", "phase: running");
    fs::write(&task_path, edited).unwrap();
    let error = store.adopt(&task).expect_err("a state edit is refused");
    assert_eq!(error.exit(), Exit::State);
    assert!(error.message().contains("state"), "{}", error.message());
    assert!(error.message().contains("phase"), "{}", error.message());
    // Nothing was recorded: the file is still outside arc.
    assert!(store.load_task(&task).unwrap().changed_outside_arc);

    // An attempt edit is refused too.
    let text = read(&task_path);
    fs::write(
        &task_path,
        text.replace("state: doing", "state: todo")
            .replace("phase: running", "phase: ready")
            .replace("attempt: 0", "attempt: 1"),
    )
    .unwrap();
    let error = store.adopt(&task).expect_err("an attempt edit is refused");
    assert!(error.message().contains("attempt"), "{}", error.message());
}

#[test]
fn a_plan_file_follows_the_same_rules() {
    let scratch = fixture("plan-adopt");
    let store = initialized(&scratch);
    let plan = store.add_plan("p", "P", "the request").unwrap();
    let plan_path = scratch
        .repo()
        .join(".radian/arc/plans")
        .join(format!("{plan}.md"));

    let text = read(&plan_path);
    fs::write(&plan_path, text.replace("the request", "a better request")).unwrap();
    assert!(store.load_plan(&plan).unwrap().changed_outside_arc);
    store.adopt(&plan).expect("a request edit is adoptable");

    let text = read(&plan_path);
    fs::write(
        &plan_path,
        text.replace("status: draft", "status: confirmed"),
    )
    .unwrap();
    let error = store.adopt(&plan).expect_err("a status edit is refused");
    assert!(error.message().contains("status"), "{}", error.message());
}

#[test]
fn add_task_to_a_confirmed_plan_is_refused() {
    let scratch = fixture("confirmed");
    let store = initialized(&scratch);
    let plan = store.add_plan("p", "P", "request").unwrap();
    store
        .change_plan(&plan, |plan| {
            plan.status = "confirmed".to_owned();
            Ok(())
        })
        .expect("a confirmed plan is one state change");
    assert_eq!(store.load_plan(&plan).unwrap().item.status, "confirmed");
    let error = store
        .add_task(&plan, "t", "T", Vec::new(), false, Vec::new(), "brief")
        .expect_err("a confirmed plan gets no new tasks");
    assert_eq!(error.exit(), Exit::State);
    assert!(error.message().contains("confirmed"), "{}", error.message());
}

#[test]
fn after_may_name_a_task_of_the_plan_or_a_done_task() {
    let scratch = fixture("after");
    let store = initialized(&scratch);
    let plan = store.add_plan("p", "P", "request").unwrap();
    let other = store.add_plan("o", "O", "request").unwrap();
    let first = store
        .add_task(&plan, "first", "F", Vec::new(), false, Vec::new(), "brief")
        .unwrap();
    let second_of_other = store
        .add_task(&other, "other", "O", Vec::new(), false, Vec::new(), "brief")
        .unwrap();

    // Same plan: fine.
    let added = store
        .add_task(
            &plan,
            "second",
            "S",
            vec![first.clone()],
            true,
            vec!["mutation".into()],
            "brief",
        )
        .expect("a task of the same plan is a valid dependency");
    let task = store.load_task(&added).unwrap().item;
    assert_eq!(task.after, vec![first]);
    assert_eq!(task.allow_noop, Some(true));
    assert_eq!(task.extra_stages, vec!["mutation".to_owned()]);

    // Another plan's task that is not done: refused.
    let error = store
        .add_task(
            &plan,
            "x",
            "X",
            vec![second_of_other.clone()],
            false,
            Vec::new(),
            "brief",
        )
        .expect_err("a task of another plan that is not done is refused");
    assert_eq!(error.exit(), Exit::State);

    // Once it is done: allowed.
    store
        .change_task(&second_of_other, |task| {
            task.state = model::TaskState::Done;
            task.phase = "merged".to_owned();
            Ok(())
        })
        .unwrap();
    store
        .add_task(
            &plan,
            "y",
            "Y",
            vec![second_of_other],
            false,
            Vec::new(),
            "brief",
        )
        .expect("a done task is a valid dependency");

    // A missing task: refused.
    let error = store
        .add_task(
            &plan,
            "z",
            "Z",
            vec!["TK0042-missing".into()],
            false,
            Vec::new(),
            "brief",
        )
        .expect_err("a missing dependency is refused");
    assert_eq!(error.exit(), Exit::State);
}

#[test]
fn an_unknown_stage_in_add_task_is_a_usage_error() {
    let scratch = fixture("stage");
    let store = initialized(&scratch);
    let plan = store.add_plan("p", "P", "request").unwrap();
    let error = store
        .add_task(
            &plan,
            "t",
            "T",
            Vec::new(),
            false,
            vec!["integration".into()],
            "brief",
        )
        .expect_err("integration is not a profile stage");
    assert_eq!(error.exit(), Exit::Usage);
}

#[test]
fn a_change_replaces_the_file_in_one_rename() {
    use std::os::unix::fs::MetadataExt;
    let scratch = fixture("rename");
    let store = initialized(&scratch);
    let plan = store.add_plan("p", "P", "request").unwrap();
    let task = store
        .add_task(&plan, "t", "T", Vec::new(), false, Vec::new(), "brief")
        .unwrap();
    let path = scratch
        .repo()
        .join(".radian/arc/tasks")
        .join(format!("{task}.md"));
    let before = fs::metadata(&path).unwrap().ino();
    store
        .change_task(&task, |task| {
            task.attempt += 1;
            Ok(())
        })
        .unwrap();
    let after = fs::metadata(&path).unwrap().ino();
    assert_ne!(
        before, after,
        "a change replaces the file with a rename, never writes it in place"
    );
    let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "no temp file is left: {leftovers:?}");
}

#[test]
fn the_ledger_and_the_last_copy_are_written_for_every_file() {
    let scratch = fixture("ledger");
    let store = initialized(&scratch);
    let plan = store.add_plan("p", "P", "request").unwrap();
    let task = store
        .add_task(&plan, "t", "T", Vec::new(), false, Vec::new(), "brief")
        .unwrap();
    let root = scratch.repo().join(".radian/arc/ledger");
    for path in [
        root.join("plans").join(format!("{plan}.json")),
        root.join("plans").join(format!("{plan}.last.md")),
        root.join("tasks").join(format!("{task}.json")),
        root.join("tasks").join(format!("{task}.last.md")),
    ] {
        assert!(path.is_file(), "{} is written", path.display());
    }
    let ledger: serde_json::Value =
        serde_json::from_str(&read(&root.join("tasks").join(format!("{task}.json")))).unwrap();
    assert_eq!(ledger["state_version"], 1);
    let last = read(&root.join("tasks").join(format!("{task}.last.md")));
    assert_eq!(
        last,
        read(
            &scratch
                .repo()
                .join(".radian/arc/tasks")
                .join(format!("{task}.md"))
        )
    );
    // The plan's state version moved when the task was appended to it.
    let plan_ledger: serde_json::Value =
        serde_json::from_str(&read(&root.join("plans").join(format!("{plan}.json")))).unwrap();
    assert_eq!(plan_ledger["state_version"], 2);
}

#[test]
fn the_runner_lock_is_exclusive_within_one_host() {
    let scratch = fixture("runner");
    let store = initialized(&scratch);
    let held = store.try_runner_lock().expect("the lock file opens");
    assert!(held.is_some(), "the first taker gets it");
    let other = Store::at(scratch.repo());
    assert!(
        other.try_runner_lock().unwrap().is_none(),
        "a second scheduling loop does not"
    );
    drop(held);
    assert!(
        other.try_runner_lock().unwrap().is_some(),
        "and it is free again"
    );
}

#[test]
fn a_changed_plan_blocks_adding_a_task() {
    let scratch = fixture("changed-plan");
    let store = initialized(&scratch);
    let plan = store.add_plan("p", "P", "the request").unwrap();
    let path = scratch
        .repo()
        .join(".radian/arc/plans")
        .join(format!("{plan}.md"));
    let text = read(&path);
    fs::write(&path, text.replace("the request", "a hand edit")).unwrap();

    let error = store
        .add_task(&plan, "t", "T", Vec::new(), false, Vec::new(), "brief")
        .expect_err("a plan changed outside arc is not acted on");
    assert_eq!(error.exit(), Exit::State);
    assert!(error.message().contains("changed outside arc"));
    assert!(store.plans().unwrap()[0].plan.changed_outside_arc);

    // Adopting the edit makes the plan usable again.
    store.adopt(&plan).expect("a request edit is adoptable");
    store
        .add_task(&plan, "t", "T", Vec::new(), false, Vec::new(), "brief")
        .expect("the plan matches its ledger again");
}

#[test]
fn init_writes_the_default_worker_brief_once_and_keeps_an_edit() {
    let scratch = fixture("init-worker-brief");
    let report = Store::init(&scratch.repo(), false).expect("init succeeds");
    let path = scratch.repo().join(".radian/arc/worker-brief.md");
    assert!(report.worker_brief_created);
    assert_eq!(
        report.worker_brief_path,
        fs::canonicalize(scratch.repo())
            .expect("the repository canonicalizes")
            .join(".radian/arc/worker-brief.md")
    );
    assert_eq!(
        read(&path),
        store::WORKER_BRIEF_DEFAULT,
        "init writes the default"
    );

    fs::write(&path, "my own standing instructions\n").expect("the file is writable");
    let report = Store::init(&scratch.repo(), false).expect("the second init succeeds");
    assert!(!report.worker_brief_created);
    assert_eq!(
        read(&path),
        "my own standing instructions\n",
        "init never overwrites an edited file"
    );
}

/// The manual quotes the default `worker-brief.md` `init` writes, and the code holds it in
/// one place: a change to the constant without the page fails here (ADR 0048 decision 7).
#[test]
fn the_manual_quotes_the_default_worker_brief() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate lives in <root>/crates/radian-arc");
    let page = read(&root.join("docs/user/arc/files.md"));
    let quoted = format!("```text\n{}\n```", store::WORKER_BRIEF_DEFAULT);
    assert!(
        page.contains(&quoted),
        "docs/user/arc/files.md must quote WORKER_BRIEF_DEFAULT in a `text` block"
    );
}

/// `init` writes no file in the repository's git dir but `.git/info/exclude`: the fetch lock
/// appears with the first fetch arc runs (ADR 0048 decision 10). The manual must say so, or
/// the `init` section reads as a promise that `init` creates the lock.
#[test]
fn the_manual_says_the_first_fetch_creates_the_lock() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate lives in <root>/crates/radian-arc");
    let page = read(&root.join("docs/user/arc/planning.md"));
    let line = page
        .lines()
        .find(|line| line.contains("radian-arc-fetch.lock"))
        .expect("docs/user/arc/planning.md names the fetch lock");
    assert!(
        line.contains("first fetch"),
        "the page must say when the lock appears, not leave it to `init`: {line}"
    );
}

#[test]
fn init_refuses_outside_a_repository_and_without_an_origin_remote() {
    let scratch = Scratch::new("init-errors");
    let error = Store::init(&scratch.path("plain"), false).expect_err("no repository");
    assert_eq!(error.exit(), Exit::Error);
    assert!(error.message().contains("git"), "{}", error.message());

    fs::create_dir_all(scratch.repo()).unwrap();
    git(&scratch.repo(), &["init", "-q"]);
    let error = Store::init(&scratch.repo(), false).expect_err("no origin remote");
    assert_eq!(error.exit(), Exit::Error);
    assert!(error.message().contains("origin"), "{}", error.message());
    assert!(!scratch.repo().join(".radian/arc/config.toml").exists());
}

/// A bare remote named `name` with `main`, added to the repository `repo` and pushed to, with
/// its `HEAD` set, so `init` can read the main branch from it.
fn add_remote(scratch: &Scratch, repo: &Path, name: &str) {
    let bare = format!("{name}.git");
    git(&scratch.dir, &["init", "-q", "--bare", &bare]);
    let path = scratch.path(&bare);
    git(&path, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(repo, &["remote", "add", name, path.to_str().unwrap()]);
    git(repo, &["push", "-q", "-u", name, "main"]);
    git(repo, &["remote", "set-head", name, "main"]);
}

/// A repository whose only remote is `upstream` initializes: `init` uses the remote it finds
/// instead of a literal `origin`, and the generated `after_merge` lines name it (ADR 0048
/// decision 10).
#[test]
fn init_uses_a_remote_that_is_not_origin() {
    let scratch = Scratch::new("init-other-remote");
    git(&scratch.dir, &["init", "-q", "repo"]);
    git(
        &scratch.repo(),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
    scratch.write("repo/README.md", "hello\n");
    git(&scratch.repo(), &["add", "README.md"]);
    git(&scratch.repo(), &["commit", "-q", "-m", "first"]);
    add_remote(&scratch, &scratch.repo(), "upstream");

    let report = Store::init(&scratch.repo(), false).expect("init succeeds without origin");
    assert_eq!(report.remote, "upstream");
    assert_eq!(report.main_branch, "main");
    assert!(report.config_created);
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "`upstream` reachable" && check.passed()),
        "{:?}",
        report.checks
    );
    let config = config::load(&report.config_path).expect("the config init wrote parses");
    assert_eq!(config.repository.remote, "upstream");
    assert_eq!(config.repository.main_branch, "main");
    assert_eq!(
        config.hooks.after_merge.first().map(String::as_str),
        Some("git push upstream --delete work/$ARC_TASK")
    );
    assert!(
        !config
            .hooks
            .after_merge
            .iter()
            .any(|line| line.contains("origin")),
        "{:?}",
        config.hooks.after_merge
    );
}

/// A repository whose `config.toml` names another remote as operational uses that remote: the
/// checks, the report and the main branch are read from it, and the file is kept.
#[test]
fn init_uses_the_remote_the_config_names() {
    let scratch = fixture("init-configured-remote");
    add_remote(&scratch, &scratch.repo(), "upstream");
    scratch.write(
        "repo/.radian/arc/config.toml",
        "schema_version = 1\n[repository]\nremote = \"upstream\"\n",
    );

    let report = Store::init(&scratch.repo(), false).expect("init succeeds with upstream");
    assert_eq!(report.remote, "upstream");
    assert!(!report.config_created, "an existing config is kept");
    assert!(
        report.remote_url.ends_with("upstream.git"),
        "{}",
        report.remote_url
    );
    assert_eq!(report.main_branch, "main");
    for name in ["`upstream` reachable", "push to `upstream` `main`"] {
        assert!(
            report.checks.iter().any(|check| check.name == name),
            "missing check {name}: {:?}",
            report.checks
        );
    }
    assert!(
        !report
            .checks
            .iter()
            .any(|check| check.name.contains("origin")),
        "{:?}",
        report.checks
    );
}

/// Several remotes and no `origin` is ambiguous, so `init` refuses and names every remote
/// instead of guessing.
#[test]
fn init_refuses_to_guess_between_remotes() {
    let scratch = fixture("init-ambiguous-remote");
    git(&scratch.repo(), &["remote", "remove", "origin"]);
    add_remote(&scratch, &scratch.repo(), "upstream");
    add_remote(&scratch, &scratch.repo(), "mirror");

    let error = Store::init(&scratch.repo(), false).expect_err("two remotes and no origin");
    assert_eq!(error.exit(), Exit::Error);
    for name in ["upstream", "mirror"] {
        assert!(
            error.message().contains(name),
            "the error names `{name}`: {}",
            error.message()
        );
    }
    assert!(!scratch.repo().join(".radian/arc/config.toml").exists());
}

#[test]
fn a_task_file_parses_through_the_model() {
    let scratch = fixture("model");
    let store = initialized(&scratch);
    let plan = store.add_plan("p", "P", "request").unwrap();
    let task = store
        .add_task(&plan, "t", "T", Vec::new(), false, Vec::new(), "brief")
        .unwrap();
    let text = read(
        &scratch
            .repo()
            .join(".radian/arc/tasks")
            .join(format!("{task}.md")),
    );
    let parsed = TaskFile::from_text(&text).expect("the file parses");
    assert_eq!(parsed.state, model::TaskState::Todo);
    assert_eq!(parsed.phase, "ready");
    assert_eq!(parsed.attempt, 0);
    assert_eq!(&parsed.body, "brief\n");
}

/// A plan file written before the `priority` field existed reads as `P3` and is written back
/// unchanged: only `radian arc priority` adds the field. A hand edit of the field is the
/// owner's field, so `adopt` refuses it, and a value outside the set is a parse error naming
/// the set.
#[test]
fn an_old_plan_without_a_priority_reads_as_p3_and_keeps_its_bytes() {
    let scratch = fixture("priority-old");
    let store = initialized(&scratch);
    let plan = store
        .add_plan("old", "Old", "the request")
        .expect("add a plan");
    store
        .add_task(&plan, "t", "T", Vec::new(), false, Vec::new(), "brief")
        .expect("add a task");
    let path = scratch
        .repo()
        .join(".radian/arc/plans")
        .join(format!("{plan}.md"));

    // A new plan file has no `priority` field: that is what an older file looks like too.
    let before = read(&path);
    assert!(!before.contains("priority:"), "{before}");
    let loaded = store.load_plan(&plan).expect("the plan reads");
    assert_eq!(loaded.item.priority(), model::DEFAULT_PRIORITY);
    assert!(!loaded.changed_outside_arc);

    // Another write (confirm, then auto) leaves the file without the field.
    radian_arc::store::plan_ops::confirm(&store, &plan).expect("confirm");
    let confirmed = read(&path);
    assert!(!confirmed.contains("priority:"), "{confirmed}");
    assert_eq!(
        store
            .load_plan(&plan)
            .expect("the plan reads")
            .item
            .priority(),
        "P3"
    );

    // A value outside the set is refused (exit 3) and writes nothing; the message lists every
    // value.
    for bad in ["P6", "p1", "high"] {
        let error = radian_arc::store::plan_ops::set_priority(&store, &plan, bad)
            .expect_err("a bad priority is refused");
        assert_eq!(error.exit(), Exit::State, "{bad}: {}", error.message());
        for value in model::PLAN_PRIORITIES {
            assert!(
                error.message().contains(value),
                "{bad}: {}",
                error.message()
            );
        }
    }
    assert_eq!(read(&path), confirmed, "a refusal writes nothing");

    // Setting it writes the field once, and later writes keep it.
    radian_arc::store::plan_ops::set_priority(&store, &plan, "P1").expect("P1");
    let set = read(&path);
    assert!(set.contains("\npriority: P1\n"), "{set}");
    assert_eq!(
        store
            .load_plan(&plan)
            .expect("the plan reads")
            .item
            .priority(),
        "P1"
    );
    radian_arc::store::plan_ops::set_auto(&store, &plan, "run").expect("auto run");
    assert!(read(&path).contains("\npriority: P1\n"));

    // An outside edit of the priority is the owner's field: `adopt` refuses it.
    let hand_edited = read(&path).replace("priority: P1", "priority: P4");
    fs::write(&path, &hand_edited).expect("the plan is writable");
    let outside = store.load_plan(&plan).expect("the plan still parses");
    assert!(outside.changed_outside_arc);
    assert_eq!(outside.item.priority(), "P4");
    let refused = store
        .adopt(&plan)
        .expect_err("adopt refuses the owner's field");
    assert_eq!(refused.exit(), Exit::State, "{}", refused.message());
    assert!(
        refused.message().contains("priority"),
        "{}",
        refused.message()
    );

    // A value outside the set cannot even be read: the file names every value it may hold.
    let broken = read(&path).replace("priority: P4", "priority: P6");
    fs::write(&path, &broken).expect("the plan is writable");
    let error = store.load_plan(&plan).expect_err("P6 does not parse");
    for value in model::PLAN_PRIORITIES {
        assert!(error.message().contains(value), "{}", error.message());
    }
}
