# Tool concurrency measurement: how much would parallel tool execution save?

Date: 2026-10-15 · Worktree `oc/tool-measure` · Measurement only, no Rust change.

## Question

radian waits for the whole model response before it dispatches any tool, and then runs
`Exclusive` tools one at a time (`crates/agent-core/src/turn.rs`, `BatchCx::schedule`,
about line 1629). Consecutive `Shared` calls in one response already overlap; an
`Exclusive` call waits for everything before it and runs alone. Changing this (for example
starting a tool before the response finishes, or overlapping `Shared` calls across an
`Exclusive`) needs an ADR 0002 revision, because the execution markers and recovery assume
tools start after the response is journaled. Before deciding, this measures the headroom
that is visible in the journal.

Script: `scripts/measure-tool-concurrency.py` (stdlib only, `python3 -I`). Run:

```
python3 -I scripts/measure-tool-concurrency.py \
    .radian/data/ab2-radian.jsonl \
    .radian/data/ab3-radian.jsonl \
    .radian/data/t4-bar-input.jsonl
```

## Data

Three real radian session journals (schema v1, ADR 0006), copies under `.radian/data/`:

| journal | records | window (first -> last `ts`) | wall |
|---|---|---|---|
| `ab2-radian.jsonl` | 525 | 2026-10-15T17:29:48Z -> 18:21:26Z | 3098.1 s |
| `ab3-radian.jsonl` | 545 | 2026-10-15T18:59:22Z -> 20:06:46Z | 4044.1 s |
| `t4-bar-input.jsonl` | 1795 | 2026-10-15T16:56:09Z -> 19:05:00Z | 7731.2 s |

`ab3` has 173 `MessageAppended`, 93 `ToolExecutionStarted`/`ToolExecutionSettled` pairs and
78 `ModelUsageRecorded`, as the task states. The script confirms 93 matched pairs.

## Method

Record shapes come from ADR 0006 section 5 and ADR 0002 (not guessed):

- A model response that asked for tools is a `MessageAppended` whose `message` has an
  `assistant` variant with a non-empty `tool_calls` array; each element is
  `{id, name, arguments}`.
- An execution of a call is `ToolExecutionStarted` (with `call_id`, `name`) followed by the
  matching `ToolExecutionSettled` (same `call_id`). Its wall time is
  `Settled.ts - Started.ts`. `Started` is journaled before the tool body runs, so the pair
  brackets the tool's own execution (including the `Settled` fsync, ~4 ms).
- `ts` is RFC 3339 UTC; the script parses it with `datetime` and subtracts.

### Concurrency class table used

Taken from each built-in `ToolSpec` in the source (file and line in the script header), not
from memory:

| tool | class | source |
|---|---|---|
| `read` | Shared | `crates/radian-coding/src/read.rs:60` |
| `list` | Shared | `crates/radian-coding/src/list.rs:38` |
| `git_status` | Shared | `crates/radian-coding/src/git.rs:22` |
| `git_diff` | Shared | `crates/radian-coding/src/git.rs:46` |
| `job_output` | Shared | `crates/radian-coding/src/jobs.rs:777` |
| `job_list` | Shared | `crates/radian-coding/src/jobs.rs:795` |
| `job_kill` | Shared | `crates/radian-coding/src/jobs.rs:818` |
| `write` | Exclusive | `crates/radian-coding/src/write.rs:39` |
| `patch` | Exclusive | `crates/radian-coding/src/patch.rs:45` |
| `shell` | Exclusive | `crates/radian-coding/src/shell.rs:144` |
| `ask_user` | Exclusive | `crates/radian-coding/src/ask.rs:397` |
| `git_commit` | Exclusive | `crates/radian-host/src/git_commit.rs:81` |
| `skill` | Shared | `crates/radian-host/src/skills/tools.rs:185` |
| `mermaid_check` | Shared | `crates/radian-host/src/mermaid.rs:181` |
| `memory_search` | Shared | `crates/radian-host/src/memory/tools.rs:372` |
| `memory_get` | Shared | `crates/radian-host/src/memory/tools.rs:388` |
| `memory_save` | Exclusive | `crates/radian-host/src/memory/tools.rs:424` |
| `memory_revise` | Exclusive | `crates/radian-host/src/memory/tools.rs:453` |

MCP tools (aliased `<server>__<tool>`) are config-driven, not fixed:
`crates/agent-extensions/src/session.rs:508-521` makes a tool `Shared` only when it is in
`shared_tools`, or when `trust_read_only_hint` is set and the server's own `readOnlyHint` is
true; otherwise `Exclusive`. The script classes any other name as `Exclusive` — the
`ToolConcurrency` default and the conservative choice. No MCP tool ran in these three
journals, so the table covers every executed name. `shell` is `Exclusive` even when a call
uses `background: true`: the class is per `ToolSpec`, and `background` only changes what the
single call returns, not whether it may overlap.

### Metric definitions

1. Call-count histogram over responses: how many carried 1, 2, or 3+ `tool_calls`.
2. Of the 2+ responses: how many contain at least one `Exclusive` name, and how many are all
   `Shared`.
3. Per tool name, over matched pairs: count, total, median, p90 (nearest-rank) and max of the
   per-call wall times.
4. For each 2+ response, two times:
   - **serial** = sum of the calls' wall times (each call one after another);
   - **overlapped lower bound** = `max(Shared wall times) + sum(Exclusive wall times)`
     (all `Shared` calls fully overlap, `Exclusive` calls stay serial). Only calls that
     produced a matched `Started`/`Settled` pair count.
   Saving = serial − lower bound. Reported per journal and as a share of that journal's wall
   time, and in total.
5. Model time between responses: for each `assistant` `MessageAppended`, its `ts` minus the
   previous anchor, where the anchor is the previous `ToolExecutionSettled`, the previous
   user message, or `TurnStarted`. Summed and shown next to the summed tool execution time
   and the session wall.

### What cannot be measured

- **When a tool call first appeared in the model stream.** Stream deltas are live-only
  (ADR 0010) and are not in the journal. So any saving from dispatching a call *before* the
  whole response has arrived cannot be estimated from these files. This measurement says
  nothing about it; the numbers below are only about overlapping calls that are already in a
  finished response.
- **Per-call CPU vs wait.** `Started`…`Settled` is elapsed wall time, which for `shell` and
  `job_output` is dominated by waiting.
- **Attribution inside "other".** The residual in section 5 includes turn setup/teardown,
  permission resolution, journaling and idle time between turns; the journal cannot split it.

## Results

### 1. Responses by number of tool calls

| journal | 1 | 2 | 3+ | responses with tool calls |
|---|---|---|---|---|
| `ab2` | 84 | 1 | 0 | 85 |
| `ab3` | 61 | 16 | 0 | 77 |
| `t4` | 271 | 13 | 1 | 285 |
| **total** | **416** | **30** | **1** | **447** |

Responses with 2+ calls are 31 of 447 (6.9%). Most responses ask for exactly one tool.

### 2. Responses with 2+ calls: Exclusive vs all-Shared

| journal | 2+ total | at least one Exclusive | all Shared |
|---|---|---|---|
| `ab2` | 1 | 0 | 1 |
| `ab3` | 16 | 7 | 9 |
| `t4` | 14 | 6 | 8 |
| **total** | **31** | **13** | **18** |

Every call in these three journals had a measured execution (no denied or pre-flight-failed
call in a 2+ response: 0 calls without a `Started`/`Settled` pair).

### 3. Tool execution wall time per tool name

Per journal (`count`, `total_s`, `median_s`, `p90_s`, `max_s`):

`ab2-radian.jsonl`

| tool | count | total_s | median_s | p90_s | max_s |
|---|---|---|---|---|---|
| `shell` | 55 | 2557.62 | 0.054 | 92.525 | 600.010 |
| `read` | 14 | 0.15 | 0.009 | 0.016 | 0.023 |
| `patch` | 9 | 0.09 | 0.009 | 0.017 | 0.017 |
| `write` | 3 | 0.02 | 0.008 | 0.008 | 0.008 |
| `git_diff` | 2 | 0.06 | 0.028 | 0.034 | 0.034 |
| `git_status` | 1 | 0.02 | 0.021 | 0.021 | 0.021 |
| `list` | 1 | 0.02 | 0.020 | 0.020 | 0.020 |
| `git_commit` | 1 | 0.12 | 0.123 | 0.123 | 0.123 |
| **all** | **86** | **2558.10** | **0.027** | **58.893** | **600.010** |

`ab3-radian.jsonl`

| tool | count | total_s | median_s | p90_s | max_s |
|---|---|---|---|---|---|
| `job_output` | 5 | 1991.10 | 361.060 | 600.028 | 600.028 |
| `shell` | 46 | 187.06 | 0.047 | 4.661 | 91.353 |
| `read` | 25 | 0.32 | 0.013 | 0.019 | 0.022 |
| `write` | 5 | 0.06 | 0.012 | 0.017 | 0.017 |
| `patch` | 5 | 0.05 | 0.010 | 0.016 | 0.016 |
| `list` | 4 | 0.05 | 0.013 | 0.017 | 0.017 |
| `git_commit` | 2 | 0.42 | 0.212 | 0.278 | 0.278 |
| `git_status` | 1 | 0.09 | 0.094 | 0.094 | 0.094 |
| **all** | **93** | **2179.16** | **0.022** | **4.661** | **600.028** |

`t4-bar-input.jsonl`

| tool | count | total_s | median_s | p90_s | max_s |
|---|---|---|---|---|---|
| `shell` | 152 | 5001.20 | 1.753 | 89.098 | 600.014 |
| `read` | 64 | 0.77 | 0.010 | 0.020 | 0.045 |
| `patch` | 70 | 0.70 | 0.009 | 0.014 | 0.039 |
| `write` | 8 | 0.09 | 0.008 | 0.038 | 0.038 |
| `git_commit` | 5 | 0.82 | 0.156 | 0.212 | 0.212 |
| `list` | 1 | 0.01 | 0.013 | 0.013 | 0.013 |
| **all** | **300** | **5003.59** | **0.023** | **16.382** | **600.014** |

Total across the three journals:

| tool | count | total_s | median_s | p90_s | max_s |
|---|---|---|---|---|---|
| `shell` | 253 | 7745.88 | 0.108 | 72.279 | 600.014 |
| `job_output` | 5 | 1991.10 | 361.060 | 600.028 | 600.028 |
| `git_commit` | 8 | 1.36 | 0.154 | 0.278 | 0.278 |
| `read` | 103 | 1.24 | 0.010 | 0.019 | 0.045 |
| `patch` | 84 | 0.83 | 0.009 | 0.014 | 0.039 |
| `write` | 16 | 0.18 | 0.009 | 0.017 | 0.038 |
| `git_status` | 2 | 0.11 | 0.057 | 0.094 | 0.094 |
| `list` | 6 | 0.08 | 0.013 | 0.020 | 0.020 |
| `git_diff` | 2 | 0.06 | 0.028 | 0.034 | 0.034 |
| **all** | **479** | **9740.85** | **0.023** | **16.998** | **600.028** |

The totals are dominated by a few long `shell` and `job_output` waits: the medians are tens
of milliseconds, the maxima are the 600 s foreground/job waits. Those long calls are each
alone in their response — they cannot overlap anything in the same batch.

### 4. Overlap saving in 2+ responses

| journal | 2+ responses | serial_s | overlapped lower bound_s | saving_s | share of journal wall |
|---|---|---|---|---|---|
| `ab2` | 1 | 0.04 | 0.02 | 0.01 | 0.0005% |
| `ab3` | 16 | 0.95 | 0.82 | 0.13 | 0.0031% |
| `t4` | 14 | 1.08 | 0.95 | 0.12 | 0.0016% |
| **total** | **31** | **2.06** | **1.79** | **0.27** | **0.0018%** |

The total possible saving is **0.27 s**, or **0.0018%** of the ~14873 s of wall time in the
three sessions.

Two caveats make this an upper bound, not a target:

- radian already overlaps *consecutive* `Shared` calls, so the "serial" column is not what
  the sessions actually spent. The observed wall time of these 31 batches (first `Started` to
  last `Settled`) is ~2.09 s — essentially the same as the 2.06 s serial sum, because the
  `Shared` calls are ~10 ms each and the overlap buys almost nothing. The saving is the extra
  from also overlapping `Shared` calls that sit on opposite sides of an `Exclusive`.
- The 13 batches that contain an `Exclusive` are the only ones where that extra is even
  possible, and in all of them the `Exclusive` call is the slow one.

### 5. Model time versus tool time

| journal | wall_s | tool execution_s | model_s | other_s |
|---|---|---|---|---|
| `ab2` | 3098.1 | 2558.1 (82.6%) | 523.9 (16.9%) | 16.1 (0.5%) |
| `ab3` | 4044.1 | 2179.2 (53.9%) | 465.2 (11.5%) | 1399.7 (34.6%) |
| `t4` | 7731.2 | 5003.6 (64.7%) | 2671.8 (34.6%) | 55.8 (0.7%) |
| **total** | **14873.4** | **9740.8 (65.5%)** | **3661.0 (24.6%)** | **1471.6 (9.9%)** |

Tool execution (as bracketed by the markers) is about **two thirds** of the wall time and
model time about a quarter. "Other" is the residual: turn setup/teardown, permission
resolution, journaling, and idle time between turns. In `ab3` it is almost all idle — the
first `TurnFinished` is followed 1373 s later by the next `InboxEnqueued` (the session sat
between two inputs).

## Sanity checks

- **Pairs.** Every journal matches 1:1: `ab2` 86/86, `ab3` 93/93, `t4` 300/300 started to
  settled, 0 started-without-settled and 0 settled-without-started. Total 479/479. No
  duplicate `call_id` in a journal.
- **Independent count.** A plain substring scan (not JSON parsing,
  `"event":"ToolExecutionStarted"` / `"event":"ToolExecutionSettled"`) gives the same numbers:
  479 and 479.
- **Cross-check against the task.** `ab3` shows 173 `MessageAppended`, 93 pairs and 78
  `ModelUsageRecorded`, matching the task text.
- The script drops a torn tail (ADR 0006 section 3.2) if a file does not end in LF; all three
  files end in LF.

## Conclusion (what the numbers show)

- Responses almost always call a single tool: 416 of 447 (93.1%) ask for exactly one; only 31
  (6.9%) ask for 2+, and none asks for more than three.
- Fully overlapping `Shared` calls in those 31 responses would save at most **0.27 s**, about
  **0.0018%** of session wall time. The current behaviour (overlap consecutive `Shared`,
  serialize around `Exclusive`) already captures essentially all of it.
- Tool execution is a large share of wall time (65.5% overall) but it is concentrated in
  single long `shell` and `job_output` calls (median ~23 ms, max 600 s), which have no
  same-batch partner to overlap with.
- The one form of parallelism these journals cannot speak to — dispatching a tool call
  before the whole response has arrived — is not measurable from the journal, because stream
  deltas are live-only (ADR 0010). The numbers here do not support or rule it out.
