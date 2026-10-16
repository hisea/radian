//! `radian theme import` (ADR 0029) runs as an ordinary command: it never
//! enters the TUI, so this test drives the binary directly with a private `RADIAN_HOME`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A unique scratch directory per test, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-tui-import-{}-{label}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/themes")
}

fn fixture(stem: &str, extension: &str) -> PathBuf {
    fixtures_dir().join(format!("{stem}.{extension}"))
}

/// Runs the binary with a private root, so it neither reads nor writes the real `~/.radian`.
fn radian(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_radian"))
        .env_clear()
        .env("RADIAN_HOME", home)
        .env("HOME", home)
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .args(args)
        .output()
        .expect("run radian theme import")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The golden a converted fixture must match, byte for byte.
fn golden(stem: &str) -> String {
    std::fs::read_to_string(fixture(stem, "toml")).expect("the golden is readable")
}

/// Importing a pi fixture writes `<RADIAN_HOME>/themes/<name>.toml` (the committed golden) and
/// prints the dropped-key report.
#[test]
fn import_writes_the_golden_and_reports_the_dropped_keys() {
    let home = TempDir::new("writes");
    let input = fixture("kanagawa-wave", "json");
    let output = radian(home.path(), &["theme", "import", input.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));

    let written = home.path().join("themes/kanagawa-wave.toml");
    assert_eq!(
        std::fs::read_to_string(&written).unwrap(),
        golden("kanagawa-wave"),
        "the imported TOML must equal the committed golden"
    );
    let text = stdout(&output);
    assert!(text.contains("wrote "), "{text}");
    assert!(
        text.contains("dropped pi key(s): bashMode"),
        "the report lists the dropped keys: {text}"
    );
}

/// Without `--force` an existing `<name>.toml` is refused and left untouched; `--force`
/// overwrites it (ADR 0029).
#[test]
fn import_refuses_to_overwrite_without_force_and_force_wins() {
    let home = TempDir::new("force");
    let input = fixture("nord", "json");
    let first = radian(home.path(), &["theme", "import", input.to_str().unwrap()]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let written = home.path().join("themes/nord.toml");

    // A second import with a different name and no `--force` refuses, and the file is unchanged.
    let before = std::fs::read_to_string(&written).unwrap();
    let refused = radian(
        home.path(),
        &["theme", "import", input.to_str().unwrap(), "--name", "nord"],
    );
    assert_eq!(refused.status.code(), Some(1), "{}", stdout(&refused));
    assert!(stderr(&refused).contains("--force"), "{}", stderr(&refused));
    assert_eq!(std::fs::read_to_string(&written).unwrap(), before);

    // `--force` overwrites it.
    let forced = radian(
        home.path(),
        &[
            "theme",
            "import",
            input.to_str().unwrap(),
            "--force",
            "--name",
            "chosen",
        ],
    );
    assert_eq!(forced.status.code(), Some(0), "{}", stderr(&forced));
    assert!(
        std::fs::read_to_string(home.path().join("themes/chosen.toml"))
            .unwrap()
            .starts_with("name = \"chosen\""),
        "the --name override is the file's name"
    );
}

/// A missing input file is a one-line failure and the usage-class exit code mapping still holds.
#[test]
fn import_of_a_missing_file_fails_without_writing() {
    let home = TempDir::new("missing");
    let output = radian(home.path(), &["theme", "import", "/no/such/theme.json"]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("cannot read"),
        "{}",
        stderr(&output)
    );
    assert!(!home.path().join("themes").exists(), "nothing was written");
}
