#!/usr/bin/env python3
"""Measure parallel-tool-execution headroom from radian session journals.

Reads one or more radian session JSONL files (schema v1; see docs/adr/0006 and
docs/adr/0002) given as command-line arguments, and prints:

  1. how many assistant responses carry 1 / 2 / 3+ tool calls;
  2. of the responses with 2+ calls, how many contain an Exclusive tool and how
     many are all-Shared;
  3. wall time of each tool execution (`ToolExecutionStarted.ts` to the matching
     `ToolExecutionSettled.ts`) per tool name: count, total, median, p90, max;
  4. for 2+ responses, the serial time (sum of the calls' executions) versus the
     lower bound if all Shared calls overlapped and Exclusive calls stayed serial
     (max of the Shared + sum of the Exclusive), and the saving as a share of the
     session's wall time (first to last record);
  5. model time between responses, to put tool time in proportion.

Also prints sanity cross-checks: matched Started/Settled pairs, unmatched markers,
and independent line counts (a plain substring scan, not JSON parsing).

Stdlib only; runs under `python3 -I`. Time is parsed with `datetime`; `ts` is
RFC 3339 UTC (ADR 0006 section 3), so ordering by `ts` is safe here because every
record in these journals is written in order and clock monotonicity is not relied
on beyond the durations.

NOT measurable from a journal: when a tool call first appeared in the model
stream, so any saving from dispatching a call before the whole response arrives
is out of scope (stream deltas are live-only, ADR 0010).
"""

import datetime
import json
import math
import statistics
import sys

# ---------------------------------------------------------------------------
# Tool concurrency classes, taken from each built-in ToolSpec in the source
# (not from memory). `name -> "Shared" | "Exclusive"`.
#
#   crates/radian-coding/src/read.rs:60        read            Shared
#   crates/radian-coding/src/list.rs:38        list            Shared
#   crates/radian-coding/src/git.rs:22         git_status      Shared
#   crates/radian-coding/src/git.rs:46         git_diff        Shared
#   crates/radian-coding/src/jobs.rs:777       job_output      Shared
#   crates/radian-coding/src/jobs.rs:795       job_list        Shared
#   crates/radian-coding/src/jobs.rs:818       job_kill        Shared
#   crates/radian-coding/src/write.rs:39       write           Exclusive
#   crates/radian-coding/src/patch.rs:45       patch           Exclusive
#   crates/radian-coding/src/shell.rs:144      shell           Exclusive
#   crates/radian-coding/src/ask.rs:397        ask_user        Exclusive
#   crates/radian-host/src/git_commit.rs:81    git_commit      Exclusive
#   crates/radian-host/src/skills/tools.rs:185 skill           Shared
#   crates/radian-host/src/mermaid.rs:181      mermaid_check   Shared
#   crates/radian-host/src/memory/tools.rs:372 memory_search   Shared
#   crates/radian-host/src/memory/tools.rs:388 memory_get      Shared
#   crates/radian-host/src/memory/tools.rs:424 memory_save     Exclusive
#   crates/radian-host/src/memory/tools.rs:453 memory_revise   Exclusive
#
# MCP tools (aliased `<server>__<tool>`) are config-driven, not fixed:
# crates/agent-extensions/src/session.rs:508-521 makes a tool Shared only when
# it is listed in `shared_tools`, or (`trust_read_only_hint` and the server's own
# readOnlyHint is true); otherwise Exclusive. A name that is not a built-in is
# therefore classed: Shared if it matches a configured MCP shared tool, else
# Exclusive (the safe default). No MCP tool ran in these journals, so the table
# below covers every executed name.
# ---------------------------------------------------------------------------
BUILT_IN = {
    "read": "Shared",
    "list": "Shared",
    "git_status": "Shared",
    "git_diff": "Shared",
    "job_output": "Shared",
    "job_list": "Shared",
    "job_kill": "Shared",
    "write": "Exclusive",
    "patch": "Exclusive",
    "shell": "Exclusive",
    "ask_user": "Exclusive",
    "git_commit": "Exclusive",
    "skill": "Shared",
    "mermaid_check": "Shared",
    "memory_search": "Shared",
    "memory_get": "Shared",
    "memory_save": "Exclusive",
    "memory_revise": "Exclusive",
}


def classify(name):
    """Concurrency class for a tool name.

    Unknown names (e.g. an MCP tool that is not in `BUILT_IN`) default to
    Exclusive, which is `ToolConcurrency`'s `#[default]` and the conservative
    choice: it never overstates how much could overlap.
    """
    return BUILT_IN.get(name, "Exclusive")


def parse_ts(ts):
    return datetime.datetime.fromisoformat(ts.replace("Z", "+00:00"))


def read_records(path):
    """Parse a session JSONL file, dropping a torn tail (ADR 0006 section 3.2)."""
    with open(path, "r", encoding="utf-8") as handle:
        text = handle.read()
    if text and not text.endswith("\n"):
        # No LF means the last bytes are a torn tail: not a committed record.
        text = text[: text.rfind("\n") + 1] if "\n" in text else ""
    records = []
    for line in text.split("\n"):
        if not line:
            continue
        records.append(json.loads(line))
    return records


def raw_count(path, needle):
    """Independent cross-check: count lines containing `needle` without parsing."""
    with open(path, "r", encoding="utf-8") as handle:
        return sum(1 for line in handle if needle in line)


def p90(values):
    """Nearest-rank 90th percentile."""
    if not values:
        return None
    ordered = sorted(values)
    rank = max(0, math.ceil(0.9 * len(ordered)) - 1)
    return ordered[rank]


def analyze(path):
    records = read_records(path)
    started = {}  # call_id -> (name, ts)
    settled = {}  # call_id -> ts
    responses = []  # {ts, calls: [(call_id, name)]}

    total_tool = 0.0
    total_model = 0.0
    model_calls = 0
    anchor = None
    last_ts = None
    first_ts = None
    for rec in records:
        event = rec["event"]
        data = rec["data"]
        ts = parse_ts(rec["ts"])
        if first_ts is None:
            first_ts = ts
        last_ts = ts
        if event == "ToolExecutionStarted":
            started[data["call_id"]] = (data["name"], ts)
        elif event == "ToolExecutionSettled":
            settled[data["call_id"]] = ts
            if data["call_id"] in started:
                total_tool += (ts - started[data["call_id"]][1]).total_seconds()
            anchor = ts
        elif event == "MessageAppended":
            message = data["message"]
            if "assistant" in message:
                calls = message["assistant"].get("tool_calls") or []
                model_calls += 1
                if anchor is not None:
                    total_model += (ts - anchor).total_seconds()
                # An assistant message is the previous anchor for the next one,
                # whether or not it carried calls.
                anchor = ts
                if calls:
                    responses.append(
                        {
                            "ts": ts,
                            "calls": [(c["id"], c["name"]) for c in calls],
                        }
                    )
            elif "user" in message:
                anchor = ts
        elif event == "TurnStarted":
            anchor = ts

    # ---- 1. call-count histogram ------------------------------------------
    histogram = {1: 0, 2: 0, 3: 0}  # 3 means "3+"
    multi = {"has_exclusive": 0, "all_shared": 0, "empty": 0}
    serial = 0.0
    ideal = 0.0
    multi_batches = 0
    multi_calls_unmeasured = 0
    for resp in responses:
        n = len(resp["calls"])
        histogram[min(n, 3)] += 1
        if n < 2:
            continue
        multi_batches += 1
        classes = []
        shared = []
        exclusive = []
        for call_id, name in resp["calls"]:
            cls = classify(name)
            classes.append(cls)
            if call_id in started and call_id in settled:
                duration = (settled[call_id] - started[call_id][1]).total_seconds()
                if cls == "Shared":
                    shared.append(duration)
                else:
                    exclusive.append(duration)
            else:
                multi_calls_unmeasured += 1
        if any(c == "Exclusive" for c in classes):
            multi["has_exclusive"] += 1
        elif classes and all(c == "Shared" for c in classes):
            multi["all_shared"] += 1
        else:
            multi["empty"] += 1
        # serial: every call one after another. ideal: all Shared overlap, all
        # Exclusive serial. Only calls that actually executed count.
        serial += sum(shared) + sum(exclusive)
        ideal += (max(shared) if shared else 0.0) + sum(exclusive)

    # ---- 3. per-tool wall time --------------------------------------------
    by_name = {}
    for call_id, (name, t0) in started.items():
        if call_id not in settled:
            continue
        by_name.setdefault(name, []).append((settled[call_id] - t0).total_seconds())

    wall = (last_ts - first_ts).total_seconds() if first_ts is not None else 0.0
    return {
        "path": path,
        "records": len(records),
        "wall": wall,
        "histogram": histogram,
        "responses_with_calls": len(responses),
        "tool_calls": sum(len(r["calls"]) for r in responses),
        "multi": multi,
        "multi_batches": multi_batches,
        "multi_calls_unmeasured": multi_calls_unmeasured,
        "by_name": by_name,
        "serial": serial,
        "ideal": ideal,
        "saving": serial - ideal,
        "total_tool": total_tool,
        "total_model": total_model,
        "model_calls": model_calls,
        # marker sanity, resolved once here so the total can sum it
        "started_records": len(started),
        "settled_records": len(settled),
        "matched": sum(1 for cid in started if cid in settled),
        "raw_started": raw_count(path, '"event":"ToolExecutionStarted"'),
        "raw_settled": raw_count(path, '"event":"ToolExecutionSettled"'),
    }


def stat_line(name, durations):
    return "  %-14s %5d %10.2f %9.3f %9.3f %10.3f" % (
        name,
        len(durations),
        sum(durations),
        statistics.median(durations),
        p90(durations),
        max(durations),
    )


def print_report(result):
    print("=" * 78)
    print(result["path"])
    print("=" * 78)
    print(
        "records=%d  wall=%.1fs  assistant_model_calls=%d  responses_with_tool_calls=%d  tool_calls=%d"
        % (
            result["records"],
            result["wall"],
            result["model_calls"],
            result["responses_with_calls"],
            result["tool_calls"],
        )
    )
    hist = result["histogram"]
    print(
        "responses by call count:  1=%d  2=%d  3+=%d"
        % (hist[1], hist[2], hist[3])
    )
    multi = result["multi"]
    print(
        "2+ responses: total=%d  has_exclusive=%d  all_shared=%d  (calls without "
        "a measured execution: %d)"
        % (
            result["multi_batches"],
            multi["has_exclusive"],
            multi["all_shared"],
            result["multi_calls_unmeasured"],
        )
    )
    print()
    print("tool execution wall time (Started.ts -> matching Settled.ts):")
    print(
        "  %-14s %5s %10s %9s %9s %10s"
        % ("name", "count", "total_s", "median_s", "p90_s", "max_s")
    )
    all_durations = []
    for name in sorted(result["by_name"]):
        durations = result["by_name"][name]
        all_durations.extend(durations)
        print(stat_line(name, durations))
    if all_durations:
        print(stat_line("(all tools)", all_durations))

    print()
    print("overlap of Shared calls in 2+ responses:")
    print(
        "  serial_s=%.2f  overlapped_lower_bound_s=%.2f  saving_s=%.2f  "
        "share_of_wall=%.4f%%"
        % (
            result["serial"],
            result["ideal"],
            result["saving"],
            100.0 * result["saving"] / result["wall"] if result["wall"] else 0.0,
        )
    )
    print()
    wall = result["wall"] or 1.0
    print("time proportions (share of session wall):")
    print(
        "  tool_exec=%.1fs (%.1f%%)  model=%.1fs (%.1f%%)  other=%.1fs (%.1f%%)"
        % (
            result["total_tool"],
            100.0 * result["total_tool"] / wall,
            result["total_model"],
            100.0 * result["total_model"] / wall,
            max(0.0, wall - result["total_tool"] - result["total_model"]),
            100.0 * max(0.0, wall - result["total_tool"] - result["total_model"]) / wall,
        )
    )
    print()
    print("sanity:")
    print(
        "  started_records=%d  settled_records=%d  matched=%d  started_without_settled=%d  "
        "settled_without_started=%d"
        % (
            result["started_records"],
            result["settled_records"],
            result["matched"],
            result["started_records"] - result["matched"],
            result["settled_records"] - result["matched"],
        )
    )
    print(
        "  grep cross-check (substring, no JSON parse): "
        "ToolExecutionStarted=%d  ToolExecutionSettled=%d"
        % (result["raw_started"], result["raw_settled"])
    )
    print()


def combined(results):
    total = {
        "path": "TOTAL (all journals)",
        "records": sum(r["records"] for r in results),
        "wall": sum(r["wall"] for r in results),
        "responses_with_calls": sum(r["responses_with_calls"] for r in results),
        "tool_calls": sum(r["tool_calls"] for r in results),
        "histogram": {1: 0, 2: 0, 3: 0},
        "multi": {"has_exclusive": 0, "all_shared": 0, "empty": 0},
        "multi_batches": sum(r["multi_batches"] for r in results),
        "multi_calls_unmeasured": sum(r["multi_calls_unmeasured"] for r in results),
        "by_name": {},
        "serial": sum(r["serial"] for r in results),
        "ideal": sum(r["ideal"] for r in results),
        "saving": sum(r["saving"] for r in results),
        "total_tool": sum(r["total_tool"] for r in results),
        "total_model": sum(r["total_model"] for r in results),
        "model_calls": sum(r["model_calls"] for r in results),
        "started_records": sum(r["started_records"] for r in results),
        "settled_records": sum(r["settled_records"] for r in results),
        "matched": sum(r["matched"] for r in results),
        "raw_started": sum(r["raw_started"] for r in results),
        "raw_settled": sum(r["raw_settled"] for r in results),
    }
    for r in results:
        for key in total["histogram"]:
            total["histogram"][key] += r["histogram"][key]
        for key in total["multi"]:
            total["multi"][key] += r["multi"][key]
    # per tool name, concatenate durations (a total/median/p90 over all journals)
    for r in results:
        for name, durations in r["by_name"].items():
            total["by_name"].setdefault(name, []).extend(durations)
    # markers cannot be merged by call_id across journals (ids are unique, but
    # the sanity line is per-file anyway); leave them empty and print totals only.
    return total


def main(argv):
    if len(argv) < 2:
        print("usage: measure-tool-concurrency.py JOURNAL.jsonl [JOURNAL.jsonl ...]",
              file=sys.stderr)
        return 2
    results = []
    for path in argv[1:]:
        result = analyze(path)
        print_report(result)
        results.append(result)
    if len(results) > 1:
        total = combined(results)
        print("#" * 78)
        print("# TOTAL ACROSS %d JOURNALS" % len(results))
        print("#" * 78)
        print_report(total)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
