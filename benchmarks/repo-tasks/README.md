# Fixed repository task set

A fixed set of real tasks from this repository's own arc history, with a runner,
crate-scoped checks, a review and a report. It exists so two A/B rounds can be
compared: the tasks, the base shas and the column definitions stay fixed, and the
only thing a round changes is an arm.

The tasks are merged work, not invented fixtures: each was an arc task. Four of
them passed on the first attempt and are the ones this repository measured in the
2026-10-30 tool-set A/B; add more by the rule below. The fifth, TK0236, is the
long task: a 305-step worker run that touches five crates and whose first attempt
failed the gate (`outcome: fail`). It breaks rules 2 and 3 on purpose, so a
round can make compaction fire several times in one task; read its checks as
that, not as a first-pass baseline.

Nothing here holds a credential. An arm names the agent, the provider and the
model; radian and pi read their own stored keys. `run.sh` makes each run tree
from the task's base sha alone, outside every repository, and never writes the
repository it reads from.

## Files

| File | What it does |
|---|---|
| `tasks/<id>-<slug>.md` | One task: frontmatter (`id`, `slug`, `base`, `outcome`, `crates`) and the worker brief verbatim under `## Brief` |
| `arms/<name>.md` | One arm: `agent`, `provider`, `model`, `extra` (the agent's own flags) |
| `run.sh` | One run: run tree, agent, wall-time cap, `change.diff`, session, `run.json`, `leak.json` |
| `check.sh` | The task's crate-scoped fmt, clippy and nextest; `checks.json` and `checks.log` |
| `review.sh` | arc's first-review prompt on `change.diff`, run by `arms/reviewer.md`; `review.json` |
| `prompt.py` | Builds that prompt by reading `crates/radian-arc/src/gate/review.rs` and `verdict.rs` |
| `timeout_run.py` | The portable wall-time cap the runner and the review use |
| `report.py` | Per-task, totals and baseline-ratio tables from each run's session |
| `leak.py` | Whether a run reached the task's later commits: its leak level and `leak.json` |
| `eval.sh` | One arm end to end: every task, every k, checks, optional review, report |
| `lib.py` | The task and arm file readers, the run root and the run tree (also the CLI the shell scripts use) |
| `tests/test_repo_tasks.py` | The tests; no model is called |

The run root holds the output. Its default is outside the repository, so
nothing there is committed.

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
run of the same `(arm, task, k)` refuses until its `<runs>/<arm>/<task>/<k>`
directory and its `<runs>/trees/<arm>-<task>-<k>` tree are removed.

### What the runner does

1. Reads the arm and the task; an unknown name exits 2 and lists the valid ones.
2. Refuses a run root inside any git checkout (exit 2; the message names the
   checkout). That covers the source, the repository the script is started in
   and any other repository: an agent that leaves its tree must not find a
   history there. One run did, and ran `git checkout work/...` in the harness
   checkout that held the old default run root.
3. Makes the run tree `<runs>/trees/<arm>-<task>-<k>` holding the task's `base`
   sha and nothing else:

   ```sh
   git init <tree>
   git -C <tree> fetch --depth 1 --no-tags \
     --upload-pack 'git -c uploadpack.allowReachableSHA1InWant=true -c uploadpack.allowAnySHA1InWant=true upload-pack' \
     <source> <base>
   git -C <tree> checkout --detach FETCH_HEAD
   ```

   and then removes `.git/FETCH_HEAD`, which names the source path. The base
   need not be a branch tip. The tree has no remote, no branch, no tag and no
   commit after its base: `git log --all` shows one commit, and the task's
   merged fix is not in it.

   There is no clone because a clone carries the answers. The old runner made
   each tree a worktree of a full clone, which had `origin/main` with every
   task's merged fix and the source's `work/*` branches. Every arm runs with
   `--profile full`, so no sandbox stopped an agent, and runs used it: `git
   cherry-pick HEAD..origin/work/TK0223-...`, `git checkout
   origin/work/TK0228-... -- <files>`, `git show <the original fix>`.

   The source is only read. The upload-pack options are on the fetch's command
   line, so the source's `.git/config` is never written (workers in a shared
   clone fail when it changes). `git rev-list --count HEAD` is checked before
   the agent starts; anything but 1 refuses the run.
4. Runs the agent in the tree under a wall-time cap (default 2700 s, measured
   by the bundled `timeout_run.py`, so a machine without coreutils is still
   capped):

   - radian: `radian exec --output jsonl --workspace <tree> --session-dir
     <out>/session --provider <provider> --model <model> <extra> -p -`, with the
     brief on stdin (without `--workspace` the session has no file, shell or job
     tools);
   - pi: `pi --model <provider>/<model> --session-dir <out>/session <extra>
     -p "<brief>"`.

   An arm's `extra` is the agent's own flag string, which is how an arm sets
   `--tools read,write,patch,shell` or `--profile full --approval auto`.
5. Saves `brief.md`, the agent's stdout and stderr, `session/`, `change.diff`
   (committed and uncommitted against the base), `commits.txt`,
   `status-after.txt`, and `run.json` (`status` is `ok`, `failed` or `timeout`;
   `tree` is the run tree; `tree_commits` is the tree's commit count before the
   agent started, always 1). A run killed by the cap is `timeout`: the report
   keeps it.
6. Runs `leak.py` on the run and saves `leak.json` (see [Leak level](#leak-level)).

Environment inputs:

| Variable | Default | Meaning |
|---|---|---|
| `REPO_TASKS_SOURCE` | the repository of this script | the repository each tree's base is fetched from, and where `leak.py` looks up later shas; it is never written |
| `REPO_TASKS_RUNS` | `${XDG_CACHE_HOME:-$HOME/.cache}/radian-repo-tasks/runs` | the run root; `run.sh` refuses (exit 2) a run root inside any git checkout. `check.sh`, `review.sh` and `report.py` read the same default |
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

`check.sh` reads the task's `crates` list and runs, in the run's tree and
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

The `leak` column counts the arm's runs by leak level: `L0` to `L3`, and `no
journal` for a run whose agent left no journal (that is not `L0`). It counts
every run, also the excluded ones. A run at level 2 or 3 is left out of
`runs`, `result`, every number, `in_spread` and the ratios, and the table says
`<arm>: excluded N (leak)` under it.

`report.py --baseline <arm>` adds a ratio table (`arm / baseline`) per task and
for the totals, over `input_total`, `calls`, `output`, `fee_usd`,
`wall_seconds` and `result_bytes`. A baseline with no runs is a clear error that
lists the arms that do have runs.

## Leak level

`leak.py <run dir>` reads every tool call in the run's session journals (radian
and pi formats), parses each `git <subcommand> ...` in their string arguments,
and writes `leak.json`: `level`, `evidence` (each line that raised the level),
`outside` (absolute paths under `$HOME` that are not inside the run's tree;
`~/.cargo` and `~/.rustup` do not count), `calls` and the `source` it used.
`report.py` scans every run the same way. `--source <repo>` names the
repository later shas are looked up in; the default is `REPO_TASKS_SOURCE`,
else this repository.

A sha is **later** when the source repository has it, it is not an ancestor of
the run's base, and it is not one of the run's own commits (`commits.txt`). A
**later ref** is `origin/*`, `remotes/*`, `work/TK*`, `refs/heads/work/*` or
`main`.

| Level | Name | What raises it |
|---|---|---|
| 3 | used | `cherry-pick`, `merge`, `checkout`, `switch`, `restore`, `reset`, `rebase`, `apply` or `am` of a later sha or ref; `git show <later>:<path> >`; a second history: `git remote add` or `set-url`, `git fetch` or `git pull` of a path or url, any `git clone` (the evidence names the path or url, even if no sha from it is named later) |
| 2 | viewed | `show`, `diff`, `cat-file` or `format-patch` of a later sha or ref; `log -p`, `--patch`, `--stat` or `-- <path>` on one |
| 1 | names | `log --all`, `log` of a later ref, `branch -a`, `--all`, `-r`, `--contains` or `--list`, and `rev-list`, `reflog`, `for-each-ref`, `show-ref` or `merge-base` with a later ref or `--all` |
| 0 | clean | none of these |
| null | no journal | the run has no journal |

`outside` does not change the level. The scan cannot see content read without
`git` (`.git/objects` read directly), a sha built at run time by a shell
substitution, or a repository reached through a relative path. Read level 1 as
suspect, not clean: a subject line alone can point at the fix. With the run
tree above, a later sha or ref cannot be found in the tree; a level above 0 is
then an attempt, or a second history brought in.

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
5. **A run with leak level ≥ 2 is excluded.** It saw or used the task's later
   commits, so it measures the answer, not the agent. The report leaves it out
   of every total and ratio and says how many it left out. If that leaves an arm
   with fewer than two runs on a task, rerun it; do not read the task.

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
repository, `check.sh` with a stub cargo, the leak tests read small radian and
pi fixture journals, and the report and prompt tests read the checked-in task
files and the real arc source. The tests cover the run tree (one commit, no
ref, no later sha, a mid-history base, the agent's commits listed from base to
head), the source's `.git/config` staying byte-identical, a run root inside the
source, the caller's or another checkout being refused, every leak level in
both journal formats (the run's own commit, a second history), a run with no
journal, a level-2 run left out of the totals, an unknown arm/task naming the
valid ones, a timeout staying in the report (also when `gtimeout`/`timeout`
are absent), a baseline with no runs, a two-crate check with `--jobs`, the
review parser and review pass rule, and a prompt built from `review.rs`.
