# Benchmarking Radian on Terminal-Bench (Harbor)

This directory is Radian's Terminal-Bench adapter: a Harbor custom agent that
puts the current commit's `radian` binary into task containers and scores it.
It is **not** part of the Rust workspace and adds no Python dependency beyond
Harbor itself (the adapter and its command tests are stdlib-only).

The scripts that run the side-by-side A/B comparisons from a host live in
[`ab/`](ab/README.md): radian builds against each other, against pi and
opencode, and one binary with and without a system-prompt file. That page
documents each script, its arguments, its environment variables and the job
layout.

Pinned Harbor release: **0.24.0** (see `pyproject.toml`
`[tool.radian-bench]`). The adapter speaks the 0.24.0 agent API
(`harbor.agents.model_connection`, `harbor.agents.installed.base`); newer
Harbor releases may rename these modules.

## Files

| File | Description |
|---|---|
| `radian_agent.py` | Harbor agent: uploads the binary + CA bundle, writes `auth.json`, runs `radian exec --output jsonl -p <instruction>` |
| `radian_command.py` | Stdlib-only command builders (provider selection, `auth.json` rendering, static-binary check, shell quoting, redaction) |
| `build-static.sh` | Builds the static musl binary the task containers need |
| `metrics.py` | Stdlib-only report over finished harbor jobs: per-task matrix, per-trial rows, per-agent totals, spread, leave-one-out and intervals |
| `tests/test_radian_command.py` | Builder unit tests (no Harbor dependency) |
| `tests/test_radian_agent.py` | Agent unit tests against the real Harbor 0.24.0 package |
| `tests/test_metrics.py` | Metrics tests on `tests/fixtures/jobs/` (one fake trial per agent kind), `tests/fixtures/gen-tok-jobs/` (the `gen_tok` rules) and `tests/fixtures/analysis-jobs/` (spread, leave-one-out, intervals); `metrics.py --self-test` runs them |
| `pyproject.toml` | Descriptive metadata only (agent import path, Harbor pin) |

## Key design (why the score measures the agent loop)

- **The score measures the agent loop, not our sandbox.** Default
  `--profile full`: the task container is already the isolation boundary, so a
  nested sandbox would only measure whether the host allows nested sandboxing.
  Radian's own sandbox has its own escape-matrix tests in
  `crates/radian-sandbox`.
- **The key only ever lives in `$RADIAN_BENCH_API_KEY`.** Harbor resolves the
  trial key from that variable (`MODEL_CONNECTION` lists it in
  `api_key_envs`, tried before Harbor's own provider defaults, so it works for
  any provider slug including `opencode-go`). The command line and the uploaded
  `auth.json` name the variable; neither ever holds the value (ADR 0013).
- **Provider routing.** A model name whose prefix is a radian built-in
  descriptor id (`opencode-go`, `anthropic`, `openai`, `openai-compatible`,
  `chatgpt` — `crates/radian-host/src/provider.rs`) runs as
  `radian exec --provider <id> --model <rest>` with no `--base-url`, and
  `auth.json` gets `{"<id>": {"type": "api_key", "key": "$RADIAN_BENCH_API_KEY"}}`.
  An explicitly configured endpoint (a `*_BASE_URL` trial env var, e.g. a
  proxy) always runs as the generic `openai-compatible` provider with
  `--base-url`, including proxied Anthropic traffic — radian's `anthropic`
  provider takes no `--base-url` (ADR 0026 D0). Note `chatgpt` is OAuth-only,
  so an `api_key` entry cannot log into it; the prefix still routes there and
  radian reports the credential error.
- **CA bundle.** Task images based on `ubuntu:24.04` / debian slim often ship
  no system CA certificates, and then radian's HTTP client fails before the
  first request. `install()` uploads the host bundle (option `ca_bundle_path`,
  default `/etc/ssl/certs/ca-certificates.crt`) next to the binary and refuses
  a missing bundle with a clear error. `run()` probes the container with
  `test -s /etc/ssl/certs/ca-certificates.crt` (present **and** non-empty: an
  empty placeholder breaks TLS exactly like a missing file) and sets
  `SSL_CERT_FILE` to the uploaded bundle only when the probe fails. The
  variable is process environment, so radian's child commands (the shell tool,
  `curl` in tasks) inherit it while it is set.
- **Unattended.** Every trial runs
  `--yes --no-trust --offline-catalog`; `auth.json` is 0600 under a 0700
  `RADIAN_HOME`; sessions and stdout/stderr land in the agent logs dir
  (`radian-sessions/`).
- **Usage reporting.** After the logs sync back, `populate_context_post_run`
  sums the trial's `radian-sessions/*.jsonl` — each call's
  `ModelUsageRecorded` plus the summarizer's `CompactionUsageRecorded` — into
  Harbor's token/cost fields, with fee micros converted to USD. A counter no
  record reported stays `None` ("not reported", never 0). `run()` deliberately
  leaves the context empty: Harbor only calls the backfill while it is empty.
- **Outcome reporting.** Every trial runs `--output jsonl`, so stdout ends
  with a `Result` record (ADR 0044): `run()` reads its `exit_code` and
  `stop_reason` into the trial facts and the backfilled metadata, instead of
  inferring the outcome from the exit code alone (turn outcomes that used to
  share exit 1 — failed, over budget, unfinished — now report 1/3/4 with
  their reason named).
- **auth.json ownership.** `auth.json` goes up through Harbor's
  `_upload_config_text`, which chowns it to the container's default agent user
  (plus 0600): a 0600 root-owned file would be unreadable to radian under a
  non-root agent user. It still holds only the `$RADIAN_BENCH_API_KEY`
  reference, never the value.
- **Turn timeout.** An explicit `--turn-timeout-secs` always wins; otherwise,
  when the adapter-side exec timeout (`timeout_sec`) is set, the turn timeout
  is that deadline minus 60 s (floor 60), so radian wraps up and writes its
  journal before the kill. The task's own agent timeout cannot feed this:
  Harbor applies it as an external wait around `run()` and never passes it to
  the agent object.
- **Reproducible.** `install()` refuses a dynamically linked binary (pure-Python
  ELF `PT_INTERP` check — the task images cannot satisfy a glibc loader) and
  records the binary sha256 plus `radian exec --version` in
  `radian-binary.json`. A fixed `--context-window` skips the models.dev
  lookup.

## Unit tests (no Harbor run, no model, no Docker)

```sh
python3 benchmarks/terminal-bench/tests/test_radian_command.py   # stdlib only
python3 benchmarks/terminal-bench/metrics.py --self-test          # stdlib only
<home>/.local/share/uv/tools/harbor/bin/python benchmarks/terminal-bench/tests/test_radian_command.py
<home>/.local/share/uv/tools/harbor/bin/python benchmarks/terminal-bench/tests/test_radian_agent.py
PYTHONPATH=benchmarks/terminal-bench:benchmarks/terminal-bench/ab \
  <home>/.local/share/uv/tools/harbor/bin/python -c \
  "from harbor.utils.import_path import import_class; print(import_class('radian_agent:RadianAgent'))"
```

`metrics.py --self-test` is the same run of `tests/test_metrics.py` (the
tests on the checked-in fixture jobs) that `scripts/verify.sh` runs, so the
metrics report is covered by the repository's normal gate without Harbor, a
model or Docker. Running the test file directly with `python3` gives the same
result.

The agent tests import the real Harbor 0.24.0 package; the fake environment is
only a process double for container exec/upload. Trial orchestration is proven
by the smoke below, not by these tests.

## Metrics from a finished run (stdlib only)

`metrics.py` reads finished harbor jobs and prints a per-task matrix, one row
per trial, per-agent totals, a common-task section, and the spread,
leave-one-out and interval sections. It does not import Harbor and does not
touch a container.

```sh
python3 benchmarks/terminal-bench/metrics.py /tmp/tbench-ab/jobs
python3 benchmarks/terminal-bench/metrics.py /tmp/tbench-ab/jobs --json > metrics.json
python3 benchmarks/terminal-bench/metrics.py /tmp/tbench-ab/jobs --baseline radian-a
```

`JOBS` is a jobs root (`<jobs>/<agent>/<timestamp>/<task>__<id>/`) or any
directory above the trials. `--self-test` runs the fixture tests and takes no
`JOBS` argument. The `<agent>` level is the column label, so two
radian builds in `radian-a/` and `radian-b/` stay separate columns even when
Harbor's `agent_info.name` is `radian` for both. Every `result.json` below a
root is read; a job-level `result.json` has no `task_name` and is skipped. What
each trial contributes:

| Input | Used for |
|---|---|
| `result.json` → `verifier_result.rewards.reward` | PASS (`>= 1`) or fail |
| `result.json` → `exception_info.exception_type` | T/O for a `*Timeout*`; `CancelledError` excludes the trial |
| `result.json` → `agent_execution.started_at`/`finished_at` | agent minutes |
| `result.json` → `agent_result.cost_usd` | cost; radian falls back to journal `fee.micros`, pi/opencode to the ATIF step or final cost |
| `agent/radian-sessions/*.jsonl` | model calls, output/reasoning/generated tokens, >16k calls, generation/tool minutes, `Incomplete(MaxTokens)` |
| `agent/trajectory.json` (ATIF) | model calls, output/reasoning/generated tokens, >16k calls for pi and opencode |

Columns:

- `min` — `agent_execution` wall time.
- `cost` — USD.
- `calls` — model calls: assistant responses plus compaction calls in a radian
  journal; ATIF `llm_call_count` (default 1) over steps that carry `metrics`.
- `out_tok` / `reason` — reported output and reasoning tokens, raw and
  unchanged. `-` means no record reported the counter, never 0. Radian
  reasoning comes from the provider's `details`
  (`completion_tokens_details.reasoning_tokens` or
  `output_tokens_details.thinking_tokens`); ATIF reasoning from
  `metrics.extra.reasoning_tokens` (or a nested details object).
- `gen_tok` — visible output plus reasoning, so the column means the same thing
  for every agent. The rule comes from a table in `metrics.py`, never a guess
  from the numbers: a radian journal's `usage.output` already counts reasoning
  (`gen_tok = out_tok`); pi 1.4.0's `completion_tokens` counts reasoning
  (`gen_tok = out_tok`); opencode 1.19.4's `completion_tokens` excludes it
  (`gen_tok = out_tok + reason` per step, and a step that reports no reasoning
  count adds its completion only). An ATIF producer not in the table gets `-`,
  and the report names it once under `AGENT TOTALS`. `out_tok` is therefore not
  comparable across agents — an opencode trial's own output count leaves
  reasoning out — while `gen_tok` is.
- `over16k` — model calls with more than 16,000 output tokens. ATIF is
  per-step: a producer that merges several calls into one step (pi's
  tool-result usage) is measured on the merged count.
- `gen_min` / `tool_min` (radian only) — from journal timestamps: a model
  request from its boundary (tool settle, user message, turn start or previous
  response) to the assistant response, and each `ToolExecutionStarted` →
  `ToolExecutionSettled` pair. Parallel calls are summed, so `tool_min` can
  exceed `min`.
- `maxtok` (radian only) — the run ended `Incomplete(MaxTokens)`
  (`TurnFinished {outcome: incomplete, reason: max_tokens}`), or exec exited 4
  / the closing `Result` record named `max_tokens`.
- `cost_per_pass` (totals only) — `cost / pass`; `-` when the agent passed no
  trial or no record reported its cost. It is in both `AGENT TOTALS` and
  `COMMON-TASK TOTALS`, and in each JSON totals entry.

A `CancelledError` trial appears in the trial table as `skip`, is counted in
the `excl` column, and is in no total. The matrix has one cell per task and
agent (`PASS`/`fail`/`T/O`; repeats as `P1/2`, plus `T1` when a repeat timed
out). The common-task section keeps only tasks for which every agent has at
least one finished trial (not T/O, not excluded), and totals over exactly
those tasks. `--json` prints the same report as one object (`agents`, `tasks`,
`trials`, `matrix`, `gen_unknown_agents`, `totals`, `common_tasks`,
`common_totals`); a counter no record reported is `null`, never 0.
`trials[].generated_tokens` is the row's `gen_tok`, and each totals object
(`totals`, `common_totals`) carries `generated_tokens` as its sum.
`gen_unknown_agents` lists the ATIF producer names with no generated-token
rule; the trial row that hit one carries its name in `unruled_producer`.

### Journal metrics extractor

`benchmarks/journal_metrics.py` reads radian session journals and pi session
JSONL directly, without a Harbor job directory:

```sh
python3 benchmarks/journal_metrics.py SESSION_DIR [MORE...] [--prefix PREFIX] [--json]
```

Each input is a journal file or a directory searched recursively for
`*.jsonl`; `--prefix` keeps only files whose name starts with it. The output is
one row per journal plus a `TOTAL` row. `--json` prints the same computed rows
(`{"journals": [...], "totals": {...}}`) instead of the table.
`terminal-bench/metrics.py` uses this module for its journal summary
(`_journal_metrics`), so the two reports cannot define "model calls", "input"
or "result bytes" differently. The table below is generated from the script's
`COLUMNS` data; the test in `tests/test_metrics.py` fails when the README block
and the code differ.

| Column | Definition | Unit |
|---|---|---|
| `journal` | The journal file that produced the row; `TOTAL` on the totals row. | path |
| `source` | `radian` for a radian session journal, `pi` for a pi session JSONL. | name |
| `calls` | Model calls, equal to steps: radian assistant messages plus `CompactionUsageRecorded` summary calls; pi assistant messages. | calls |
| `input_uncached` | Input tokens billed at the full price. A radian record marked `input_includes_cache: true` subtracts `cache_read` from `input`; an unmarked radian record and a pi record report `input` alone. | tokens |
| `input_cache_read` | Input tokens served from the provider cache (`cache_read`). | tokens |
| `input_total` | `input_uncached + input_cache_read`: the same input basis for every row. | tokens |
| `output` | Reported output tokens, raw: radian `usage.output`; pi `usage.output`. | tokens |
| `reasoning` | The reasoning part of `output`, from the provider usage object (`reasoning_tokens`, `reasoning_output_tokens` or `thinking_tokens`). | tokens |
| `fee_usd` | Fee in USD: radian `fee.micros / 1,000,000` (including summary calls); pi `usage.cost.total`. | USD |
| `calls.<tool>` | Calls of one tool name. pi's `bash` is counted as `shell`, so the column means the same thing for both agents. | calls |
| `bytes.<tool>` | UTF-8 bytes of the concatenated text parts of one tool name's results, without the JSON wrapping. | bytes |
| `read_ranged_calls` | `read` calls whose arguments hold `offset` or `limit`. | calls |
| `read_ranged_bytes` | Result-text bytes of the ranged `read` calls. | bytes |
| `read_unranged_calls` | `read` calls whose arguments hold neither `offset` nor `limit`. | calls |
| `read_unranged_bytes` | Result-text bytes of the unranged `read` calls. | bytes |
| `read_large_calls` | `read` results whose text is 20,480 bytes or more (20 KiB). | calls |
| `p50ms.<bucket>` | p50 of per-call latency in one prompt bucket: from the previous ready event (turn start, user message, tool settle, or the previous assistant message) to the usage record. Bucket 0 is 0-99,999 prompt tokens, bucket 1 is 100,000-199,999, and so on. | ms |
| `p90ms.<bucket>` | p90 of the same per-bucket latency samples. | ms |
| `compactions` | `ContextCompacted` records (pi: entries with type `compaction`). | count |
| `deterministic_compactions` | The compactions whose `deterministic` field is true. | count |
| `outcomes` | Turn outcome counts. radian counts every `TurnFinished.outcome`; pi counts the last assistant `stopReason` (stop -> completed, length -> incomplete, error -> failed, aborted -> cancelled, toolUse -> incomplete). | count |
| `wall_seconds` | First committed timestamp to last committed timestamp. | seconds |

### Spread, leave-one-out and intervals

The matrix and totals say what each agent spent. Three more sections say
whether a difference is real or carried by one task. All of them use the common
tasks and the same non-excluded trials as `COMMON-TASK TOTALS`; timed-out
trials count there as they do in the totals.

`--baseline <agent>` chooses the agent every ratio divides by (`agent /
baseline`). The default is the first agent in the report's agent order (`agents`
is sorted). An unknown name is a usage error: the command exits 2 and prints
the agents that are in the report.

`SPREAD` has one row per common task and one column per agent holding that
agent's `min/median/max` `gen_tok` over the task's trials. `-` means no trial of
that agent on the task reported `gen_tok`. The median is the middle value, or
the mean of the two middle values for an even trial count, printed without a
decimal when it is whole. Each non-baseline agent then gets a ratio column: the
task's summed `gen_tok` for that agent divided by the baseline's.

`LEAVE-ONE-TASK-OUT` has one row per common task plus a first `(none)` row for
all common tasks. Each non-baseline agent gets a summed `gen_tok` ratio and a
summed `cost` ratio against the baseline over the common tasks *without* the
row's task. The row whose removal moves a ratio furthest from the `(none)` row
is marked `*`; "furthest" is the largest absolute change in any of those
ratios. The `(none)` row is therefore the ratio of the two agents'
`COMMON-TASK TOTALS`, and a `*` marks the task whose removal moves a ratio
furthest, i.e. the task that carries most of the difference.

`INTERVALS` is a task-level bootstrap for each non-baseline agent against the
baseline. It resamples the common tasks with replacement, and inside each drawn
task resamples each agent's trials of that task with replacement; a resample
that draws a task more than once contributes its resampled trials that many
times. Each resample gives the `gen_tok` ratio, the `cost` ratio, and the
pass-rate difference in percentage points (agent minus baseline; a timed-out
trial is not a pass, as in the totals). It takes 2000 resamples from
`random.Random(0)`, so the same trials print the same interval, and reports the
2.5th and 97.5th percentiles by linear interpolation between closest ranks. A
resample that has no reported `gen_tok` or cost on either side gives no ratio
and is left out of that ratio's percentiles. A ratio interval that holds 1.0,
or a difference interval that holds 0, is labelled `inconclusive`. With fewer
than 3 common tasks the table is replaced by one line saying the interval
needs at least 3 common tasks.

`--json` also carries `baseline`, `spread` (per task: each agent's
`min`/`median`/`max`/`trials` or `null`, and the ratios), `leave_one_out` (the
rows, the marked task or `null`), and `intervals` (the resample count, seed,
percentiles, baseline, common-task count, a `note` when there are too few
tasks, and per non-baseline agent the `gen_tok`, `cost` and `pass_rate`
interval objects with `low`, `high` and `inconclusive`).

A total without these sections is not evidence of an effect: a difference one
task carries, one inside an agent's own per-task spread, or one whose interval
holds 1.0 (or whose pass difference holds 0) is not a difference the trials
support.

Journals and `radian-stdout.txt` are the only files streamed: a long run's
journal can be many GB, so it is read line by line under a per-line cap of
16 Mi characters (`MAX_LINE_CHARS`) — an over-long line is drained and skipped,
and the rest of the file is still read, however large the file is. There is no
total-size limit on a journal. `result.json` and `trajectory.json` are
summaries, so they are read whole, but only when they are at most 64 MiB
(`MAX_JSON_BYTES`); that is a per-file bound on these two JSON inputs, never a
journal bound. A `trajectory.json` over the cap is treated as unreadable: the
trial keeps the facts from `result.json` and reports `-` for the counters the
trajectory would have supplied. An over-limit `result.json` is not a trial the
report can read, so it is skipped like a missing one.

The fixtures under `tests/fixtures/jobs/` hold one trial per agent kind plus a
cancelled and a timed-out trial; `tests/fixtures/gen-tok-jobs/` holds one trial
per `gen_tok` rule (including an ATIF producer with none);
`tests/fixtures/analysis-jobs/` holds two agents over three common tasks, where
one task carries the whole difference and one task has three trials. The
commands to run their tests are in
[Unit tests](#unit-tests-no-harbor-run-no-model-no-docker) above.

## Smoke (needs Docker + Harbor + a model key)

```sh
# 1. Build the static Linux binary (task containers are Linux; the host glibc
#    does not fit them). Prints the binary path and sha256; fails unless
#    `file` reports a static link.
benchmarks/terminal-bench/build-static.sh
BIN=~/.cache/radian-musl/target/release/radian
SHA=$(cut -d' ' -f1 "$BIN.sha256")

# 2. Install this box's Harbor release (pinned: 0.24.0).
export PATH="$HOME/.local/bin:$PATH"
uv tool install harbor==0.24.0

# 3. Export the trial key WITHOUT printing it (never echo/env/set -x it).
#    The smoke uses the opencode-go key with model deepseek-v4.1-flash.
export RADIAN_BENCH_API_KEY="$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.radian/credentials/auth.json")))["opencode-go"]["key"])')"

# 4. Run 3-5 easy tasks (repeat -i per task, -n 2 at most). Harbor resolves the
#    key from the exported variable; PYTHONPATH puts radian_agent on the import path.
PYTHONPATH=benchmarks/terminal-bench timeout 5400 harbor run \
  -d terminal-bench@2.0 \
  -i fix-git -i prove-plus-comm -i constraints-scheduling \
  -a radian_agent:RadianAgent \
  -m opencode-go/deepseek-v4.1-flash \
  --ak binary_path="$BIN" --ak binary_sha256="$SHA" \
  -n 2 -o /tmp/tbench-smoke/jobs -y
```

After the run, confirm the key leaked nowhere (this prints only a count, never
the key):

```sh
grep -rlF -- "$RADIAN_BENCH_API_KEY" /tmp/tbench-smoke/jobs | wc -l   # must be 0
```

Results land under the jobs dir (`-o`): per-trial `result.json` carries the
reward (0/1 — task pass rate does not matter for the smoke, only that the
verifier ran and recorded one), the agent logs carry `radian-binary.json`,
`radian-stdout.txt`, `radian-stderr.txt` and `radian-sessions/` (radian's
native JSONL journal — read that when a trial needs debugging, not the
trajectory converters). Success means the infrastructure works on every chosen
task: binary uploaded and runs, key reaches radian, radian talks to
opencode-go, the verifier runs and a reward is recorded, with no agent
setup/exec exception. Record per task: name, base image, reward, radian exit
code, model calls, wall time, and any error.
