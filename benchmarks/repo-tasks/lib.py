#!/usr/bin/env python3
"""Read the fixed repository task set and its arm files.

    python3 benchmarks/repo-tasks/lib.py task TK0226 brief
    python3 benchmarks/repo-tasks/lib.py task TK0226 crates
    python3 benchmarks/repo-tasks/lib.py arm full agent
    python3 benchmarks/repo-tasks/lib.py list-tasks
    python3 benchmarks/repo-tasks/lib.py list-arms

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
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
TASKS_DIR = Path(os.environ.get("REPO_TASKS_TASKS_DIR", HERE / "tasks"))
ARMS_DIR = Path(os.environ.get("REPO_TASKS_ARMS_DIR", HERE / "arms"))

TASK_KEYS = ("id", "slug", "base", "outcome", "crates")
ARM_KEYS = ("agent", "provider", "model", "extra")


class LookupError(Exception):
    """A task or arm name names no file; the message lists the valid names."""


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


def parse_review_text(text: str) -> dict:
    """The last closed fenced `json` block of a review, as arc reads it.

    `blocking` counts every item in the block, at every level: arc's review
    stage fails on any blocking item, whatever its `level`.
    """
    blocks = re.findall(r"```([^\n`]*)\n(.*?)```", text, re.S)
    verdict = None
    for info, body in reversed(blocks):
        if info.strip().lower() != "json":
            continue
        try:
            candidate = json.loads(body)
        except ValueError:
            continue
        if isinstance(candidate, dict) and "blocking" in candidate:
            verdict = candidate
        break
    items = (verdict or {}).get("blocking", [])
    items = items if isinstance(items, list) else []
    return {
        "parsed": verdict is not None,
        "blocking": len(items),
        "levels": [
            item.get("level") if isinstance(item, dict) else None for item in items
        ],
        "verdict": verdict,
    }


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
    except (LookupError, ValueError) as error:
        print(error, file=sys.stderr)
        return 2
    print(__doc__.strip(), file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(_main(sys.argv[1:]))
