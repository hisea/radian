#!/usr/bin/env python3
"""Per-task and per-agent metrics from finished Harbor job directories.

    python3 benchmarks/terminal-bench/metrics.py JOBS [JOBS ...] [--json] [--baseline AGENT]

`JOBS` is a jobs directory (``<jobs>/<agent>/<timestamp>/<task>__<id>/``) or any
directory above its trials. Every trial is read from its ``result.json``. A
radian trial's ``agent/radian-sessions/*.jsonl`` journal adds generation/tool
minutes and the MaxTokens outcome; a pi/opencode trial's
``agent/trajectory.json`` (ATIF) adds the per-call token metrics they report.
The reported output and reasoning counts stay raw; ``generated_tokens`` (visible
output plus reasoning) follows a per-producer rule, so it means the same thing
across agents.

A trial whose exception is ``CancelledError`` is excluded from every total and
counted separately. The common-task section keeps only tasks for which every
agent has at least one finished (not timed-out, not excluded) trial. The
``SPREAD``, ``LEAVE-ONE-TASK-OUT`` and ``INTERVALS`` sections use exactly those
common tasks and non-excluded trials. They show one agent's own per-task
spread, which task carries a total's difference, and whether a ratio or pass
difference is larger than the resampling noise. ``--baseline`` names the agent
every ratio divides by; it defaults to the first agent in the report.

A job can hold a multi-GB journal, so only the journal and stdout files are
streamed: line by line, under a per-line cap of ``MAX_LINE_CHARS`` (an
over-long line is drained and skipped, and reading continues), with no limit on
the file's total size. ``result.json`` and ``trajectory.json`` are summaries and
are read whole, but only when each is at most ``MAX_JSON_BYTES``; an over-limit
trajectory is treated as unreadable, and its trial keeps the facts from
``result.json`` only.

Stdlib only. ``--self-test`` runs ``tests/test_metrics.py`` on the checked-in
fixture jobs; the same suite is a step of ``scripts/verify.sh``. See
``README.md`` for the column meanings.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import math
import random
import statistics
import sys
from collections.abc import Iterable, Iterator
from datetime import datetime
from pathlib import Path
from typing import Any

# "More than 16k output tokens" in the report. A decimal 16k keeps the column
# easy to check by hand; no provider reports a meaningful 384-token boundary.
OVER_16K_TOKENS = 16_000

# Read bounds. One trial's journal is hundreds of MB in a long run, and the
# jobs directory is untrusted input: an over-limit line or JSON file is dropped
# instead of being loaded into memory. The journal writer caps a tool result
# near 64 KiB (a base64 image is at most ~7 MB), so a 16 MiB line is generous.
MAX_LINE_CHARS = 16 * 1024 * 1024
MAX_JSON_BYTES = 64 * 1024 * 1024


def _load_journal_metrics():
    """Load `benchmarks/journal_metrics.py` by path (it is not a sibling module)."""
    path = Path(__file__).resolve().parents[1] / "journal_metrics.py"
    spec = importlib.util.spec_from_file_location("journal_metrics", path)
    if spec is None or spec.loader is None:  # pragma: no cover - a broken tree
        raise ImportError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


journal_metrics = _load_journal_metrics()

# The task-level bootstrap behind INTERVALS. A fixed seed and a fixed number of
# resamples make the printed interval reproducible from the same trials.
BOOTSTRAP_RESAMPLES = 2000
BOOTSTRAP_SEED = 0
INTERVAL_PERCENTILES = (2.5, 97.5)
INTERVAL_MIN_TASKS = 3
INTERVAL_MIN_TASKS_NOTE = "the interval needs at least 3 common tasks"

# Reasoning lives in a provider-shaped usage sub-object. Radian keeps the
# provider's own object in `details`; ATIF producers put it in `metrics.extra`.
_REASONING_KEYS = ("reasoning_tokens", "reasoning_output_tokens", "thinking_tokens")

# The rule that turns a producer's own output counter into `generated_tokens`
# (visible output plus reasoning). The numbers cannot tell you which basis a
# producer used, so the rule is a table keyed by the producer's name:
#
#   includes_reasoning  the output counter already counts reasoning
#   excludes_reasoning  it does not, so the step's reasoning is added
#
# pi 1.4.0 counts reasoning in `completion_tokens`; opencode 1.19.4 does not (in
# ab3, 200 of 415 opencode steps had more reasoning than completion, while 0 of
# 464 pi steps did). A radian journal's `usage.output` counts reasoning.
# An ATIF producer that is not in this table gets `gen_tok` `-`, never a guess.
GENERATED_RULES = {
    "opencode": "excludes_reasoning",
    "pi": "includes_reasoning",
    "radian": "includes_reasoning",
}


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


def _iter_lines(path: Path, limit: int | None = None) -> Iterator[str]:
    """Yield a file's lines without loading the file whole.

    A line longer than ``limit`` characters is drained and skipped: it cannot
    be a trial record this report understands, and reading it whole is what a
    multi-GB journal must not force. The final line is yielded even without a
    trailing newline; a journal caller treats that as a torn tail (ADR 0006),
    while a stdout caller keeps it as the closing record.
    """
    cap = MAX_LINE_CHARS if limit is None else limit
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        while True:
            line = handle.readline(cap)
            if not line:
                return
            if not line.endswith("\n") and len(line) >= cap:
                while True:  # drain the rest of the overlong line
                    rest = handle.readline(cap)
                    if not rest or rest.endswith("\n"):
                        break
                continue
            yield line
            if not line.endswith("\n"):
                return


def _read_json(path: Path, limit: int | None = None) -> Any:
    """A JSON file of at most ``limit`` bytes, or None when absent or over it."""
    cap = MAX_JSON_BYTES if limit is None else limit
    try:
        with path.open("rb") as handle:
            data = handle.read(cap + 1)
    except OSError:
        return None
    if len(data) > cap:
        return None
    try:
        return json.loads(data)
    except ValueError:
        return None


def _last_result(stdout_path: Path) -> dict[str, Any]:
    """The closing `Result` record of `radian exec --output jsonl`, or {}."""
    last = ""
    try:
        for line in _iter_lines(stdout_path):
            if line.strip():
                last = line
    except OSError:
        return {}
    if not last:
        return {}
    try:
        record = json.loads(last)
    except ValueError:
        return {}
    return record if isinstance(record, dict) and record.get("type") == "Result" else {}


def _journal_metrics(sessions_dir: Path) -> dict[str, Any]:
    """Calls, tokens and timing from one trial's radian session journals.

    The fixed definitions live in `benchmarks/journal_metrics.py`; this function
    keeps the Harbor report's keys and values unchanged. Only the directory's own
    `*.jsonl` files are read: a nested JSONL artifact is not a session journal,
    which is what the replaced `sessions_dir.glob("*.jsonl")` did. Generation
    time runs from the request boundary (a tool settling, a user message, a turn
    start, or the previous response) to the assistant message that ends it. Tool
    time is the sum of each matched `ToolExecutionStarted` ->
    `ToolExecutionSettled` pair; parallel calls are summed, so tool minutes can
    exceed the trial's wall time. A torn tail (no final LF) is not a committed
    record and is dropped, like the Rust reader. The journal is streamed, so one
    trial's memory use is one line, not the file.
    """
    source = journal_metrics.usage_metrics([sessions_dir], recursive=False)
    return {
        "model_calls": source["model_calls"],
        "output_tokens": source["output_tokens"],
        "generated_tokens": source["generated_tokens"],
        "reasoning_tokens": source["reasoning_tokens"],
        "over_16k_calls": source["over_16k_calls"],
        "generation_minutes": source["generation_minutes"],
        "tool_minutes": source["tool_minutes"],
        "cost_usd": source["cost_usd"],
        "max_tokens": source["max_tokens"],
    }


def _iter_steps(trajectory: dict[str, Any]) -> Iterator[dict[str, Any]]:
    for step in trajectory.get("steps") or []:
        if isinstance(step, dict):
            yield step
    for subagent in trajectory.get("subagent_trajectories") or []:
        if isinstance(subagent, dict):
            yield from _iter_steps(subagent)


def _producer_name(trajectory: dict[str, Any]) -> str:
    """The ATIF producer's own name, normalized for the rule table."""
    name = _dict(trajectory.get("agent")).get("name")
    return name.strip().lower() if isinstance(name, str) else ""


def _trajectory_metrics(trajectory: dict[str, Any]) -> dict[str, Any]:
    """Per-call metrics an ATIF producer reported, summed over its steps."""
    producer = _producer_name(trajectory)
    rule = GENERATED_RULES.get(producer)
    model_calls = 0
    output_tokens = reasoning_tokens = generated_tokens = cost_usd = None
    over_16k = 0
    seen = False
    for step in _iter_steps(trajectory):
        metrics = _dict(step.get("metrics"))
        if not metrics:
            continue
        seen = True
        count = _int(step.get("llm_call_count"))
        model_calls += count if count and count > 0 else 1
        completion = _int(metrics.get("completion_tokens"))
        if completion is not None:
            output_tokens = _add(output_tokens, completion)
            if completion > OVER_16K_TOKENS:
                over_16k += 1
        count = _reasoning(_dict(metrics.get("extra")))
        if count is None:
            count = _reasoning(metrics)
        if count is not None:
            reasoning_tokens = _add(reasoning_tokens, count)
        if completion is not None:
            if rule == "includes_reasoning":
                generated_tokens = _add(generated_tokens, completion)
            elif rule == "excludes_reasoning":
                # A step that reported no reasoning adds its completion only.
                generated_tokens = _add(generated_tokens, completion + (count or 0))
        cost_usd = _add(cost_usd, _float(metrics.get("cost_usd")))
    if not seen:
        # A producer that wrote only aggregate metrics still reports tokens.
        final = _dict(trajectory.get("final_metrics"))
        output_tokens = _int(final.get("total_completion_tokens"))
        cost_usd = _float(final.get("total_cost_usd"))
        # Without steps there is no per-step reasoning to add, so only a rule
        # that includes reasoning in the output counter can give `gen_tok`.
        generated_tokens = output_tokens if rule == "includes_reasoning" else None
        return {
            "model_calls": None,
            "output_tokens": output_tokens,
            "generated_tokens": generated_tokens,
            "reasoning_tokens": None,
            "over_16k_calls": None,
            "cost_usd": cost_usd,
            "unruled_producer": producer if rule is None else None,
        }
    return {
        "model_calls": model_calls,
        "output_tokens": output_tokens,
        "generated_tokens": generated_tokens,
        "reasoning_tokens": reasoning_tokens,
        "over_16k_calls": over_16k,
        "cost_usd": cost_usd,
        "unruled_producer": producer if rule is None else None,
    }


def _read_trajectory(path: Path, limit: int | None = None) -> dict[str, Any]:
    data = _read_json(path, limit)
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
    result = _read_json(trial_dir / "result.json")
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
        "generated_tokens": source.get("generated_tokens"),
        "reasoning_tokens": source.get("reasoning_tokens"),
        "unruled_producer": source.get("unruled_producer"),
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
        "generated_tokens": None,
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
            "generated_tokens",
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
    cost = totals["cost_usd"]
    totals["cost_per_pass"] = (
        cost / totals["pass"] if cost is not None and totals["pass"] else None
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


def _included(
    trials: list[dict[str, Any]], agent: str, task: str | None = None
) -> list[dict[str, Any]]:
    """The non-excluded trials of one agent (and task): the trials totals use."""
    return [
        trial
        for trial in trials
        if not trial["excluded"]
        and trial["agent"] == agent
        and (task is None or trial["task"] == task)
    ]


def _sum_reported(trials: list[dict[str, Any]], key: str) -> float | int | None:
    """The sum of a counter over the trials that reported it, or None."""
    values = [trial[key] for trial in trials if trial[key] is not None]
    return sum(values) if values else None


def _ratio(agent_value: Any, baseline_value: Any) -> float | None:
    """`agent / baseline`, or None when either side is unknown or zero."""
    if agent_value is None or baseline_value is None or baseline_value == 0:
        return None
    return agent_value / baseline_value


def spread(
    trials: list[dict[str, Any]],
    agents: list[str],
    common: list[str],
    baseline: str | None,
) -> dict[str, Any]:
    """Per common task: each agent's gen_tok min/median/max, and the ratios."""
    cells: dict[str, dict[str, dict[str, Any] | None]] = {}
    ratios: dict[str, dict[str, float | None]] = {}
    for task in common:
        cells[task] = {}
        for agent in agents:
            values = sorted(
                trial["generated_tokens"]
                for trial in _included(trials, agent, task)
                if trial["generated_tokens"] is not None
            )
            cells[task][agent] = (
                {
                    "min": values[0],
                    "median": statistics.median(values),
                    "max": values[-1],
                    "trials": len(values),
                }
                if values
                else None
            )
        base_sum = _sum_reported(
            _included(trials, baseline, task) if baseline is not None else [],
            "generated_tokens",
        )
        ratios[task] = {
            agent: _ratio(
                _sum_reported(_included(trials, agent, task), "generated_tokens"),
                base_sum,
            )
            for agent in agents
            if agent != baseline
        }
    return {
        "tasks": common,
        "agents": agents,
        "cells": cells,
        "ratios": ratios,
        "baseline": baseline,
    }


def _furthest_row(rows: list[dict[str, Any]], agents: list[str]) -> str | None:
    """The removed task whose row moves a ratio furthest from the first row."""
    if not rows:
        return None
    none_row = rows[0]
    marked = None
    furthest = None
    for row in rows[1:]:
        moved = None
        for metric in ("gen_tok", "cost"):
            for agent in agents:
                before = none_row[metric].get(agent)
                after = row[metric].get(agent)
                if before is None or after is None:
                    continue
                distance = abs(after - before)
                moved = distance if moved is None else max(moved, distance)
        if moved is not None and (furthest is None or moved > furthest):
            furthest = moved
            marked = row["task"]
    return marked


def leave_one_out(
    trials: list[dict[str, Any]],
    agents: list[str],
    common: list[str],
    baseline: str | None,
) -> dict[str, Any]:
    """Summed gen_tok and cost ratios over the common tasks without each task."""
    non_baseline = [agent for agent in agents if agent != baseline]
    base_trials = (
        _included(trials, baseline) if baseline is not None else []
    )
    rows: list[dict[str, Any]] = []
    for removed in [None, *common]:
        tasks = {task for task in common if task != removed}
        selected = {
            agent: [trial for trial in _included(trials, agent) if trial["task"] in tasks]
            for agent in non_baseline
        }
        base_selected = [
            trial for trial in base_trials if trial["task"] in tasks
        ]
        rows.append(
            {
                "task": removed,
                "gen_tok": {
                    agent: _ratio(
                        _sum_reported(selected[agent], "generated_tokens"),
                        _sum_reported(base_selected, "generated_tokens"),
                    )
                    for agent in non_baseline
                },
                "cost": {
                    agent: _ratio(
                        _sum_reported(selected[agent], "cost_usd"),
                        _sum_reported(base_selected, "cost_usd"),
                    )
                    for agent in non_baseline
                },
            }
        )
    return {
        "rows": rows,
        "marked": _furthest_row(rows, non_baseline),
        "agents": non_baseline,
        "baseline": baseline,
    }


def _percentile(values: list[float], percent: float) -> float | None:
    """A percentile by linear interpolation between closest ranks."""
    ordered = sorted(values)
    if not ordered:
        return None
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * percent / 100.0
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower)


def _interval(values: list[float], center: float) -> dict[str, Any]:
    """The 2.5th-97.5th percentile interval and whether it holds `center`."""
    if not values:
        return {"low": None, "high": None, "inconclusive": None}
    low = _percentile(values, INTERVAL_PERCENTILES[0])
    high = _percentile(values, INTERVAL_PERCENTILES[1])
    return {"low": low, "high": high, "inconclusive": low <= center <= high}


def intervals(
    trials: list[dict[str, Any]],
    agents: list[str],
    common: list[str],
    baseline: str | None,
) -> dict[str, Any]:
    """A task-level bootstrap of the gen_tok/cost ratios and pass difference."""
    non_baseline = [agent for agent in agents if agent != baseline]
    result: dict[str, Any] = {
        "resamples": BOOTSTRAP_RESAMPLES,
        "seed": BOOTSTRAP_SEED,
        "percentiles": list(INTERVAL_PERCENTILES),
        "baseline": baseline,
        "common_tasks": len(common),
        "note": None,
        "agents": {},
    }
    if len(common) < INTERVAL_MIN_TASKS:
        result["note"] = INTERVAL_MIN_TASKS_NOTE
        return result
    rng = random.Random(BOOTSTRAP_SEED)
    # Index the non-excluded trials once by agent and task. Looking them up per
    # (draw, agent, task) would rescan the whole trial list
    # BOOTSTRAP_RESAMPLES * len(common) * agents times.
    indexed: dict[tuple[str, str], list[dict[str, Any]]] = {}
    for trial in trials:
        if not trial["excluded"]:
            indexed.setdefault((trial["agent"], trial["task"]), []).append(trial)
    for agent in non_baseline:
        gen_tok: list[float] = []
        cost: list[float] = []
        pass_rate: list[float] = []
        for _ in range(BOOTSTRAP_RESAMPLES):
            drawn = [rng.choice(common) for _ in common]
            accumulators = {
                who: {"gen_tok": None, "cost": None, "pass": 0, "trials": 0}
                for who in (baseline, agent)
            }
            for task in drawn:
                for who in (baseline, agent):
                    on_task = indexed.get((who, task), [])
                    for trial in (rng.choice(on_task) for _ in on_task):
                        accumulator = accumulators[who]
                        if trial["generated_tokens"] is not None:
                            accumulator["gen_tok"] = _add(
                                accumulator["gen_tok"], trial["generated_tokens"]
                            )
                        if trial["cost_usd"] is not None:
                            accumulator["cost"] = _add(
                                accumulator["cost"], trial["cost_usd"]
                            )
                        accumulator["pass"] += trial["result"] == "PASS"
                        accumulator["trials"] += 1
            base = accumulators[baseline]
            other = accumulators[agent]
            ratio = _ratio(other["gen_tok"], base["gen_tok"])
            if ratio is not None:
                gen_tok.append(ratio)
            ratio = _ratio(other["cost"], base["cost"])
            if ratio is not None:
                cost.append(ratio)
            if base["trials"] and other["trials"]:
                pass_rate.append(
                    (
                        other["pass"] / other["trials"]
                        - base["pass"] / base["trials"]
                    )
                    * 100.0
                )
        result["agents"][agent] = {
            "gen_tok": _interval(gen_tok, 1.0),
            "cost": _interval(cost, 1.0),
            "pass_rate": _interval(pass_rate, 0.0),
        }
    return result


def build_report(jobs: Iterable[str | Path], baseline: str | None = None) -> dict[str, Any]:
    trials = collect(jobs)
    agents = sorted({trial["agent"] for trial in trials})
    if baseline is not None and baseline not in agents:
        available = ", ".join(agents) if agents else "none"
        raise ValueError(f"unknown baseline agent {baseline!r}; agents: {available}")
    if baseline is None:
        baseline = agents[0] if agents else None
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
        "baseline": baseline,
        "tasks": tasks,
        "trials": trials,
        "matrix": matrix,
        # ATIF producers with no generated-token rule: their gen_tok is `-`.
        "gen_unknown_agents": sorted(
            {trial["unruled_producer"] for trial in trials if trial["unruled_producer"]}
        ),
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
        "spread": spread(trials, agents, common, baseline),
        "leave_one_out": leave_one_out(trials, agents, common, baseline),
        "intervals": intervals(trials, agents, common, baseline),
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
            _show(trial["generated_tokens"], ",d"),
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
                "gen_tok",
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
                _show(totals[agent]["cost_per_pass"], ".4f"),
                _show(totals[agent]["model_calls"], ",d"),
                _show(totals[agent]["output_tokens"], ",d"),
                _show(totals[agent]["reasoning_tokens"], ",d"),
                _show(totals[agent]["generated_tokens"], ",d"),
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
                "cost_per_pass",
                "calls",
                "out_tok",
                "reason",
                "gen_tok",
                "over16k",
                "gen_min",
                "tool_min",
                "maxtok",
            ],
            rows,
        )

    blocks.append(totals_table("AGENT TOTALS", report["totals"]))
    unruled = report["gen_unknown_agents"]
    if unruled:
        blocks.append(
            "gen_tok has no rule for: " + ", ".join(unruled) + " (reported as -)"
        )
    common = report["common_tasks"]
    blocks.append(
        "COMMON TASKS (every agent finished one trial): "
        + (", ".join(common) if common else "none")
    )
    if common:
        blocks.append(totals_table("COMMON-TASK TOTALS", report["common_totals"]))
    blocks.extend(_analysis_blocks(report))
    return "\n\n".join(blocks)


def _number_text(value: Any) -> str:
    """An integer-ish count without a decimal; a fraction with one place."""
    if value is None:
        return "-"
    if float(value).is_integer():
        return f"{int(value):,d}"
    return f"{value:,.1f}"


def _spread_text(cell: dict[str, Any] | None) -> str:
    if cell is None:
        return "-"
    return "/".join(
        _number_text(cell[key]) for key in ("min", "median", "max")
    )


def _ratio_text(value: float | None) -> str:
    return "-" if value is None else f"{value:.2f}"


def _analysis_blocks(report: dict[str, Any]) -> list[str]:
    """The SPREAD, LEAVE-ONE-TASK-OUT and INTERVALS sections."""
    agents: list[str] = report["agents"]
    baseline = report["baseline"]
    non_baseline = [agent for agent in agents if agent != baseline]
    blocks: list[str] = []

    spread_data = report["spread"]
    spread_headers = [
        "task",
        *agents,
        *(f"{agent}/{baseline}" for agent in non_baseline),
    ]
    spread_rows = [
        [
            task,
            *(_spread_text(spread_data["cells"][task][agent]) for agent in agents),
            *(_ratio_text(spread_data["ratios"][task][agent]) for agent in non_baseline),
        ]
        for task in spread_data["tasks"]
    ]
    spread_title = "SPREAD (gen_tok min/median/max per task"
    if non_baseline:
        spread_title += f"; ratio = agent/{baseline}"
    blocks.append(spread_title + ")\n" + _table(spread_headers, spread_rows))

    leave_data = report["leave_one_out"]
    leave_headers = [
        "task",
        *(
            cell
            for agent in non_baseline
            for cell in (f"{agent} gen_tok", f"{agent} cost")
        ),
    ]
    leave_rows = []
    for row in leave_data["rows"]:
        label = "(none)" if row["task"] is None else row["task"]
        if row["task"] is not None and row["task"] == leave_data["marked"]:
            label += "*"
        cells = [label]
        for agent in non_baseline:
            cells.extend(
                [
                    _ratio_text(row["gen_tok"][agent]),
                    _ratio_text(row["cost"][agent]),
                ]
            )
        leave_rows.append(cells)
    leave_title = (
        "LEAVE-ONE-TASK-OUT (summed ratio against the baseline without the "
        "row's task"
    )
    if non_baseline:
        leave_title += (
            f"; * moves a ratio furthest from (none); ratio = agent/{baseline}"
        )
    blocks.append(leave_title + ")\n" + _table(leave_headers, leave_rows))

    interval_data = report["intervals"]
    if interval_data["note"]:
        blocks.append("INTERVALS: " + interval_data["note"])
        return blocks
    metrics_rows = (
        ("gen_tok ratio", "gen_tok", ".2f"),
        ("cost ratio", "cost", ".2f"),
        ("pass diff (pp)", "pass_rate", ".1f"),
    )
    interval_rows = []
    for agent in non_baseline:
        for label, key, spec in metrics_rows:
            interval = interval_data["agents"][agent][key]
            note = "inconclusive" if interval["inconclusive"] else ""
            interval_rows.append(
                [
                    agent,
                    label,
                    _show(interval["low"], spec),
                    _show(interval["high"], spec),
                    note,
                ]
            )
    low, high = interval_data["percentiles"]
    blocks.append(
        "INTERVALS (task-level bootstrap, "
        f"{interval_data['resamples']} resamples, seed {interval_data['seed']}, "
        f"{low}-{high} percentiles)\n"
        + _table(["agent", "metric", "low", "high", "note"], interval_rows)
    )
    return blocks


def _self_test() -> int:
    """Run ``tests/test_metrics.py`` on the checked-in fixture jobs."""
    import unittest

    tests_dir = Path(__file__).resolve().parent / "tests"
    suite = unittest.defaultTestLoader.discover(str(tests_dir), pattern="test_metrics.py")
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Per-task matrix and per-agent totals from Harbor job directories."
    )
    parser.add_argument(
        "jobs",
        nargs="*",
        metavar="JOBS",
        help="jobs directory (or any parent of trials)",
    )
    parser.add_argument("--json", action="store_true", help="print the report as JSON")
    parser.add_argument(
        "--baseline",
        metavar="AGENT",
        help="agent every ratio divides by (default: the first agent in the report)",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run tests/test_metrics.py on the fixture jobs and exit",
    )
    args = parser.parse_args(argv)
    if args.self_test:
        return _self_test()
    if not args.jobs:
        parser.error("JOBS is required unless --self-test is given")
    try:
        report = build_report(args.jobs, baseline=args.baseline)
    except ValueError as error:
        parser.error(str(error))
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
