//! `radian hub` through the real binary: `add`, `list`, `--json`, `remove`, the exit codes,
//! and the bare command that says the loop and the API are not built yet.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radian-hub-cli-{label}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the temp dir is creatable");
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn home(&self) -> PathBuf {
        self.path("home")
    }

    /// A directory that looks like an arc repository: it has `.radian/arc/`.
    fn repo(&self, name: &str) -> PathBuf {
        let repo = self.path(name);
        fs::create_dir_all(repo.join(".radian/arc")).expect("the arc directory is creatable");
        repo
    }

    fn registry(&self) -> PathBuf {
        self.home().join("hub/repos.toml")
    }

    fn run(&self, args: &[&str]) -> Run {
        let output = Command::new(env!("CARGO_BIN_EXE_radian"))
            .current_dir(&self.dir)
            .env("RADIAN_HOME", self.home())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("radian runs");
        Run {
            code: output.status.code(),
            out: String::from_utf8_lossy(&output.stdout).into_owned(),
            err: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// One finished `radian hub` run.
#[derive(Debug)]
struct Run {
    code: Option<i32>,
    out: String,
    err: String,
}

fn canonical(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("the path resolves")
        .display()
        .to_string()
}

#[test]
fn add_and_list_print_the_registered_repository_in_text_and_json() {
    let scratch = Scratch::new("add-list");
    let repo = scratch.repo("repo");

    let added = scratch.run(&["hub", "add", repo.to_str().unwrap()]);
    assert_eq!(added.code, Some(0), "{added:?}");
    let written = fs::read_to_string(scratch.registry()).expect("the registry is written");
    assert!(written.contains("name = \"repo\""), "{written}");
    assert!(
        written.contains(&format!("path = \"{}\"", canonical(&repo))),
        "{written}"
    );

    let listed = scratch.run(&["hub", "list"]);
    assert_eq!(listed.code, Some(0), "{listed:?}");
    assert_eq!(listed.out, format!("repo  {}\n", canonical(&repo)));

    let json = scratch.run(&["hub", "list", "--json"]);
    assert_eq!(json.code, Some(0), "{json:?}");
    let parsed: serde_json::Value = serde_json::from_str(&json.out).expect("json parses");
    assert_eq!(
        parsed,
        serde_json::json!([{ "name": "repo", "path": canonical(&repo) }])
    );
}

#[test]
fn add_refuses_a_directory_without_an_arc_store() {
    let scratch = Scratch::new("not-arc");
    let plain = scratch.path("plain");
    fs::create_dir_all(&plain).unwrap();

    let run = scratch.run(&["hub", "add", plain.to_str().unwrap()]);
    assert_eq!(run.code, Some(1), "{run:?}");
    assert!(
        run.err.contains(&canonical(&plain)),
        "the error names the path: {}",
        run.err
    );
    assert!(run.err.contains(".radian/arc"), "{}", run.err);
    assert!(
        !scratch.registry().exists(),
        "a refused add creates no registry"
    );
}

#[test]
fn remove_refuses_an_unknown_name_and_removes_only_the_named_entry() {
    let scratch = Scratch::new("remove");
    let one = scratch.repo("one");
    let two = scratch.repo("two");
    assert_eq!(
        scratch.run(&["hub", "add", one.to_str().unwrap()]).code,
        Some(0)
    );
    assert_eq!(
        scratch.run(&["hub", "add", two.to_str().unwrap()]).code,
        Some(0)
    );

    let unknown = scratch.run(&["hub", "remove", "missing"]);
    assert_eq!(unknown.code, Some(1), "{unknown:?}");
    assert!(unknown.err.contains("`missing`"), "{}", unknown.err);

    let removed = scratch.run(&["hub", "remove", "one"]);
    assert_eq!(removed.code, Some(0), "{removed:?}");
    let listed = scratch.run(&["hub", "list"]);
    assert_eq!(listed.out, format!("two  {}\n", canonical(&two)));
}

#[test]
fn hub_without_an_argument_exits_three_with_the_not_built_line() {
    let scratch = Scratch::new("bare");
    let run = scratch.run(&["hub"]);
    assert_eq!(run.code, Some(3), "{run:?}");
    assert!(
        run.err
            .contains("the hub loop and the API are not built yet"),
        "{}",
        run.err
    );
}
