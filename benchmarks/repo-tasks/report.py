#!/usr/bin/env python3
"""Report over the fixed repository task set's runs.

    python3 benchmarks/repo-tasks/report.py [--runs DIR] [--baseline ARM] [--json]
    python3 benchmarks/repo-tasks/report.py --self-test

Every metric comes from `benchmarks/journal_metrics.py`: one `build_report` call
over each run's session journal, then sums per arm and task. The columns and
their definitions are the ones in that module's `COLUMNS` table; this report
does not redefine them. A run that hit the wall-time cap is a `timeout` row,
never a dropped run.

The text output has:

- one table per task (arm x columns);
- one totals table over every task;
- one ratios table (`arm / baseline`) per task and for the totals, when
  `--baseline` names an arm.

`--json` prints the same data as one object.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
BENCHMARKS = HERE.parent
sys.path.insert(0, str(BENCHMARKS))
sys.path.insert(0, str(HERE))

import journal_metrics  # noqa: E402

DEFAULT_RUNS = HERE / "runs"

# The tool names the tables always show a byte column for, when the runs have
# them. Other tools add their own column.
TABLE_COLUMNS = (
    ("calls", "calls"),
    ("input_uncached", "uncached_in"),
    ("input_cache_read", "cache_read_in"),
    ("input_total", "total_in"),
    ("output", "output"),
    ("reasoning", "reasoning"),
    ("fee_usd", "fee_usd"),
    ("read_ranged_calls", "read_ranged"),
    ("read_unranged_calls", "read_unranged"),
    ("read_large_calls", "read_20kb"),
    ("compactions", "compactions"),
)


class ReportError(Exception):
    """The report cannot be made; the message is the one to print."""


def _int(value) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def _num(value) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def _add(left, right):
    if right is None:
        return left
    return (left or 0) + right


def _sum(values):
    total = None
    for value in values:
        total = _add(total, value)
    return total


def _json(path: Path) -> dict | None:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def _journal_metrics(run: dict) -> dict | None:
    """The run's columns, read by `journal_metrics` and nowhere else."""
    target = run.get("session_dir") or run.get("journal")
    if not target:
        session = Path(run["dir"]) / "session"
        files = sorted(session.glob("*.jsonl")) if session.is_dir() else []
        target = str(files[0]) if files else None
    if not target or not Path(target).exists():
        return None
    report = journal_metrics.build_report([target], recursive=False)
    if not report["journals"]:
        return None
    # A session directory may hold more than one journal (a resumed run); the
    # run's row is their total, exactly the module's TOTAL row.
    return report["totals"] if Path(target).is_dir() else report["journals"][0]


def _result(run: dict) -> str:
    """One run's outcome: timeout, failed, checks-fail, review-fail, pass, no-check."""
    if run.get("timed_out") or run.get("status") == "timeout":
        return "timeout"
    if run.get("status") == "failed":
        return "failed"
    checks = run.get("checks")
    if checks is not None and not checks.get("checks_ok", False):
        return "checks-fail"
    review = run.get("review")
    if review is not None and not review.get("review_ok", False):
        return "review-fail"
    if checks is not None and checks.get("checks_ok", False):
        return "pass"
    if run.get("status") == "failed":
        return "failed"
    return "no-check"


def collect(runs_dir: Path) -> list[dict]:
    """Every run under `<runs>/<arm>/<task>/<k>/run.json`."""
    runs = []
    for path in sorted(runs_dir.glob("*/*/*/run.json")):
        data = _json(path)
        if not isinstance(data, dict):
            continue
        out = path.parent
        data["dir"] = str(out)
        data["checks"] = _json(out / "checks.json")
        data["review"] = _json(out / "review.json")
        data["metrics"] = _journal_metrics(data)
        data["result"] = _result(data)
        runs.append(data)
    return runs


def _status_cell(runs: list[dict]) -> str:
    counts: dict[str, int] = {}
    for run in runs:
        counts[run["result"]] = counts.get(run["result"], 0) + 1
    return " ".join(f"{name}={count}" for name, count in sorted(counts.items()))


def _spread(runs: list[dict]) -> str:
    values = [
        run["metrics"]["input_total"]
        for run in runs
        if run["metrics"] and run["metrics"]["input_total"] is not None
    ]
    if not values:
        return "-"
    low, high = min(values), max(values)
    if low == high:
        return f"{low:,}"
    return f"{low:,}-{high:,}"


def aggregate(runs: list[dict]) -> dict:
    """One arm's totals over its runs of one task (or over every task)."""
    rows = [run["metrics"] for run in runs if run["metrics"]]
    tool_names: set[str] = set()
    for row in rows:
        tool_names.update(row.get("tool_bytes", {}))
    totals: dict = {
        "runs": len(runs),
        "result": _status_cell(runs),
        "spread": _spread(runs),
        "tool_bytes": {},
        "outcomes": {},
    }
    for key, _ in TABLE_COLUMNS:
        totals[key] = _sum(row.get(key) for row in rows)
    totals["fee_usd"] = _sum(row.get("fee_usd") for row in rows)
    totals["wall_seconds"] = _sum(row.get("wall_seconds") for row in rows)
    for name in sorted(tool_names):
        totals["tool_bytes"][name] = _sum(
            row.get("tool_bytes", {}).get(name) for row in rows
        )
    for row in rows:
        for name, count in row.get("outcomes", {}).items():
            totals["outcomes"][name] = totals["outcomes"].get(name, 0) + count
    totals["result_bytes"] = _sum(totals["tool_bytes"].values())
    return totals


def _count(value) -> str:
    if value is None:
        return "-"
    if isinstance(value, float) and not value.is_integer():
        return f"{value:,.1f}"
    return f"{int(value):,d}"


def _money(value) -> str:
    return "-" if value is None else f"{value:.6f}"


def _ratio(value, baseline) -> str:
    if value is None or baseline in (None, 0):
        return "-"
    return f"{value / baseline:.3f}"


def _table(headers: list[str], rows: list[list[str]]) -> str:
    widths = [
        max([len(header)] + [len(row[index]) for row in rows])
        for index, header in enumerate(headers)
    ]

    def line(cells: list[str]) -> str:
        return "  ".join(cell.ljust(width) for cell, width in zip(cells, widths)).rstrip()

    return "\n".join(
        [
            line(headers),
            line(["-" * width for width in widths]),
            *(line(row) for row in rows),
        ]
    )


def _tool_names(tasks: dict) -> list[str]:
    names: set[str] = set()
    for arms in tasks.values():
        for totals in arms.values():
            names.update(totals["tool_bytes"])
    return sorted(names)


def _columns(tasks: dict) -> list[str]:
    tool_names = _tool_names(tasks)
    return [
        *[label for _, label in TABLE_COLUMNS],
        *(f"bytes.{name}" for name in tool_names),
        "result_bytes",
        "wall_s",
    ]


def _arm_row(totals: dict, columns: list[str]) -> list[str]:
    keys = {label: key for key, label in TABLE_COLUMNS}
    values = []
    for label in columns:
        if label.startswith("bytes."):
            values.append(_count(totals["tool_bytes"].get(label[len("bytes.") :])))
        elif label == "result_bytes":
            values.append(_count(totals["result_bytes"]))
        elif label == "wall_s":
            values.append(_count(totals["wall_seconds"]))
        elif label == "fee_usd":
            values.append(_money(totals.get("fee_usd")))
        else:
            values.append(_count(totals.get(keys.get(label, label))))
    return values


def build(runs_dir: Path, baseline: str | None = None) -> dict:
    runs = collect(runs_dir)
    if not runs:
        raise ReportError(f"no run under {runs_dir} (looked for <arm>/<task>/<k>/run.json)")
    arms = sorted({run["arm"] for run in runs})
    tasks = sorted({run["task"] for run in runs})
    if baseline is not None and baseline not in {run["arm"] for run in runs}:
        raise ReportError(
            f"unknown baseline arm `{baseline}`; arms with runs: {', '.join(arms)}"
        )
    by_task: dict[str, dict[str, list[dict]]] = {task: {} for task in tasks}
    for run in runs:
        by_task[run["task"]].setdefault(run["arm"], []).append(run)
    tables = {
        task: {
            arm: aggregate(sorted(task_runs, key=lambda run: run.get("k", 0)))
            for arm, task_runs in by_task[task].items()
        }
        for task in tasks
    }
    totals = {
        arm: aggregate([run for run in runs if run["arm"] == arm]) for arm in arms
    }
    ratios: dict[str, dict[str, dict[str, str]]] = {}
    if baseline is not None:
        for task in [*tasks, "TOTAL"]:
            cells = tables.get(task) or totals
            baseline_totals = cells.get(baseline)
            row: dict[str, dict[str, str]] = {}
            for arm in arms:
                if arm == baseline:
                    continue
                arm_totals = cells.get(arm)
                if arm_totals is None or baseline_totals is None:
                    row[arm] = {key: "-" for key in (
                        "total_in", "calls", "output", "fee_usd", "wall_seconds",
                        "result_bytes",
                    )}
                    continue
                row[arm] = {
                    "total_in": _ratio(arm_totals["input_total"], baseline_totals["input_total"]),
                    "calls": _ratio(arm_totals["calls"], baseline_totals["calls"]),
                    "output": _ratio(arm_totals["output"], baseline_totals["output"]),
                    "fee_usd": _ratio(arm_totals["fee_usd"], baseline_totals["fee_usd"]),
                    "wall_seconds": _ratio(arm_totals["wall_seconds"], baseline_totals["wall_seconds"]),
                    "result_bytes": _ratio(arm_totals["result_bytes"], baseline_totals["result_bytes"]),
                }
            ratios[task] = row
    return {
        "runs_dir": str(runs_dir),
        "agents": arms,
        "tasks": tasks,
        "runs": [
            {
                key: run.get(key)
                for key in (
                    "arm", "task", "k", "base", "agent", "provider", "model",
                    "status", "timed_out", "exit_code", "result",
                )
            }
            | {"journal": run.get("journal"), "metrics": run.get("metrics")}
            for run in sorted(runs, key=lambda run: (run["task"], run["arm"], run.get("k", 0)))
        ],
        "tasks_totals": tables,
        "totals": totals,
        "baseline": baseline,
        "ratios": ratios,
    }


def format_report(report: dict) -> str:
    tasks = report["tasks_totals"]
    columns = _columns(tasks)
    headers = ["arm", "runs", "result", *columns, "in_spread"]
    blocks = []
    for task in report["tasks"]:
        rows = []
        for arm in sorted(tasks[task]):
            totals = tasks[task][arm]
            rows.append([arm, str(totals["runs"]), totals["result"], *_arm_row(totals, columns), totals["spread"]])
        blocks.append(f"TASK {task}\n" + _table(headers, rows))
    totals_rows = []
    for arm in report["agents"]:
        totals = report["totals"][arm]
        totals_rows.append([arm, str(totals["runs"]), totals["result"], *_arm_row(totals, columns), totals["spread"]])
    blocks.append("TOTALS\n" + _table(headers, totals_rows))
    if report["baseline"] is not None:
        ratio_headers = ["task", "arm", "total_in", "calls", "output", "fee_usd", "wall_s", "result_bytes"]
        rows = []
        for task in [*report["tasks"], "TOTAL"]:
            for arm, values in report["ratios"].get(task, {}).items():
                rows.append([
                    task, arm, values["total_in"], values["calls"], values["output"],
                    values["fee_usd"], values["wall_seconds"], values["result_bytes"],
                ])
        blocks.append(
            f"RATIOS (arm / {report['baseline']})\n" + _table(ratio_headers, rows)
        )
    else:
        blocks.append("RATIOS\nno --baseline given")
    return "\n\n".join(blocks)


def _main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--runs", default=os.environ.get("REPO_TASKS_RUNS", DEFAULT_RUNS),
                        help="the runs root")
    parser.add_argument("--baseline", metavar="ARM",
                        help="the arm every ratio divides by")
    parser.add_argument("--json", action="store_true", help="print one JSON object")
    parser.add_argument("--self-test", action="store_true",
                        help="run tests/test_repo_tasks.py")
    args = parser.parse_args(argv)
    if args.self_test:
        return subprocess.call([sys.executable, str(HERE / "tests" / "test_repo_tasks.py")])
    try:
        report = build(Path(args.runs), args.baseline)
    except ReportError as error:
        print(f"report.py: {error}", file=sys.stderr)
        return 2
    print(json.dumps(report, indent=2) if args.json else format_report(report))
    return 0


if __name__ == "__main__":
    sys.exit(_main())
