#!/usr/bin/env python3
"""Whether a run reached the reference solution: one run's leak level.

    python3 benchmarks/repo-tasks/leak.py <run dir> [--source REPO] [--journal FILE]...

Reads every tool call in the run's session journals (radian and pi formats),
parses each `git <subcommand> ...` segment of its string arguments, and writes
`<run dir>/leak.json`:

- `level`: 0 to 3 (below), or null when the run has no journal (the agent did
  not start); null is "no journal", never 0.
- `evidence`: every line that raised the level, as `{"level", "text"}`.
- `outside`: absolute paths under `$HOME` that are not inside the run's tree
  (`~/.cargo` and `~/.rustup` excluded). This does not change the level.

Levels:

- 3, used: `cherry-pick`, `merge`, `checkout`, `switch`, `restore`, `reset`,
  `rebase`, `apply` or `am` of a later sha or a later ref; `git show
  <later>:<path> >`; or a second history brought in: `git remote add` (or
  `set-url`), `git fetch` or `git pull` of a path or url, or any `git clone`.
- 2, viewed: `show`, `diff`, `cat-file` or `format-patch` of a later sha or ref,
  or `log -p`, `--patch`, `--stat` or `-- <path>` on one.
- 1, names: `log --all`, `log` of a later ref, `branch -a`, `--all`, `-r`,
  `--contains` or `--list`, and `rev-list`, `reflog`, `for-each-ref`,
  `show-ref` or `merge-base` with a later ref or `--all`.
- 0: none of these.

A sha is later when the source repository has it, it is not an ancestor of the
run's base, and it is not one of the run's own commits (`commits.txt`). A later
ref is `origin/*`, `remotes/*`, `work/TK*`, `refs/heads/work/*` or `main`. The
source repository is `--source`, else the one `run.sh` fetches from
(`REPO_TASKS_SOURCE`, else the repository holding this file).

What this cannot see: content read without a `git` command (`.git/objects`
read directly), a sha built at run time by a shell substitution, and a
repository reached through a relative path.

Stdlib only.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import lib  # noqa: E402

SHA_RE = re.compile(r"\b[0-9a-f]{7,40}\b")
LATER_REF = re.compile(
    r"(?<![\w/])(?:origin/[\w./-]+|remotes/[\w./-]+|work/TK[\w.-]+|refs/heads/work/[\w.-]+)"
)
MAIN_REF = re.compile(r"(?<![\w/-])main\b")
# `git`, its global options, the subcommand, and the rest of the shell segment.
GIT_SEG = re.compile(
    r"\bgit\b(?:\s+-C\s+\S+|\s+-c\s+\S+|\s+--[\w-]+(?:=\S+)?)*\s+([\w-]+)([^;&|\n]*)"
)
SHOW_TO_FILE = re.compile(r"[0-9a-f]{7,40}:\S+\s*>")
BRINGS = ("cherry-pick", "merge", "checkout", "switch", "restore", "reset", "rebase", "apply", "am")
# Excluded from `outside`: the toolchain under the home directory.
SAFE_HOME_DIRS = (".cargo", ".rustup")
PATH_END = r"[^\s'\"`;|&)>]*"


class Scanner:
    """One run's scan; `is_later` answers are cached per sha."""

    def __init__(self, source, base: str, own_shas, tree: str | None, home: str):
        self.source = str(source) if source else None
        self.base = base
        self.own = {sha for sha in own_shas if sha}
        self.tree = tree.rstrip("/") if tree else None
        self.home = home.rstrip("/")
        self._later: dict[str, bool] = {}
        self._home_re = re.compile(
            r"(?:(?<![\w/.~-])" + re.escape(self.home) + r"|(?<![\w/])~|\$HOME|\$\{HOME\})"
            r"(/" + PATH_END + r"|(?=[\s'\"`;|&)>]|$))"
        )

    def own_sha(self, sha: str) -> bool:
        return any(sha.startswith(own) or own.startswith(sha) for own in self.own)

    def is_later(self, sha: str) -> bool:
        if self.own_sha(sha) or not self.source:
            return False
        if sha not in self._later:
            known = lib.git(self.source, "cat-file", "-e", f"{sha}^{{commit}}", check=False)
            if known.returncode != 0:
                self._later[sha] = False
            else:
                ancestor = lib.git(
                    self.source, "merge-base", "--is-ancestor", sha, self.base, check=False
                )
                self._later[sha] = ancestor.returncode != 0
        return self._later[sha]

    def outside(self, text: str) -> list[str]:
        paths = []
        for match in self._home_re.finditer(text):
            path = self.home + match.group(1).rstrip(".,:")
            if self.tree and (path == self.tree or path.startswith(self.tree + "/")):
                continue
            rest = path[len(self.home) + 1 :]
            if rest.split("/", 1)[0] in SAFE_HOME_DIRS:
                continue
            paths.append(path)
        return paths

    def git_level(self, sub: str, rest: str) -> tuple[int, str]:
        """The level of one `git <sub> <rest>` segment and its evidence text."""
        snippet = (sub + rest).strip()[:200]
        second = second_history(sub, rest)
        if second is not None:
            return 3, f"{snippet} [second history: {second}]"
        later = [s for s in SHA_RE.findall(rest) if self.is_later(s)]
        later += LATER_REF.findall(rest)
        if MAIN_REF.search(rest):
            later.append("main")
        if sub in BRINGS and later:
            return 3, snippet
        if sub == "show" and later and SHOW_TO_FILE.search(rest):
            return 3, snippet
        if sub in ("show", "diff", "cat-file", "format-patch") and later:
            return 2, snippet
        if sub == "log" and later and re.search(r"(-p\b|--patch|--stat|-- )", rest):
            return 2, snippet
        if sub == "log" and (later or "--all" in rest):
            return 1, snippet
        if sub == "branch" and re.search(r"(-a\b|--all|-r\b|--contains|--list)", rest):
            return 1, snippet
        if sub in ("rev-list", "reflog", "for-each-ref", "show-ref", "merge-base") and (
            later or "--all" in rest
        ):
            return 1, snippet
        return 0, snippet


def _words(rest: str) -> list[str]:
    """The segment's arguments without options and shell redirections."""
    try:
        tokens = shlex.split(rest)
    except ValueError:
        tokens = rest.split()
    words = []
    skip = False
    for token in tokens:
        if skip:
            skip = False
            continue
        if re.match(r"^\d*[<>]", token):
            # `>` or `2>` alone takes the next token as its file.
            skip = re.fullmatch(r"\d*[<>]+&?", token) is not None
            continue
        if token.startswith("-"):
            continue
        words.append(token)
    return words


def is_location(word: str) -> bool:
    """A path or a url, as opposed to a remote or ref name."""
    return (
        word.startswith(("/", "~", "./", "../", "$HOME", "${HOME}"))
        or word in (".", "..")
        or "://" in word
        or re.match(r"^[\w.-]+@[\w.-]+:", word) is not None
    )


def second_history(sub: str, rest: str) -> str | None:
    """The path or url a segment brings a second history from, or None."""
    words = _words(rest)
    if sub == "clone":
        return words[0] if words else "(clone)"
    if sub == "remote":
        if words and words[0] in ("add", "set-url"):
            for word in words[1:]:
                if is_location(word):
                    return word
            return None
        return None
    if sub in ("fetch", "pull"):
        for word in words:
            if is_location(word):
                return word
    return None


def tool_calls(path: Path):
    """Yield (tree, name, arguments) for every tool call in a radian or pi journal.

    A call is any object with a string `name` (or `tool`, `tool_name`) and
    `arguments`, `input` or `args`: radian's `ToolExecutionStarted`, an
    assistant message's tool call, and pi's `toolCall` all have that shape. The
    tree is pi's `session` record `cwd` or radian's `SessionCreated` workspace.
    """
    tree = None
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        for line in handle:
            try:
                record = json.loads(line)
            except ValueError:
                continue
            if not isinstance(record, dict):
                continue
            if tree is None:
                if record.get("type") == "session" and record.get("cwd"):
                    tree = record["cwd"]
                elif record.get("event") == "SessionCreated":
                    data = record.get("data")
                    if isinstance(data, dict):
                        tree = data.get("workspace")
            stack = [record]
            while stack:
                value = stack.pop()
                if isinstance(value, dict):
                    name = value.get("name") or value.get("tool") or value.get("tool_name")
                    args = None
                    if isinstance(name, str):
                        for key in ("arguments", "input", "args"):
                            if key in value:
                                args = value[key]
                    if args is not None:
                        if isinstance(args, str):
                            try:
                                args = json.loads(args)
                            except ValueError:
                                pass
                        yield tree, name, args
                        continue
                    stack.extend(value.values())
                elif isinstance(value, list):
                    stack.extend(value)


def strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for item in value.values():
            yield from strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from strings(item)


def scan(journals, source, base: str, own_shas=(), tree: str | None = None,
         home: str | None = None) -> dict:
    """The leak result over a run's journals; `level` is None with no journal."""
    journals = [Path(path) for path in journals]
    home = home if home is not None else os.path.expanduser("~")
    result = {
        "level": None,
        "calls": 0,
        "evidence": [],
        "outside": [],
        "journals": [str(path) for path in journals],
        "source": str(source) if source else None,
        "base": base,
        "tree": tree,
    }
    if not journals:
        return result
    scanner = None
    seen = set()
    level = 0
    outside: set[str] = set()
    for path in journals:
        for journal_tree, name, args in tool_calls(path):
            if scanner is None or (scanner.tree is None and journal_tree):
                tree = tree or journal_tree
                scanner = Scanner(source, base, own_shas, tree, home)
            # A journal can carry one call twice (the stream event and the
            # message's tool call); count it once.
            key = (name, json.dumps(args, sort_keys=True))
            if key in seen:
                continue
            seen.add(key)
            result["calls"] += 1
            for text in strings(args):
                outside.update(scanner.outside(text))
                for match in GIT_SEG.finditer(text):
                    found, snippet = scanner.git_level(match.group(1), match.group(2))
                    if found:
                        level = max(level, found)
                        result["evidence"].append({"level": found, "text": snippet})
    result["level"] = level
    result["tree"] = tree
    result["outside"] = sorted(outside)
    return result


def run_journals(run_dir: Path, run: dict) -> list[Path]:
    """The run's session journals: `session/**/*.jsonl`, else run.json's `journal`."""
    session = Path(run.get("session_dir") or run_dir / "session")
    found = sorted(session.rglob("*.jsonl")) if session.is_dir() else []
    if not found and run.get("journal") and Path(run["journal"]).is_file():
        found = [Path(run["journal"])]
    return found


def own_commits(run_dir: Path) -> set[str]:
    path = run_dir / "commits.txt"
    if not path.is_file():
        return set()
    return {line.split()[0] for line in path.read_text().splitlines() if line.split()}


def scan_run(run_dir, source=None, journals=None, home: str | None = None) -> dict:
    """The leak result of one run directory (it needs `run.json` for the base)."""
    run_dir = Path(run_dir)
    run = json.loads((run_dir / "run.json").read_text(encoding="utf-8"))
    if source is None:
        source = lib.source_repo()
    if journals is None:
        journals = run_journals(run_dir, run)
    return scan(journals, source, run["base"], own_commits(run_dir), run.get("tree"), home)


def write_leak(run_dir, result: dict) -> Path:
    path = Path(run_dir) / "leak.json"
    path.write_text(json.dumps(result, indent=1) + "\n", encoding="utf-8")
    return path


def describe(level) -> str:
    return "no journal" if level is None else f"L{level}"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("run_dir", help="<runs>/<arm>/<task>/<k>")
    parser.add_argument("--source", help="the repository later shas are looked up in")
    parser.add_argument("--journal", action="append",
                        help="a journal to read instead of the run's session (repeatable)")
    args = parser.parse_args(argv)
    run_dir = Path(args.run_dir)
    if not (run_dir / "run.json").is_file():
        print(f"leak.py: no run.json in {run_dir}", file=sys.stderr)
        return 2
    result = scan_run(run_dir, args.source, args.journal)
    write_leak(run_dir, result)
    strongest = max(result["evidence"], key=lambda item: item["level"], default=None)
    line = f"{describe(result['level'])}\tcalls={result['calls']}\toutside={len(result['outside'])}"
    if strongest:
        line += f"\t{strongest['text']}"
    print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
