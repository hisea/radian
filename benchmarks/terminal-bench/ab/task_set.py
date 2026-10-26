#!/usr/bin/env python3
"""Resolve an A/B task set to one Terminal-Bench task name per line.

    python3 task_set.py resolve <name-or-path> <tasks-dir>

A bare name (no ``/``) means ``<tasks-dir>/<name>.txt``; a value with a ``/`` is
a path to a task set file. A file holds one task name per line; ``#`` starts a
comment that runs to the end of the line, and blank lines are ignored. Every
task name must match ``^[a-z0-9][a-z0-9-]*$``, a set may not be empty, and a
name may not repeat.

Any refusal prints one line to standard error and exits 1, so a calling script
can stop before Harbor starts. Standard library only.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# A Terminal-Bench task name: lowercase letters and digits, hyphen-separated.
TASK_NAME = re.compile(r"^[a-z0-9][a-z0-9-]*$")


class TaskSetError(Exception):
    """A task set that cannot be used."""


def resolve(value: str, tasks_dir: Path) -> Path:
    """The file a `value` names: a bare name under `tasks_dir`, or a path."""
    if "/" in value:
        return Path(value)
    return tasks_dir / f"{value}.txt"


def parse(path: Path) -> list[str]:
    """The task names in `path`, or a `TaskSetError` naming what is wrong."""
    if not path.is_file():
        raise TaskSetError(f"no task set file {str(path)!r}")
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise TaskSetError(f"cannot read task set {str(path)!r}: {error}") from error
    names: list[str] = []
    seen: set[str] = set()
    for number, line in enumerate(text.splitlines(), 1):
        name = line.split("#", 1)[0].strip()
        if not name:
            continue
        if not TASK_NAME.fullmatch(name):
            raise TaskSetError(
                f"{path}:{number}: task name {name!r} does not match "
                f"{TASK_NAME.pattern}"
            )
        if name in seen:
            raise TaskSetError(f"{path}:{number}: duplicate task name {name!r}")
        seen.add(name)
        names.append(name)
    if not names:
        raise TaskSetError(f"task set {str(path)!r} is empty (no task names)")
    return names


def main(argv: list[str]) -> int:
    if len(argv) != 3 or argv[0] != "resolve":
        print(
            "usage: task_set.py resolve <name-or-path> <tasks-dir>",
            file=sys.stderr,
        )
        return 2
    value, tasks_dir = argv[1], Path(argv[2])
    try:
        path = resolve(value, tasks_dir)
        if not path.is_file():
            raise TaskSetError(
                f"no task set file {value!r}"
                if "/" in value
                else f"unknown task set {value!r} (no {path})"
            )
        names = parse(path)
    except TaskSetError as error:
        print(f"task_set.py: {error}", file=sys.stderr)
        return 1
    print("\n".join(names))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
