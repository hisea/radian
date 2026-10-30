#!/usr/bin/env python3
"""Build arc's first-review prompt from the repository's own source.

    python3 benchmarks/repo-tasks/prompt.py <task> <tree> <base> <diff-file>

The prompt text is not copied here: `crates/radian-arc/src/gate/review.rs` holds
the one `build_prompt` template and `report_format` block that arc runs, and
`crates/radian-arc/src/gate/verdict.rs` holds `REVIEW_LEVELS`. This module reads
those files (from the repository containing this script, or from
`REPO_TASKS_REVIEW_RS` / `--review-rs`) and fills the same placeholders arc
fills: the task id, the brief, the base and head shas, the commit list, the
diff, and the report format. A change to arc's prompt changes this one too; a
test writes a marked copy of `review.rs` and looks for the mark in the output.

The diff is the run's saved `change.diff` (committed and uncommitted), so a
review covers exactly what the report reads. `arc` appends a configured
`prompt_file` (`.radian/arc/review-prompt.md`) after the built-in text; that file
is repository-local and not part of this fixed task set, so it is not read here.

Stdlib only.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]

sys.path.insert(0, str(HERE))
import lib  # noqa: E402


def _parse_rust_string(text: str, start: int) -> tuple[str, int]:
    """The string literal that starts at `text[start] == '"'` and its end index."""
    out: list[str] = []
    index = start + 1
    while index < len(text):
        char = text[index]
        if char == '"':
            return "".join(out), index + 1
        if char != "\\":
            out.append(char)
            index += 1
            continue
        index += 1
        if index >= len(text):
            break
        escaped = text[index]
        if escaped == "\n":
            # A Rust line continuation: the newline and the leading whitespace of
            # the next line are not part of the string.
            index += 1
            while index < len(text) and text[index] in " \t\r":
                index += 1
            continue
        out.append(
            {
                "n": "\n",
                "r": "\r",
                "t": "\t",
                "0": "\0",
                "\\": "\\",
                '"': '"',
                "'": "'",
            }.get(escaped, escaped)
        )
        index += 1
    raise ValueError("unterminated Rust string literal")


def _body(source: str, function: str) -> str:
    """The text between the braces of `fn <function>` (strings are skipped)."""
    start = source.index(f"fn {function}(")
    opening = source.index("{", start)
    depth = 1
    index = opening + 1
    in_string = False
    while index < len(source):
        char = source[index]
        if in_string:
            if char == "\\":
                index += 2
                continue
            if char == '"':
                in_string = False
        elif char == '"':
            in_string = True
        elif char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return source[opening + 1 : index]
        index += 1
    raise ValueError(f"unterminated function {function}")


def _template(source: str, function: str) -> str:
    """The last string literal of `format!(` in `fn <function>`.

    The last one is the function's own template: `report_format` first builds
    one string per level with a small `format!`.
    """
    body = _body(source, function)
    call = body.rindex("format!(")
    quote = body.index('"', call)
    literal, _ = _parse_rust_string(body, quote)
    return literal


def _levels(source: str) -> str:
    """`REVIEW_LEVELS` rendered the way `report_format` renders it."""
    match = re.search(
        r"pub const REVIEW_LEVELS:\s*&\[&str\]\s*=\s*&\[(.*?)\];", source, re.S
    )
    if match is None:
        raise ValueError("REVIEW_LEVELS not found")
    names = re.findall(r'"([^"]*)"', match.group(1))
    if not names:
        raise ValueError("REVIEW_LEVELS is empty")
    return ", ".join(f"`{name}`" for name in names)


def _unescape_braces(template: str) -> str:
    """A Rust format string's `{{`/`}}` are one literal brace."""
    return template.replace("{{", "{").replace("}}", "}")


def report_format(review_rs: Path) -> str:
    source = review_rs.read_text(encoding="utf-8")
    template = _unescape_braces(_template(source, "report_format"))
    levels = _levels((review_rs.parent / "verdict.rs").read_text(encoding="utf-8"))
    return template.replace("{levels}", levels)


def build_prompt(
    review_rs: Path,
    task: str,
    brief: str,
    base: str,
    sha: str,
    commits: str,
    diff: str,
) -> str:
    source = review_rs.read_text(encoding="utf-8")
    template = _unescape_braces(_template(source, "build_prompt"))
    text = (
        template.replace("{task}", task)
        .replace("{brief}", brief)
        .replace("{base}", base)
        .replace("{sha}", sha)
        .replace("{commits}", commits)
        .replace("{diff}", diff)
    )
    return text.replace("{format}", report_format(review_rs))


def _git(tree: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(tree), *args], capture_output=True, text=True, check=True
    ).stdout.strip()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("task", help="task id or task file stem")
    parser.add_argument("tree", help="the run's worktree")
    parser.add_argument("base", help="the task's base sha")
    parser.add_argument("diff_file", help="the run's change.diff")
    parser.add_argument(
        "--review-rs",
        default=os.environ.get(
            "REPO_TASKS_REVIEW_RS",
            REPO / "crates/radian-arc/src/gate/review.rs",
        ),
        help="review.rs to read the prompt from (tests point it elsewhere)",
    )
    args = parser.parse_args(argv)
    try:
        task = lib.find_task(args.task)
    except (lib.LookupError, ValueError) as error:
        print(error, file=sys.stderr)
        return 2
    review_rs = Path(args.review_rs)
    if not review_rs.is_file():
        print(f"prompt.py: no review source at {review_rs}", file=sys.stderr)
        return 2
    tree = Path(args.tree)
    sha = _git(tree, "rev-parse", "HEAD")
    commits = _git(
        tree, "log", "--no-decorate", "--format=%h %s", f"{args.base}..{sha}"
    )
    diff = Path(args.diff_file).read_text(encoding="utf-8")
    try:
        prompt = build_prompt(
            review_rs,
            str(task["id"]),
            str(task["brief"]),
            args.base,
            sha,
            commits,
            diff,
        )
    except ValueError as error:
        print(f"prompt.py: {error}", file=sys.stderr)
        return 2
    sys.stdout.write(prompt)
    return 0


if __name__ == "__main__":
    sys.exit(main())
