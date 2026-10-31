#!/usr/bin/env python3
"""Read the fixed repository task set and its arm files.

    python3 benchmarks/repo-tasks/lib.py task TK0226 brief
    python3 benchmarks/repo-tasks/lib.py task TK0226 crates
    python3 benchmarks/repo-tasks/lib.py arm full agent
    python3 benchmarks/repo-tasks/lib.py list-tasks
    python3 benchmarks/repo-tasks/lib.py list-arms
    python3 benchmarks/repo-tasks/lib.py source
    python3 benchmarks/repo-tasks/lib.py runs-root
    python3 benchmarks/repo-tasks/lib.py check-runs-root <dir>
    python3 benchmarks/repo-tasks/lib.py make-tree <source> <base> <tree>

A task file is one Markdown file under ``tasks/``: a frontmatter block of
``key: value`` lines between two ``---`` lines, then ``## Brief`` and the worker
brief exactly as the original worker received it (no ``## Rework`` section).
An arm file is one file under ``arms/`` with the same ``key = value`` shape (a
``#`` starts a comment). No file holds a credential; the runner reads the
agent's own stored credentials.

``REPO_TASKS_TASKS_DIR`` and ``REPO_TASKS_ARMS_DIR`` point the reader at another
directory; the tests use them to run against a temporary task set.

Stdlib only.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
TASKS_DIR = Path(os.environ.get("REPO_TASKS_TASKS_DIR", HERE / "tasks"))
ARMS_DIR = Path(os.environ.get("REPO_TASKS_ARMS_DIR", HERE / "arms"))
# `crates/radian-arc/src/gate/verdict.rs` holds `REVIEW_LEVELS`, the one closed
# list of levels arc's parser accepts.
VERDICT_RS = REPO / "crates/radian-arc/src/gate/verdict.rs"

TASK_KEYS = ("id", "slug", "base", "outcome", "crates")
ARM_KEYS = ("agent", "provider", "model", "extra")


# Variables that make git look at another repository than the `-C` one. A run
# started from a git hook would carry them; every git call here drops them.
_GIT_REPO_ENV = (
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
)

# Lets `git fetch` ask a local repository for a bare sha that no ref points at.
# It is passed on the fetch's command line, so the source's config never changes.
UPLOAD_PACK = (
    "git -c uploadpack.allowReachableSHA1InWant=true "
    "-c uploadpack.allowAnySHA1InWant=true upload-pack"
)


class SetupError(Exception):
    """A run root or a run tree cannot be used; the message is the one to print."""


class LookupError(Exception):
    """A task or arm name names no file; the message lists the valid names."""


def review_levels(verdict_rs: Path | None = None) -> tuple[str, ...]:
    """`REVIEW_LEVELS` from the repository source, in its own order."""
    path = VERDICT_RS if verdict_rs is None else Path(verdict_rs)
    match = re.search(
        r"pub const REVIEW_LEVELS:\s*&\[&str\]\s*=\s*&\[(.*?)\];",
        path.read_text(encoding="utf-8"),
        re.S,
    )
    if match is None:
        raise ValueError(f"{path}: REVIEW_LEVELS not found")
    names = tuple(re.findall(r'"([^"]*)"', match.group(1)))
    if not names:
        raise ValueError(f"{path}: REVIEW_LEVELS is empty")
    return names


def _frontmatter(path: Path) -> tuple[dict[str, str], str]:
    """The ``key: value`` map and the body after it (task files)."""
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines(keepends=True)
    if not lines or lines[0].strip() != "---":
        raise ValueError(f"{path}: no frontmatter")
    fields: dict[str, str] = {}
    for index, line in enumerate(lines[1:], start=1):
        if line.strip() == "---":
            return fields, "".join(lines[index + 1 :])
        key, sep, value = line.partition(":")
        if not sep:
            raise ValueError(f"{path}:{index + 1}: expected `key: value`")
        fields[key.strip()] = value.strip()
    raise ValueError(f"{path}: unterminated frontmatter")


def parse_task(path: Path) -> dict[str, object]:
    """One task file as its fields plus ``brief`` (verbatim worker text)."""
    fields, body = _frontmatter(path)
    missing = [key for key in TASK_KEYS if key not in fields]
    if missing:
        raise ValueError(f"{path}: missing {', '.join(missing)}")
    marker = "## Brief"
    if not body.startswith(marker):
        raise ValueError(f"{path}: the body does not start with `{marker}`")
    brief = body[len(marker) :]
    if brief.startswith("\r\n"):
        brief = brief[2:]
    elif brief.startswith("\n"):
        brief = brief[1:]
    if brief.lstrip("\r\n") != brief:
        brief = brief.lstrip("\r\n")
    task = dict(fields)
    task["brief"] = brief
    task["file"] = str(path)
    task["crates"] = [name.strip() for name in fields["crates"].split(",") if name.strip()]
    return task


def _last_json_block(text: str) -> str | None:
    """The body of the last closed fence whose info string is `json` or `jsonc`.

    Line for line, this is `crates/radian-arc/src/gate/review.rs`'s
    `last_json_block`: an unclosed fence at the end is not a block.
    """
    blocks: list[tuple[str, str]] = []
    current: tuple[str, list[str]] | None = None
    for line in text.splitlines():
        trimmed = line.strip()
        if current is not None:
            if trimmed == "```":
                info, body = current
                blocks.append((info, "".join(part + "\n" for part in body)))
                current = None
            else:
                current[1].append(line)
        elif trimmed.startswith("```"):
            current = (trimmed[3:].strip(), [])
    for info, body in reversed(blocks):
        if info.lower() in ("json", "jsonc"):
            return body
    return None


def _review_item(value, levels: tuple[str, ...]) -> dict | None:
    """One `ReviewItem`: a plain string, or arc's object form, or None."""
    if isinstance(value, str):
        return {"level": None, "file": None, "line": None, "text": value}
    if not isinstance(value, dict):
        return None
    text = value.get("text")
    if not isinstance(text, str):
        return None
    level = value.get("level")
    if level is not None and (not isinstance(level, str) or level not in levels):
        return None
    file = value.get("file")
    if file is not None and not isinstance(file, str):
        return None
    line = value.get("line")
    if line is not None and (
        isinstance(line, bool)
        or not isinstance(line, int)
        or not 0 <= line <= 0xFFFFFFFF
    ):
        return None
    pre_existing = value.get("pre_existing")
    if pre_existing is not None and not isinstance(pre_existing, bool):
        return None
    return {"level": level, "file": file, "line": line, "text": text}


def _review_items(value, levels: tuple[str, ...]) -> list[dict] | None:
    if not isinstance(value, list):
        return None
    items = []
    for entry in value:
        item = _review_item(entry, levels)
        if item is None:
            return None
        items.append(item)
    return items


def _unparsed() -> dict:
    return {
        "parsed": False,
        "blocking": 0,
        "levels": [],
        "summary": None,
        "verified": [],
        "suggestions": 0,
        "verdict": None,
    }


def parse_review_text(text: str, levels: tuple[str, ...] | None = None) -> dict:
    """The last closed `json` block of a review, read exactly as arc reads it.

    `crates/radian-arc/src/gate/review.rs`'s `parse_report` accepts the block
    only when `summary` is a string, `blocking` and `suggestions` are lists of
    items and `verified` is a list of strings; an item is a string or an object
    with a string `text` and, when present, a known `level`. A missing or
    malformed field is no report at all, so a reviewer that prints only
    `{"blocking": []}` does not pass.

    `blocking` counts every item in the block, at every level: arc's review
    stage fails on any blocking item, whatever its `level`.
    """
    block = _last_json_block(text)
    if block is None:
        return _unparsed()
    try:
        value = json.loads(block)
    except ValueError:
        return _unparsed()
    if not isinstance(value, dict):
        return _unparsed()
    if levels is None:
        try:
            levels = review_levels()
        except (OSError, ValueError):
            return _unparsed()
    summary = value.get("summary")
    blocking = _review_items(value.get("blocking"), levels)
    suggestions = _review_items(value.get("suggestions"), levels)
    verified = value.get("verified")
    if not isinstance(summary, str) or blocking is None or suggestions is None:
        return _unparsed()
    if not isinstance(verified, list) or not all(
        isinstance(entry, str) for entry in verified
    ):
        return _unparsed()
    return {
        "parsed": True,
        "blocking": len(blocking),
        "levels": [item["level"] for item in blocking],
        "summary": summary,
        "verified": verified,
        "suggestions": len(suggestions),
        "verdict": value,
    }


def final_assistant_text(session_dir) -> str | None:
    """The last assistant message of the session journals under a directory.

    `radian exec --output jsonl` writes its stdout as JSON lines, so a radian
    answer only appears as plain text in the session journal. The journals are
    read in name order (a later file is a resumed run) and the last committed
    `MessageAppended` assistant message wins; a torn final line is not a
    committed record (ADR 0006) and is dropped.
    """
    directory = Path(session_dir)
    if not directory.is_dir():
        return None
    answer = None
    for path in sorted(directory.glob("*.jsonl")):
        try:
            handle = path.open("r", encoding="utf-8", errors="replace")
        except OSError:
            continue
        with handle:
            for line in handle:
                if not line.endswith("\n"):
                    break
                try:
                    record = json.loads(line)
                except ValueError:
                    continue
                if not isinstance(record, dict) or record.get("event") != "MessageAppended":
                    continue
                data = record.get("data")
                message = data.get("message") if isinstance(data, dict) else None
                if not isinstance(message, dict):
                    continue
                assistant = message.get("assistant")
                if not isinstance(assistant, dict):
                    continue
                parts = []
                for part in assistant.get("content") or []:
                    text = part.get("text") if isinstance(part, dict) else None
                    if isinstance(text, dict) and isinstance(text.get("text"), str):
                        parts.append(text["text"])
                if parts:
                    answer = "".join(parts)
    return answer


def parse_arm(path: Path) -> dict[str, str]:
    """One arm file as its fields; ``extra`` is the raw flag string."""
    fields: dict[str, str] = {}
    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        key, sep, value = line.partition("=")
        if not sep:
            raise ValueError(f"{path}:{number}: expected `key = value`")
        fields[key.strip()] = value.strip()
    missing = [key for key in ARM_KEYS if key not in fields]
    if missing:
        raise ValueError(f"{path}: missing {', '.join(missing)}")
    fields["file"] = str(path)
    return fields


def _files(directory: Path) -> list[Path]:
    return sorted(
        path
        for path in directory.glob("*.md")
        if not path.name.startswith(".")
    )


def task_names() -> list[str]:
    return [path.stem for path in _files(TASKS_DIR)]


def arm_names() -> list[str]:
    return [path.stem for path in _files(ARMS_DIR)]


def _find(name: str, directory: Path, names: list[str], kind: str) -> Path:
    for path in _files(directory):
        if path.stem == name:
            return path
    # `TK0226` is enough for `TK0226-cli-manual-exit-4-truncation-rule`.
    matches = [path for path in _files(directory) if path.stem.startswith(name + "-")]
    if len(matches) == 1:
        return matches[0]
    raise LookupError(
        f"unknown {kind} `{name}`; valid {kind}s: {', '.join(names) or '(none)'}"
    )


def find_task(name: str) -> dict[str, object]:
    return parse_task(_find(name, TASKS_DIR, task_names(), "task"))


def find_arm(name: str) -> dict[str, str]:
    return parse_arm(_find(name, ARMS_DIR, arm_names(), "arm"))


def git_env() -> dict[str, str]:
    env = dict(os.environ)
    for name in _GIT_REPO_ENV:
        env.pop(name, None)
    return env


def git(directory, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", "-C", str(directory), *args],
        capture_output=True,
        text=True,
        env=git_env(),
        check=check,
    )


def source_repo() -> Path | None:
    """The repository the run trees are fetched from.

    `REPO_TASKS_SOURCE` when set, else the repository holding this file. A
    relative path is taken from the current directory.
    """
    value = os.environ.get("REPO_TASKS_SOURCE")
    if value:
        return Path(value).absolute()
    result = git(HERE, "rev-parse", "--show-toplevel", check=False)
    if result.returncode != 0 or not result.stdout.strip():
        return None
    return Path(result.stdout.strip())


def runs_root() -> Path:
    """`REPO_TASKS_RUNS`, else `${XDG_CACHE_HOME:-$HOME/.cache}/radian-repo-tasks/runs`."""
    value = os.environ.get("REPO_TASKS_RUNS")
    if value:
        return Path(value).absolute()
    cache = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    return Path(cache).absolute() / "radian-repo-tasks" / "runs"


def checkout_holding(path) -> str | None:
    """The git checkout (or git directory) that holds `path`, or None.

    `path` need not exist yet: its nearest existing ancestor is asked.
    """
    directory = Path(path).absolute()
    while not directory.exists():
        if directory.parent == directory:
            return None
        directory = directory.parent
    if not directory.is_dir():
        directory = directory.parent
    top = git(directory, "rev-parse", "--show-toplevel", check=False)
    if top.returncode == 0 and top.stdout.strip():
        return top.stdout.strip()
    # Inside a `.git` directory there is no work tree, but it is a repository.
    git_dir = git(directory, "rev-parse", "--absolute-git-dir", check=False)
    if git_dir.returncode == 0 and git_dir.stdout.strip():
        return git_dir.stdout.strip()
    return None


def check_runs_root(path) -> None:
    """Refuse a run root inside any git checkout.

    A tree under a checkout lets an agent that leaves its tree reach that
    checkout's history and branches (one run did: it ran `git checkout work/...`
    in the harness worktree that held the default run root).
    """
    holder = checkout_holding(path)
    if holder is not None:
        raise SetupError(
            f"the run root {path} is inside the git checkout {holder}; "
            "set REPO_TASKS_RUNS to a directory outside every repository"
        )


def tree_commits(tree) -> int:
    """How many commits the tree's HEAD reaches (`git rev-list --count HEAD`)."""
    return int(git(tree, "rev-list", "--count", "HEAD").stdout.strip())


def check_tree(tree) -> int:
    """The tree's commit count; anything but 1 means it holds history past its base."""
    count = tree_commits(tree)
    if count != 1:
        raise SetupError(
            f"the run tree {tree} reaches {count} commits, not 1; "
            "it must hold its base and nothing else"
        )
    return count


def make_tree(source, base: str, tree) -> int:
    """Make a run tree holding only `base`, fetched from `source`, and no ref.

    `git init`, a depth-1 fetch of the one sha, a detached checkout. There is no
    clone and no remote, so the tree has no `origin/*`, no branch and no later
    commit to find. `FETCH_HEAD` names the source path, so it is removed. The
    source is only read: the upload-pack options are on the command line.
    Returns the tree's commit count, which is always 1 (else SetupError).
    """
    tree = Path(tree)
    if tree.exists():
        raise SetupError(f"the run tree exists: {tree} (remove it first)")
    tree.parent.mkdir(parents=True, exist_ok=True)
    steps = (
        ("init", tree.parent, ("-c", "init.defaultBranch=base", "init", "-q", str(tree))),
        (
            "fetch",
            tree,
            (
                "fetch", "--quiet", "--depth", "1", "--no-tags",
                "--upload-pack", UPLOAD_PACK, str(source), base,
            ),
        ),
        ("checkout", tree, ("checkout", "--quiet", "--detach", "FETCH_HEAD")),
    )
    for name, directory, args in steps:
        result = git(directory, *args, check=False)
        if result.returncode != 0:
            raise SetupError(
                f"cannot make the run tree {tree} at {base} from {source}: "
                f"git {name}: "
                f"{result.stderr.strip()}"
            )
    fetch_head = tree / ".git" / "FETCH_HEAD"
    if fetch_head.exists():
        fetch_head.unlink()
    return check_tree(tree)


def _main(argv: list[str]) -> int:
    if not argv:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    command = argv[0]
    try:
        if command == "list-tasks":
            print("\n".join(task_names()))
            return 0
        if command == "list-arms":
            print("\n".join(arm_names()))
            return 0
        if command == "source" and len(argv) == 1:
            source = source_repo()
            if source is None:
                print("no source repository; set REPO_TASKS_SOURCE", file=sys.stderr)
                return 2
            print(source)
            return 0
        if command == "runs-root" and len(argv) == 1:
            print(runs_root())
            return 0
        if command == "check-runs-root" and len(argv) == 2:
            check_runs_root(argv[1])
            return 0
        if command == "make-tree" and len(argv) == 4:
            print(make_tree(argv[1], argv[2], argv[3]))
            return 0
        if command == "brief" and len(argv) == 2:
            sys.stdout.write(str(find_task(argv[1])["brief"]))
            return 0
        if command == "task" and len(argv) == 3:
            value = find_task(argv[1]).get(argv[2])
            if value is None:
                print(f"no task field `{argv[2]}`", file=sys.stderr)
                return 2
            if isinstance(value, list):
                value = ", ".join(value)
            print(value)
            return 0
        if command == "arm" and len(argv) == 3:
            value = find_arm(argv[1]).get(argv[2])
            if value is None:
                print(f"no arm field `{argv[2]}`", file=sys.stderr)
                return 2
            print(value)
            return 0
    except (LookupError, ValueError, SetupError) as error:
        print(error, file=sys.stderr)
        return 2
    print(__doc__.strip(), file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(_main(sys.argv[1:]))
