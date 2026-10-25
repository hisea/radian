#!/usr/bin/env python3
"""Per-task and per-agent metrics from finished Harbor job directories.

    python3 benchmarks/terminal-bench/metrics.py JOBS [JOBS ...] [--json]

`JOBS` is a jobs directory (``<jobs>/<agent>/<timestamp>/<task>__<id>/``) or any
directory above its trials. Every trial is read from its ``result.json``. A
radian trial's ``agent/radian-sessions/*.jsonl`` journal adds generation/tool
minutes and the MaxTokens outcome; a pi/opencode trial's
``agent/trajectory.json`` (ATIF) adds the per-call token metrics they report.

A trial whose exception is ``CancelledError`` is excluded from every total and
counted separately. The common-task section keeps only tasks for which every
agent has at least one finished (not timed-out, not excluded) trial.

Stdlib only. The tests in ``tests/test_metrics.py`` run on
``tests/fixtures/jobs``; see ``README.md`` for the column meanings.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Iterable, Iterator
from datetime import datetime
from pathlib import Path
from typing import Any

# "More than 16k output tokens" in the report. A decimal 16k keeps the column
# easy to check by hand; no provider reports a meaningful 384-token boundary.
OVER_16K_TOKENS = 16_000

# Reasoning lives in a provider-shaped usage sub-object. Radian keeps the
# provider's own object in `details`; ATIF producers put it in `metrics.extra`.
_REASONING_KEYS = ("reasoning_tokens", "reasoning_output_tokens", "thinking_tokens")


def _dict(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


def _int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float) and value.is_integer():
        return int(value)
    return None


def _float(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def _time(value: Any) -> datetime | None:
    if not isinstance(value, str) or not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def _minutes(started: Any, finished: Any) -> float | None:
    begin, end = _time(started), _time(finished)
    if begin is None or end is None:
        return None
    try:
        return max(0.0, (end - begin).total_seconds() / 60.0)
    except TypeError:  # one timestamp naive, the other aware
        return None


def _seconds(begin: datetime | None, end: datetime | None) -> float | None:
    """Elapsed seconds between two timestamps, or None when either is missing."""
    if begin is None or end is None:
        return None
    try:
        return max(0.0, (end - begin).total_seconds())
    except TypeError:  # one timestamp naive, the other aware
        return None


def _reasoning(details: Any) -> int | None:
    """The reasoning token count inside a provider usage sub-object, or None."""
    if not isinstance(details, dict):
        return None
    for key in _REASONING_KEYS:
        count = _int(details.get(key))
        if count is not None:
            return count
    for value in details.values():
        if isinstance(value, dict):
            count = _reasoning(value)
            if count is not None:
                return count
    return None


def _add(total: float | int | None, value: float | int | None) -> float | int | None:
    """Add a reported value; a missing value is "not reported", never zero."""
    if value is None:
        return total
    return (total or 0) + value


def _last_result(stdout_path: Path) -> dict[str, Any]:
    """The closing `Result` record of `radian exec --output jsonl`, or {}."""
    try:
        lines = [
            line for line in stdout_path.read_text(encoding="utf-8").splitlines() if line.strip()
        ]
    except OSError:
        return {}
    if not lines:
        return {}
    try:
        record = json.loads(lines[-1])
    except ValueError:
        return {}
    return record if isinstance(record, dict) and record.get("type") == "Result" else {}


def _journal_metrics(sessions_dir: Path) -> dict[str, Any]:
    """Calls, tokens and timing from one trial's radian session journals.

    Generation time runs from the request boundary (a tool settling, a user
    message, a turn start, or the previous response) to the assistant message
    that ends it. Tool time is the sum of each matched
    `ToolExecutionStarted` -> `ToolExecutionSettled` pair; parallel calls are
    summed, so tool minutes can exceed the trial's wall time. A torn tail (no
    final LF) is not a committed record and is dropped, like the Rust reader.
    """
    files = sorted(sessions_dir.glob("*.jsonl"))
    model_calls = 0
    output_tokens = reasoning_tokens = fee_micros = None
    over_16k = 0
    generation = tool = 0.0
    max_tokens = False
    for path in files:
        try:
            text = path.read_text(encoding="utf-8")
        except OSError:
            continue
        if text and not text.endswith("\n"):
            text = text[: text.rfind("\n") + 1] if "\n" in text else ""
        started: dict[str, datetime] = {}
        anchor: datetime | None = None
        for line in text.splitlines():
            if not line.strip():
                continue
            try:
                record = json.loads(line)
            except ValueError:
                continue
            if not isinstance(record, dict):
                continue
            event = record.get("event")
            data = _dict(record.get("data"))
            ts = _time(record.get("ts"))
            if event == "ToolExecutionStarted":
                call_id = data.get("call_id")
                if isinstance(call_id, str) and ts is not None:
                    started[call_id] = ts
            elif event == "ToolExecutionSettled":
                call_id = data.get("call_id")
                begin = started.pop(call_id, None) if isinstance(call_id, str) else None
                elapsed = _seconds(begin, ts)
                if elapsed is not None:
                    tool += elapsed
                if ts is not None:
                    anchor = ts
            elif event == "MessageAppended":
                message = _dict(data.get("message"))
                if "assistant" in message:
                    model_calls += 1
                    elapsed = _seconds(anchor, ts)
                    if elapsed is not None:
                        generation += elapsed
                    if ts is not None:
                        anchor = ts
                elif "user" in message and ts is not None:
                    anchor = ts
            elif event == "TurnStarted" and ts is not None:
                anchor = ts
            elif event in ("ModelUsageRecorded", "CompactionUsageRecorded"):
                usage = _dict(data.get("usage"))
                count = _int(usage.get("output"))
                if count is not None:
                    output_tokens = _add(output_tokens, count)
                    if count > OVER_16K_TOKENS:
                        over_16k += 1
                count = _reasoning(usage.get("details"))
                if count is not None:
                    reasoning_tokens = _add(reasoning_tokens, count)
                count = _int(_dict(data.get("fee")).get("micros"))
                if count is not None:
                    fee_micros = _add(fee_micros, count)
                if event == "CompactionUsageRecorded":
                    # A summarization call is a real model call and real money.
                    model_calls += 1
                    elapsed = _seconds(anchor, ts)
                    if elapsed is not None:
                        generation += elapsed
                    if ts is not None:
                        anchor = ts
            elif event == "TurnFinished":
                if data.get("outcome") == "incomplete" and data.get("reason") == "max_tokens":
                    max_tokens = True
    return {
        "model_calls": model_calls,
        "output_tokens": output_tokens,
        "reasoning_tokens": reasoning_tokens,
        "over_16k_calls": over_16k,
        "generation_minutes": generation / 60.0,
        "tool_minutes": tool / 60.0,
        "cost_usd": fee_micros / 1_000_000 if fee_micros is not None else None,
        "max_tokens": max_tokens,
    }


def _iter_steps(trajectory: dict[str, Any]) -> Iterator[dict[str, Any]]:
    for step in trajectory.get("steps") or []:
        if isinstance(step, dict):
            yield step
    for subagent in trajectory.get("subagent_trajectories") or []:
        if isinstance(subagent, dict):
            yield from _iter_steps(subagent)


def _trajectory_metrics(trajectory: dict[str, Any]) -> dict[str, Any]:
    """Per-call metrics an ATIF producer reported, summed over its steps."""
    model_calls = 0
    output_tokens = reasoning_tokens = cost_usd = None
    over_16k = 0
    seen = False
    for step in _iter_steps(trajectory):
        metrics = _dict(step.get("metrics"))
        if not metrics:
            continue
        seen = True
        count = _int(step.get("llm_call_count"))
        model_calls += count if count and count > 0 else 1
        count = _int(metrics.get("completion_tokens"))
        if count is not None:
            output_tokens = _add(output_tokens, count)
            if count > OVER_16K_TOKENS:
                over_16k += 1
        count = _reasoning(_dict(metrics.get("extra")))
        if count is None:
            count = _reasoning(metrics)
        if count is not None:
            reasoning_tokens = _add(reasoning_tokens, count)
        cost_usd = _add(cost_usd, _float(metrics.get("cost_usd")))
    if not seen:
        # A producer that wrote only aggregate metrics still reports tokens.
        final = _dict(trajectory.get("final_metrics"))
        output_tokens = _int(final.get("total_completion_tokens"))
        cost_usd = _float(final.get("total_cost_usd"))
        return {
            "model_calls": None,
            "output_tokens": output_tokens,
            "reasoning_tokens": None,
            "over_16k_calls": None,
            "cost_usd": cost_usd,
        }
    return {
        "model_calls": model_calls,
        "output_tokens": output_tokens,
        "reasoning_tokens": reasoning_tokens,
        "over_16k_calls": over_16k,
        "cost_usd": cost_usd,
    }


def _read_trajectory(path: Path) -> dict[str, Any]:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return _trajectory_metrics(data) if isinstance(data, dict) else {}


def _exit_four(trial_dir: Path, metadata: dict[str, Any]) -> bool:
    """Whether exec ended Incomplete (exit 4) or named max_tokens as the stop."""
    for key in ("exit_code", "return_code"):
        if _int(metadata.get(key)) == 4:
            return True
    if metadata.get("stop_reason") == "max_tokens":
        return True
    result = _last_result(trial_dir / "agent" / "radian-stdout.txt")
    return _int(result.get("exit_code")) == 4 or result.get("stop_reason") == "max_tokens"


def load_trial(trial_dir: Path) -> dict[str, Any] | None:
    """One trial's report row, or None when the file is not a trial result."""
    try:
        result = json.loads((trial_dir / "result.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(result, dict):
        return None
    agent_info = _dict(result.get("agent_info"))
    task = result.get("task_name")
    # A job-level result.json has neither field; only a trial result carries both.
    if not isinstance(task, str) or not task or not agent_info:
        return None

    agent = str(agent_info.get("name") or "")
    # The layout's `<agent>` level is the report label, so two radian builds in
    # separate directories stay separate columns. `agent_info.name` only
    # decides how a trial is read (a radian journal, an ATIF trajectory).
    label = str(trial_dir.parent.parent.name or agent or "unknown")
    trial = str(result.get("trial_name") or trial_dir.name)
    exception = str(_dict(result.get("exception_info")).get("exception_type") or "")
    excluded = exception.rsplit(".", 1)[-1] == "CancelledError"
    timed_out = "timeout" in exception.lower()
    rewards = _dict(_dict(result.get("verifier_result")).get("rewards"))
    reward = _float(rewards.get("reward"))
    if excluded:
        outcome = "skip"
    elif timed_out:
        outcome = "T/O"
    elif reward is not None and reward >= 1:
        outcome = "PASS"
    else:
        outcome = "fail"

    agent_result = _dict(result.get("agent_result"))
    metadata = _dict(agent_result.get("metadata"))
    cost_usd = _float(agent_result.get("cost_usd"))
    execution = _dict(result.get("agent_execution"))
    minutes = _minutes(execution.get("started_at"), execution.get("finished_at"))

    sessions_dir = trial_dir / "agent" / "radian-sessions"
    is_radian = agent.lower() == "radian" or sessions_dir.is_dir()
    if is_radian:
        source = _journal_metrics(sessions_dir) if sessions_dir.is_dir() else {}
        max_tokens = bool(source.get("max_tokens")) or _exit_four(trial_dir, metadata)
        generation_minutes = source.get("generation_minutes")
        tool_minutes = source.get("tool_minutes")
    else:
        source = _read_trajectory(trial_dir / "agent" / "trajectory.json")
        max_tokens = generation_minutes = tool_minutes = None
    if cost_usd is None:
        cost_usd = source.get("cost_usd")

    return {
        "agent": label,
        "task": task,
        "trial": trial,
        "path": str(trial_dir),
        "result": outcome,
        "excluded": excluded,
        "minutes": minutes,
        "cost_usd": cost_usd,
        "model_calls": source.get("model_calls"),
        "output_tokens": source.get("output_tokens"),
        "reasoning_tokens": source.get("reasoning_tokens"),
        "over_16k_calls": source.get("over_16k_calls"),
        "generation_minutes": generation_minutes,
        "tool_minutes": tool_minutes,
        "max_tokens": max_tokens,
    }


def collect(jobs: Iterable[str | Path]) -> list[dict[str, Any]]:
    """Every trial result under the given roots, sorted and de-duplicated."""
    trials: list[dict[str, Any]] = []
    seen: set[str] = set()
    for root in map(Path, jobs):
        for result_path in sorted(root.rglob("result.json")):
            trial_dir = result_path.parent
            key = str(trial_dir.resolve())
            if key in seen:
                continue
            seen.add(key)
            trial = load_trial(trial_dir)
            if trial is not None:
                trials.append(trial)
    trials.sort(key=lambda t: (t["agent"], t["task"], t["trial"]))
    return trials


def summarize(trials: list[dict[str, Any]]) -> dict[str, Any]:
    """Per-agent totals. Excluded trials are counted, never summed."""
    included = [trial for trial in trials if not trial["excluded"]]
    totals: dict[str, Any] = {
        "trials": len(included),
        "excluded": len(trials) - len(included),
        "pass": 0,
        "fail": 0,
        "timeout": 0,
        "minutes": None,
        "cost_usd": None,
        "model_calls": None,
        "output_tokens": None,
        "reasoning_tokens": None,
        "over_16k_calls": None,
        "generation_minutes": None,
        "tool_minutes": None,
        "max_tokens_trials": None,
    }
    for trial in included:
        totals["pass"] += trial["result"] == "PASS"
        totals["fail"] += trial["result"] == "fail"
        totals["timeout"] += trial["result"] == "T/O"
        for key in (
            "minutes",
            "cost_usd",
            "model_calls",
            "output_tokens",
            "reasoning_tokens",
            "over_16k_calls",
            "generation_minutes",
            "tool_minutes",
        ):
            totals[key] = _add(totals[key], trial[key])
        if trial["max_tokens"] is not None:
            totals["max_tokens_trials"] = _add(
                totals["max_tokens_trials"], int(trial["max_tokens"])
            )
    return totals


def _cell(trials: list[dict[str, Any]]) -> dict[str, int] | None:
    included = [trial for trial in trials if not trial["excluded"]]
    if not included:
        return None
    return {
        "trials": len(included),
        "passed": sum(trial["result"] == "PASS" for trial in included),
        "failed": sum(trial["result"] == "fail" for trial in included),
        "timeout": sum(trial["result"] == "T/O" for trial in included),
    }


def common_tasks(trials: list[dict[str, Any]], agents: list[str]) -> list[str]:
    """Tasks with at least one finished trial for every agent."""
    common = []
    for task in sorted({trial["task"] for trial in trials}):
        finished = {
            trial["agent"]
            for trial in trials
            if trial["task"] == task and trial["result"] in ("PASS", "fail")
        }
        if all(agent in finished for agent in agents):
            common.append(task)
    return common


def build_report(jobs: Iterable[str | Path]) -> dict[str, Any]:
    trials = collect(jobs)
    agents = sorted({trial["agent"] for trial in trials})
    # A task known only from an excluded trial has no comparable result.
    tasks = sorted({trial["task"] for trial in trials if not trial["excluded"]})
    matrix: dict[str, dict[str, dict[str, int] | None]] = {}
    for task in tasks:
        matrix[task] = {
            agent: _cell([t for t in trials if t["task"] == task and t["agent"] == agent])
            for agent in agents
        }
    common = common_tasks(trials, agents)
    return {
        "agents": agents,
        "tasks": tasks,
        "trials": trials,
        "matrix": matrix,
        "totals": {
            agent: summarize([t for t in trials if t["agent"] == agent])
            for agent in agents
        },
        "common_tasks": common,
        "common_totals": {
            agent: summarize(
                [t for t in trials if t["agent"] == agent and t["task"] in common]
            )
            for agent in agents
        },
    }


def _table(headers: list[str], rows: list[list[str]]) -> str:
    widths = [
        max(len(header), *(len(row[index]) for row in rows))
        for index, header in enumerate(headers)
    ]

    def line(cells: list[str]) -> str:
        return "  ".join(cell.ljust(width) for cell, width in zip(cells, widths)).rstrip()

    return "\n".join(
        [line(headers), line(["-" * width for width in widths]), *(line(row) for row in rows)]
    )


def _show(value: Any, spec: str) -> str:
    return "-" if value is None else format(value, spec)


def _cell_text(cell: dict[str, int] | None) -> str:
    if cell is None:
        return "-"
    if cell["trials"] == 1:
        if cell["passed"]:
            return "PASS"
        return "T/O" if cell["timeout"] else "fail"
    text = f"P{cell['passed']}/{cell['trials']}"
    return text + (f" T{cell['timeout']}" if cell["timeout"] else "")


def _max_tokens_text(value: Any) -> str:
    return "-" if value is None else ("yes" if value else "no")


def format_report(report: dict[str, Any]) -> str:
    agents: list[str] = report["agents"]
    trials: list[dict[str, Any]] = report["trials"]
    blocks: list[str] = []

    matrix_rows = [
        [task, *(_cell_text(report["matrix"][task][agent]) for agent in agents)]
        for task in report["tasks"]
    ]
    blocks.append("MATRIX (per task; repeats as P<passed>/<n>)\n" + _table(["task", *agents], matrix_rows))

    trial_rows = [
        [
            trial["agent"],
            trial["task"],
            trial["trial"],
            trial["result"],
            _show(trial["minutes"], ".1f"),
            _show(trial["cost_usd"], ".4f"),
            _show(trial["model_calls"], ",d"),
            _show(trial["output_tokens"], ",d"),
            _show(trial["reasoning_tokens"], ",d"),
            _show(trial["over_16k_calls"], ",d"),
            _show(trial["generation_minutes"], ".2f"),
            _show(trial["tool_minutes"], ".2f"),
            _max_tokens_text(trial["max_tokens"]),
        ]
        for trial in trials
    ]
    blocks.append(
        "TRIALS (skip = CancelledError; excluded from every total)\n"
        + _table(
            [
                "agent",
                "task",
                "trial",
                "result",
                "min",
                "cost",
                "calls",
                "out_tok",
                "reason",
                "over16k",
                "gen_min",
                "tool_min",
                "maxtok",
            ],
            trial_rows,
        )
    )

    def totals_table(label: str, totals: dict[str, dict[str, Any]]) -> str:
        rows = [
            [
                agent,
                str(totals[agent]["trials"]),
                str(totals[agent]["pass"]),
                str(totals[agent]["fail"]),
                str(totals[agent]["timeout"]),
                str(totals[agent]["excluded"]),
                _show(totals[agent]["minutes"], ".1f"),
                _show(totals[agent]["cost_usd"], ".4f"),
                _show(totals[agent]["model_calls"], ",d"),
                _show(totals[agent]["output_tokens"], ",d"),
                _show(totals[agent]["reasoning_tokens"], ",d"),
                _show(totals[agent]["over_16k_calls"], ",d"),
                _show(totals[agent]["generation_minutes"], ".2f"),
                _show(totals[agent]["tool_minutes"], ".2f"),
                _show(totals[agent]["max_tokens_trials"], ",d"),
            ]
            for agent in agents
        ]
        return label + "\n" + _table(
            [
                "agent",
                "trials",
                "pass",
                "fail",
                "T/O",
                "excl",
                "min",
                "cost",
                "calls",
                "out_tok",
                "reason",
                "over16k",
                "gen_min",
                "tool_min",
                "maxtok",
            ],
            rows,
        )

    blocks.append(totals_table("AGENT TOTALS", report["totals"]))
    common = report["common_tasks"]
    blocks.append(
        "COMMON TASKS (every agent finished one trial): "
        + (", ".join(common) if common else "none")
    )
    if common:
        blocks.append(totals_table("COMMON-TASK TOTALS", report["common_totals"]))
    return "\n\n".join(blocks)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Per-task matrix and per-agent totals from Harbor job directories."
    )
    parser.add_argument(
        "jobs",
        nargs="+",
        metavar="JOBS",
        help="jobs directory (or any parent of trials)",
    )
    parser.add_argument("--json", action="store_true", help="print the report as JSON")
    args = parser.parse_args(argv)
    report = build_report(args.jobs)
    if args.json:
        print(json.dumps(report, indent=2))
    elif not report["trials"]:
        print("no trial result.json found under: " + ", ".join(args.jobs), file=sys.stderr)
        return 1
    else:
        print(format_report(report))
    return 0


if __name__ == "__main__":
    sys.exit(main())
