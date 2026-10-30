# Fixed repository task set

A fixed set of real tasks from this repository's own arc history, with a runner,
crate-scoped checks, a review and a report. It exists so two A/B rounds can be
compared: the tasks, the base shas and the column definitions stay fixed, and the
only thing a round changes is an arm.

The tasks are merged work, not invented fixtures: each was an arc task whose
first attempt passed. The four here are the ones this repository measured in the
2026-10-30 tool-set A/B; add more by the rule below.

Nothing here holds a credential. An arm names the agent, the provider and the
model; radian and pi read their own stored keys. `run.sh` clones the repository
into `runs/clone` (or `REPO_TASKS_CLONE`) and makes its worktrees there, never in
the repository it is started in.

## Files

| File | What it does |
|---|---|
| `tasks/<id>-<slug>.md` | One task: frontmatter (`id`, `slug`, `base`, `outcome`, `crates`) and the worker brief verbatim under `## Brief` |
| `arms/<name>.md` | One arm: `agent`, `provider`, `model`, `extra` (the agent's own flags) |
| `run.sh` | One run: worktree, agent, wall-time cap, `change.diff`, session, `run.json` |
| `check.sh` | The task's crate-scoped fmt, clippy and nextest; `checks.json` and `checks.log` |
| `review.sh` | arc's first-review prompt on `change.diff`, run by `arms/reviewer.md`; `review.json` |
| `prompt.py` | Builds that prompt by reading `crates/radian-arc/src/gate/review.rs` and `verdict.rs` |
| `timeout_run.py` | The portable wall-time cap the runner and the review use |
| `report.py` | Per-task, totals and baseline-ratio tables from each run's session |
| `eval.sh` | One arm end to end: every task, every k, checks, optional review, report |
| `lib.py` | The task and arm file readers (also the CLI the shell scripts use) |
| `tests/test_repo_tasks.py` | The tests; no model is called |

`runs/` is output and is not committed.

## One arm end to end, one command

```sh
benchmarks/repo-tasks/eval.sh full 1 2
```

That runs every task twice for the `full` arm, checks each run, and prints the
report. Add `REPO_TASKS_REVIEW=1` to also review each run, and
`REPO_TASKS_BASELINE=<arm>` to print the ratios against an arm that already has
runs. The individual steps are:

```sh
bash benchmarks/repo-tasks/run.sh   full TK0226 1
bash benchmarks/repo-tasks/check.sh full TK0226 1
bash benchmarks/repo-tasks/review.sh full TK0226 1      # needs the reviewer's key
python3 benchmarks/repo-tasks/report.py --baseline full
```

`run.sh <arm> <task> <k>` takes the task's id or its full file stem. A second
run of the same `(arm, task, k)` refuses until its `runs/<arm>/<task>/<k>`
directory is removed.

### What the runner does

1. Reads the arm and the task; an unknown name exits 2 and lists the valid ones.
2. Makes `runs/clone` a clone of `REPO_TASKS_SOURCE` (default: the repository
   holding this script) if it is not one, and a detached worktree of the task's
   `base` sha at `runs/trees/<arm>-<task>-<k>`. `git worktree` and `git branch`
   never run in the source repository, so its `.git/config` is untouched. A
   `REPO_TASKS_CLONE` that is the source, the caller's repository or a worktree
   of either is refused with exit 2, because `git worktree add` would write that
   repository's `.git/config`. An existing plain directory (even inside one of
   those repositories) is allowed: cloning into it creates its own `.git`.
3. Runs the agent in the worktree under a wall-time cap (default 2700 s, measured
   by the bundled `timeout_run.py`, so a machine without coreutils is still
   capped):

   - radian: `radian exec --output jsonl --session-dir <out>/session --provider
     <provider> --model <model> <extra> -p -`, with the brief on stdin;
   - pi: `pi --model <provider>/<model> <extra> -p "<brief>"`.

   An arm's `extra` is the agent's own flag string, which is how an arm sets
   `--tools read,write,patch,shell` or `--profile full --approval auto`.
4. Saves `brief.md`, the agent's stdout and stderr, `session/`, `change.diff`
   (committed and uncommitted against the base), `commits.txt`,
   `status-after.txt`, and `run.json` (`status` is `ok`, `failed` or `timeout`).
   A run killed by the cap is `timeout`: the report keeps it.

Environment inputs:

| Variable | Default | Meaning |
|---|---|---|
| `REPO_TASKS_SOURCE` | the repository of this script | repository cloned for the worktrees |
| `REPO_TASKS_CLONE` | `<runs>/clone` | the separate clone; the source is never written, and a clone that is the source or the caller repository (or a worktree of either) is refused |
| `REPO_TASKS_RUNS` | `benchmarks/repo-tasks/runs` | the run root |
| `REPO_TASKS_TIMEOUT` | `2700` | the wall-time cap in seconds (`timeout_run.py`) |
| `REPO_TASKS_RADIAN_BIN` | `radian` | the radian binary |
| `REPO_TASKS_PI_BIN` | `pi` | the pi binary |
| `REPO_TASKS_CARGO` | `cargo` | the cargo binary the checks run |
| `REPO_TASKS_MAX_JOBS` | unset | `--jobs` for clippy and nextest in the checks (never `cargo fmt`, which would pass it to rustfmt) |
| `REPO_TASKS_TASKS_DIR` / `REPO_TASKS_ARMS_DIR` | `tasks` / `arms` | another task or arm set (tests) |
| `REPO_TASKS_REVIEW_RS` | `crates/radian-arc/src/gate/review.rs` | the prompt source (tests) |
| `REPO_TASKS_REVIEW` | `0` | `eval.sh` reviews each run when `1` |
| `REPO_TASKS_BASELINE` | unset | `eval.sh` passes this to `report.py --baseline` |

### Checks

`check.sh` reads the task's `crates` list and runs, in the run's worktree and
target directory:

```sh
cargo fmt --all -- --check
cargo clippy -p <crate> --all-targets -- -D warnings
cargo nextest run -p <crate> --locked --no-fail-fast
```

Every listed crate is checked even after one fails: a two-crate task runs both,
and either failure makes `checks.json`'s `checks_ok` false and the exit code 1.
`REPO_TASKS_MAX_JOBS` is passed as `--jobs` to clippy and nextest; `cargo fmt`
takes no job option, and passing one after `--` would reach rustfmt.

### Review

`review.sh` runs `crates/radian-arc`'s first-review prompt: `prompt.py` reads
`build_prompt` and `report_format` from `src/gate/review.rs` and `REVIEW_LEVELS`
from `src/gate/verdict.rs`, so the text is the one arc writes, not a copy. The
prompt holds the task, the brief, the base-head commits and the run's
`change.diff`; it ends with arc's fenced `json` report. The reviewer is the
arm-like `arms/reviewer.md` (read-only tools). A completed, exit-0 session that
parses a report with no blocking item passes; a blocking item, an unparsable or
incomplete report, a non-zero exit or a timeout exits 1 and is recorded in
`review.json`. The report is read exactly as arc reads it: the last closed
`json`/`jsonc` block, with a string `summary`, item lists for `blocking` and
`suggestions`, a string list `verified`, and every item's `level` from arc's
`REVIEW_LEVELS`.

A radian reviewer's stdout is JSONL, where the answer is only an escaped string
inside journal lines, so its report is read from the final assistant message in
`review-session/`. A pi reviewer's stdout is the plain answer and is read
directly.

## Columns

The column definitions are the `COLUMNS` table in
[`benchmarks/journal_metrics.py`](../journal_metrics.py), rendered in
[the Terminal-Bench page](../terminal-bench/README.md#journal-metrics-extractor).
Every number in the report comes from that module: `report.py` calls
`journal_metrics.build_report` on each run's session journal and sums the
per-run rows per arm and task. It does not redefine "model calls", "input",
"result bytes" or "wall".

The text tables show, for each arm: `runs`, the `result` counts, and the fixed
column names from `journal_metrics.COLUMNS` — `calls`, `input_uncached`,
`input_cache_read`, `input_total`, `output`, `reasoning`, `fee_usd`,
`read_ranged_calls`, `read_unranged_calls`, `read_large_calls`, `compactions` —
plus `calls.<tool>` and `bytes.<tool>` for every tool the runs used,
`result_bytes`, `wall_seconds`, and `in_spread` (the smallest and largest
`input_total` over that arm's runs, the same-arm noise band).
`-` means no journal reported the value; it is never a zero.

`report.py --baseline <arm>` adds a ratio table (`arm / baseline`) per task and
for the totals, over `input_total`, `calls`, `output`, `fee_usd`,
`wall_seconds` and `result_bytes`. A baseline with no runs is a clear error that
lists the arms that do have runs.

## Decision rules

These are fixed before a round, in that round's brief, and not changed after the
numbers are in:

1. **k ≥ 2 per arm.** One run per arm is not evidence.
2. **The same-arm repeat spread is the noise band.** The difference between two
   runs of one arm on one task (`in_spread`) is noise. An effect smaller than
   that band is treated as no effect. The 2026-10-30 round measured the same
   configuration moving by 1.16, 1.76, 0.55 and 0.94 across tasks; a k=1 total
   can look 20–25% different and mean nothing.
3. **Per-task ordering beats sums.** An arm wins a task only when every one of
   its runs beats every run of the other arm on that task. A lower total carried
   by one task is not a win.
4. **Write the rule in the round's brief before the run.** State which arm wins
   and on how many tasks; do not turn a mixed result into a conclusion
   afterwards.

## Adding a task

A task joins the set when it meets all four rules:

1. **Merged.** The original arc task is on `main`; `base` is the commit its
   first attempt started from (from that task's merged history), and `outcome`
   is the first attempt's result (`pass` = checks and review passed).
2. **Passed first time.** The first attempt passed; a task that needed a rework
   measures the rework, not the agent.
3. **One crate's change.** The original diff touches one crate (plus docs). This
   keeps the check cheap and the failure attributable.
4. **Different crates are covered.** The set spans the workspace: at least one
   task per crate the round's change could touch, and no two tasks duplicate a
   crate's central path.

Then copy the worker brief verbatim (the text `radian arc show <id>` prints,
without any `## Rework` section) under `## Brief`, fill the five frontmatter
fields, and put the crate names `check.sh` should run in `crates`. The crates are
usually the crates the original diff touched plus `radian-coding` for the docs
drift tests. Run `eval.sh <arm> 1` once to confirm the runner and the checks
work before a round uses the task.

## Tests

```sh
python3 benchmarks/repo-tasks/tests/test_repo_tasks.py
python3 benchmarks/repo-tasks/report.py --self-test   # the same tests
```

No test calls a model: `run.sh` is driven with a stub radian and a temporary git
repository, `check.sh` with a stub cargo, and the report and prompt tests read
the checked-in task files and the real arc source. The tests cover the caller's
`.git/config` staying byte-identical (including a clone path that points at the
caller), an unknown arm/task naming the valid ones, a timeout staying in the
report (also when `gtimeout`/`timeout` are absent), a baseline with no runs, a
two-crate check with `--jobs`, the review parser and review pass rule, and a
prompt built from `review.rs`.
