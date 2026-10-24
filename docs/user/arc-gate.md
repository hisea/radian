---
description: The arc gate: verification stages, the review stage, failures, retries, the budget, and reconcile.
---

# arc: the gate

This page covers the gate run and its failures; the commands are in [`arc`](arc.md), the scheduling loop is in [`arc-loop`](arc-loop.md), and the files arc keeps are in [`arc-files`](arc-files.md).

## The gate run

A task in `review.verifying` is checked against `verified_sha`, which is the pushed `head_sha` of the task's current attempt.
`start` runs the gate in the same process after a pushed attempt, and `radian arc run` starts it as a background process: a pass starts the gate and returns, and a later pass reads its verdicts and moves the task ([`run`](arc-loop.md#radian-arc-run)).
A gate writes its verdict files and one record, `runs/<task>/gate.json`; it never changes the task's state itself.

- arc fetches `[repository] remote` first.
  The remote's `work/<task>` must still be `head_sha`.
  When it differs, arc records the reason in the attempt's `review.json` and the task stays in `review.verifying`; `show` prints the reason.
  The fetch is a git call that talks to a remote, so [`limits.git_remote_timeout_secs`](arc-files.md#configtoml) bounds it; a fetch that runs out fails the gate with `git fetch timed out after <n> s`.
- The stages are the configured profile's (`gate.profile`, then `gate.profiles.<name>.stages`), in order, plus the task's `extra_stages`.
  `human` is your approval and does not run here.
  When the profile lists both `commands` and `review`, those two start at the same time; every other stage keeps its order.
- Every stage runs against the verification candidate, in its own checkout under `.radian/arc/gates/<stage>/`.
  The review stage's checkouts are one per concurrency slot, `.radian/arc/gates/review-<n>/`.
  The candidate is `verified_sha` when the remote main is an ancestor of it; otherwise it is a merge commit of `verified_sha` onto the remote main, built in that checkout and never pushed.
  The merge uses arc's own commit identity, so the same inputs build the same tree.
  `verified_sha` stays the task's commit: the approval, the digest and `merge` bind to it, and the candidate is evidence only.
  A stage's commands run in order and stop at the first failure; each has its own log and the profile's timeout.
- A verification stage's verdict records `sha` (the task's `verified_sha`), `main_sha` (the remote main the candidate was built on) and `candidate_sha` (the commit the stage ran on; absent when the merge conflicted).
- A merge conflict is a failed stage: the verdict's reason is `conflict`, its `conflict_paths` lists the paths, the task goes to `review.rework_pending`, and `show` prints `conflict: <paths>`.
  Nothing is pushed.
- `review` is a read-only radian session, not the worker session.
  It runs at the same time as `commands` when the profile lists both and before `human`; a profile that lists both must list `commands` first.
  For each run it starts one `radian exec` in its own slot checkout, `.radian/arc/gates/review-<n>/`, on the candidate, with the candidate as the workspace and only the `read`, `grep` and `glob` tools mounted: no shell tool, no write tool, and no MCP server.
  Its model route is `[gate.review] exec_args`, and `[gate.review] timeout_secs` limits one session.
  A task's **first review** is a review stage run when no earlier attempt of the task has a `review` verdict.
  It reads `[gate.review.first] exec_args` and `[gate.review.first] timeout_secs` when they are set; an unset key falls back to the same `[gate.review]` key and, for `exec_args`, then to `worker.exec_args`.
  A rework review never reads `[gate.review.first]`.
  Both keys are read when the stage starts, so a change takes effect at the next review.
  The prompt is built in: the task's brief, the commit list and the diff from the merge base of the remote main and `verified_sha` to `verified_sha` (the task's own change, not main's), and the instruction to check the change against the brief and the repository's rules (`AGENTS.md` and the design records the brief names).
  A rework round's prompt holds the increment instead of the full diff (see [A rework round](#a-rework-round)).
  `[gate.review] prompt_file` is appended to it.
  The session must end with one fenced `json` block, and arc parses the last such block:

  ```json
  {"summary": "what the change does", "blocking": [{"level": "major", "file": "path", "line": 12, "text": "what is wrong"}], "suggestions": [], "verified": []}
  ```

  `summary` is 2 to 6 sentences. Each item of `blocking` and `suggestions` is an object: `text` is required, and `level`, `file` and `line` are optional. `level` is one of `critical`, `major`, `minor`. A plain string is an item with only its `text`, so a verdict written before the object form still reads.
  `blocking` lists what must change before the change can merge; an empty list passes the stage. `suggestions` and `verified` never fail the stage; the suggestions of every review round of a merged task become its follow-up task (see [The review's follow-up](#the-reviews-follow-up)).
  A blocking item fails the stage with reason `review_blocking`; the task goes to `review.rework_pending` and `rework_reason` lists the items, each as `[level] path:line text` with the parts an item does not have left out. An item whose `text` starts with a tag that repeats its own level (`[major]`, any case, spaces allowed) prints the level once: the leading tag is dropped from the text. A `text` that starts with another tag keeps it, and the verdict on disk is never rewritten.
  No parsable block, a missing field, a `level` outside the three values, a timeout, or a session error runs the session once more; when the second run is unusable too the stage fails with reason `review_output`, or `review_timeout` when the last run timed out. Both runs' session ids and their summed cost are in the verdict.
  A gate that is started again for the same attempt does not reuse a review session whose journal exists: the stage names the run `<attempt id>-review-<run>-<k>` with the smallest free `k`, writes its logs as `review-<run>-<k>.jsonl` and `review-<run>-<k>.stderr.log`, and `review_session_ids` lists the ids it used, so every run's session is found.
  The review verdict is `runs/<task>/attempts/<attempt id>/verdicts/review.json`; it records `review` (the four fields), `review_session_ids`, `review_cost_micros`, `review_settings` (`round`, `first` or `rework`; the `exec_args` source key; `timeout_secs` and the key it came from) and, in a rework round, `review_pre_existing` (how many items arc moved), beside the usual `sha`, `main_sha` and `candidate_sha`. It is bound to `verified_sha`, is part of `verdict_set_digest`, and `reverify` runs it again.
- `review` failure reasons: `review_blocking` (with its items in `rework_reason`), `review_output` (no usable block after two runs), and `review_timeout` (the last run hit `[gate.review] timeout_secs`).
- When the profile lists both `commands` and `review`, the round's outcome follows the profile order.
  A `commands` failure decides it with the commands reason, even when the review passed or failed too: the review verdict is still written, its cost counts, `show` lists its items, and arc writes no automatic review rework for that round.
  When `commands` passes and `review` fails, the review path runs (see [A failed review](#a-failed-review)).
  Both verdicts are written when each stage ends, and a verdict already written is reused: a gate that died while the pair ran resumes only the stage without a verdict.
  The gate waits for both stages to end before it decides; a failing stage does not kill the other, and the review is bounded by its own limit.
  A stop signal to the gate reaches both running stages.
- This build runs `commands` and `review`.
  A profile or a task that lists `mutation` or `ci` is refused with `stage <name> is not built yet`; the task stays in `review.verifying` and `show` prints that reason.
  `init`'s default profile lists only `commands` and `human`.
- A verdict is `runs/<task>/attempts/<attempt id>/verdicts/<stage>.json`; a command's log is `runs/<task>/attempts/<attempt id>/logs/<stage>-<n>.log`.
  A verdict is written once and never changed.
  A verdict that already exists for this attempt and stage is reused instead of rerun, so a crash between stages resumes where it stopped; when the `commands`/`review` pair was interrupted, only the stage without a verdict runs again.
- One gate runs per task at a time.
  The gate process holds `gates/<task>.gate.lock` from the moment a pass starts it, so the pass that follows starts no second gate for the same commit.
  A signalled or killed gate releases the lock; the next pass starts it again, and the verdicts already written are reused.
  Stages of different tasks still queue on their own locks: one `commands` stage at a time, one checkout per stage, and the review stage has its own.
  Up to [`gate.review.concurrency`](arc-files.md#configtoml) review sessions run at once (default `2`, `1` or more); each takes one of the locks `gates/review-0.lock` … `gates/review-<n-1>.lock` and the checkout of that slot, so no two sessions share a checkout. With `1`, a second review waits for the first.
  The `review` stage is a read-only session, so it does not wait for another task's `commands`.
- The gate's own output is `runs/<task>/gate.log`.
- `runs/<task>/gate.json` holds one record, with the attempt it ran for.
  Its `status` is one of `started`, `running`, `done`, `refused`, `error`, `no_record`, `died` and `interrupted`.
  A pass applies a `done` record; reports a `refused` one in the summary and starts no new gate for that attempt until the inputs change (for verification the branch comes back; for integration a new `head_sha`, a new main or a changed `config.toml`; or `start`/`reverify` claims a new attempt); reports an `error` one as an error (exit `1` for a one-shot `run`); and prints the text of a `no_record` or `died` one as the reason the verification cannot run.
  `radian arc show` keeps printing the recorded reason.
- Before a pass starts a gate it writes to that record the inputs the gate will read and how many times a gate already died for them (`status` `started`), and names the gate process there once it exists; the gate replaces the status with `running`, keeps the inputs and the count, and an integration gate is the same.
  A gate that exits before writing any record leaves `started`: the next pass records `no_record` for that attempt with those inputs, reports it in the summary as `<task> verify cannot run: its gate exited without a record`, starts no gate while they are unchanged, and does not count the pass as changed. A new `head_sha`, a new main or a changed `config.toml` allows one new start.
- A gate that wrote `running` and then died is restarted, and the death is counted: the record's `deaths` goes up by one for that attempt and those inputs.
  When the count would pass [`limits.max_gate_deaths`](arc-files.md#configtoml) (integer, default `2`, `0` or more), the pass starts no gate and records `died` instead, which behaves exactly like `no_record`: the summary prints `<task> verify cannot run: its gate died after it started`, `show` prints the same text as the `rework_reason`, no gate starts while the inputs are unchanged, and the pass does not count as changed.
  A new `head_sha`, a new main or a changed `config.toml` allows one new start and begins again at 0 deaths; `0` stops at the first death, so the gate is started `1 + max_gate_deaths` times at most.
  The count needs a digest on both sides: when the gate could not be planned, there is nothing to compare and the gate is started again as before.
- A gate that the scheduling loop stops itself is `interrupted`, not a death.
  On SIGINT or SIGTERM the loop stops the gate it finds (also one whose record is still `started` while it holds the gate lock), waits `[limits] cancel_grace_secs` and kills it; once the gate process is gone the record becomes `interrupted`, keeping the attempt and the inputs.
  The next pass starts the gate again at once, reuses the verdicts already written, and counts nothing.
- Any stage failing moves the task to `review.rework_pending`.
  The stage, its reason and the verdict path are in the task's `rework_reason` (`show` prints it).
  Main moving after a verification does not send the task back to `review.verifying` by itself; `merge` integrates onto the then-current main and runs the key commands there.
- Every required stage passing moves the task to `review.awaiting_human` and records `verified_sha` and `verdict_set_digest` in the task.
  The digest covers the verification verdicts for `verified_sha`; it leaves out the `human` approval and the `integration` verdict, which are written later, at merge.
  A new verdict for the same commit changes the digest and invalidates an approval.

`list` and `show` do not use the network: they do not check the remote and cannot see a branch that moved after the gate.
`merge` checks the remote before it acts.

### A rework round

A review is a rework round when an earlier attempt of the same task has a review verdict for a `sha` other than the commit under review; the newest such verdict is the previous review.
A review of the same commit as the previous review (a `reverify` of an unchanged head) gets the first review's prompt: the full diff, and no item is pre-existing.
It reads the `[gate.review]` settings, not `[gate.review.first]`, because an earlier review verdict exists, so it is not a first review.
A genuine first review, one with no earlier verdict at all, keeps the full-diff prompt and the `[gate.review.first]` settings.

A rework round's prompt holds the brief, the previous review's blocking items, the commit list `<previous sha>..<sha>`, and the increment instead of the full diff.
It asks the session to check that each previous blocking item is fixed, to review the increment, and to list every problem it finds; the session can read the files.

The increment is the task's own diff restricted to the hunks that hold a line the rework changed.
A line is an increment line when it is a new-side line of `git diff <previous sha> <sha>` and also a new-side line of the task's own diff, which starts at the merge base of the remote main and `sha`.
A line that came in only with a merge of main is not in the task's own diff, so it drops out.

arc, not the session, decides whether a blocking item is pre-existing.
After a rework round's report is parsed, an item is moved to `suggestions` and marked `"pre_existing": true` unless one of these holds:

- its `file:line` is within 3 lines of an increment line in that file;
- it is in the same file as and within 10 lines of a blocking item of the previous review, so an item that was not fixed stays blocking;
- its `level` is `critical`: a `critical` item blocks in every round, wherever it is;
- it has no `file` or no `line`: it stays in `blocking` at its level, because the review prompt asks for both fields and an omission is not evidence that the problem is old.

A moved item keeps its `level` (`major` included), and a `major` item outside both windows is still moved, so the rework is not sent back for a problem it did not touch; `minor` items follow the same window rules.
The stage passes when no blocking item is left, and the verdict records how many items arc moved as `review_pre_existing`.
The follow-up task of a merged task carries a moved item at its level (see [The review's follow-up](#the-reviews-follow-up)).

When a rework round's review still fails with blocking items and the plan's `auto` is not `off`, arc writes the next attempt's brief itself (see [A failed review](#a-failed-review)).

### A failed review

A review that failed with `review_blocking` returns the task to `todo.ready` with a brief arc writes, the way a conflict with main does, when the plan's `auto` is not `off`.
The heading is `## Rework <n> (review)`, with the lead's `add_rework` numbering.
The text says the review found these items, to fix them and nothing else, to append commits and not to rebase; it lists the blocking items, one per line, as `[level] path:line text`; and it names the review verdict to read for detail.
It counts as an automatic rework: it raises `auto_rework`, and `max_auto_rework` pauses the plan (see the [`limits.max_auto_rework` row](arc-files.md#configtoml)); the task then stays in `review.rework_pending`.
With `auto` `off`, the task stays in `review.rework_pending` and waits for you, as before.
`radian arc run` prints `<task> review blocking -> todo.ready (automatic rework)`.

### The review's follow-up

When a task reaches `done.merged`, arc reads every `review` verdict of the task, oldest attempt first, and collects every `suggestions` item of every round; the items a rework round moved (marked `pre_existing`) are in `suggestions`.
Every round is read, not only the newest: a round that failed can carry a demoted item, and a later round can pass with nothing to suggest.
Nothing from `blocking` is collected: a merged task's blocking items were fixed or the review would not have passed, so a round that only had blocking items contributes nothing.

Two items are the same when their `file`, `line` and `text` are equal after a leading tag that repeats the item's own level is removed and whitespace is collapsed; arc keeps the newest copy.
An item with the same `file:line` and other text is kept as its own item.

When at least one item is left, and `[gate.review] propose_followups` is `true` (the default), arc adds one task to the same plan, and no more:

- the id is the next task id;
- the slug is `<merged task slug>-followups`, cut to the 48-character slug limit;
- the title is `Follow-ups from <task id>'s review`;
- its `after` is the merged task;
- its phase is `todo.proposed`, so the loop never claims it until you confirm it (see [`confirm`](arc.md#radian-arc-confirm-id)).

Its brief is fixed: one line saying the items are from the review of `<task id>`, which is now merged, and that each item is to be fixed or left with a one-line reason in the final answer; then the items grouped by the round they came from, each group headed `From review <n> (attempt <n>):`, one item per line as `[level] path:line text`; then the path of every review verdict.
An item whose `text` starts with a tag that repeats its own level prints the level once, as in the rework reason above.

A task gets at most one follow-up.
A follow-up task gets no follow-up of its own: when the merged task's title and `after` are the ones arc gives a follow-up, arc proposes nothing, and the review's unfixed items stay in its verdict and in the task's final report.
A task merged without a review stage, or with no item left, gets none.
With `propose_followups` `false` arc adds none (see the [`propose_followups` row](arc-files.md#configtoml)); the lead can still add a task to the plan with MCP `add_task`.
`radian arc run` prints `<new task id> proposed (follow-ups from <task id>)`.

A follow-up arc cannot add (the plan's file changed outside arc) does not undo the merge: `radian arc merge` prints it as a `warning:` line after the merge, and `radian arc run` reports it in the summary.
The plan keeps the state it had.

### A conflict with main

A verification stage (`commands`, `mutation`, `review`) or the integration stage can fail with reason `conflict`: the task's commit or branch does not merge with main.
With `merge-main` on, the next attempt starts mid-merge, so arc writes its brief and moves the task to `todo.ready`.
The heading is `## Rework <n> (conflict with main)`, with the lead's `add_rework` numbering.
The text names the main commit (short sha) and the stage, lists the conflicted paths, says the next attempt starts mid-merge, and asks the worker to resolve the conflict keeping both sides' intent, commit the merge, rebuild, run the touched tests, and change nothing else.
Every other reason, and a conflict without `merge-main`, keeps `review.rework_pending`; a `reverify` conflict takes the automatic path above.
It never raises `auto_rework`, never pauses the plan, and never sends the task to `failed.exhausted` by itself: a conflict rework is not a retry, but it keeps the task's `retry_count` instead of resetting it, so a worker that keeps failing behind repeated conflict reworks still uses up `max_attempts`.

A run of conflicts is bounded by [`limits.max_conflict_rework`](arc-files.md#configtoml) (default `3`).
It is how many conflict reworks arc writes in a row: while it has written fewer than the limit, the conflict gets the brief above; once it has written the limit, arc writes no brief and the task stays in `review.rework_pending`.
`radian arc run` then prints `<task> conflict with main -> review.rework_pending: <reason>`, and `show` prints the same text as part of the `rework_reason`.
An attempt that does not end in a conflict sets the run back to `0`, and so does your own rework (`add_rework`), which is you stepping in; arc's review and conflict reworks do not.
`0` writes none, so the first conflict waits for you; `1` writes exactly one.
`show` prints the task's `retry_count`, not this run.
`auto` decides whether the next attempt starts by itself.

### A command failure and a moved main

A `commands` stage, or an integration key command, can fail because of the tree rather than the task: a flaky test, or a main that is broken itself.
When the remote main is no longer the main the failing verdict recorded, `radian arc run` re-verifies the task by itself, the way `radian arc reverify` does: it claims a new attempt without a worker and starts its gate in the background ([`run`](arc-loop.md#radian-arc-run)).

It applies only when all of these hold:

- the task's plan has `auto` other than `off`;
- the task is `review.rework_pending` because a `commands` or integration stage failed with reason `command_failed` or `timeout`;
- the remote main is not the `main_sha` the failing verdict recorded; an integration verdict records that main as `base_sha`.

A review stage failure, a conflict, a reject note, a failed delivery check, and a task the lead returned to `todo.ready` are never re-verified by themselves.

Two guards stop a loop:

- each `(head_sha, remote main)` pair is re-verified by itself at most once, and the pairs are kept in `runs/<task>/auto_reverify.json`. A main that does not move runs nothing again.
- [`limits.max_auto_reverify`](arc-files.md#configtoml) counts the automatic reverifies of one task. When the count is used up, the task stays in `review.rework_pending`, `radian arc run` prints one line with the reason, and `show` prints the same text as the `rework_reason`. The plan is not paused.

An automatic reverify starts no worker, is not a retry (it changes neither `retry_count` nor `max_attempts`), and does not count toward `auto_rework`.
A hand `radian arc reverify` ignores both guards and does not use up the count.

## A gate for this repository

Two bash scripts in this repository are the gate. A gate profile's `commands` call them:

```toml
[gate.profiles.default]
commands = [
  "/abs/path/to/repo/scripts/arc-gate.sh quick",
  "/abs/path/to/repo/scripts/arc-gate.sh full",
]
```

| Script | What it runs |
|---|---|
| `scripts/verify.sh [--log-dir <dir>]` | The full Verify of this repository: `cargo build -p radian-tui --bin radian --locked`, `cargo build -p agent-extensions --bin mcp-fixture --locked`, every command of the `Verify` block in `AGENTS.md`, then the three provider contract suites. It runs every step even after a failure, prints one `rc=<n> <seconds>s <command>` line per step, and keeps each step's output in `<dir>` (default `.radian/verify/`). It exits `0` only when every step passed, else `1`. |
| `scripts/arc-gate.sh quick` | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, the two binary builds, then `cargo nextest run --locked --no-fail-fast` for every crate under `crates/` that the checkout changed since its merge base with `origin/main`, plus `radian-tui` always. It stops at the first failing step. |
| `scripts/arc-gate.sh full` | `scripts/verify.sh`, here or on `ARC_GATE_HOST` when that is set. It prints the step lines and the names of failing tests, and exits with the verify exit code. |

Before their first step, `verify.sh` and `arc-gate.sh` check the file system that holds `${TMPDIR:-/tmp}` with `df`: `quick` always, `full` when it runs here. When fewer than 50,000 inodes or fewer than 2 GiB are free, they fail at once, before building anything, with exit code `1` and one line on stderr that names the path, the free and total inodes, the free space and the two limits, for example:

```
verify: /tmp has 0 of 1048576 inodes free (limit 50000) and 45 GiB free (limit 2 GiB); remove leaked test directories first
```

The check deletes nothing. A file system that reports no usable inode count (some macOS volumes report none, `-`, or a number too large for the shell to compare) skips the inode check and keeps the space check. A `df` that cannot read the file system skips that check with a warning. A negative free space (GNU `df` on ext4 when fewer bytes are free than the reserved-block count) is a real value and fails the space check; it is not read as unreadable. The two limits are the two variables below. For arc the failure is an ordinary failed command (`command_failed`), not a new failure class.

With `ARC_GATE_HOST` set, `full` ships `HEAD` as a git bundle (`HEAD --not origin/main`; no bundle when `HEAD` is already in `origin/main`). On that host it takes a blocking lock on `~/.arc-gate.lock`, fetches the bundle, checks out exactly that commit detached inside `ARC_GATE_TREE`, and runs that checkout's `scripts/verify.sh`. It does not check the local temp file system: the remote `verify.sh` checks the host's own `${TMPDIR:-/tmp}`, and the remote environment supplies both limits (they are not forwarded over ssh).

| Environment variable | Type | Default | What it does |
|---|---|---|---|
| `ARC_GATE_HOST` | string | unset: `full` runs in this checkout | The ssh host that runs the full gate. |
| `ARC_GATE_TREE` | string | unset | Required when `ARC_GATE_HOST` is set: the gate checkout on that host, as a path relative to the remote home directory. |
| `RADIAN_VERIFY_MIN_FREE_INODES` | integer | `50000` | A whole number, `0` or more and at most `999999999999999999` (18 digits after leading zeros); `0` turns the inode check off. The fewest free inodes on the temp file system at which `verify.sh` and `arc-gate.sh` start their first step. Unset or empty uses the default. It takes effect on the next run of either script, in the environment that script runs in (with `ARC_GATE_HOST` that is the remote environment). Any other value (not all digits, or above that bound) fails the check with a message naming the variable. |
| `RADIAN_VERIFY_MIN_FREE_GIB` | integer | `2` | A whole number, `0` or more and at most `999999999999999999` (18 digits after leading zeros); `0` turns the space check off. The fewest free GiB on the temp file system at which `verify.sh` and `arc-gate.sh` start their first step. Unset or empty uses the default. It takes effect on the next run of either script, in the environment that script runs in (with `ARC_GATE_HOST` that is the remote environment). Any other value (not all digits, or above that bound) fails the check with a message naming the variable. |

Neither script sets `CARGO_TARGET_DIR`: a build cache is the caller's setting.

**Trust.** arc runs a gate's `commands` inside a checkout of the task's own commit.
A relative `scripts/...` path there would name the task's own script, so a task could change its own gate.
Call both scripts by the main checkout's absolute path (`/abs/path/to/repo/scripts/arc-gate.sh`), never a relative path.

## Failures, retries and the budget

One attempt's result maps to one class and one next state.
The class is recorded in `runs/<task>/attempts/<attempt id>/attempt_result.json`.

| Result | Class | Next state |
|---|---|---|
| `completed` and pushed | — | `review.verifying`, with `base_sha` and `head_sha` |
| `completed`, but the wrapper's delivery check failed | `verification` | `review.rework_pending`; the reason is in the attempt result, arc writes no brief |
| `poisoned` | `model` | `failed.irrecoverable` (never retries) |
| `refused` | `model` | retry |
| `failed` | `model` | retry |
| `incomplete` | `model` | retry |
| `budget_exceeded` | `model` | retry |
| `cancelled` from inside the worker (not your `cancel`) | `model` | retry |
| `terminated` (no `Result`, a crash or a kill) | `infrastructure` | retry |
| an outcome this build does not know | `infrastructure` | retry |
| a wrapper exit `4` (the worktree lock is held) | `infrastructure` | retry |
| `stall` (the wrapper sent SIGTERM) | `stall` | retry |
| `wall_time` (the wrapper sent SIGTERM) | `wall_time` | retry |
| `cancelled` by `radian arc cancel` | `cancel` | `failed.cancelled` (never retries) |
| `reverify` (no worker: `radian arc reverify`) | `reverify` | `review.verifying`, verified at once |

A retry moves the task to `todo.retry_wait` with `retry_at` = now + a backoff: 10 seconds for the first retry, doubled for each later one, capped at 5 minutes.
`max_attempts` (the task's, else [`limits.max_attempts`](arc-files.md#configtoml)) counts retries, not attempts: every retry raises the task's `retry_count`, and when the count has reached the limit the task goes to `failed.exhausted` instead.
Every rework sets `retry_count` back to `0` (the lead's brief and arc's review rework), so the new instructions get their own retries.
A conflict rework keeps the count: it continues the same line of work, and the failures between conflicts then use up `max_attempts` even while a conflict keeps returning the task to `todo.ready`.
The attempt number is never compared with the limit; `show` prints `retry_count`.
A `reverify` attempt is not a retry: it changes neither `retry_count` nor the limit.
`todo.retry_wait` becomes `todo.ready` when `retry_at` has passed; the scheduling loop and `start` do this before they claim.
`start` also claims a `todo.retry_wait` task whose time has not come, without waiting for it (see [`start`](arc.md#radian-arc-start-task)).

Every attempt of a task runs in a worker session.
With [`worker.rework_session`](arc-files.md#configtoml) at `fresh` (the default) each attempt has a session of its own, `<task>-<attempt>`, and none is ever resumed; an attempt after the first gets the hand-off section (see [`start`](arc.md#radian-arc-start-task)).
With `resume` the session id is the task id: the first attempt that reaches the model creates it, and every later attempt, a retry or a rework, continues it.
An attempt that failed before the model started (a missing credential, a worktree that could not be prepared) leaves no session, so the next attempt creates it.
A bad `worker.exec_args` entry no longer gets that far: `start`, `run` and `confirm` refuse it before the claim.
The attempt's session id is decided by the claim, which writes it to `attempts/<task>/<attempt>/start.json` before the worker starts; the wrapper writes the session it used to `attempts/<task>/<attempt>/identity.json` when it spawns the worker and to `runs/<task>/attempts/<attempt id>/execution.json` when it ends.
A reader takes the first of those records that names one: `execution.json`, then `identity.json`, then `start.json`, so the worker's journal is found by its session id from the claim on, while the attempt still runs (see [`arc-files`](arc-files.md)).

Cost is counted per attempt.
`Result.cost` is the session total, so the attempt's cost is the difference from the previous attempt's session total when both ran in the same session (`resume`), and the whole session total when the attempt has a session of its own (`fresh`).
The delta is added to the task's `cost_usd` and its `total_cost_usd`; the attempt result records `cost_usd` and `session_cost_usd`.
A `reverify` attempt runs no worker and costs nothing, but a `review` stage it runs costs the same as any review.

A `review` stage's cost counts too: when arc applies a gate's verdicts, the sum of the attempt's `review_cost_micros` (every session the stage ran; a run that reported none adds `0`) is added to the task's `cost_usd` and `total_cost_usd`, and both budgets are checked then.
Both counters include every worker attempt and every review; `reset-budget` sets `cost_usd` back to `0` and leaves `total_cost_usd` alone.

When a retry is due and the budget is used up, the task goes to `todo.paused_budget` instead of `todo.retry_wait`:

- the task's `max_cost_usd`, else `[limits] max_cost_usd`;
- the plan's `max_cost_usd`, counted over every task of the plan. A plan budget also sets the plan's `auto` to `off`.

The same check runs when a review verdict is applied: a review cost that reaches the budget stops the task in `todo.paused_budget` after that gate, arc writes no automatic rework for the round, and the task waits for `reset-budget`.

`reset-budget` is the only way back.

## Reconcile

arc keeps no attempt state in memory.
Before `start` or `cancel` acts, and after a restart, it reconciles every task in `doing`:

- a wrapper that failed after its attempt directory existed wrote its own end record before it exited, whatever the failure: a refused argument, a worktree lock another attempt holds, a fetch that failed, a worktree that could not be prepared, or a worker that could not start. The record is applied at once: the attempt's concurrency slot is free in the next pass, the class is an infrastructure failure, and the retry rules apply. A retry is not counted as a rework;
- only a failure before those directories exist (a task id that names no path, a directory that cannot be created) leaves no record. That attempt is applied as an infrastructure failure when its heartbeat is older than the stall window, as below;
- the wrapper's `attempt_finished.json` is present and its token is current: the result is applied;
- the wrapper's last heartbeat (`attempts/<task>/<attempt>/heartbeat.json`) is younger than the attempt's stall window (its `stall_timeout_secs`, else `[limits] stall_timeout_secs`): the attempt is alive and left alone.
  The wrapper beats at most every 15 seconds, and at a third of the stall window when that is shorter, so three beats always fit inside the attempt's stall window; the gap between two beats is that interval.
  It beats from the moment the attempt directory exists until both end records are written; it keeps beating while the wrapper sets up the worktree, waits for the model, or checks and pushes the result.
  When both stall values are `0` the window is three beats, 45 seconds.
  A claim whose wrapper has not beaten yet is judged by its `start.json`;
- the heartbeat is older than that window and there is no result: the attempt is an `infrastructure` (terminated) failure and the retry rules above apply.

Reconcile never starts a task again.
It reads no process list; `ps` is refused inside the `workspace` sandbox and is never needed here.
`doing.reconciling` is left for you to resolve; `radian arc cancel` moves it to `failed.cancelled` after the same process check.
The one exception is a task of a `cancelled` plan: the plan is final, so the next `radian arc run` pass ends it at `failed.cancelled` after that same process check, and it stops counting against `[limits] concurrency`.
