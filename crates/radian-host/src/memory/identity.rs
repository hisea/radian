//! Project identity (ADR 0021 D2): the **git common dir** of the repository containing the
//! workspace, or the workspace's canonical path when it is not in a repository. Pure Rust:
//! no `git` process is ever spawned.
//!
//! The rule is what makes two worktrees of one repository share a `ProjectId`: a normal
//! repository's `.git` is a directory and *is* the common dir; a worktree's `.git` is a file
//! whose `gitdir:` points at `<common>/worktrees/<name>`, and the `commondir` file there
//! names the common dir. Both resolve to the same canonical path.

use std::path::{Path, PathBuf};

/// The identity key for `workspace`.
///
/// Walks up from the workspace to the nearest `.git`:
///
/// - a **directory** → its canonical path (a normal repository; the common dir is `.git`
///   itself);
/// - a **file** → the `gitdir:` pointer is resolved (a relative value against the file's
///   directory), then the common dir is read from `<gitdir>/commondir` (a relative value
///   against the gitdir); when that file is absent the gitdir itself is the common dir (a
///   submodule's `<super>/.git/modules/<name>`). A pointer that cannot be read or resolved falls through
///   to the workspace path.
/// - no `.git` at all → the workspace's canonical path.
///
/// The workspace is canonicalized first, so a symlinked workspace and its target share one
/// key. This function never fails: an unreadable path falls back rather than erroring.
pub fn project_identity_key(workspace: &Path) -> PathBuf {
    resolve(workspace).0
}

/// Whether the identity key comes from a git repository rather than the workspace path. This
/// is what makes a directory a **project workspace** (ADR 0021): only such a
/// workspace has a project a default-scope write may register. `false` for the fallbacks of
/// [`project_identity_key`] — no `.git` above the workspace, or a `.git` file with no usable
/// `gitdir:` pointer — because there the key is the workspace path itself.
pub fn is_git_identity(workspace: &Path) -> bool {
    resolve(workspace).1
}

/// `(identity key, key comes from git)`: the shared body of [`project_identity_key`] and
/// [`is_git_identity`], so the two cannot disagree about what was found.
fn resolve(workspace: &Path) -> (PathBuf, bool) {
    let start = canonical(workspace);
    let mut dir = start.as_path();
    loop {
        let dot_git = dir.join(".git");
        match std::fs::symlink_metadata(&dot_git) {
            // A normal repository: `.git` itself is the common dir.
            Ok(meta) if meta.is_dir() => return (canonical(&dot_git), true),
            // A worktree (or a submodule): the pointer names the common dir.
            Ok(meta) if meta.is_file() => {
                if let Some(common) = common_dir_from_gitfile(&dot_git) {
                    return (common, true);
                }
                break;
            }
            _ => {}
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    (start, false)
}

/// The name two identity keys are compared by when looking for candidate projects
/// (ADR 0021 D2: "同名仓库"). It is the final path component, except that a git common dir
/// (the final component `.git`) is named by its parent directory — the repository name. So a
/// moved repository still matches its old project, and all worktrees of one repository match
/// each other, while the key itself stays the full common-dir path.
pub fn candidate_name(identity_key: &str) -> String {
    let path = Path::new(identity_key);
    let last = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if last == ".git" {
        path.parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or(last)
            .to_owned()
    } else {
        last.to_owned()
    }
}

fn common_dir_from_gitfile(dot_git: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dot_git).ok()?;
    let raw = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))?
        .trim();
    if raw.is_empty() {
        return None;
    }
    let pointer = PathBuf::from(raw);
    let pointer = if pointer.is_absolute() {
        pointer
    } else {
        dot_git.parent()?.join(pointer)
    };
    let gitdir = canonical(&pointer);

    // `commondir` is the authoritative pointer (git writes it for every linked worktree,
    // usually `../..`). Without it the gitdir **is** the common dir: that is a submodule,
    // whose gitdir is `<super>/.git/modules/<name>` — taking the grandparent there would
    // silently give the submodule the superproject's identity.
    let common = std::fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .map(|text| {
            let path = PathBuf::from(&text);
            if path.is_absolute() {
                path
            } else {
                gitdir.join(path)
            }
        })
        .unwrap_or(gitdir);
    Some(canonical(&common))
}

/// `std::fs::canonicalize`, falling back to the path itself when it cannot be resolved (the
/// function then still returns a stable value instead of failing).
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> crate::test_support::TempDir {
        crate::test_support::TempDir::with_label("radian-memory-id", label)
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repository without `git`: `.git/` as a plain directory is already the common dir, so
    /// the identity key is its canonical path.
    #[test]
    fn a_directory_git_is_the_identity_key() {
        let dir = scratch("dir-git");
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let key = project_identity_key(&repo);
        assert_eq!(key, std::fs::canonicalize(repo.join(".git")).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_subdirectory_finds_the_repository_above_it() {
        let dir = scratch("subdir");
        let repo = dir.join("repo");
        let nested = repo.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(
            project_identity_key(&nested),
            std::fs::canonicalize(repo.join(".git")).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn outside_a_repository_the_canonical_workspace_is_the_key() {
        let dir = scratch("no-repo");
        let ws = dir.join("plain");
        std::fs::create_dir_all(&ws).unwrap();
        let key = project_identity_key(&ws);
        assert_eq!(key, std::fs::canonicalize(&ws).unwrap());
        // A symlink to the same directory gives the same key.
        #[cfg(unix)]
        {
            let link = dir.join("link");
            std::os::unix::fs::symlink(&ws, &link).unwrap();
            assert_eq!(project_identity_key(&link), key);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hand-written worktree pointer with a **relative** `gitdir:` and a `commondir`: the
    /// case the parser must handle without `git`.
    #[test]
    fn a_relative_gitdir_and_commondir_are_resolved() {
        let dir = scratch("rel-gitdir");
        let common = dir.join("main").join(".git");
        let gitdir = common.join("worktrees").join("wt");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        let wt = dir.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        // Relative to the `.git` file's directory (the worktree).
        let pointer = std::path::PathBuf::from("..").join(gitdir.strip_prefix(&dir).unwrap());
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", pointer.display())).unwrap();

        assert_eq!(
            project_identity_key(&wt),
            std::fs::canonicalize(&common).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without a `commondir` file, `<common>/worktrees/<name>` still gives the common dir.
    #[test]
    fn a_missing_commondir_means_the_gitdir_is_the_common_dir() {
        // git's rule: `commondir` absent → the gitdir itself is the common dir (a submodule's
        // `<super>/.git/modules/<name>`). Never the grandparent: that would be the
        // superproject's `.git`.
        let dir = scratch("no-commondir");
        let gitdir = dir.join("super").join(".git").join("modules").join("sub");
        std::fs::create_dir_all(&gitdir).unwrap();
        let sub = dir.join("super").join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").unwrap();
        assert_eq!(
            project_identity_key(&sub),
            std::fs::canonicalize(&gitdir).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_submodule_is_its_own_project_not_the_superprojects() {
        if !git_available() {
            eprintln!("skipping: git is not on PATH");
            return;
        }
        let dir = scratch("submodule");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        git(&sub, &["init", "-q"]);
        git(&sub, &["config", "user.email", "t@example.invalid"]);
        git(&sub, &["config", "user.name", "t"]);
        git(&sub, &["commit", "-q", "--allow-empty", "-m", "s"]);
        let sup = dir.join("super");
        std::fs::create_dir_all(&sup).unwrap();
        git(&sup, &["init", "-q"]);
        git(
            &sup,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub.to_str().unwrap(),
                "sub",
            ],
        );

        let super_key = project_identity_key(&sup);
        let sub_key = project_identity_key(&sup.join("sub"));
        assert_ne!(super_key, sub_key, "a submodule is not its superproject");
        // What git itself says the submodule's common dir is.
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .current_dir(sup.join("sub"))
            .output()
            .unwrap();
        let git_says =
            std::fs::canonicalize(String::from_utf8(out.stdout).unwrap().trim()).unwrap();
        assert_eq!(sub_key, git_says);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_worktree_and_its_main_repository_share_one_key() {
        if !git_available() {
            eprintln!("skipping: git is not on PATH");
            return;
        }
        let dir = scratch("worktree");
        let main = dir.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@example.invalid"]);
        git(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join("a.txt"), "hi\n").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-qm", "init"]);
        let wt = dir.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wtb"],
        );

        let main_key = project_identity_key(&main);
        let wt_key = project_identity_key(&wt);
        assert_eq!(
            main_key, wt_key,
            "a worktree shares the main repository's key"
        );
        assert!(main_key.ends_with(".git"), "{}", main_key.display());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_identity_matches_git_common_dir_when_git_is_available() {
        if !git_available() {
            eprintln!("skipping: git is not on PATH");
            return;
        }
        let dir = scratch("git-common");
        let main = dir.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@example.invalid"]);
        git(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join("a.txt"), "hi\n").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-qm", "init"]);
        let wt = dir.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wtb"],
        );

        for workspace in [&main, &wt] {
            let out = std::process::Command::new("git")
                .current_dir(workspace)
                .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
                .output()
                .expect("run git rev-parse");
            assert!(out.status.success());
            let reported = String::from_utf8(out.stdout).unwrap();
            let reported = std::fs::canonicalize(reported.trim()).unwrap();
            assert_eq!(project_identity_key(workspace), reported);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidate_names_use_the_repository_directory_for_a_common_dir() {
        assert_eq!(candidate_name("/x/y/radian/.git"), "radian");
        assert_eq!(candidate_name("/x/y/other/.git"), "other");
        assert_eq!(candidate_name("/x/y/plain"), "plain");
    }

    /// Only a key that came from git marks a **project workspace** (ADR 0021): the
    /// workspace path itself is the fallback, and there no write may register a project.
    #[test]
    fn only_a_git_identity_is_a_project_workspace() {
        let dir = scratch("git-identity");
        let repo = dir.join("repo");
        let nested = repo.join("a");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert!(is_git_identity(&repo));
        assert!(
            is_git_identity(&nested),
            "a subdirectory is in the repository"
        );

        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(!is_git_identity(&plain));

        // A `.git` file without a usable `gitdir:` pointer falls back to the workspace path,
        // so the key is not a git identity either.
        let broken = dir.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join(".git"), "not a pointer\n").unwrap();
        assert!(!is_git_identity(&broken));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
