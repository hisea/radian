//! `radian arc` through the real binary: setup refusal, `init`, the exit codes, `--json`,
//! two concurrent writers, a held lock, and a file changed outside arc.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
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
    let _ = git_out(dir, args);
}

fn git_out(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
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

/// Writes `[worker] exec_args` into the repository's `config.toml`.
fn set_exec_args(scratch: &Scratch, args: &[&str]) {
    let path = scratch.repo().join(".radian/arc/config.toml");
    let text = fs::read_to_string(&path).expect("config.toml is readable");
    let list = args
        .iter()
        .map(|arg| format!("\"{arg}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let replaced = text.replace("exec_args = []", &format!("exec_args = [{list}]"));
    assert_ne!(replaced, text, "the default exec_args key is in the file");
    fs::write(&path, replaced).expect("config.toml is writable");
}

/// A confirmed plan with one `todo.ready` task: the shape `start` and `run` would claim.
fn confirmed_task(scratch: &Scratch) -> (String, String) {
    let plan = add_plan(scratch, "worker");
    let task = add_task(scratch, &plan, "work", "the brief\n");
    let confirmed = run(scratch, &["arc", "confirm", &plan], "");
    assert_eq!(confirmed.code, Some(0), "{confirmed:?}");
    (plan, task)
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
        vec!["arc", "show", "PL0001-a"],
        vec!["arc", "adopt", "PL0001-a"],
        vec!["arc", "confirm", "PL0001-a"],
        vec!["arc", "auto", "PL0001-a", "run"],
        vec!["arc", "pause", "PL0001-a"],
        vec!["arc", "start", "TK0001-a"],
        vec!["arc", "reverify", "TK0001-a"],
        vec!["arc", "cancel", "TK0001-a"],
        vec!["arc", "reset-budget", "PL0001-a"],
        vec!["arc", "merge", "TK0001-a"],
        vec!["arc", "reject", "TK0001-a", "--note", "no"],
        vec!["arc", "run"],
        vec!["arc", "mcp"],
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
    assert_eq!(plan, "PL0001-first");
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
    assert_eq!(task, "TK0001-work");

    let list = run(&scratch, &["arc", "list"], "");
    assert_eq!(list.code, Some(0), "{list:?}");
    assert!(list.out.contains("PL0001-first"), "{}", list.out);
    assert!(list.out.contains("TK0001-work"), "{}", list.out);
    assert!(list.out.contains("todo.ready"), "{}", list.out);
    assert!(list.out.contains("attempt 0"), "{}", list.out);

    let json = run(&scratch, &["arc", "list", "--json"], "");
    assert_eq!(json.code, Some(0), "{json:?}");
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("list --json parses");
    assert_eq!(value[0]["id"], "PL0001-first");
    assert_eq!(value[0]["status"], "draft");
    assert_eq!(value[0]["tasks"][0]["id"], "TK0001-work");
    assert_eq!(value[0]["tasks"][0]["state"], "todo");
    assert_eq!(value[0]["tasks"][0]["phase"], "ready");

    let shown = run(&scratch, &["arc", "show", &plan, "--json"], "");
    assert_eq!(shown.code, Some(0), "{shown:?}");
    let value: serde_json::Value = serde_json::from_str(&shown.out).expect("show --json parses");
    assert_eq!(value["id"], "PL0001-first");
    assert!(value["body"].as_str().unwrap().contains("the request"));

    let shown = run(&scratch, &["arc", "show", &task, "--json"], "");
    let value: serde_json::Value = serde_json::from_str(&shown.out).expect("task json parses");
    assert_eq!(value["plan"], "PL0001-first");
    assert_eq!(value["allow_noop"], true);
    assert_eq!(value["extra_stages"][0], "mutation");
    assert!(value["body"].as_str().unwrap().contains("the brief"));

    let human = run(&scratch, &["arc", "show", &task], "");
    assert_eq!(human.code, Some(0), "{human:?}");
    for text in [
        "task TK0001-work",
        "state: todo",
        "phase: ready",
        "the brief",
    ] {
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
            .join(".radian/arc/tasks/TK0001-late.md")
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
            "PL0001-p",
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
    let missing = run(&scratch, &["arc", "show", "TK0099-missing"], "");
    assert_eq!(missing.code, Some(1), "{missing:?}");
    assert!(missing.err.contains("no plan or task"), "{}", missing.err);
}

/// An id without a prefix is malformed: a usage error (`2`), not a missing item (`1`).
#[test]
fn an_id_without_a_prefix_exits_two() {
    let scratch = fixture("no-prefix");
    init(&scratch);
    for args in [
        vec!["arc", "show", "0001-a"],
        vec!["arc", "adopt", "0001-a"],
        vec!["arc", "start", "0001-a"],
    ] {
        let run = run(&scratch, &args, "");
        assert_eq!(run.code, Some(2), "{args:?}: {run:?}");
        assert!(run.err.contains("prefix"), "{args:?}: {run:?}");
    }
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
fn confirm_a_proposed_task_moves_it_to_ready_and_prints_the_phase() {
    let scratch = fixture("confirm-task");
    init(&scratch);
    let plan = add_plan(&scratch, "proposed");
    let task = add_task(&scratch, &plan, "work", "the brief\n");
    assert_eq!(run(&scratch, &["arc", "confirm", &plan], "").code, Some(0));
    assert_eq!(
        run(&scratch, &["arc", "auto", &plan, "run"], "").code,
        Some(0)
    );
    radian_arc::store::Store::at(scratch.repo())
        .change_task(&task, |task| {
            task.phase = "proposed".to_owned();
            Ok(())
        })
        .expect("the task is proposed");

    // `start` refuses it and names the phase.
    let refused = run(&scratch, &["arc", "start", &task], "");
    assert_eq!(refused.code, Some(3), "{refused:?}");
    assert!(refused.err.contains("todo.proposed"), "{refused:?}");
    assert_eq!(stored_task(&scratch, &task).attempt, 0);

    let confirmed = run(&scratch, &["arc", "confirm", &task], "");
    assert_eq!(confirmed.code, Some(0), "{confirmed:?}");
    assert_eq!(confirmed.out.trim(), format!("{task} todo.ready"));
    let stored = stored_task(&scratch, &task);
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("todo", "ready")
    );
    let json = run(&scratch, &["arc", "list", "--json"], "");
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("list --json parses");
    assert_eq!(value[0]["status"], "confirmed", "the plan stays confirmed");
    assert_eq!(
        value[0]["auto"], "run",
        "the plan's auto level is unchanged"
    );

    // A task in any other state is refused.
    let again = run(&scratch, &["arc", "confirm", &task], "");
    assert_eq!(again.code, Some(3), "{again:?}");
    assert!(again.err.contains("todo.ready"), "{again:?}");
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
fn start_refuses_a_draft_plan_and_writes_no_attempt() {
    let scratch = fixture("start-draft");
    init(&scratch);
    let plan = add_plan(&scratch, "draft");
    let task = add_task(&scratch, &plan, "work", "the brief\n");

    let run = run(&scratch, &["arc", "start", &task], "");
    assert_eq!(run.code, Some(3), "{run:?}");
    assert!(run.err.contains("confirm"), "{}", run.err);
    let stored = radian_arc::store::Store::at(scratch.repo())
        .load_task(&task)
        .expect("the task reads")
        .item;
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("todo", "ready")
    );
    assert_eq!(stored.attempt, 0, "a refusal writes nothing");
    assert!(
        !scratch.repo().join(".radian/arc/runs").join(&task).exists(),
        "a refused start runs no wrapper"
    );
}

#[test]
fn a_retry_time_is_printed_by_list_and_show() {
    let scratch = fixture("retry-list");
    init(&scratch);
    let plan = add_plan(&scratch, "retry");
    let task = add_task(&scratch, &plan, "work", "the brief\n");
    radian_arc::store::Store::at(scratch.repo())
        .change_task(&task, |task| {
            task.phase = "retry_wait".to_owned();
            task.retry_at = Some("2026-10-22T10:11:12Z".to_owned());
            Ok(())
        })
        .expect("the task waits for a retry");

    let list = run(&scratch, &["arc", "list"], "");
    assert_eq!(list.code, Some(0), "{list:?}");
    assert!(
        list.out.contains("retry at 2026-10-22T10:11:12Z"),
        "{}",
        list.out
    );
    let show = run(&scratch, &["arc", "show", &task], "");
    assert_eq!(show.code, Some(0), "{show:?}");
    assert!(
        show.out.contains("retry_at: 2026-10-22T10:11:12Z"),
        "{}",
        show.out
    );
}

#[test]
fn list_hides_finished_work_unless_all_and_json_is_unfiltered() {
    let scratch = fixture("list-filter");
    init(&scratch);
    let store = radian_arc::store::Store::at(scratch.repo());
    let merge = |task: &str| {
        store
            .change_task(task, |task| {
                task.state = radian_arc::store::model::TaskState::Done;
                task.phase = "merged".to_owned();
                Ok(())
            })
            .expect("the task is merged");
    };

    let finished = add_plan(&scratch, "finished");
    let finished_one = add_task(&scratch, &finished, "one", "the brief\n");
    let finished_two = add_task(&scratch, &finished, "two", "the brief\n");
    merge(&finished_one);
    merge(&finished_two);

    let mixed = add_plan(&scratch, "mixed");
    let merged = add_task(&scratch, &mixed, "one", "the brief\n");
    let ready = add_task(&scratch, &mixed, "two", "the brief\n");
    merge(&merged);

    let default = run(&scratch, &["arc", "list"], "");
    assert_eq!(default.code, Some(0), "{default:?}");
    assert!(
        !default.out.contains(&finished),
        "the finished plan is hidden: {}",
        default.out
    );
    assert!(
        !default.out.contains(&merged),
        "the merged task is hidden: {}",
        default.out
    );
    assert!(default.out.contains(&ready), "{}", default.out);
    assert!(
        default
            .out
            .contains(&format!("{mixed}  draft  auto: off  1/2 merged  a plan")),
        "the shown plan counts what it hid: {}",
        default.out
    );
    assert!(
        default
            .out
            .ends_with("1 finished plan and 3 merged tasks hidden (--all to show)\n"),
        "{}",
        default.out
    );

    let all = run(&scratch, &["arc", "list", "--all"], "");
    assert_eq!(all.code, Some(0), "{all:?}");
    for id in [&finished, &finished_one, &finished_two, &merged, &ready] {
        assert!(
            all.out.contains(id.as_str()),
            "`{id}` with --all: {}",
            all.out
        );
    }
    assert!(
        !all.out.contains("hidden"),
        "--all hides nothing: {}",
        all.out
    );

    let plain = run(&scratch, &["arc", "list", "--json"], "");
    let with_all = run(&scratch, &["arc", "list", "--json", "--all"], "");
    assert_eq!(plain.code, Some(0), "{plain:?}");
    assert_eq!(
        plain.out, with_all.out,
        "`--all` changes nothing about the JSON"
    );
    let value: serde_json::Value = serde_json::from_str(&plain.out).expect("list --json parses");
    let plans = value.as_array().expect("an array of plans");
    assert_eq!(plans.len(), 2, "every plan is in the JSON: {value}");
    let finished_json = plans
        .iter()
        .find(|plan| plan["id"] == finished.as_str())
        .expect("the finished plan is in the JSON");
    assert_eq!(finished_json["tasks"].as_array().unwrap().len(), 2);
    let mixed_json = plans
        .iter()
        .find(|plan| plan["id"] == mixed.as_str())
        .expect("the mixed plan is in the JSON");
    assert_eq!(mixed_json["tasks"].as_array().unwrap().len(), 2);

    let one = run(&scratch, &["arc", "list", "--plan", &finished], "");
    assert_eq!(one.code, Some(0), "{one:?}");
    assert!(
        one.out
            .contains(&format!("{finished}  draft  auto: off  2/2 merged  a plan")),
        "a finished plan named by --plan keeps its header: {}",
        one.out
    );
    assert!(
        one.out.ends_with("2 merged tasks hidden (--all to show)\n"),
        "{}",
        one.out
    );
}

#[test]
fn a_review_task_shows_its_rework_reason() {
    let scratch = fixture("rework-reason");
    init(&scratch);
    let plan = add_plan(&scratch, "rework");
    let task = add_task(&scratch, &plan, "work", "the brief\n");
    let store = radian_arc::store::Store::at(scratch.repo());
    let attempt = format!("{task}-1");
    store
        .change_task(&task, |task| {
            task.state = radian_arc::store::model::TaskState::Review;
            task.phase = "rework_pending".to_owned();
            task.attempt = 1;
            task.attempt_id = Some(attempt.clone());
            Ok(())
        })
        .expect("the task is in review");
    let dir = scratch
        .repo()
        .join(".radian/arc/runs")
        .join(&task)
        .join("attempts")
        .join(&attempt);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("reject.md"), "not what the plan asked for\n").unwrap();

    let shown = run(&scratch, &["arc", "show", &task], "");
    assert_eq!(shown.code, Some(0), "{shown:?}");
    assert!(
        shown
            .out
            .contains("rework_reason: not what the plan asked for"),
        "{}",
        shown.out
    );
    let json = run(&scratch, &["arc", "show", &task, "--json"], "");
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("show --json parses");
    assert_eq!(value["rework_reason"], "not what the plan asked for");
    assert_eq!(value["phase"], "rework_pending");
}

/// `radian arc reverify` re-runs the gate for the same commit without a worker: a task whose
/// gate command was broken goes to `review.awaiting_human` on attempt 2, and no result stream
/// is written. Mutation: count the reverify attempt toward `max_attempts` (covered at the
/// store level) or accept a reject note (`reverify_refuses_a_todo_task` and the store tests).
#[test]
fn reverify_reruns_the_gate_without_a_worker() {
    let scratch = fixture("reverify");
    init(&scratch);
    let (_plan, task) = confirmed_task(&scratch);
    let base = git_out(&scratch.repo(), &["rev-parse", "HEAD"]);
    let branch = format!("work/{task}");
    git(&scratch.repo(), &["checkout", "-q", "-B", &branch, &base]);
    scratch.write("repo/feature.txt", "feature\n");
    git(&scratch.repo(), &["add", "feature.txt"]);
    git(&scratch.repo(), &["commit", "-q", "-m", "feature"]);
    let head = git_out(&scratch.repo(), &["rev-parse", "HEAD"]);
    git(&scratch.repo(), &["push", "-q", "origin", &branch]);

    // The failed gate the first attempt recorded: a verification-stage reason.
    let store = radian_arc::store::Store::at(scratch.repo());
    let label = format!("{task}-1");
    store
        .change_task(&task, |task| {
            task.state = radian_arc::store::model::TaskState::Review;
            task.phase = "rework_pending".to_owned();
            task.attempt = 1;
            task.attempt_id = Some(label.clone());
            task.token = Some("token-1".to_owned());
            task.base_sha = Some(base.clone());
            task.head_sha = Some(head.clone());
            Ok(())
        })
        .expect("the task is in review.rework_pending");
    let dir = scratch
        .repo()
        .join(".radian/arc/runs")
        .join(&task)
        .join("attempts")
        .join(&label);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("review.json"),
        serde_json::json!({
            "action": "verify",
            "stage": "commands",
            "reason": "command_failed",
            "verdict": null,
            "text": "commands: command_failed",
        })
        .to_string(),
    )
    .unwrap();

    // The gate command was wrong; the owner fixes it.
    let config = scratch.repo().join(".radian/arc/config.toml");
    let text = fs::read_to_string(&config).expect("config.toml");
    fs::write(
        &config,
        text.replacen("\ncommands = []", "\ncommands = [\"true\"]", 1),
    )
    .expect("the fixed config.toml");

    let run = run(&scratch, &["arc", "reverify", &task], "");
    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(
        run.out
            .contains(&format!("{task}-2 reverify -> review.verifying")),
        "{}",
        run.out
    );
    assert!(
        run.out
            .contains(&format!("{task}-2 verify -> review.awaiting_human")),
        "{}",
        run.out
    );
    let stored = stored_task(&scratch, &task);
    assert_eq!(stored.attempt, 2);
    assert_eq!(
        stored.attempt_id.as_deref(),
        Some(format!("{task}-2").as_str())
    );
    assert_eq!(stored.head_sha.as_deref(), Some(head.as_str()));
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("review", "awaiting_human")
    );
    let new_dir = scratch
        .repo()
        .join(".radian/arc/runs")
        .join(&task)
        .join("attempts")
        .join(format!("{task}-2"));
    assert!(
        !new_dir.join("result.jsonl").exists(),
        "a reverify attempt runs no worker"
    );
    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(new_dir.join("attempt_result.json")).unwrap())
            .unwrap();
    assert_eq!(record["class"], "reverify");
}

/// A task in `todo.ready` is refused with exit `3`, and nothing is claimed.
#[test]
fn reverify_refuses_a_todo_task() {
    let scratch = fixture("reverify-todo");
    init(&scratch);
    let (_plan, task) = confirmed_task(&scratch);
    let run = run(&scratch, &["arc", "reverify", &task], "");
    assert_eq!(run.code, Some(3), "{run:?}");
    assert!(run.err.contains("todo.ready"), "{run:?}");
    assert_eq!(stored_task(&scratch, &task).attempt, 0);
}

#[test]
fn merge_and_reject_refuse_a_task_outside_review_and_a_missing_note() {
    let scratch = fixture("merge-refuse");
    init(&scratch);
    let plan = add_plan(&scratch, "refuse");
    let task = add_task(&scratch, &plan, "work", "the brief\n");

    let merged = run(&scratch, &["arc", "merge", &task], "");
    assert_eq!(merged.code, Some(3), "{merged:?}");
    assert!(merged.err.contains("awaiting_human"), "{merged:?}");

    let rejected = run(&scratch, &["arc", "reject", &task, "--note", "no"], "");
    assert_eq!(rejected.code, Some(3), "{rejected:?}");
    assert!(rejected.err.contains("awaiting_human"), "{rejected:?}");

    // `--note` is required: clap refuses the call before arc sees it.
    let no_note = run(&scratch, &["arc", "reject", &task], "");
    assert_eq!(no_note.code, Some(2), "{no_note:?}");
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

/// A path under the checkout's `scripts/`.
fn repo_script(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts")
        .join(name)
}

/// `git rev-parse HEAD` in `dir`.
fn head_sha(dir: &Path) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git rev-parse HEAD: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The gate script has no argument surprise: no argument is a usage error (`2`),
/// and `--help` prints the usage (`0`).
#[test]
fn arc_gate_without_arguments_exits_two_and_help_exits_zero() {
    let script = repo_script("arc-gate.sh");
    let none = Command::new("bash")
        .arg(&script)
        .output()
        .expect("bash runs");
    assert_eq!(none.status.code(), Some(2), "{none:?}");
    let err = String::from_utf8_lossy(&none.stderr);
    assert!(err.contains("usage: arc-gate.sh"), "{err}");
    assert!(err.contains("quick") && err.contains("full"), "{err}");

    let help = Command::new("bash")
        .arg(&script)
        .arg("--help")
        .output()
        .expect("bash runs");
    assert_eq!(help.status.code(), Some(0), "{help:?}");
    let out = String::from_utf8_lossy(&help.stdout);
    assert!(out.contains("usage: arc-gate.sh"), "{out}");
    assert!(out.contains("quick") && out.contains("full"), "{out}");
}

/// `quick` runs nextest for the crates the checkout changed since its merge base
/// with `origin/main`, and always for `radian-tui`. A fake `cargo` records the
/// arguments, so the test never builds or reaches the network.
#[test]
fn quick_tests_the_changed_crates_and_radian_tui() {
    let scratch = Scratch::new("gate-crates");
    let repo = scratch.repo();
    fs::create_dir_all(&repo).expect("the repo is creatable");
    git(&repo, &["init", "-q"]);
    scratch.write("repo/README.md", "base\n");
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base = head_sha(&repo);
    git(&repo, &["update-ref", "refs/remotes/origin/main", &base]);

    scratch.write("repo/crates/a/x", "a\n");
    scratch.write("repo/crates/b/y", "b\n");
    git(&repo, &["add", "crates"]);
    git(&repo, &["commit", "-q", "-m", "change two crates"]);

    let bin = scratch.path("bin");
    fs::create_dir_all(&bin).expect("the stub dir is creatable");
    let cargo = bin.join("cargo");
    fs::write(
        &cargo,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CARGO_CALLS\"\nexit 0\n",
    )
    .expect("the stub is writable");
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755))
            .expect("the stub is executable");
    }

    let calls = scratch.path("calls.txt");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("bash")
        .arg(repo_script("arc-gate.sh"))
        .arg("quick")
        .current_dir(&repo)
        .env("PATH", path)
        .env("CARGO_CALLS", &calls)
        .output()
        .expect("the gate runs");
    assert!(
        out.status.success(),
        "code {:?}, stderr {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = fs::read_to_string(&calls).expect("the fake cargo was called");
    let nextest = calls
        .lines()
        .find(|line| line.starts_with("nextest run"))
        .unwrap_or_else(|| panic!("nextest ran: {calls}"));
    assert!(
        nextest.contains("-p a -p b -p radian-tui"),
        "the changed crates and radian-tui: {nextest}"
    );
}

fn stored_task(scratch: &Scratch, task: &str) -> radian_arc::store::model::TaskFile {
    radian_arc::store::Store::at(scratch.repo())
        .load_task(task)
        .expect("the task reads")
        .item
}

/// A `[worker] exec_args` line `radian exec` would reject refuses `start`, `run` and
/// `confirm` with exit `3` before anything is claimed or written (ADR 0048 decision 7).
#[test]
fn a_rejected_worker_argument_line_refuses_start_run_and_confirm() {
    let scratch = fixture("bad-worker-args");
    init(&scratch);
    let (plan, task) = confirmed_task(&scratch);
    // `run` claims only at a level other than `off`; the check must fire even then.
    assert_eq!(
        run(&scratch, &["arc", "auto", &plan, "run"], "").code,
        Some(0)
    );
    set_exec_args(&scratch, &["-m", "x"]);

    let started = run(&scratch, &["arc", "start", &task], "");
    assert_eq!(started.code, Some(3), "{started:?}");
    assert!(started.err.contains("worker.exec_args"), "{started:?}");
    assert!(started.err.contains("-m"), "{started:?}");
    let stored = stored_task(&scratch, &task);
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("todo", "ready")
    );
    assert_eq!(stored.attempt, 0, "a refused start claims nothing");
    assert!(
        !scratch.repo().join(".radian/arc/runs").join(&task).exists(),
        "a refused start runs no wrapper"
    );

    let ran = run(&scratch, &["arc", "run"], "");
    assert_eq!(ran.code, Some(3), "{ran:?}");
    assert!(ran.err.contains("worker.exec_args"), "{ran:?}");
    let stored = stored_task(&scratch, &task);
    assert_eq!(
        (stored.state.as_str(), stored.phase.as_str()),
        ("todo", "ready")
    );
    assert_eq!(stored.attempt, 0, "a refused run claims nothing");

    let draft = add_plan(&scratch, "draft");
    add_task(&scratch, &draft, "draftwork", "the brief\n");
    let confirmed = run(&scratch, &["arc", "confirm", &draft], "");
    assert_eq!(confirmed.code, Some(3), "{confirmed:?}");
    assert!(confirmed.err.contains("worker.exec_args"), "{confirmed:?}");
    let plan_file = radian_arc::store::Store::at(scratch.repo())
        .load_plan(&draft)
        .expect("the plan reads")
        .item;
    assert_eq!(
        plan_file.status, "draft",
        "a refusal leaves the plan a draft"
    );
}

/// An `exec_args` entry that repeats an argument the wrapper sets is refused by name: the
/// line itself parses, but the wrapper's value would be overridden.
#[test]
fn an_exec_args_entry_the_wrapper_sets_is_refused_by_name() {
    let scratch = fixture("owned-worker-args");
    init(&scratch);
    let (_plan, task) = confirmed_task(&scratch);
    set_exec_args(&scratch, &["--profile", "full"]);

    let refused = run(&scratch, &["arc", "start", &task], "");
    assert_eq!(refused.code, Some(3), "{refused:?}");
    assert!(
        refused.err.contains("must not repeat `--profile`"),
        "the refusal is the wrapper's own, not only the parser's duplicate: {refused:?}"
    );
    assert!(refused.err.contains("worker.exec_args"), "{refused:?}");
    assert_eq!(stored_task(&scratch, &task).attempt, 0);
}

/// A valid line passes the check: `confirm` and an idle `run` (auto `off`) act normally.
#[test]
fn a_valid_worker_argument_line_passes_the_check() {
    let scratch = fixture("good-worker-args");
    init(&scratch);
    let plan = add_plan(&scratch, "valid");
    add_task(&scratch, &plan, "work", "the brief\n");
    set_exec_args(&scratch, &["--model", "x"]);

    // `confirm` runs no worker: it passing proves the line was accepted.
    let confirmed = run(&scratch, &["arc", "confirm", &plan], "");
    assert_eq!(confirmed.code, Some(0), "{confirmed:?}");
    // `run` claims nothing at auto `off`, so it also checks the line and exits 0.
    let ran = run(&scratch, &["arc", "run"], "");
    assert_eq!(ran.code, Some(0), "{ran:?}");
}

/// A child killed and reaped when a test ends before it waited, so a failed assertion does not
/// leave a `run --watch` loop behind.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn spawn_arc(scratch: &Scratch, args: &[&str]) -> ChildGuard {
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_radian"))
            .current_dir(scratch.repo())
            .env("RADIAN_HOME", scratch.path("home"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("radian runs"),
    )
}

/// The child's stdout, read line by line on its own thread, so a test sees a line while the
/// child still runs.
struct Lines {
    rx: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Lines {
    fn new(stdout: std::process::ChildStdout) -> Self {
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
            rx,
            seen: Vec::new(),
        }
    }

    fn next_line(&mut self, timeout: Duration) -> Option<String> {
        let line = self.rx.recv_timeout(timeout).ok()?;
        self.seen.push(line.clone());
        Some(line)
    }

    /// The next line that contains `needle`, within `timeout`.
    fn until(&mut self, needle: &str, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let line = self.next_line(left)?;
            if line.contains(needle) {
                return Some(line);
            }
        }
    }
}

fn drain_stderr(child: &mut ChildGuard) -> std::thread::JoinHandle<String> {
    let mut stderr = child.0.stderr.take().expect("stderr is piped");
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    })
}

fn wait_within(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("the child's status") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A mock model on its own runtime: the test thread blocks on the child, so the server serves
/// from worker threads while the test reads its stdout.
struct Mock {
    server: MockServer,
    _runtime: tokio::runtime::Runtime,
}

impl Mock {
    fn start(replies: Vec<Reply>) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the mock runtime");
        let server = runtime.block_on(MockServer::start(replies));
        Self {
            server,
            _runtime: runtime,
        }
    }

    fn base_url(&self) -> &str {
        &self.server.base_url
    }

    fn requests(&self) -> usize {
        self.server.requests().len()
    }
}

/// A model reply whose stream is not finished until `pause` has passed: the worker waits
/// inside the HTTP response, so `run` is alive while a test reads its stdout.
fn waiting_reply(pause: Duration) -> Reply {
    let content = chunk(
        serde_json::json!({"role": "assistant", "content": "done"}),
        None,
    );
    let stop = chunk(serde_json::json!({}), Some("stop"));
    Reply::Sse {
        pieces: vec![
            format!("data: {content}\n\n").into_bytes(),
            format!("data: {stop}\n\n").into_bytes(),
            b"data: [DONE]\n\n".to_vec(),
        ],
        pause,
        headers: Vec::new(),
    }
}

/// Moves a task to `todo.retry_wait` with a `retry_at` that is already past.
fn wait_for_a_retry(scratch: &Scratch, task: &str) {
    radian_arc::store::Store::at(scratch.repo())
        .change_task(task, |task| {
            task.phase = "retry_wait".to_owned();
            task.retry_at = Some("2026-01-01T00:00:00Z".to_owned());
            Ok(())
        })
        .expect("the task waits for an already-due retry");
}

/// A transition line is on stdout while `run` still runs: the loop flushes it when it happens,
/// not when the command ends. The worker waits inside the mock's stream, so the child is alive
/// while the test reads.
///
/// Mutation: collect the lines and print them at the end, and the line does not arrive within
/// the one-second read while the worker waits.
#[test]
fn run_writes_a_transition_line_before_it_exits() {
    let scratch = fixture("streaming");
    init(&scratch);
    let (plan, task) = confirmed_task(&scratch);
    assert_eq!(
        run(&scratch, &["arc", "auto", &plan, "run"], "").code,
        Some(0)
    );
    wait_for_a_retry(&scratch, &task);
    let mock = Mock::start(vec![waiting_reply(Duration::from_secs(1))]);
    set_exec_args(
        &scratch,
        &[
            "--model",
            "m",
            "--base-url",
            mock.base_url(),
            "--offline-catalog",
        ],
    );

    let mut child = spawn_arc(&scratch, &["arc", "run"]);
    let mut lines = Lines::new(child.0.stdout.take().expect("stdout is piped"));
    let stderr = drain_stderr(&mut child);

    let line = lines.until(
        "released: todo.retry_wait -> todo.ready",
        Duration::from_secs(1),
    );
    let alive = child.0.try_wait().expect("the child's status").is_none();
    assert!(
        line.is_some(),
        "no transition line while run ran: {:?}",
        lines.seen
    );
    assert!(
        alive,
        "run must still be running when the line arrives: {:?}",
        lines.seen
    );

    let status = child.0.wait().expect("run finishes");
    let err = stderr.join().expect("the stderr reader");
    assert_eq!(status.code(), Some(0), "{err}");
    assert!(
        lines.until("summary", Duration::from_secs(2)).is_some(),
        "the pass ends with a summary: {:?}",
        lines.seen
    );
    // The pass does not wait for the worker it started; the worker still reaches the model.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while mock.requests() < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker never reached the mock model"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `--watch` runs another pass on its own: a retry that becomes due between passes is released
/// with no second command, and SIGTERM stops the loop with exit `0` and the lock released.
///
/// Mutations: buffer the lines again and the released line misses its deadline; keep the lock
/// (and the process) on SIGTERM and the child does not exit within five seconds.
#[test]
fn watch_runs_another_pass_and_exits_zero_on_sigterm() {
    let scratch = fixture("watch");
    init(&scratch);
    let plan = add_plan(&scratch, "watch");
    let task = add_task(&scratch, &plan, "work", "the brief\n");

    let mut child = spawn_arc(&scratch, &["arc", "run", "--watch", "--interval", "5"]);
    let mut lines = Lines::new(child.0.stdout.take().expect("stdout is piped"));
    let stderr = drain_stderr(&mut child);

    assert!(
        lines.until("summary", Duration::from_secs(5)).is_some(),
        "the first pass ends quiet with a summary: {:?}",
        lines.seen
    );

    // The loop holds the lock between passes: a second one-shot run exits `4`.
    let blocked = run(&scratch, &["arc", "run"], "");
    assert_eq!(blocked.code, Some(4), "{blocked:?}");

    // No second command runs: the loop's own next pass releases the retry.
    wait_for_a_retry(&scratch, &task);
    assert!(
        lines
            .until(
                "released: todo.retry_wait -> todo.ready",
                Duration::from_secs(15)
            )
            .is_some(),
        "a later pass did not release the retry: {:?}",
        lines.seen
    );
    assert!(
        lines.until("summary", Duration::from_secs(5)).is_some(),
        "the releasing pass prints its summary: {:?}",
        lines.seen
    );

    let pid = child.0.id() as libc::pid_t;
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGTERM) },
        0,
        "SIGTERM reaches run --watch"
    );
    let status = wait_within(&mut child.0, Duration::from_secs(5));
    let Some(status) = status else {
        let _ = child.0.kill();
        let _ = child.0.wait();
        let err = stderr.join().unwrap_or_default();
        panic!("run --watch did not exit on SIGTERM: {err}");
    };
    let err = stderr.join().expect("the stderr reader");
    assert_eq!(status.code(), Some(0), "SIGTERM is not an error: {err}");

    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(scratch.repo().join(".radian/arc/runner.lock"))
        .expect("runner.lock exists");
    assert!(
        lock.try_lock_exclusive().is_ok(),
        "run --watch releases runner.lock"
    );
}

/// The interval is a closed range, and it only means something with `--watch`.
#[test]
fn a_watch_interval_outside_five_to_3600_is_a_usage_error() {
    let scratch = fixture("interval");
    init(&scratch);
    for args in [
        vec!["arc", "run", "--watch", "--interval", "4"],
        vec!["arc", "run", "--watch", "--interval", "3601"],
        vec!["arc", "run", "--interval", "5"],
    ] {
        let run = run(&scratch, &args, "");
        assert_eq!(run.code, Some(2), "{args:?}: {run:?}");
    }
}

/// `radian arc priority`: a value outside the closed set is refused with exit 3 and a message
/// that lists every value; the default is `P3`; `list`, `show --json`, and the MCP documents
/// carry the field; and `list` prints plans in claim order.
#[test]
fn priority_is_set_listed_and_shown_and_a_bad_value_exits_three() {
    let scratch = fixture("priority");
    init(&scratch);
    let first = add_plan(&scratch, "first");
    let second = add_plan(&scratch, "second");
    add_task(&scratch, &first, "work", "the brief\n");
    let confirmed = run(&scratch, &["arc", "confirm", &first], "");
    assert_eq!(confirmed.code, Some(0), "{confirmed:?}");

    for bad in ["P6", "p1", "high"] {
        let refused = run(&scratch, &["arc", "priority", &first, bad], "");
        assert_eq!(refused.code, Some(3), "{bad}: {refused:?}");
        for value in radian_arc::store::PLAN_PRIORITIES {
            assert!(
                refused.err.contains(value),
                "the refusal for `{bad}` must list `{value}`: {}",
                refused.err
            );
        }
    }
    let store = radian_arc::store::Store::at(scratch.repo());
    let unchanged = store.load_plan(&first).expect("the plan reads");
    assert_eq!(unchanged.item.priority(), "P3", "a refusal writes nothing");
    assert!(!unchanged.changed_outside_arc);

    // The default is shown everywhere before anyone sets it.
    let list = run(&scratch, &["arc", "list"], "");
    assert!(
        list.out.contains("priority: P3"),
        "the plan line shows the priority:\n{}",
        list.out
    );
    let json = run(&scratch, &["arc", "list", "--json"], "");
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("list --json parses");
    assert_eq!(value[0]["priority"], "P3");
    let shown = run(&scratch, &["arc", "show", &first, "--json"], "");
    let value: serde_json::Value = serde_json::from_str(&shown.out).expect("show --json parses");
    assert_eq!(value["priority"], "P3");
    let shown = run(&scratch, &["arc", "show", &first], "");
    assert!(shown.out.contains("priority: P3"), "{}", shown.out);

    // Setting it on a draft plan works, and the file gets the field.
    let set = run(&scratch, &["arc", "priority", &second, "P0"], "");
    assert_eq!(set.code, Some(0), "{set:?}");
    assert!(set.out.contains("priority P0"), "{}", set.out);
    let path = scratch
        .repo()
        .join(".radian/arc/plans")
        .join(format!("{second}.md"));
    let set_text = fs::read_to_string(&path).expect("the plan file reads");
    assert!(set_text.contains("\npriority: P0\n"), "{set_text}");

    // `list` prints plans in claim order: the higher priority first, even with the larger id.
    let list = run(&scratch, &["arc", "list"], "");
    let first_at = list.out.find(&first).expect("the first plan is listed");
    let second_at = list.out.find(&second).expect("the second plan is listed");
    assert!(
        second_at < first_at,
        "the higher priority must be listed first:\n{}",
        list.out
    );
    let json = run(&scratch, &["arc", "list", "--json"], "");
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("list --json parses");
    assert_eq!(value[0]["id"], second.as_str());

    // The MCP `show` document is the same JSON.
    let json = run(&scratch, &["arc", "show", &second, "--json"], "");
    let value: serde_json::Value = serde_json::from_str(&json.out).expect("show --json parses");
    assert_eq!(value["priority"], "P0");
}
