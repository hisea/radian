//! The hub's repository registry against real temp directories: `add`, `remove`, `list`, the
//! file and directory modes, a hand-written file that breaks a rule, and the atomic rename a
//! reader relies on.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use radian_arc::hub::{Registry, valid_name};

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radian-arc-hub-{label}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the temp dir is creatable");
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    /// A directory that looks like an arc repository: it has `.radian/arc/`.
    fn repo(&self, name: &str) -> PathBuf {
        let repo = self.path(name);
        fs::create_dir_all(repo.join(".radian/arc")).expect("the arc directory is creatable");
        repo
    }

    fn registry(&self) -> Registry {
        Registry::at(self.path("hub"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn text(registry: &Registry) -> String {
    fs::read_to_string(registry.file()).expect("the registry file reads")
}

#[test]
fn add_registers_the_base_name_and_list_returns_file_order() {
    let scratch = Scratch::new("add");
    let registry = scratch.registry();
    assert!(
        registry.list().unwrap().is_empty(),
        "a missing file is empty"
    );

    let one = scratch.repo("one");
    let two = scratch.repo("two");
    let first = registry.add(&one, None).expect("the first add succeeds");
    assert_eq!(first.name, "one");
    assert_eq!(first.path, fs::canonicalize(&one).unwrap());
    registry.add(&two, None).expect("the second add succeeds");

    let listed = registry.list().expect("the registry lists");
    assert_eq!(
        listed
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["one", "two"],
        "entries keep file order"
    );
    let written = text(&registry);
    assert!(written.contains("[[repos]]"), "{written}");
    assert!(written.contains("name = \"one\""), "{written}");
    assert!(written.contains("name = \"two\""), "{written}");
}

#[test]
fn add_refuses_a_directory_without_an_arc_store_and_leaves_the_file_alone() {
    let scratch = Scratch::new("not-arc");
    let registry = scratch.registry();
    registry
        .add(&scratch.repo("real"), None)
        .expect("the first add succeeds");
    let before = text(&registry);

    let not_arc = scratch.path("plain");
    fs::create_dir_all(&not_arc).unwrap();
    let error = registry
        .add(&not_arc, None)
        .expect_err("a directory without .radian/arc/ is refused");
    let message = error.message();
    assert!(
        message.contains(&not_arc.display().to_string()),
        "the refusal names the path: {message}"
    );
    assert!(message.contains(".radian/arc"), "{message}");
    assert_eq!(text(&registry), before, "the file is unchanged");
}

#[test]
fn add_refuses_the_same_path_reached_through_a_symlink() {
    let scratch = Scratch::new("symlink");
    let registry = scratch.registry();
    let real = scratch.repo("real");
    registry.add(&real, None).expect("the first add succeeds");
    let before = text(&registry);

    let link = scratch.path("link");
    symlink(&real, &link).expect("the symlink is creatable");
    let error = registry
        .add(&link, Some("alias"))
        .expect_err("the canonical path is already registered");
    let message = error.message();
    assert!(message.contains("already registered"), "{message}");
    assert!(
        message.contains(&fs::canonicalize(&real).unwrap().display().to_string()),
        "the refusal names the canonical path: {message}"
    );
    assert_eq!(text(&registry), before, "the file is unchanged");
}

#[test]
fn add_refuses_a_duplicate_name() {
    let scratch = Scratch::new("dup-name");
    let registry = scratch.registry();
    registry
        .add(&scratch.repo("one"), Some("same"))
        .expect("the first add succeeds");
    let before = text(&registry);

    let error = registry
        .add(&scratch.repo("two"), Some("same"))
        .expect_err("the name is taken");
    assert!(error.message().contains("`same`"), "{}", error.message());
    assert_eq!(text(&registry), before, "the file is unchanged");
}

#[test]
fn remove_refuses_an_unknown_name_and_removes_only_the_named_entry() {
    let scratch = Scratch::new("remove");
    let registry = scratch.registry();
    registry.add(&scratch.repo("one"), None).unwrap();
    registry.add(&scratch.repo("two"), None).unwrap();

    let error = registry
        .remove("missing")
        .expect_err("an unknown name is refused");
    assert!(error.message().contains("`missing`"), "{}", error.message());
    assert_eq!(
        registry.list().unwrap().len(),
        2,
        "a refused remove changes nothing"
    );

    let removed = registry.remove("one").expect("the name exists");
    assert_eq!(removed.name, "one");
    let left: Vec<String> = registry
        .list()
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(left, vec!["two"]);
}

#[test]
fn a_hand_written_invalid_name_names_the_file_and_the_line() {
    let scratch = Scratch::new("invalid-name");
    let registry = scratch.registry();
    let repo = scratch.repo("real");
    fs::create_dir_all(scratch.path("hub")).unwrap();
    fs::write(
        registry.file(),
        format!(
            "[[repos]]\nname = \"Bad_Name\"\npath = \"{}\"\n",
            fs::canonicalize(&repo).unwrap().display()
        ),
    )
    .unwrap();

    let error = registry.list().expect_err("the name breaks the rule");
    let message = error.message();
    assert!(
        message.contains(&registry.file().display().to_string()),
        "the error names the file: {message}"
    );
    assert!(
        message.contains(":2:"),
        "the error names the line: {message}"
    );
    assert!(message.contains("Bad_Name"), "{message}");
    assert!(!valid_name("Bad_Name"));
}

#[test]
fn the_first_add_creates_the_directory_0700_and_the_file_0600() {
    let scratch = Scratch::new("modes");
    let registry = scratch.registry();
    registry.add(&scratch.repo("one"), None).unwrap();

    let dir_mode = fs::metadata(registry.dir()).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "the hub directory is owner-only");
    let file_mode = fs::metadata(registry.file()).unwrap().permissions().mode() & 0o777;
    assert_eq!(file_mode, 0o600, "the registry is owner-only");
}

#[test]
fn a_reader_holding_the_file_open_sees_the_old_whole_document() {
    let scratch = Scratch::new("atomic");
    let registry = scratch.registry();
    registry.add(&scratch.repo("one"), None).unwrap();

    // A child process holds fd 3 on the registry, reports that it opened it, waits for the
    // parent's go-ahead, and then reads the whole descriptor. Because the parent replaces the
    // file by rename, the descriptor keeps the old inode and must read the old whole document.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("exec 3< \"$1\"; echo opened; read line; cat <&3")
        .arg("sh")
        .arg(registry.file())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the reader starts");
    let mut stdout = BufReader::new(child.stdout.take().expect("the reader has stdout"));
    let mut opened = String::new();
    stdout.read_line(&mut opened).expect("the reader reports");
    assert_eq!(opened.trim(), "opened");

    registry.add(&scratch.repo("two"), None).unwrap();
    child
        .stdin
        .take()
        .expect("the reader has stdin")
        .write_all(b"go\n")
        .unwrap();
    let mut old = String::new();
    stdout.read_to_string(&mut old).expect("the reader reads");
    assert!(child.wait().unwrap().success(), "the reader exits cleanly");

    let old_doc: toml::Value = toml::from_str(&old).expect("the old read is a whole document");
    assert_eq!(
        old_doc["repos"].as_array().map(Vec::len),
        Some(1),
        "the held descriptor reads the old document: {old}"
    );
    let new_doc: toml::Value = toml::from_str(&text(&registry)).expect("the new document is whole");
    assert_eq!(
        new_doc["repos"].as_array().map(Vec::len),
        Some(2),
        "a fresh read sees the new document"
    );
}

/// `store_for` opens the registered repository's arc store the way the CLI does.
#[test]
fn store_for_opens_the_registered_repository() {
    let scratch = Scratch::new("store");
    let repo = scratch.repo("repo");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "hub@example.com"]);
    git(&repo, &["config", "user.name", "hub test"]);
    fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    fs::write(repo.join(".radian/arc/config.toml"), "schema_version = 1\n").unwrap();

    let registry = scratch.registry();
    registry.add(&repo, None).unwrap();
    let store = registry.store_for("repo").expect("the store opens");
    assert_eq!(store.repo(), fs::canonicalize(&repo).unwrap());

    let error = registry
        .store_for("missing")
        .expect_err("an unknown name is refused");
    assert!(error.message().contains("`missing`"), "{}", error.message());
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "hub test")
        .env("GIT_AUTHOR_EMAIL", "hub@example.com")
        .env("GIT_COMMITTER_NAME", "hub test")
        .env("GIT_COMMITTER_EMAIL", "hub@example.com")
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
