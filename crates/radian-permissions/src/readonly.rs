//! The built-in read-only shell table (backlog `shell-readonly-allow`).
//!
//! A set of `allow` rules for read-only shell commands, consulted by [`crate::hooks`]
//! after rule evaluation and before the tier's blanket shell rule: a rule `deny` still
//! wins first, and a named `ask` rule (an explicit user rule) still wins over the table.
//! A table hit bypasses [`crate::profile::ApprovalPolicy`] (`always`/`never`/`auto`
//! never turn it into a question: read-only commands are not worth reviewing, and a
//! headless turn must not lose them), and it is recorded with `approver = policy`.
//!
//! A hit needs all three at once:
//!
//! 1. The command is a simple command ([`crate::shell`]: no metacharacters, so `~`,
//!    quotes, `$`, globs and `;` already fall through to `ask`).
//! 2. Every non-option token resolves inside the workspace ([`crate::rule::resolve`]:
//!    no `..` escape, no leading `/`, no symlink out). A token that is not a path at
//!    all (a pattern, a count, a revision) still has to satisfy this; resolving it
//!    just joins it to the workspace, so plain words pass and only real escapes fail.
//!    Over-asking is the intended failure mode.
//! 3. No executable, long-lived or file-writing option, and no unknown option. Each
//!    program carries its own forbidden list plus a small safe list; anything else
//!    falls through to `ask`.
//!
//! `git diff`/`git log`/`git show` hits are a [`ReadOnlyHit::Rewrite`]: the two flags
//! `--no-ext-diff --no-textconv` are inserted right after the subcommand, so a
//! model-writable `.gitattributes` driver can never make the sandboxed call execute
//! an external diff or textconv command (ADR 0039 decision 3). Only those three
//! subcommands take the flags: `git branch/status/rev-parse/remote/stash` reject
//! `--no-textconv` (checked on git 2.53.0), and `git rev-parse` echoes it back as a
//! revision instead of accepting it.
//!
//! Option lists below were read from each program's `--help` on the dev box, except
//! `rg`, `tree` and `cargo`, which are not installed there: their lists come from
//! their documented options and are marked as such.

use std::path::Path;

use agent_core::ToolCall;

use crate::{rule, shell};

/// What a table hit decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyHit {
    /// Allowed as-is.
    Allow,
    /// Allowed with this exact `command` string instead.
    Rewrite { command: String },
}

/// The table's verdict for one tool call: `None` when the call is not a simple `shell`
/// command in the table.
pub fn match_command(workspace: &Path, call: &ToolCall) -> Option<ReadOnlyHit> {
    if call.name != "shell" {
        return None;
    }
    let command = call.arguments.get("command")?.as_str()?;
    if !shell::is_simple(command) {
        return None;
    }
    let tokens = shell::tokens(command);
    let program = tokens.first()?.as_str();
    match program {
        "git" => match_git(workspace, &tokens),
        "cargo" => match_cargo(workspace, &tokens),
        "ls" => match_plain(workspace, &tokens, &LS),
        "pwd" => match_plain(workspace, &tokens, &PWD),
        "wc" => match_plain(workspace, &tokens, &WC),
        "cat" => match_plain(workspace, &tokens, &CAT),
        "head" => match_plain(workspace, &tokens, &HEAD),
        "tail" => match_plain(workspace, &tokens, &TAIL),
        "stat" => match_plain(workspace, &tokens, &STAT),
        "file" => match_plain(workspace, &tokens, &FILE),
        "du" => match_plain(workspace, &tokens, &DU),
        "tree" => match_plain(workspace, &tokens, &TREE),
        "rg" => match_plain(workspace, &tokens, &RG),
        "grep" => match_plain(workspace, &tokens, &GREP),
        "find" => match_plain(workspace, &tokens, &FIND),
        _ => None,
    }
}

/// One program's option shape. `shorts` are the single-dash flag characters
/// (`-la` passes when both are listed); `words` are whole single-dash words (`-name`,
/// `-maxdepth`); `longs` are `--words` (a `--word=value` form is accepted when the
/// value resolves inside the workspace, exactly like a separate argument). Anything
/// else is not read-only. The `forbid_*` lists reject first, so they also cover
/// negated (`--no-*`) and valued forms.
struct Spec {
    shorts: &'static str,
    words: &'static [&'static str],
    longs: &'static [&'static str],
    forbid_shorts: &'static str,
    forbid_words: &'static [&'static str],
    forbid_longs: &'static [&'static str],
}

const LS: Spec = Spec {
    // From `ls --help` on the box. `-o` is a display format (long without group),
    // not an output file; `-L` follows every symlink during `-R` traversal, so it
    // stays out.
    shorts: "laRrtShdFU1ioucb0123456789",
    words: &[],
    longs: &[
        "all",
        "almost-all",
        "long",
        "human-readable",
        "recursive",
        "reverse",
        "sort",
        "time",
        "format",
        "color",
        "classify",
        "directory",
        "inode",
        "dereference-command-line",
        "quoting-style",
        "time-style",
    ],
    forbid_shorts: "L",
    forbid_words: &[],
    forbid_longs: &["dereference"],
};

const PWD: Spec = Spec {
    shorts: "LP",
    words: &[],
    longs: &["logical", "physical"],
    forbid_shorts: "",
    forbid_words: &[],
    forbid_longs: &[],
};

const WC: Spec = Spec {
    shorts: "cmlwL",
    words: &[],
    longs: &["bytes", "chars", "lines", "words", "max-line-length"],
    forbid_shorts: "",
    forbid_words: &[],
    forbid_longs: &[],
};

const CAT: Spec = Spec {
    shorts: "AbeEnstTuv",
    words: &[],
    longs: &[
        "show-all",
        "number-nonblank",
        "show-ends",
        "number",
        "squeeze-blank",
        "show-tabs",
        "show-nonprinting",
    ],
    forbid_shorts: "",
    forbid_words: &[],
    forbid_longs: &[],
};

const HEAD: Spec = Spec {
    shorts: "ncqv0123456789",
    words: &[],
    longs: &["lines", "bytes", "quiet", "silent", "verbose"],
    forbid_shorts: "",
    forbid_words: &[],
    forbid_longs: &[],
};

const TAIL: Spec = Spec {
    // From `tail --help`: `-f`/`-F`/`--follow`/`--retry` never terminate, so they
    // are not read-only; everything else is display-only.
    shorts: "ncq0123456789",
    words: &[],
    longs: &["lines", "bytes", "quiet", "silent", "verbose"],
    forbid_shorts: "fF",
    forbid_words: &[],
    forbid_longs: &["follow", "retry"],
};

const STAT: Spec = Spec {
    shorts: "Lftc",
    words: &[],
    longs: &["dereference", "file-system", "terse", "format", "printf"],
    forbid_shorts: "",
    forbid_words: &[],
    forbid_longs: &[],
};

const FILE: Spec = Spec {
    // From `file --help`: `-C`/`--compile` writes a compiled magic file, so it is
    // forbidden. `-m`/`-f` values are paths and go through the normal path check
    // (separate argument or `--opt=value` alike).
    shorts: "bikhlnNrszZFemf",
    words: &[],
    longs: &[
        "brief",
        "mime",
        "mime-type",
        "mime-encoding",
        "apple",
        "extension",
        "dereference",
        "no-dereference",
        "keep-going",
        "list",
        "uncompress",
        "raw",
        "special-files",
        "no-buffer",
        "no-pad",
        "exclude",
        "magic-file",
        "files-from",
        "separator",
    ],
    forbid_shorts: "C",
    forbid_words: &[],
    forbid_longs: &["compile"],
};

const DU: Spec = Spec {
    // From `du --help`: `-L`/`--dereference` follows every symlink found during
    // traversal, so only the command-line-only forms (`-D`/`-H`) are allowed; their
    // arguments are resolved like any other path.
    shorts: "shcakmbdDHPX0tlSx0123456789",
    words: &[],
    longs: &[
        "summarize",
        "human-readable",
        "total",
        "all",
        "kilobytes",
        "megabytes",
        "bytes",
        "block-size",
        "max-depth",
        "threshold",
        "null",
        "dereference-args",
        "separate-dirs",
        "one-file-system",
        "exclude",
        "exclude-from",
        "files0-from",
    ],
    forbid_shorts: "L",
    forbid_words: &[],
    forbid_longs: &["dereference"],
};

/// Not installed on the dev box; from the documented options.
const TREE: Spec = Spec {
    // `-o` writes the listing to a file; `-l` follows symlinks like directories.
    shorts: "adfhiprtsugDCnLPI0123456789",
    words: &[],
    longs: &["all", "dirs-only", "full-path", "noreport", "dirsfirst"],
    forbid_shorts: "ol",
    forbid_words: &[],
    forbid_longs: &["output"],
};

/// Not installed on the dev box; from the documented options. `--pre`/`--pre-glob`
/// run an external command per file; `-L`/`--follow` descends through symlinks found
/// during traversal.
const RG: Spec = Spec {
    shorts: "nlvFwcqmABCNIHget0123456789",
    words: &[],
    longs: &[
        "line-number",
        "no-line-number",
        "ignore-case",
        "files-with-matches",
        "invert-match",
        "fixed-strings",
        "word-regexp",
        "count",
        "quiet",
        "hidden",
        "glob",
        "type",
        "max-count",
        "context",
        "before-context",
        "after-context",
        "stats",
        "json",
        "files",
        "no-messages",
        "color",
        "heading",
        "no-heading",
        "with-filename",
        "no-filename",
    ],
    forbid_shorts: "L",
    forbid_words: &[],
    forbid_longs: &["follow", "pre", "pre-glob"],
};

const GREP: Spec = Spec {
    // From `grep --help`: `-o` is `--only-matching` (display), not an output file;
    // `-R` follows all symlinks during `-r` traversal, so only `-r` is allowed (its
    // command-line arguments are resolved like any other path). GNU grep has no
    // `--pre`, but the backlog names it: listed anyway.
    shorts: "nilvFwrcqshHboamABCEGdDeIf0123456789",
    words: &[],
    longs: &[
        "line-number",
        "ignore-case",
        "files-with-matches",
        "files-without-matches",
        "invert-match",
        "fixed-strings",
        "word-regexp",
        "recursive",
        "count",
        "quiet",
        "silent",
        "no-messages",
        "with-filename",
        "no-filename",
        "byte-offset",
        "only-matching",
        "text",
        "binary-files",
        "devices",
        "max-count",
        "context",
        "before-context",
        "after-context",
        "color",
        "basic-regexp",
        "extended-regexp",
        "exclude",
        "include",
        "exclude-dir",
    ],
    forbid_shorts: "R",
    forbid_words: &[],
    forbid_longs: &["dereference-recursive", "pre", "pre-glob"],
};

const FIND: Spec = Spec {
    // From `find --help`: `-exec`/`-execdir`/`-ok`/`-okdir` run commands,
    // `-delete` writes, `-fprint*`/`-fls` write files, `-L`/`-follow` follows every
    // symlink found during traversal. Single-dash clusters are not a find
    // thing, so `shorts` stays empty and every word must match exactly; the
    // `-exec`/`-ok` prefixes would also swallow the harmless `-executable`,
    // which then over-asks (intended).
    shorts: "0123456789",
    words: &[
        "maxdepth", "mindepth", "name", "iname", "path", "ipath", "type", "empty", "size", "print",
        "print0", "printf", "ls", "prune", "quit", "not", "a", "o", "P", "H",
    ],
    longs: &[],
    forbid_shorts: "L",
    forbid_words: &[
        "exec", "execdir", "ok", "okdir", "delete", "fprint", "fprint0", "fprintf", "fls", "follow",
    ],
    forbid_longs: &["follow"],
};

/// Generic program: every token after the program name is either a safe option or a
/// path that resolves inside the workspace.
fn match_plain(workspace: &Path, tokens: &[String], spec: &Spec) -> Option<ReadOnlyHit> {
    for token in &tokens[1..] {
        if !token_ok(workspace, token, spec) {
            return None;
        }
    }
    Some(ReadOnlyHit::Allow)
}

/// `cargo metadata` / `cargo tree`: no global options, so the subcommand is the
/// second token; the rest is the generic shape. Not installed on the dev box; from
/// the documented options. Neither subcommand builds or writes: `--manifest-path`
/// and friends are plain paths through the normal check.
fn match_cargo(workspace: &Path, tokens: &[String]) -> Option<ReadOnlyHit> {
    let subcommand = tokens.get(1)?.as_str();
    let spec = match subcommand {
        "metadata" => &CARGO_METADATA,
        "tree" => &CARGO_TREE,
        _ => return None,
    };
    for token in &tokens[2..] {
        if !token_ok(workspace, token, spec) {
            return None;
        }
    }
    Some(ReadOnlyHit::Allow)
}

const CARGO_METADATA: Spec = Spec {
    shorts: "q",
    words: &[],
    longs: &[
        "format-version",
        "no-deps",
        "filter-platform",
        "features",
        "all-features",
        "no-default-features",
        "manifest-path",
        "locked",
        "offline",
        "frozen",
        "quiet",
    ],
    forbid_shorts: "",
    forbid_words: &[],
    forbid_longs: &[],
};

const CARGO_TREE: Spec = Spec {
    shorts: "pq",
    words: &[],
    longs: &[
        "package",
        "depth",
        "edges",
        "prefix",
        "workspace",
        "duplicates",
        "charset",
        "manifest-path",
        "locked",
        "offline",
        "frozen",
        "quiet",
        "features",
        "all-features",
        "no-default-features",
        "target",
        "all-targets",
    ],
    forbid_shorts: "",
    forbid_words: &[],
    forbid_longs: &[],
};

/// `git`: global options (only `--no-pager`), then exactly one read-only
/// subcommand. `-c`/`-C` run configured commands (`core.fsmonitor`,
/// `core.sshCommand`), `--exec-path`/`--git-dir`/`--work-tree` re-root the
/// repository, and global `-p`/`--paginate` executes the pager; all fall through
/// to `ask`.
fn match_git(workspace: &Path, tokens: &[String]) -> Option<ReadOnlyHit> {
    let mut index = 1;
    while index < tokens.len() && tokens[index].starts_with('-') {
        if !global_ok(&tokens[index]) {
            return None;
        }
        index += 1;
    }
    let subcommand = tokens.get(index)?.as_str();
    let rest = &tokens[index + 1..];
    match subcommand {
        "status" => {
            each_token(workspace, rest, &GIT_STATUS)?;
            Some(ReadOnlyHit::Allow)
        }
        "rev-parse" => {
            each_token(workspace, rest, &GIT_REV_PARSE)?;
            Some(ReadOnlyHit::Allow)
        }
        "branch" => {
            if rest.is_empty() || rest == ["--list"] {
                Some(ReadOnlyHit::Allow)
            } else {
                None
            }
        }
        "remote" => {
            if rest == ["-v"] {
                Some(ReadOnlyHit::Allow)
            } else {
                None
            }
        }
        "stash" => {
            if rest == ["list"] {
                Some(ReadOnlyHit::Allow)
            } else {
                None
            }
        }
        "diff" | "log" | "show" => {
            each_token(workspace, rest, &GIT_DIFFISH)?;
            Some(ReadOnlyHit::Rewrite {
                command: rewrite_git(&tokens[..=index], rest),
            })
        }
        _ => None,
    }
}

const GIT_STATUS: Spec = Spec {
    shorts: "sbzvuM0123456789",
    words: &[],
    longs: &[
        "short",
        "branch",
        "show-stash",
        "ahead-behind",
        "porcelain",
        "long",
        "null",
        "untracked-files",
        "ignored",
        "ignore-submodules",
        "column",
        "no-renames",
        "renames",
        "find-renames",
        "verbose",
    ],
    forbid_shorts: "",
    forbid_words: &[],
    forbid_longs: &["output"],
};

/// `log`, `diff` and `show` share the diff option surface. `--output` writes a
/// file; `--ext-diff` and `--textconv` execute configured commands (the rewrite
/// below forces both off).
const GIT_DIFFISH: Spec = Spec {
    shorts: "psqnuUwBMCDRbO0123456789",
    words: &[],
    longs: &[
        "patch",
        "no-patch",
        "stat",
        "quiet",
        "source",
        "use-mailmap",
        "mailmap",
        "decorate",
        "max-count",
        "skip",
        "since",
        "until",
        "after",
        "before",
        "author",
        "committer",
        "grep",
        "all",
        "branches",
        "tags",
        "remotes",
        "graph",
        "format",
        "pretty",
        "oneline",
        "diff-filter",
        "no-renames",
        "renames",
        "find-renames",
        "find-copies",
        "follow",
        "first-parent",
        "merges",
        "no-merges",
        "reverse",
        "abbrev-commit",
        "no-abbrev-commit",
        "cached",
        "staged",
        "raw",
        "patch-with-raw",
        "unified",
        "minimal",
        "patience",
        "histogram",
        "word-diff",
        "color-words",
        "ignore-all-space",
        "ignore-space-change",
        "ignore-space-at-eol",
        "ignore-cr-at-eol",
        "ignore-blank-lines",
        "diff-algorithm",
    ],
    forbid_shorts: "",
    forbid_words: &[],
    forbid_longs: &["output", "ext-diff", "textconv"],
};

const GIT_REV_PARSE: Spec = Spec {
    shorts: "q0123456789",
    words: &[],
    longs: &[
        "quiet",
        "verify",
        "short",
        "abbrev-ref",
        "show-toplevel",
        "show-superproject-working-tree",
        "show-prefix",
        "absolute-git-dir",
        "symbolic",
    ],
    // `--git-dir` only prints a path, but the backlog lists it with the re-rooting
    // options, so it stays out.
    forbid_shorts: "",
    forbid_words: &[],
    forbid_longs: &["git-dir"],
};

/// The only safe global: `--no-pager` disables the pager rather than running one.
fn global_ok(token: &str) -> bool {
    token == "--no-pager"
}

fn each_token(workspace: &Path, tokens: &[String], spec: &Spec) -> Option<()> {
    for token in tokens {
        if !token_ok(workspace, token, spec) {
            return None;
        }
    }
    Some(())
}

/// One token is either a safe option (a `--word=value` value must itself resolve
/// inside the workspace) or a path that does.
fn token_ok(workspace: &Path, token: &str, spec: &Spec) -> bool {
    if token == "-" {
        // Explicit stdin: not an option, and always inside.
        return true;
    }
    if let Some(name) = token.strip_prefix("--") {
        let (name, value) = match name.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (name, None),
        };
        // A `--no-*` form is judged by what it negates; `--no-textconv` on a diff
        // is still the textconv option and stays out (over-asking).
        let judged = name.strip_prefix("no-").unwrap_or(name);
        if spec.forbid_longs.contains(&judged) {
            return false;
        }
        if spec.longs.contains(&name) {
            return match value {
                Some(value) => path_inside(workspace, value),
                None => true,
            };
        }
        return false;
    }
    if let Some(word) = token.strip_prefix('-') {
        if spec.forbid_words.contains(&word) {
            return false;
        }
        if spec.words.contains(&word) {
            return true;
        }
        // A cluster is only flag characters; anything glued on (`-ffoo`, `-U5`)
        // needs every character listed. `/` and `.` are never listed, so a glued
        // path value (`-O/etc/order`) always fails here; a passing cluster is
        // plain flags, which cannot name a path outside the workspace.
        return !word.is_empty()
            && word.chars().all(|c| spec.shorts.contains(c))
            && !word.chars().any(|c| spec.forbid_shorts.contains(c));
    }
    path_inside(workspace, token)
}

/// The flags go right after the subcommand; the rest keeps its order.
fn rewrite_git(head: &[String], rest: &[String]) -> String {
    let mut out = head.to_vec();
    out.push("--no-ext-diff".to_owned());
    out.push("--no-textconv".to_owned());
    out.extend(rest.iter().cloned());
    out.join(" ")
}

fn path_inside(workspace: &Path, raw: &str) -> bool {
    rule::resolve(workspace, raw).starts_with(workspace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    /// A workspace with `sub/f.txt` inside and a symlinked-out `link`.
    struct TestWs {
        ws: std::path::PathBuf,
        _base: std::path::PathBuf,
    }

    impl TestWs {
        fn new() -> Self {
            let n = N.fetch_add(1, Ordering::SeqCst);
            let base =
                std::env::temp_dir().join(format!("readonly-unit-{}-{n}", std::process::id()));
            let ws = base.join("ws");
            std::fs::create_dir_all(ws.join("sub")).unwrap();
            std::fs::write(ws.join("sub").join("f.txt"), "x").unwrap();
            std::fs::create_dir_all(base.join("outside")).unwrap();
            std::fs::write(base.join("outside").join("s.txt"), "x").unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(base.join("outside"), ws.join("link")).unwrap();
            let ws = std::fs::canonicalize(&ws).unwrap();
            Self { ws, _base: base }
        }

        fn hit(&self, command: &str) -> Option<ReadOnlyHit> {
            match_command(
                &self.ws,
                &ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: json!({ "command": command }),
                },
            )
        }
    }

    impl Drop for TestWs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self._base);
        }
    }

    fn rewrite(command: &str) -> Option<ReadOnlyHit> {
        Some(ReadOnlyHit::Rewrite {
            command: command.into(),
        })
    }

    /// The table's allow shapes, one per program.
    #[test]
    fn readonly_commands_are_allowed() {
        let ws = TestWs::new();
        for command in [
            "git status",
            "git status --short",
            "git status -sb",
            "git rev-parse --show-toplevel",
            "git rev-parse HEAD",
            "git branch",
            "git branch --list",
            "git remote -v",
            "git stash list",
            "git --no-pager status",
            "ls -la",
            "ls --color=always .",
            "pwd",
            "pwd -L",
            "wc -l sub/f.txt",
            "cat sub/f.txt",
            "cat -n sub/f.txt",
            "head -n 5 sub/f.txt",
            "tail -n 5 sub/f.txt",
            "stat sub/f.txt",
            "file sub/f.txt",
            "file -b sub/f.txt",
            "du -sh .",
            "du --max-depth=1 .",
            "grep -rn foo .",
            "grep -o foo sub/f.txt",
            "find . -maxdepth 2 -name x -print",
            "find . -o -print",
            "cargo metadata --format-version 1",
            "cargo tree -p foo --depth 2",
            "rg --stats foo .",
            "tree -L 2 .",
        ] {
            assert_eq!(ws.hit(command), Some(ReadOnlyHit::Allow), "{command:?}");
        }
    }

    /// `git diff`/`log`/`show` gain the two flags right after the subcommand.
    #[test]
    fn diffish_commands_are_rewritten_with_both_flags() {
        let ws = TestWs::new();
        for (command, rewritten) in [
            ("git log -p", "git log --no-ext-diff --no-textconv -p"),
            (
                "git log --oneline -5",
                "git log --no-ext-diff --no-textconv --oneline -5",
            ),
            (
                "git diff --stat",
                "git diff --no-ext-diff --no-textconv --stat",
            ),
            ("git show", "git show --no-ext-diff --no-textconv"),
            (
                "git --no-pager show --stat HEAD",
                "git --no-pager show --no-ext-diff --no-textconv --stat HEAD",
            ),
        ] {
            assert_eq!(ws.hit(command), rewrite(rewritten), "{command:?}");
        }
    }

    /// Everything outside the table, outside the workspace, or with a forbidden
    /// or unknown option falls through to `ask` (`None` here; the hooks layer
    /// turns that into the normal ask flow).
    #[test]
    fn non_readonly_commands_do_not_match() {
        let ws = TestWs::new();
        let outside = ws
            ._base
            .join("outside")
            .join("s.txt")
            .to_string_lossy()
            .into_owned();
        let mut cases: Vec<String> = vec![
            // Not a shell command, or not a simple one (`~`, quotes, `$`, `;`
            // never reach the option check).
            "cat ~/.ssh/id_rsa",
            "cat ../x",
            "cat sub/../../outside/s.txt",
            "find . -exec rm {} +",
            "git status; rm -rf /",
            "rm -rf x",
            "git push",
            "cargo build",
            "cargo +nightly metadata",
            "cargo --verbose metadata",
            // Absolute paths outside the workspace.
            "cat /etc/passwd",
            // Executable, long-lived and file-writing options.
            "find . -exec rm +",
            "find . -execdir rm +",
            "find . -ok rm +",
            "find . -delete",
            "find . -fls out",
            "find . -L",
            "find . -follow",
            "git -c core.pager=x log",
            "git -C .. status",
            "git --exec-path=/bin status",
            "git --git-dir=x status",
            "git --work-tree=x status",
            "git -p log",
            "git diff --output=x",
            "git diff --ext-diff",
            "git log --textconv",
            "git log --no-textconv",
            "git status --output=x",
            "tail -f log",
            "tail -F log",
            "tail --follow=name log",
            "tail --retry log",
            "ls -L",
            "grep -R foo .",
            "du -L .",
            "file -C",
            "file --compile",
            "tree -o out .",
            "rg --pre cat foo",
            "cargo metadata --verbose",
            // Unknown options over-ask, even harmless ones.
            "ls --colorful",
            "cat --tabs=4",
            "git status --bogus",
            "git stash push",
            "git remote",
            "git remote add x y",
            "git branch -a",
            "git branch foo",
            "git stash list --all",
            // `--` does not separate pathspecs here; it is an unknown option.
            "ls -- .",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        cases.push(format!("cat {outside}"));
        // A symlink out of the workspace (only created on unix).
        #[cfg(unix)]
        cases.push("cat link/s.txt".to_owned());
        cases.push(format!("cargo metadata --manifest-path {outside}"));
        cases.push(format!("cargo metadata --manifest-path={outside}"));
        for command in &cases {
            assert_eq!(ws.hit(command), None, "{command:?}");
        }
    }

    /// Non-shell tools and non-string commands never match.
    #[test]
    fn only_shell_string_commands_match() {
        let ws = TestWs::new();
        assert_eq!(
            match_command(
                &ws.ws,
                &ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: json!({ "path": "sub/f.txt" }),
                },
            ),
            None
        );
        assert_eq!(
            match_command(
                &ws.ws,
                &ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: json!({}),
                },
            ),
            None
        );
    }
}
