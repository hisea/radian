//! `radian arc` through the real binary: setup refusal, `init`, the exit codes, `--json`,
//! two concurrent writers, a held lock, and a file changed outside arc.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use fs2::FileExt;

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radian-arc-cli-{label}-{}-{n}", std::process::id()));
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

/// One finished `radian arc` run.
struct Run {
    code: Option<i32>,
    out: String,
    err: String,
}

fn run(scratch: &Scratch, args: &[&str], stdin: &str) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_radian"))
        .current_dir(scratch.repo())
        .env("RADIAN_HOME", scratch.path("home"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("radian runs");
    if let Some(mut pipe) = child.stdin.take() {
        let _ = pipe.write_all(stdin.as_bytes());
    }
    let output = child.wait_with_output().expect("the child finishes");
    Run {
        code: output.status.code(),
        out: String::from_utf8_lossy(&output.stdout).into_owned(),
        err: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Starts `radian arc add task` without waiting, so two runs overlap.
fn spawn_add_task(scratch: &Scratch, plan: &str, slug: &str, brief: &str) -> std::process::Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_radian"))
        .current_dir(scratch.repo())
        .env("RADIAN_HOME", scratch.path("home"))
        .args([
            "arc", "add", "task", "--plan", plan, "--slug", slug, "--title", "a task",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("radian runs");
    if let Some(mut pipe) = child.stdin.take() {
        let _ = pipe.write_all(brief.as_bytes());
    }
    child
}

fn finish(child: std::process::Child) -> Run {
    let output = child.wait_with_output().expect("the child finishes");
    Run {
        code: output.status.code(),
        out: String::from_utf8_lossy(&output.stdout).into_owned(),
        err: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn init(scratch: &Scratch) {
    let run = run(scratch, &["arc", "init"], "");
    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(run.out.contains("arc init"), "{}", run.out);
}

fn add_plan(scratch: &Scratch, slug: &str) -> String {
    let run = run(
        scratch,
        &["arc", "add", "plan", "--slug", slug, "--title", "a plan"],
        "the request\n",
    );
    assert_eq!(run.code, Some(0), "{run:?}");
    run.out.trim().to_owned()
}

fn add_task(scratch: &Scratch, plan: &str, slug: &str, brief: &str) -> String {
    let run = run(
        scratch,
        &[
            "arc", "add", "task", "--plan", plan, "--slug", slug, "--title", "a task",
        ],
        brief,
    );
    assert_eq!(run.code, Some(0), "{run:?}");
    run.out.trim().to_owned()
}

impl std::fmt::Debug for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "code {:?} out {:?} err {:?}",
            self.code, self.out, self.err
        )
    }
}

#[test]
fn without_setup_the_subcommands_name_init_and_exit_three() {
    let scratch = fixture("no-setup");
    for args in [
        vec!["arc"],
        vec!["arc", "list"],
        vec!["arc", "show", "0001-a"],
        vec!["arc", "adopt", "0001-a"],
        vec!["arc", "confirm", "0001-a"],
        vec!["arc", "auto", "0001-a", "run"],
        vec!["arc", "pause", "0001-a"],
    ] {
        let run = run(&scratch, &args, "");
        assert_eq!(run.code, Some(3), "{args:?}: {run:?}");
        assert!(
            run.err.contains("radian arc init"),
            "{args:?} names init: {run:?}"
        );
    }
}

#[test]
fn init_prints_the_layout_checks_and_the_mcp_command() {
    let scratch = fixture("init");
    let first = run(&scratch, &["arc", "init"], "");
    assert_eq!(first.code, Some(0), "{first:?}");
    for text in [
        "repository",
        "main branch  main",
        "config",
        "created",
        "checks",
        "provider credential",
        "`origin` reachable",
        "workspace sandbox",
        "claude mcp add arc -- radian arc mcp",
    ] {
        assert!(first.out.contains(text), "missing `{text}`:\n{}", first.out);
    }
    assert!(scratch.repo().join(".radian/arc/config.toml").is_file());

    let second = run(&scratch, &["arc", "init"], "");
    assert_eq!(second.code, Some(0), "{second:?}");
    assert!(second.out.contains("kept"), "{}", second.out);
    assert!(second.out.contains("already current"), "{}", second.out);

    let tracked = run(&scratch, &["arc", "init", "--track"], "");
    assert_eq!(tracked.code, Some(0), "{tracked:?}");
    // A second or third run never repeats a line.
    let exclude = fs::read_to_string(git_path(&scratch.repo(), "info/exclude")).unwrap();
    for line in ["/.radian/arc/ledger/", "/.radian/arc/worktrees/"] {
        assert_eq!(
            exclude.matches(line).count(),
            1,
            "`{line}` once:\n{exclude}"
        );
    }
}

fn git_path(repo: &Path, path: &str) -> PathBuf {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "--git-path", path])
        .output()
        .expect("git runs");
    assert!(output.status.success());
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }
}

#[test]
fn a_bare_arc_says_the_tui_is_not_built_and_exits_two() {
    let scratch = fixture("bare");
    init(&scratch);
    let run = run(&scratch, &["arc"], "");
    assert_eq!(run.code, Some(2), "{run:?}");
    assert!(run.out.contains("not built yet"), "{}", run.out);
    for subcommand in radian_arc::store::cli::SUBCOMMANDS {
        assert!(
            run.out.contains(subcommand),
            "lists {subcommand}: {}",
            run.out
        );
    }
}

#[test]
fn add_list_and_show_work_and_json_parses() {
    let scratch = fixture("list");
    init(&scratch);
    let plan = add_plan(&scratch, "first");
    assert_eq!(plan, "0001-first");
    let task = run(
        &scratch,
        &[
            "arc",
            "add",
            "task",
            "--plan",
            &plan,
            "--slug",
            "work",
            "--title",
            "do it",
            "--allow-noop",
            "--stage",
            "mutation",
        ],
        "the brief\n",
    );
    assert_eq!(task.code, Some(0), "{task:?}");
    let task = task.out.trim().to_owned();
    assert_eq!(task, "0001-work");

    let list = run(&scratch, &["arc", "list"], "");
    assert_eq!(list.code, Some(0), "{list:?}");
    assert!(list.out.contains("0001-first"), "{}", list.out);
    assert!(list.out.contains("0001-work"), "{}", list.out);
    assert!(list.out.contains("todo.ready"), "{}", list.out);
    assert!(list.out.contains("attempt 0"), "{}", list.out);

    let json = run(&scratch, &["arc", "list", "--json"], "");
    assert_eq!(json.code, Some(0), "{json:?}");
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("list --json parses");
    assert_eq!(value[0]["id"], "0001-first");
    assert_eq!(value[0]["status"], "draft");
    assert_eq!(value[0]["tasks"][0]["id"], "0001-work");
    assert_eq!(value[0]["tasks"][0]["state"], "todo");
    assert_eq!(value[0]["tasks"][0]["phase"], "ready");

    let shown = run(&scratch, &["arc", "show", &plan, "--json"], "");
    assert_eq!(shown.code, Some(0), "{shown:?}");
    let value: serde_json::Value = serde_json::from_str(&shown.out).expect("show --json parses");
    assert_eq!(value["id"], "0001-first");
    assert!(value["body"].as_str().unwrap().contains("the request"));

    let shown = run(&scratch, &["arc", "show", &task, "--json"], "");
    let value: serde_json::Value = serde_json::from_str(&shown.out).expect("task json parses");
    assert_eq!(value["plan"], "0001-first");
    assert_eq!(value["allow_noop"], true);
    assert_eq!(value["extra_stages"][0], "mutation");
    assert!(value["body"].as_str().unwrap().contains("the brief"));

    let human = run(&scratch, &["arc", "show", &task], "");
    assert_eq!(human.code, Some(0), "{human:?}");
    for text in ["task 0001-work", "state: todo", "phase: ready", "the brief"] {
        assert!(human.out.contains(text), "missing `{text}`:\n{}", human.out);
    }
    assert!(
        human.out.contains("changed outside arc: no"),
        "{}",
        human.out
    );
}

#[test]
fn two_concurrent_add_task_processes_get_different_numbers() {
    let scratch = fixture("concurrent");
    init(&scratch);
    let plan = add_plan(&scratch, "parallel");
    let one = spawn_add_task(&scratch, &plan, "one", "brief one\n");
    let two = spawn_add_task(&scratch, &plan, "two", "brief two\n");
    let one = finish(one);
    let two = finish(two);
    assert_eq!(one.code, Some(0), "{one:?}");
    assert_eq!(two.code, Some(0), "{two:?}");
    let one = one.out.trim().to_owned();
    let two = two.out.trim().to_owned();
    assert_ne!(one, two, "two writers never share a number");
    let number = |id: &str| id.split_once('-').expect("an id").0.to_owned();
    assert_ne!(
        number(&one),
        number(&two),
        "the numbers differ, not only the slugs: {one} {two}"
    );
    let store = radian_arc::store::Store::at(scratch.repo());
    for id in [&one, &two] {
        let task = store.load_task(id).expect("the task reads");
        assert!(!task.changed_outside_arc, "{id} matches its ledger");
        assert_eq!(task.item.plan, plan);
        let ledger = fs::read_to_string(
            scratch
                .repo()
                .join(".radian/arc/ledger/tasks")
                .join(format!("{id}.json")),
        )
        .expect("the ledger exists");
        let ledger: serde_json::Value = serde_json::from_str(&ledger).unwrap();
        assert_eq!(ledger["state_version"], 1);
    }
    let view = store.plan_view(&plan).expect("the plan reads");
    assert_eq!(view.plan.item.tasks.len(), 2, "both tasks are on the plan");
}

#[test]
fn a_task_cannot_be_added_to_a_confirmed_plan() {
    let scratch = fixture("confirmed");
    init(&scratch);
    let plan = add_plan(&scratch, "done");
    let store = radian_arc::store::Store::at(scratch.repo());
    store
        .change_plan(&plan, |plan| {
            plan.status = "confirmed".to_owned();
            Ok(())
        })
        .expect("the plan is confirmed");
    let run = run(
        &scratch,
        &[
            "arc", "add", "task", "--plan", &plan, "--slug", "late", "--title", "late",
        ],
        "brief\n",
    );
    assert_eq!(run.code, Some(3), "{run:?}");
    assert!(run.err.contains("confirmed"), "{}", run.err);
    assert!(
        !scratch
            .repo()
            .join(".radian/arc/tasks/0001-late.md")
            .exists()
    );
}

#[test]
fn a_held_state_lock_exits_four() {
    let scratch = fixture("locked");
    init(&scratch);
    let lock_path = scratch.repo().join(".radian/arc/state.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .expect("the lock file opens");
    file.lock_exclusive().expect("the test takes the lock");
    let run = run(
        &scratch,
        &[
            "arc", "add", "plan", "--slug", "blocked", "--title", "blocked",
        ],
        "request\n",
    );
    assert_eq!(run.code, Some(4), "{run:?}");
    assert!(run.err.contains("locked by another process"), "{}", run.err);
    FileExt::unlock(&file).expect("the lock is released");
}

#[test]
fn a_changed_file_is_marked_and_adopt_resolves_it() {
    let scratch = fixture("outside");
    init(&scratch);
    let plan = add_plan(&scratch, "p");
    let task = run(
        &scratch,
        &[
            "arc", "add", "task", "--plan", &plan, "--slug", "t", "--title", "T",
        ],
        "the brief\n",
    )
    .out
    .trim()
    .to_owned();
    let path = scratch
        .repo()
        .join(".radian/arc/tasks")
        .join(format!("{task}.md"));
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replace("the brief", "an edited brief")).unwrap();

    let list = run(&scratch, &["arc", "list"], "");
    assert!(list.out.contains("changed outside arc"), "{}", list.out);
    let shown = run(&scratch, &["arc", "show", &task], "");
    assert!(
        shown.out.contains("changed outside arc: yes"),
        "{}",
        shown.out
    );

    let adopted = run(&scratch, &["arc", "adopt", &task], "");
    assert_eq!(adopted.code, Some(0), "{adopted:?}");
    assert!(adopted.out.contains("adopted"), "{}", adopted.out);
    let list = run(&scratch, &["arc", "list"], "");
    assert!(!list.out.contains("changed outside arc"), "{}", list.out);

    let text = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        text.replace("state: todo", "state: doing")
            .replace("phase: ready", "phase: running"),
    )
    .unwrap();
    let refused = run(&scratch, &["arc", "adopt", &task], "");
    assert_eq!(refused.code, Some(3), "{refused:?}");
    assert!(refused.err.contains("state"), "{}", refused.err);
}

#[test]
fn an_unknown_config_key_and_a_bad_slug_use_their_own_exit_codes() {
    let scratch = fixture("codes");
    init(&scratch);
    let config = scratch.repo().join(".radian/arc/config.toml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(&config, text.replace("[limits]\n", "[limits]\nnope = 1\n")).unwrap();
    let list = run(&scratch, &["arc", "list"], "");
    assert_eq!(list.code, Some(1), "{list:?}");
    assert!(list.err.contains("nope"), "{}", list.err);

    // A usage error: the slug rule is the code's.
    fs::write(&config, text).unwrap();
    let bad_slug = run(
        &scratch,
        &["arc", "add", "plan", "--slug", "Bad_Slug", "--title", "x"],
        "request\n",
    );
    assert_eq!(bad_slug.code, Some(2), "{bad_slug:?}");
    assert!(bad_slug.err.contains("slug"), "{}", bad_slug.err);

    // clap's own closed set for `--stage` is a usage error too.
    let bad_stage = run(
        &scratch,
        &[
            "arc",
            "add",
            "task",
            "--plan",
            "0001-p",
            "--slug",
            "t",
            "--title",
            "T",
            "--stage",
            "integration",
        ],
        "brief\n",
    );
    assert_eq!(bad_stage.code, Some(2), "{bad_stage:?}");

    // An unknown id is an error, not a state refusal.
    let missing = run(&scratch, &["arc", "show", "0099-missing"], "");
    assert_eq!(missing.code, Some(1), "{missing:?}");
    assert!(missing.err.contains("no plan or task"), "{}", missing.err);
}

#[test]
fn confirm_refuses_an_empty_plan_and_a_second_confirm() {
    let scratch = fixture("confirm");
    init(&scratch);
    let plan = add_plan(&scratch, "empty");

    let refused = run(&scratch, &["arc", "confirm", &plan], "");
    assert_eq!(refused.code, Some(3), "{refused:?}");
    assert!(refused.err.contains("no tasks"), "{}", refused.err);
    let stored = radian_arc::store::Store::at(scratch.repo())
        .load_plan(&plan)
        .expect("the plan reads");
    assert_eq!(stored.item.status, "draft", "a refusal writes nothing");

    add_task(&scratch, &plan, "work", "the brief\n");
    let confirmed = run(&scratch, &["arc", "confirm", &plan], "");
    assert_eq!(confirmed.code, Some(0), "{confirmed:?}");
    assert!(confirmed.out.contains("confirmed"), "{}", confirmed.out);
    let json = run(&scratch, &["arc", "list", "--json"], "");
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("list --json parses");
    assert_eq!(value[0]["status"], "confirmed");

    let again = run(&scratch, &["arc", "confirm", &plan], "");
    assert_eq!(again.code, Some(3), "{again:?}");
    assert!(again.err.contains("already"), "{}", again.err);
}

#[test]
fn auto_needs_a_confirmed_plan_and_a_level_from_the_set() {
    let scratch = fixture("auto");
    init(&scratch);
    let plan = add_plan(&scratch, "levels");
    add_task(&scratch, &plan, "work", "the brief\n");

    // A draft plan stays `off`; `off` itself is always allowed.
    for level in ["run", "merge"] {
        let refused = run(&scratch, &["arc", "auto", &plan, level], "");
        assert_eq!(refused.code, Some(3), "{level}: {refused:?}");
        assert!(refused.err.contains("confirmed"), "{}", refused.err);
    }
    let off = run(&scratch, &["arc", "auto", &plan, "off"], "");
    assert_eq!(off.code, Some(0), "{off:?}");

    // A level outside the closed set is a usage error, from the parser.
    let bad = run(&scratch, &["arc", "auto", &plan, "sometimes"], "");
    assert_eq!(bad.code, Some(2), "{bad:?}");

    let confirmed = run(&scratch, &["arc", "confirm", &plan], "");
    assert_eq!(confirmed.code, Some(0), "{confirmed:?}");
    let set = run(&scratch, &["arc", "auto", &plan, "merge"], "");
    assert_eq!(set.code, Some(0), "{set:?}");
    assert!(set.out.contains("merge"), "{}", set.out);
    let json = run(&scratch, &["arc", "list", "--json"], "");
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("list --json parses");
    assert_eq!(value[0]["auto"], "merge");
    assert_eq!(value[0]["status"], "confirmed");
}

#[test]
fn pause_sets_auto_off_and_leaves_a_started_task_alone() {
    let scratch = fixture("pause");
    init(&scratch);
    let plan = add_plan(&scratch, "paused");
    let task = add_task(&scratch, &plan, "work", "the brief\n");
    assert_eq!(run(&scratch, &["arc", "confirm", &plan], "").code, Some(0));
    assert_eq!(
        run(&scratch, &["arc", "auto", &plan, "run"], "").code,
        Some(0)
    );

    // A task that already started keeps its state: nothing in this build runs a task, so
    // the test moves it the way a later scheduling loop would.
    radian_arc::store::Store::at(scratch.repo())
        .change_task(&task, |task| {
            task.state = radian_arc::store::model::TaskState::Doing;
            task.phase = "running".to_owned();
            Ok(())
        })
        .expect("the task starts");

    let paused = run(&scratch, &["arc", "pause", &plan], "");
    assert_eq!(paused.code, Some(0), "{paused:?}");
    assert!(paused.out.contains("auto off"), "{}", paused.out);
    let json = run(&scratch, &["arc", "list", "--json"], "");
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("list --json parses");
    assert_eq!(value[0]["auto"], "off");
    assert_eq!(value[0]["tasks"][0]["state"], "doing");
    assert_eq!(value[0]["tasks"][0]["phase"], "running");
}
