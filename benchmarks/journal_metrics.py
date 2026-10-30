#!/usr/bin/env python3
"""Fixed-definition metrics from radian session journals and pi session JSONL.

    python3 benchmarks/journal_metrics.py JOURNALS... [--prefix PREFIX] [--json]

``JOURNALS`` is any number of radian session journal files or directories, plus
pi session JSONL files. A directory is searched recursively for ``*.jsonl``;
``--prefix`` keeps only files whose name starts with the prefix. A path that
names no journal — a missing file or directory, or a directory with no file
after the prefix filter — is an error that names the path, so a typo cannot
read as an empty run. One row is printed per journal and a ``TOTAL`` row after
them. ``--json`` prints the same computed rows as JSON.

The column table below (``COLUMNS``) is the single source for this help text and
for the generated table in ``benchmarks/terminal-bench/README.md``: a test
compares the README block with ``columns_markdown()``, so the two cannot drift.

The definitions are fixed on purpose, so numbers from two A/B rounds can be
compared. A radian journal is read as ADR 0006 writes it; a pi session line is
read as ``{"type": "message", ...}`` records. A torn final line (no final LF) is
not a committed record and is dropped, as the Rust reader drops it.

Stdlib only.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from datetime import datetime
from pathlib import Path
from typing import Any, Iterator

# One latency bucket per 100k prompt tokens: bucket 0 is 0-99,999, bucket 1 is
# 100,000-199,999, and so on.
LATENCY_BUCKET_TOKENS = 100_000

# A `read` result at or above 20 KB is the "single large result" column. Here
# 20 KB means 20 KiB, so 20,480 bytes is the first large value.
LARGE_READ_BYTES = 20 * 1024

# A usage record with more output tokens than this is counted for the existing
# Harbor report (`metrics._journal_metrics`), not as a table column.
OVER_16K_TOKENS = 16_000# A journal is streamed line by line. The cap matches `terminal-bench/metrics.py`
# so both readers drop the same over-long line.
MAX_LINE_CHARS = 16 * 1024 * 1024

# The fixed column table. `name` is the JSON key (or key pattern), `definition`
# says exactly what is counted, `unit` says what one unit is. The table is
# data, not prose: `columns_text()` and `columns_markdown()` read it.
COLUMNS = (
    {
        "name": "journal",
        "definition": "The journal file that produced the row; `TOTAL` on the totals row.",
        "unit": "path",
    },
    {
        "name": "source",
        "definition": "`radian` for a radian session journal, `pi` for a pi session JSONL.",
        "unit": "name",
    },
    {
        "name": "calls",
        "definition": (
            "Model calls, equal to steps: radian assistant messages plus "
            "`CompactionUsageRecorded` summary calls; pi assistant messages."
        ),
        "unit": "calls",
    },
    {
        "name": "input_uncached",
        "definition": (
            "Input tokens billed at the full price (ADR 0007 `uncached_input()`). "
            "A radian record marked `input_includes_cache: true` subtracts both "
            "`cache_read` and `cache_write` from `input`; a record marked false "
            "and an unmarked one report `input` alone, as does a pi record."
        ),
        "unit": "tokens",
    },
    {
        "name": "input_cache_read",
        "definition": (
            "Input tokens served from the provider cache (`cache_read`). An "
            "unmarked radian record's value is reported but not added to "
            "`input_total` (it may already sit inside `input`)."
        ),
        "unit": "tokens",
    },
    {
        "name": "input_total",
        "definition": (
            "The whole prompt the provider saw (ADR 0007 `prompt_total()`): "
            "`input` when it is marked `input_includes_cache: true` or unmarked, "
            "`input + cache_read + cache_write` when marked false. A cache write "
            "has no column of its own, so the total can exceed "
            "`input_uncached + input_cache_read`."
        ),
        "unit": "tokens",
    },
    {
        "name": "output",
        "definition": (
            "Reported output tokens, raw: radian `usage.output`; pi `usage.output`."
        ),
        "unit": "tokens",
    },
    {
        "name": "reasoning",
        "definition": (
            "The reasoning part of `output`, from the provider usage object "
            "(`reasoning_tokens`, `reasoning_output_tokens` or `thinking_tokens`)."
        ),
        "unit": "tokens",
    },
    {
        "name": "fee_usd",
        "definition": (
            "Fee in USD: radian `fee.micros / 1,000,000` (including summary "
            "calls); pi `usage.cost.total`."
        ),
        "unit": "USD",
    },
    {
        "name": "calls.<tool>",
        "definition": (
            "Calls of one tool name. pi's `bash` is counted as `shell`, so the "
            "column means the same thing for both agents."
        ),
        "unit": "calls",
    },
    {
        "name": "bytes.<tool>",
        "definition": (
            "UTF-8 bytes of the concatenated text parts of one tool name's "
            "results, without the JSON wrapping."
        ),
        "unit": "bytes",
    },
    {
        "name": "read_ranged_calls",
        "definition": "`read` calls whose arguments hold `offset` or `limit`.",
        "unit": "calls",
    },
    {
        "name": "read_ranged_bytes",
        "definition": "Result-text bytes of the ranged `read` calls.",
        "unit": "bytes",
    },
    {
        "name": "read_unranged_calls",
        "definition": "`read` calls whose arguments hold neither `offset` nor `limit`.",
        "unit": "calls",
    },
    {
        "name": "read_unranged_bytes",
        "definition": "Result-text bytes of the unranged `read` calls.",
        "unit": "bytes",
    },
    {
        "name": "read_large_calls",
        "definition": "`read` results whose text is 20,480 bytes or more (20 KiB).",
        "unit": "calls",
    },
    {
        "name": "p50ms.<bucket>",
        "definition": (
            "p50 of per-call latency in one prompt bucket: from the previous "
            "ready event (turn start, user message, tool settle, or the previous "
            "assistant message) to the usage record. Bucket 0 is 0-99,999 prompt "
            "tokens, bucket 1 is 100,000-199,999, and so on."
        ),
        "unit": "ms",
    },
    {
        "name": "p90ms.<bucket>",
        "definition": "p90 of the same per-bucket latency samples.",
        "unit": "ms",
    },
    {
        "name": "compactions",
        "definition": "`ContextCompacted` records (pi: entries with type `compaction`).",
        "unit": "count",
    },
    {
        "name": "deterministic_compactions",
        "definition": "The compactions whose `deterministic` field is true.",
        "unit": "count",
    },
    {
        "name": "outcomes",
        "definition": (
            "Turn outcome counts. radian counts every `TurnFinished.outcome`; pi "
            "counts the last assistant `stopReason` (stop -> completed, length -> "
            "incomplete, error -> failed, aborted -> cancelled, toolUse -> "
            "incomplete)."
        ),
        "unit": "count",
    },
    {
        "name": "wall_seconds",
        "definition": "First committed timestamp to last committed timestamp.",
        "unit": "seconds",
    },
)

# pi's own tool name for a shell command is `bash`; the shared column is `shell`.
PI_TOOL_NAMES = {"bash": "shell"}

# pi has no turn event, so the last assistant message's stop reason is its
# outcome. A final `toolUse` means the run stopped with work still requested.
PI_OUTCOMES = {
    "stop": "completed",
    "end": "completed",
    "length": "incomplete",
    "toolUse": "incomplete",
    "tool_use": "incomplete",
    "error": "failed",
    "aborted": "cancelled",
}

REASONING_KEYS = (
    "reasoning_tokens",
    "reasoning_output_tokens",
    "thinking_tokens",
    "reasoningTokens",
    "reasoning",
)


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


def _add(total: float | int | None, value: float | int | None) -> float | int | None:
    """Add a reported value; a missing value is "not reported", never zero."""
    if value is None:
        return total
    return (total or 0) + value


def _time(value: Any) -> datetime | None:
    if not isinstance(value, str) or not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def _seconds(begin: datetime | None, end: datetime | None) -> float | None:
    if begin is None or end is None:
        return None
    try:
        return max(0.0, (end - begin).total_seconds())
    except TypeError:  # one timestamp naive, the other aware
        return None


def _first_int(mapping: dict[str, Any], keys: tuple[str, ...]) -> int | None:
    for key in keys:
        value = _int(mapping.get(key))
        if value is not None:
            return value
    return None


def _reasoning(details: Any) -> int | None:
    """The reasoning count inside a provider usage object, or None."""
    if not isinstance(details, dict):
        return None
    for key in REASONING_KEYS:
        count = _int(details.get(key))
        if count is not None:
            return count
    for value in details.values():
        if isinstance(value, dict):
            count = _reasoning(value)
            if count is not None:
                return count
    return None


def _iter_lines(path: Path, limit: int | None = None) -> Iterator[str]:
    """A file's lines without loading it whole; an over-long line is drained."""
    cap = MAX_LINE_CHARS if limit is None else limit
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        while True:
            line = handle.readline(cap)
            if not line:
                return
            if not line.endswith("\n") and len(line) >= cap:
                while True:  # drain the rest of the over-long line
                    rest = handle.readline(cap)
                    if not rest or rest.endswith("\n"):
                        break
                continue
            yield line
            if not line.endswith("\n"):
                return


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


def _content_text(content: Any) -> str:
    """The text parts of radian or pi content, concatenated like `ToolResult::text`."""
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    parts: list[str] = []
    for part in content:
        if not isinstance(part, dict):
            continue
        # radian: {"text": {"text": "..."}} models `ContentPart::Text`.
        radian_text = _dict(part.get("text"))
        if isinstance(radian_text.get("text"), str):
            parts.append(radian_text["text"])
        # pi: {"type": "text", "text": "..."}.
        elif part.get("type") == "text" and isinstance(part.get("text"), str):
            parts.append(part["text"])
    return "".join(parts)


def _tool_result_text(result: Any) -> str:
    """The text of a radian `ToolExecutionSettled.result`."""
    if not isinstance(result, dict):
        return ""
    return _content_text(result.get("content"))


def _arguments(value: Any) -> dict[str, Any]:
    if isinstance(value, dict):
        return value
    if isinstance(value, str) and value.strip():
        try:
            parsed = json.loads(value)
        except ValueError:
            return {}
        return parsed if isinstance(parsed, dict) else {}
    return {}


def _read_is_ranged(arguments: dict[str, Any]) -> bool:
    """`offset` or `limit` selects a range; neither one reads the whole file."""
    return "offset" in arguments or "limit" in arguments


def _radian_inputs(usage: dict[str, Any]) -> tuple[int | None, int | None, int | None]:
    """(uncached, cache_read, prompt total) for a radian usage object (ADR 0007 J11).

    These are exactly `ModelUsage::uncached_input()` and `prompt_total()`:

    - `input_includes_cache: true` (OpenAI-compatible): `input` is the whole
      prompt, so the full-price part subtracts **both** cache counters and the
      total is `input` itself.
    - `input_includes_cache: false` (Anthropic): `input` is the uncached part
      and the cache counters are its siblings, so the total adds both.
    - unmarked (a record written before ADR 0007): `input` stands alone both as
      the full-price part and as the prompt total. Its `cache_read` is still
      reported, but it is not added again: it may already be inside `input`,
      and adding it would double-count.
    """
    input_tokens = _int(usage.get("input"))
    cache_read = _int(usage.get("cache_read"))
    cache_write = _int(usage.get("cache_write"))
    includes = usage.get("input_includes_cache")
    if input_tokens is None:
        return None, cache_read, None
    cache = (cache_read or 0) + (cache_write or 0)
    if includes is True:
        return max(0, input_tokens - cache), cache_read, input_tokens
    if includes is False:
        return input_tokens, cache_read, input_tokens + cache
    return input_tokens, cache_read, input_tokens


def _pi_inputs(usage: dict[str, Any]) -> tuple[int | None, int | None, int | None]:
    """(uncached, cache_read, prompt total) for a pi usage object.

    pi reports `input` without the cache counters (the separate `cacheRead` and
    `cacheWrite` fields are why), so the total adds both.
    """
    input_tokens = _first_int(usage, ("input", "input_tokens", "inputTokens"))
    cache_read = _first_int(
        usage,
        ("cacheRead", "cache_read", "cache_read_input_tokens", "cacheReadInputTokens"),
    )
    cache_write = _first_int(
        usage,
        ("cacheWrite", "cache_write", "cache_creation_input_tokens", "cacheWriteInputTokens"),
    )
    if input_tokens is None:
        return None, cache_read, None
    return input_tokens, cache_read, input_tokens + (cache_read or 0) + (cache_write or 0)


def _pi_cost(usage: dict[str, Any]) -> float | None:
    cost = usage.get("cost")
    if isinstance(cost, dict):
        return _float(cost.get("total"))
    return _float(cost)


def _add_tool(
    row: dict[str, Any], name: str, text: str, arguments: dict[str, Any]
) -> None:
    """Add one settled tool call, its result bytes and, for `read`, its range bucket."""
    row["tool_calls"][name] = row["tool_calls"].get(name, 0) + 1
    size = len(text.encode("utf-8"))
    row["tool_bytes"][name] = row["tool_bytes"].get(name, 0) + size
    if name == "read":
        if _read_is_ranged(arguments):
            row["read_ranged_calls"] += 1
            row["read_ranged_bytes"] += size
        else:
            row["read_unranged_calls"] += 1
            row["read_unranged_bytes"] += size
        if size >= LARGE_READ_BYTES:
            row["read_large_calls"] += 1


def _facts(path: Path) -> dict[str, Any]:
    """Every field for one journal, plus the private aggregates the old report needs."""
    row: dict[str, Any] = {
        "journal": str(path),
        "source": None,
        "calls": 0,
        "input_uncached": None,
        "input_cache_read": None,
        "input_total": None,
        "output": None,
        "reasoning": None,
        "fee_usd": None,
        "tool_calls": {},
        "tool_bytes": {},
        "read_ranged_calls": 0,
        "read_ranged_bytes": 0,
        "read_unranged_calls": 0,
        "read_unranged_bytes": 0,
        "read_large_calls": 0,
        "compactions": 0,
        "deterministic_compactions": 0,
        "outcomes": {},
        "wall_seconds": None,
        # Latency samples stay a list until the row is final; the public field is
        # `latency` ({bucket: {calls, p50_ms, p90_ms}}).
        "_latencies": {},
        "_latency_anchor": None,
        "_input_total": None,
        "_fee_micros": None,
        "_anchor": None,
        "_pending": {},
        "_tool_starts": {},
        "_generation_seconds": 0.0,
        "_tool_seconds": 0.0,
        "_over_16k_calls": 0,
        "_max_tokens": False,
        "_first_ts": None,
        "_last_ts": None,
        "_pi_stop_reason": None,
    }
    try:
        for line in _iter_lines(path):
            if not line.endswith("\n"):
                break  # torn tail: not a committed record
            if not line.strip():
                continue
            try:
                record = json.loads(line)
            except ValueError:
                continue
            if not isinstance(record, dict):
                continue
            if "event" in record:
                row["source"] = row["source"] or "radian"
                _radian_record(row, record)
            elif record.get("type") == "message":
                row["source"] = row["source"] or "pi"
                _pi_message(row, record)
            elif record.get("type") == "compaction":
                row["source"] = row["source"] or "pi"
                # A pi compaction is a committed record with a timestamp: when it
                # bounds the session it bounds `wall_seconds` too.
                _note_time(row, _time(record.get("timestamp")))
                row["compactions"] += 1
                if record.get("deterministic") is True:
                    row["deterministic_compactions"] += 1
    except OSError:
        pass
    _finish_inputs(row)
    if row["_first_ts"] is not None and row["_last_ts"] is not None:
        row["wall_seconds"] = _seconds(row["_first_ts"], row["_last_ts"])
    if row["source"] == "pi" and row["_pi_stop_reason"] is not None:
        outcome = PI_OUTCOMES.get(row["_pi_stop_reason"], row["_pi_stop_reason"])
        row["outcomes"][outcome] = row["outcomes"].get(outcome, 0) + 1
    row["latency"] = {
        _bucket_label(bucket): {
            "calls": len(samples),
            "p50_ms": (_percentile(samples, 50) or 0.0) * 1000.0,
            "p90_ms": (_percentile(samples, 90) or 0.0) * 1000.0,
        }
        for bucket, samples in sorted(row["_latencies"].items())
    }
    return row


def _note_time(row: dict[str, Any], ts: datetime | None) -> None:
    if ts is None:
        return
    if row["_first_ts"] is None:
        row["_first_ts"] = ts
    row["_last_ts"] = ts


def _usage(
    row: dict[str, Any],
    usage: dict[str, Any],
    ts: datetime | None,
    inputs: tuple[int | None, int | None, int | None],
    fee_usd: float | None = None,
    fee_micros: int | None = None,
) -> None:
    """Add one usage record: the two input columns, output, reasoning, fee, latency."""
    uncached, cache_read, total = inputs
    row["input_uncached"] = _add(row["input_uncached"], uncached)
    row["input_cache_read"] = _add(row["input_cache_read"], cache_read)
    row["_input_total"] = _add(row["_input_total"], total)
    output = _int(usage.get("output"))
    if output is not None:
        row["output"] = _add(row["output"], output)
        if output > OVER_16K_TOKENS:
            row["_over_16k_calls"] += 1
    reasoning = _reasoning(usage.get("details"))
    if reasoning is None:
        reasoning = _reasoning(usage)
    if reasoning is not None:
        row["reasoning"] = _add(row["reasoning"], reasoning)
    if fee_micros is not None:
        row["_fee_micros"] = _add(row["_fee_micros"], fee_micros)
    if fee_usd is not None:
        row["fee_usd"] = _add(row["fee_usd"], fee_usd)
    elapsed = _seconds(row["_latency_anchor"], ts)
    if elapsed is not None and total is not None:
        bucket = total // LATENCY_BUCKET_TOKENS
        row["_latencies"].setdefault(bucket, []).append(elapsed)
    # The response this usage describes is the ready event for the next call.
    if ts is not None:
        row["_latency_anchor"] = ts


def _radian_record(row: dict[str, Any], record: dict[str, Any]) -> None:
    event = record.get("event")
    data = _dict(record.get("data"))
    ts = _time(record.get("ts"))
    _note_time(row, ts)
    if event == "ToolExecutionStarted":
        call_id = data.get("call_id")
        name = data.get("name")
        if isinstance(call_id, str) and isinstance(name, str):
            row["_pending"][call_id] = (name, _arguments(data.get("arguments")))
            if ts is not None:
                row["_tool_starts"][call_id] = ts
    elif event == "ToolExecutionSettled":
        call_id = data.get("call_id")
        name = None
        arguments: dict[str, Any] = {}
        if isinstance(call_id, str):
            name, arguments = row["_pending"].pop(call_id, (None, {}))
        if isinstance(name, str):
            _add_tool(row, name, _tool_result_text(data.get("result")), arguments)
        elapsed = _seconds(
            row["_tool_starts"].pop(call_id, None) if isinstance(call_id, str) else None,
            ts,
        )
        if elapsed is not None:
            row["_tool_seconds"] += elapsed
        if ts is not None:
            row["_anchor"] = ts
            row["_latency_anchor"] = ts
    elif event == "MessageAppended":
        message = _dict(data.get("message"))
        if "assistant" in message:
            row["calls"] += 1
            elapsed = _seconds(row["_anchor"], ts)
            if elapsed is not None:
                row["_generation_seconds"] += elapsed
            if ts is not None:
                row["_anchor"] = ts
        elif "user" in message and ts is not None:
            row["_anchor"] = ts
            row["_latency_anchor"] = ts
    elif event == "TurnStarted":
        if ts is not None:
            row["_anchor"] = ts
            row["_latency_anchor"] = ts
    elif event in ("ModelUsageRecorded", "CompactionUsageRecorded"):
        usage = _dict(data.get("usage"))
        _usage(
            row,
            usage,
            ts,
            _radian_inputs(usage),
            fee_micros=_int(_dict(data.get("fee")).get("micros")),
        )
        if event == "CompactionUsageRecorded":
            row["calls"] += 1
            elapsed = _seconds(row["_anchor"], ts)
            if elapsed is not None:
                row["_generation_seconds"] += elapsed
            if ts is not None:
                row["_anchor"] = ts
    elif event == "TurnFinished":
        outcome = data.get("outcome")
        if isinstance(outcome, str):
            row["outcomes"][outcome] = row["outcomes"].get(outcome, 0) + 1
        if data.get("outcome") == "incomplete" and data.get("reason") == "max_tokens":
            row["_max_tokens"] = True
    elif event == "ContextCompacted":
        row["compactions"] += 1
        if data.get("deterministic") is True:
            row["deterministic_compactions"] += 1


def _pi_message(row: dict[str, Any], record: dict[str, Any]) -> None:
    message = _dict(record.get("message"))
    role = message.get("role")
    ts = _time(record.get("timestamp")) or _time(message.get("timestamp"))
    _note_time(row, ts)
    if role == "user":
        if ts is not None:
            row["_anchor"] = ts
            row["_latency_anchor"] = ts
    elif role == "assistant":
        row["calls"] += 1
        usage = _dict(message.get("usage"))
        if usage:
            _usage(
                row,
                usage,
                ts,
                _pi_inputs(usage),
                fee_usd=_pi_cost(usage),
            )
        for part in message.get("content") or []:
            if not isinstance(part, dict) or part.get("type") != "toolCall":
                continue
            call_id = part.get("id")
            name = part.get("name")
            if isinstance(call_id, str) and isinstance(name, str):
                row["_pending"][call_id] = (name, _arguments(part.get("arguments")))
        stop = message.get("stopReason")
        if isinstance(stop, str):
            row["_pi_stop_reason"] = stop
        if ts is not None:
            row["_anchor"] = ts
            row["_latency_anchor"] = ts
    elif role == "toolResult":
        call_id = message.get("toolCallId")
        name = message.get("toolName")
        arguments: dict[str, Any] = {}
        if isinstance(call_id, str):
            pending_name, arguments = row["_pending"].pop(call_id, (None, {}))
            if not isinstance(name, str):
                name = pending_name
        if isinstance(name, str):
            name = PI_TOOL_NAMES.get(name, name)
            _add_tool(row, name, _content_text(message.get("content")), arguments)
        if ts is not None:
            row["_anchor"] = ts
            row["_latency_anchor"] = ts


def _finish_inputs(row: dict[str, Any]) -> None:
    """Publish the per-record prompt total and fold in a radian fee."""
    if "_input_total" in row:
        row["input_total"] = row["_input_total"]
    if isinstance(row.get("_fee_micros"), int):
        row["fee_usd"] = _add(row["fee_usd"], row["_fee_micros"] / 1_000_000)


def _bucket_label(bucket: int) -> str:
    low = bucket * LATENCY_BUCKET_TOKENS
    return f"{low}-{low + LATENCY_BUCKET_TOKENS - 1}"


def public_row(facts: dict[str, Any]) -> dict[str, Any]:
    """The row as the table and JSON print it (private aggregates removed)."""
    return {key: value for key, value in facts.items() if not key.startswith("_")}


def _totals(facts_rows: list[dict[str, Any]]) -> dict[str, Any]:
    totals: dict[str, Any] = {
        "journal": "TOTAL",
        "source": None,
        "calls": 0,
        "input_uncached": None,
        "input_cache_read": None,
        "input_total": None,
        "output": None,
        "reasoning": None,
        "fee_usd": None,
        "tool_calls": {},
        "tool_bytes": {},
        "read_ranged_calls": 0,
        "read_ranged_bytes": 0,
        "read_unranged_calls": 0,
        "read_unranged_bytes": 0,
        "read_large_calls": 0,
        "compactions": 0,
        "deterministic_compactions": 0,
        "outcomes": {},
        "wall_seconds": None,
        "latency": {},
    }
    pooled: dict[int, list[float]] = {}
    for facts in facts_rows:
        for key in (
            "calls",
            "input_uncached",
            "input_cache_read",
            "input_total",
            "output",
            "reasoning",
            "fee_usd",
            "wall_seconds",
            "read_ranged_calls",
            "read_ranged_bytes",
            "read_unranged_calls",
            "read_unranged_bytes",
            "read_large_calls",
            "compactions",
            "deterministic_compactions",
        ):
            totals[key] = _add(totals[key], facts[key])
        for name, count in facts["tool_calls"].items():
            totals["tool_calls"][name] = totals["tool_calls"].get(name, 0) + count
        for name, size in facts["tool_bytes"].items():
            totals["tool_bytes"][name] = totals["tool_bytes"].get(name, 0) + size
        for outcome, count in facts["outcomes"].items():
            totals["outcomes"][outcome] = totals["outcomes"].get(outcome, 0) + count
        for bucket, samples in facts["_latencies"].items():
            pooled.setdefault(bucket, []).extend(samples)
    _finish_inputs(totals)
    totals["latency"] = {
        _bucket_label(bucket): {
            "calls": len(samples),
            "p50_ms": (_percentile(samples, 50) or 0.0) * 1000.0,
            "p90_ms": (_percentile(samples, 90) or 0.0) * 1000.0,
        }
        for bucket, samples in sorted(pooled.items())
    }
    return totals


def collect_files(
    inputs: list[str | Path], prefix: str | None = None, recursive: bool = True
) -> list[Path]:
    """Every journal file named by the inputs, sorted and de-duplicated.

    A directory contributes its `*.jsonl` files recursively by default. With
    `recursive=False` only the directory's own files are read, which is what the
    Harbor report does: a nested JSONL artifact in a session directory is not a
    session journal.
    """
    files: list[Path] = []
    seen: set[str] = set()
    for raw in map(Path, inputs):
        if raw.is_dir():
            candidates = sorted(raw.rglob("*.jsonl") if recursive else raw.glob("*.jsonl"))
        else:
            candidates = [raw]
        for path in candidates:
            if prefix is not None and not path.name.startswith(prefix):
                continue
            key = str(path.resolve())
            if key in seen:
                continue
            seen.add(key)
            files.append(path)
    files.sort(key=str)
    return files


def _inputs_without_journals(
    inputs: list[str | Path], prefix: str | None = None
) -> list[Path]:
    """The inputs that name no journal: a missing path, or a directory with no match.

    `build_report` still returns no rows for such an input, so a caller that
    wants an empty report (the Harbor summary) can ask for one. The command line
    uses this to refuse a typo instead of printing a row of zeros for it.
    """
    unmatched: list[Path] = []
    for raw in map(Path, inputs):
        if not raw.exists():
            unmatched.append(raw)
        elif raw.is_dir() and not any(
            prefix is None or path.name.startswith(prefix)
            for path in raw.rglob("*.jsonl")
        ):
            unmatched.append(raw)
    return unmatched


def build_report(
    inputs: list[str | Path], prefix: str | None = None, recursive: bool = True
) -> dict[str, Any]:
    """The computed rows: one per journal plus the totals row. One source for both outputs."""
    facts_rows = [_facts(path) for path in collect_files(inputs, prefix, recursive)]
    return {
        "journals": [public_row(facts) for facts in facts_rows],
        "totals": _totals(facts_rows),
    }


def usage_metrics(
    inputs: list[str | Path], prefix: str | None = None, recursive: bool = True
) -> dict[str, Any]:
    """The Harbor report's journal summary, kept byte-for-byte compatible.

    `terminal-bench/metrics.py` returns exactly these keys and values. Generation
    minutes run from the request boundary to the assistant response; tool
    minutes sum each `ToolExecutionStarted` -> `ToolExecutionSettled` pair.
    """
    calls = 0
    output = reasoning = None
    over_16k = 0
    generation = tool = 0.0
    fee_micros = None
    max_tokens = False
    for path in collect_files(inputs, prefix, recursive):
        facts = _facts(path)
        calls += facts["calls"]
        output = _add(output, facts["output"])
        reasoning = _add(reasoning, facts["reasoning"])
        over_16k += facts["_over_16k_calls"]
        generation += facts["_generation_seconds"]
        tool += facts["_tool_seconds"]
        fee_micros = _add(fee_micros, facts["_fee_micros"])
        max_tokens = max_tokens or facts["_max_tokens"]
    return {
        "model_calls": calls,
        "output_tokens": output,
        "generated_tokens": output,
        "reasoning_tokens": reasoning,
        "over_16k_calls": over_16k,
        "generation_minutes": generation / 60.0,
        "tool_minutes": tool / 60.0,
        "cost_usd": fee_micros / 1_000_000 if fee_micros is not None else None,
        "max_tokens": max_tokens,
    }


def columns_text() -> str:
    """The fixed column table as help text: name, definition, unit."""
    lines = []
    for column in COLUMNS:
        lines.append(f"{column['name']}  [{column['unit']}]")
        lines.append(f"    {column['definition']}")
    return "\n".join(lines)


def columns_markdown() -> str:
    """The fixed column table as a Markdown table, for the generated README block."""
    lines = ["| Column | Definition | Unit |", "|---|---|---|"]
    for column in COLUMNS:
        lines.append(
            f"| `{column['name']}` | {column['definition']} | {column['unit']} |"
        )
    return "\n".join(lines)


def _table(headers: list[str], rows: list[list[str]]) -> str:
    widths = [
        max(len(header), *(len(row[index]) for row in rows))
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


def _show(value: Any, spec: str = "") -> str:
    return "-" if value is None else format(value, spec)


def _count(value: Any) -> str:
    if value is None:
        return "-"
    if isinstance(value, float) and not value.is_integer():
        return f"{value:,.1f}"
    return f"{int(value):,d}"


def _number(value: Any, spec: str) -> str:
    return "-" if value is None else format(value, spec)


def _outcomes_text(outcomes: dict[str, int]) -> str:
    if not outcomes:
        return "-"
    return " ".join(f"{name}={count}" for name, count in sorted(outcomes.items()))


def _tool_columns(report: dict[str, Any]) -> list[str]:
    names = set()
    for row in [*report["journals"], report["totals"]]:
        names.update(row["tool_calls"])
        names.update(row["tool_bytes"])
    columns = []
    for name in sorted(names):
        columns.extend([f"calls.{name}", f"bytes.{name}"])
    return columns


def _buckets(report: dict[str, Any]) -> list[str]:
    buckets = set()
    for row in [*report["journals"], report["totals"]]:
        buckets.update(row["latency"])
    return sorted(buckets, key=lambda label: int(label.split("-", 1)[0]))


def format_report(report: dict[str, Any]) -> str:
    """The text table: one row per journal plus the totals row, same rows as JSON."""
    rows = [*report["journals"], report["totals"]]
    tool_columns = _tool_columns(report)
    buckets = _buckets(report)
    # Each header is a `COLUMNS` name (with `<tool>` and `<bucket>` filled in),
    # so the table and the generated definition table name the same columns.
    headers = [
        "journal",
        "source",
        "calls",
        "input_uncached",
        "input_cache_read",
        "input_total",
        "output",
        "reasoning",
        "fee_usd",
        *tool_columns,
        "read_ranged_calls",
        "read_ranged_bytes",
        "read_unranged_calls",
        "read_unranged_bytes",
        "read_large_calls",
        *(f"p50ms.{bucket}" for bucket in buckets),
        *(f"p90ms.{bucket}" for bucket in buckets),
        "compactions",
        "deterministic_compactions",
        "outcomes",
        "wall_seconds",
    ]
    cells: list[list[str]] = []
    for row in rows:
        values = [
            row["journal"],
            row["source"] or "-",
            _count(row["calls"]),
            _count(row["input_uncached"]),
            _count(row["input_cache_read"]),
            _count(row["input_total"]),
            _count(row["output"]),
            _count(row["reasoning"]),
            _number(row["fee_usd"], ".6f"),
        ]
        for column in tool_columns:
            kind, name = column.split(".", 1)
            source = row["tool_calls" if kind == "calls" else "tool_bytes"]
            values.append(_count(source.get(name)))
        values.extend(
            [
                _count(row["read_ranged_calls"]),
                _count(row["read_ranged_bytes"]),
                _count(row["read_unranged_calls"]),
                _count(row["read_unranged_bytes"]),
                _count(row["read_large_calls"]),
            ]
        )
        for bucket in buckets:
            samples = row["latency"].get(bucket)
            values.append(_number(samples["p50_ms"] if samples else None, ".0f"))
        for bucket in buckets:
            samples = row["latency"].get(bucket)
            values.append(_number(samples["p90_ms"] if samples else None, ".0f"))
        values.extend(
            [
                _count(row["compactions"]),
                _count(row["deterministic_compactions"]),
                _outcomes_text(row["outcomes"]),
                _number(row["wall_seconds"], ".3f"),
            ]
        )
        cells.append(values)
    return _table(headers, cells)


def _main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Fixed-definition metrics from radian journals and pi sessions.",
        epilog="Column table:\n" + columns_text(),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("journals", nargs="+", metavar="JOURNAL")
    parser.add_argument(
        "--prefix",
        metavar="PREFIX",
        help="only read files whose name starts with PREFIX",
    )
    parser.add_argument("--json", action="store_true", help="print the rows as JSON")
    args = parser.parse_args(argv)
    unmatched = _inputs_without_journals(args.journals, args.prefix)
    if unmatched:
        for path in unmatched:
            if path.is_dir():
                detail = "no *.jsonl"
                if args.prefix is not None:
                    detail += f" named {args.prefix}*"
                print(f"no journal file under: {path} ({detail})", file=sys.stderr)
            else:
                print(f"no journal file at: {path}", file=sys.stderr)
        return 1
    report = build_report(args.journals, prefix=args.prefix)
    if not report["journals"]:
        print(
            "no journal file found under: " + ", ".join(args.journals),
            file=sys.stderr,
        )
        return 1
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(format_report(report))
    return 0


if __name__ == "__main__":
    sys.exit(_main())
