//! Read-only git tools, and the hard assertion that they leave `.git` byte-for-byte and
//! mtime-for-mtime unchanged.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use common::{TempDir, assert_outside, call, text, tools};
use radian_coding::{CodingTools, Limits};
use serde_json::json;

/// Sets up a small repository with one commit, one modification, and one untracked file.
fn repo(dir: &TempDir) -> std::path::PathBuf {
    let root = dir.file("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "test@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    // `git commit` triggers git's background auto-maintenance, which can create
    // `.git/objects/maintenance.lock` after this setup returns and race the `.git` snapshot
    // in `git_diff_stat_reports_...`. That is git's own job, not the read-only tools', so pin
    // the repo: no auto maintenance, no auto gc. (Observed on git 2.55 in the Linux VM; the
    // macOS git 2.53 did not run it, which is why the test only flaked there.)
    git(&root, &["config", "maintenance.auto", "false"]);
    git(&root, &["config", "gc.auto", "0"]);
    std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
    git(&root, &["add", "tracked.txt"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    std::fs::write(root.join("tracked.txt"), "one\ntwo\n").unwrap();
    std::fs::write(root.join("untracked.txt"), "new\n").unwrap();
    root
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git must be installed to run these tests");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Item 4: the `git_diff` description names the exact status command the code runs
/// (`--porcelain=v1 --branch --untracked-files=all`), not "the short form".
#[tokio::test]
async fn the_git_diff_description_names_the_status_command() {
    use agent_core::{ToolProvider, ToolSession};
    let dir = TempDir::new("git-desc");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let session = tools(&root).open_session().await.unwrap();
    let spec = session
        .tool_specs()
        .iter()
        .find(|spec| spec.name == "git_diff")
        .expect("the git_diff spec");
    assert!(
        spec.description.contains("--porcelain=v1 --branch --untracked-files=all"),
        "the description must name the status command: {}",
        spec.description
    );
    assert!(
        !spec.description.contains("short form"),
        "the old 'short form' wording must be gone: {}",
        spec.description
    );
    let field = &spec.input_schema["properties"]["stat"]["description"];
    assert!(
        field
            .as_str()
            .is_some_and(|text| text.contains("--porcelain=v1 --branch")),
        "the `stat` field must name the status command: {field}"
    );
}

/// Every file under `.git`, keyed by relative path, with length, mtime, and content, so a
/// rewrite of identical bytes is still detected.
fn snapshot_git(root: &Path) -> BTreeMap<String, (u64, std::time::SystemTime, Vec<u8>)> {
    let mut out = BTreeMap::new();
    let git_dir = root.join(".git");
    let mut stack = vec![git_dir.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path);
            } else {
                let key = path
                    .strip_prefix(&git_dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let mtime = meta.modified().unwrap();
                let bytes = std::fs::read(&path).unwrap_or_default();
                out.insert(key, (meta.len(), mtime, bytes));
            }
        }
    }
    out
}

#[tokio::test]
async fn git_diff_stat_reports_the_worktree_and_leaves_dot_git_untouched() {
    let dir = TempDir::new("git-status");
    let root = repo(&dir);
    let tools = tools(&root);

    let before = snapshot_git(&root);

    let status = call(&tools, "git_diff", json!({"stat": true}))
        .await
        .unwrap();
    let status_text = text(&status);
    // The short status: the branch header, the modified tracked file and the untracked file.
    assert!(status_text.contains("tracked.txt"), "{status_text}");
    assert!(status_text.contains("untracked.txt"), "{status_text}");
    // The `--stat` summary names the modified file too.
    assert!(
        status_text.contains("tracked.txt |"),
        "the --stat summary: {status_text}"
    );

    let diff = call(&tools, "git_diff", json!({})).await.unwrap();
    let diff_text = text(&diff);
    assert!(diff_text.contains("+two"), "{diff_text}");

    let after = snapshot_git(&root);
    assert_eq!(
        before, after,
        ".git must be byte-for-byte and mtime-for-mtime unchanged by read-only git tools"
    );
}

/// ADR 0043 decision 3: `git_status` is a one-version alias. It runs `git_diff { stat: true }`
/// and the result starts with one line naming the rename; everything after that line is the new
/// call's output, byte for byte.
#[tokio::test]
async fn the_git_status_alias_runs_git_diff_stat_and_says_so() {
    let dir = TempDir::new("git-status-alias");
    let root = repo(&dir);
    let tools = tools(&root);

    let alias = call(&tools, "git_status", json!({})).await.unwrap();
    let alias_text = text(&alias);
    let (first, rest) = alias_text.split_once('\n').expect("a rename line");
    assert!(first.contains("renamed"), "{first}");
    assert!(first.contains("git_diff"), "{first}");

    let new = call(&tools, "git_diff", json!({"stat": true}))
        .await
        .unwrap();
    assert_eq!(
        rest,
        text(&new),
        "the alias is exactly the new call's output"
    );
}

#[tokio::test]
async fn staged_diff_only_shows_staged_changes() {
    let dir = TempDir::new("git-staged");
    let root = repo(&dir);
    git(&root, &["add", "tracked.txt"]);
    let tools = tools(&root);

    let unstaged = call(&tools, "git_diff", json!({})).await.unwrap();
    assert!(!text(&unstaged).contains("+two"), "{}", text(&unstaged));

    let staged = call(&tools, "git_diff", json!({"staged": true}))
        .await
        .unwrap();
    assert!(text(&staged).contains("+two"), "{}", text(&staged));
}

#[tokio::test]
async fn a_path_limited_diff_stays_inside_the_workspace() {
    let dir = TempDir::new("git-path");
    let root = repo(&dir);
    let outside = common::sentinel_outside(&dir);
    let tools = tools(&root);

    let diff = call(&tools, "git_diff", json!({"path": "tracked.txt"}))
        .await
        .unwrap();
    assert!(text(&diff).contains("tracked.txt"));

    let failure = call(
        &tools,
        "git_diff",
        json!({"path": outside.join("sentinel.txt").to_string_lossy()}),
    )
    .await
    .unwrap_err();
    assert_outside(&failure, "absolute");

    let failure = call(
        &tools,
        "git_diff",
        json!({"path": "../outside/sentinel.txt"}),
    )
    .await
    .unwrap_err();
    assert_outside(&failure, "../outside");
}

#[tokio::test]
async fn git_outside_a_repository_is_a_clear_failure() {
    let dir = TempDir::new("git-not-repo");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = tools(&root);

    let failure = call(&tools, "git_diff", json!({"stat": true}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "git_failed", "{failure:?}");
    assert!(
        failure.message.contains("not a git repository"),
        "{failure:?}"
    );
}

#[tokio::test]
async fn git_output_is_capped_with_a_marker() {
    let dir = TempDir::new("git-cap");
    let root = repo(&dir);
    let tools = CodingTools::new(&root).unwrap().with_limits(Limits {
        max_git_output_bytes: 16,
        ..Limits::default()
    });

    let output = call(&tools, "git_diff", json!({"stat": true}))
        .await
        .unwrap();
    assert!(text(&output).ends_with("[git output truncated]\n"));
    assert!(text(&output).len() < 200);
}

#[tokio::test]
async fn git_tools_are_not_confused_with_the_other_tools() {
    // `git_diff` accepts only its own arguments: an unknown key is refused with the argument
    // echo, not passed to git.
    let dir = TempDir::new("git-extra");
    let root = repo(&dir);
    let tools = tools(&root);
    let failure = call(&tools, "git_diff", json!({"anything": 1}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "invalid_arguments", "{failure:?}");
    assert!(
        failure.message.contains("unknown argument `anything`"),
        "{}",
        failure.message
    );
    // The alias keeps the old spec: it takes no arguments at all.
    let failure = call(&tools, "git_status", json!({"anything": 1}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "invalid_arguments", "{failure:?}");
}

/// ADR 0039 decision 3: the read-only git tools run in the host, so a model-writable
/// `.gitattributes` naming a marker filter must not make `git_diff` run the clean
/// filter of a stat-dirty file.
#[tokio::test]
async fn the_git_tools_never_run_a_repository_filter() {
    let dir = TempDir::new("git-filter");
    let root = repo(&dir);
    let marker = common::sentinel_outside(&dir).join("FILTER-RAN");
    let script = root.join("evil-clean.sh");
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
    git(
        &root,
        &["config", "filter.evil.clean", script.to_str().unwrap()],
    );
    git(
        &root,
        &["config", "filter.evil.smudge", script.to_str().unwrap()],
    );
    git(&root, &["config", "filter.evil.required", "true"]);
    std::fs::write(root.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    // `repo()` left tracked.txt stat-dirty; rewrite it (same size as the index blob) so git
    // hashes the content and runs the clean filter on the status read too.
    std::fs::write(root.join("tracked.txt"), "two\n").unwrap();

    let tools = tools(&root);
    let status = call(&tools, "git_diff", json!({"stat": true}))
        .await
        .expect("status");
    assert!(text(&status).contains("tracked.txt"), "{}", text(&status));
    let _ = call(&tools, "git_diff", json!({})).await.expect("diff");
    assert!(
        !marker.exists(),
        "git_diff must not run a repository filter in the host"
    );
}

/// ADR 0039 decision 3: a `.gitattributes` line the model can write (`*.txt diff=evil`)
/// must not make the host `git_diff` run the driver's external command. That command runs
/// with host privileges, so the test points it at a marker file outside the workspace and
/// asserts the marker never appears — while the diff itself still shows the change. An
/// external command's stdout replaces the diff, so the silent script below would also hide
/// `+two` if it ever ran.
#[tokio::test]
async fn git_diff_never_runs_an_external_diff_command() {
    let dir = TempDir::new("git-ext-diff");
    let root = repo(&dir);
    let marker = common::sentinel_outside(&dir).join("EXT-DIFF-RAN");
    let script = root.join("evil-ext-diff.sh");
    std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(
        &root,
        &["config", "diff.evil.command", script.to_str().unwrap()],
    );
    std::fs::write(root.join(".gitattributes"), "*.txt diff=evil\n").unwrap();

    let tools = tools(&root);
    let diff = call(&tools, "git_diff", json!({})).await.expect("diff");
    assert!(text(&diff).contains("+two"), "{}", text(&diff));
    assert!(
        !marker.exists(),
        "git_diff must not run an external diff command in the host"
    );

    let stat = call(&tools, "git_diff", json!({"stat": true}))
        .await
        .expect("stat");
    assert!(
        text(&stat).contains("tracked.txt |"),
        "the --stat summary: {}",
        text(&stat)
    );
    assert!(
        !marker.exists(),
        "git_diff --stat must not run an external diff command in the host"
    );
}

/// ADR 0039 decision 3, the textconv twin of the test above: `diff.<driver>.textconv` is
/// the other command a `diff=` attribute can name, and it needs its own flag
/// (`--no-textconv`). The converter swallows both blobs and prints one fixed line, so a
/// run would both leave the marker and hide the change.
#[tokio::test]
async fn git_diff_never_runs_a_diff_textconv() {
    let dir = TempDir::new("git-textconv");
    let root = repo(&dir);
    let marker = common::sentinel_outside(&dir).join("TEXTCONV-RAN");
    let script = root.join("evil-textconv.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ntouch {}\ncat >/dev/null\necho MANGLED\n",
            marker.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(
        &root,
        &["config", "diff.evil.textconv", script.to_str().unwrap()],
    );
    std::fs::write(root.join(".gitattributes"), "*.txt diff=evil\n").unwrap();

    let tools = tools(&root);
    let diff = call(&tools, "git_diff", json!({})).await.expect("diff");
    assert!(text(&diff).contains("+two"), "{}", text(&diff));
    assert!(
        !marker.exists(),
        "git_diff must not run a diff textconv in the host"
    );

    let stat = call(&tools, "git_diff", json!({"stat": true}))
        .await
        .expect("stat");
    assert!(
        text(&stat).contains("tracked.txt |"),
        "the --stat summary: {}",
        text(&stat)
    );
    assert!(
        !marker.exists(),
        "git_diff --stat must not run a diff textconv in the host"
    );
}
