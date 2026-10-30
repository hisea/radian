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
REPO = HERE.parents[1]
TASKS_DIR = Path(os.environ.get("REPO_TASKS_TASKS_DIR", HERE / "tasks"))
ARMS_DIR = Path(os.environ.get("REPO_TASKS_ARMS_DIR", HERE / "arms"))
# `crates/radian-arc/src/gate/verdict.rs` holds `REVIEW_LEVELS`, the one closed
# list of levels arc's parser accepts.
VERDICT_RS = REPO / "crates/radian-arc/src/gate/verdict.rs"

TASK_KEYS = ("id", "slug", "base", "outcome", "crates")
ARM_KEYS = ("agent", "provider", "model", "extra")


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
